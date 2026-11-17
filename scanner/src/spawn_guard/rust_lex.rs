//! The Rust lexer and attribute scanner behind [`super::spawn_exemptions_are_pinned`].

use super::SILENCERS;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Tok<'a> {
    Ident(&'a str),
    Punct(&'a str),
}

/// Rust source as a token stream with comments and string, char and raw
/// string literals dropped, so nothing inside them is matched. A byte
/// string `b"…"` lexes as the ident `b` plus a dropped string. Lifetimes
/// lex as an `'` punct plus an ident, which is all a lint scan needs.
fn tokens(src: &str) -> Vec<Tok<'_>> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let skip_quoted = |mut i: usize, q: u8| {
        while i < b.len() && b[i] != q {
            i += if b[i] == b'\\' { 2 } else { 1 };
        }
        i + 1
    };
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
        } else if b[i..].starts_with(b"//") {
            i = src[i..].find('\n').map_or(b.len(), |n| i + n);
        } else if b[i..].starts_with(b"/*") {
            // Bytes, not `&str` slices: this loop steps one byte at a
            // time, so `i` can sit inside a multibyte char here.
            let mut depth = 0;
            while i < b.len() {
                if b[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if b[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
        } else if c == b'"' {
            i = skip_quoted(i + 1, b'"');
        } else if (c == b'r' || c == b'b') && raw_string_hashes(&src[i..]).is_some() {
            let (prefix, hashes) = raw_string_hashes(&src[i..]).unwrap();
            let close = format!("\"{}", "#".repeat(hashes));
            let body = i + prefix;
            i = src[body..]
                .find(&close)
                .map_or(b.len(), |n| body + n + close.len());
        } else if c == b'\'' && is_char_literal(&src[i..]) {
            i = skip_quoted(i + 1, b'\'');
        } else if c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80 {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] >= 0x80) {
                i += 1;
            }
            out.push(Tok::Ident(&src[start..i]));
        } else if b[i..].starts_with(b"::") {
            out.push(Tok::Punct("::"));
            i += 2;
        } else {
            out.push(Tok::Punct(&src[i..i + 1]));
            i += 1;
        }
    }
    out
}

/// For `r"`, `r#"`, `br##"`…: (bytes up to and including the opening
/// quote, number of hashes).
fn raw_string_hashes(s: &str) -> Option<(usize, usize)> {
    let rest = s.strip_prefix("br").or_else(|| s.strip_prefix('r'))?;
    let hashes = rest.bytes().take_while(|&c| c == b'#').count();
    (rest.as_bytes().get(hashes) == Some(&b'"'))
        .then(|| (s.len() - rest.len() + hashes + 1, hashes))
}

/// A `'` opens a char literal, not a lifetime, when the one char after
/// it is followed by a closing `'`: `'a'`, `'é'`, or an escape such as
/// `'\n'` or `'\''`. `s` starts at the opening quote.
fn is_char_literal(s: &str) -> bool {
    let mut chars = s[1..].chars();
    match chars.next() {
        Some('\\') => true,
        Some(_) => chars.next() == Some('\''),
        None => false,
    }
}

/// One silencing attribute found in a file.
#[derive(Debug, PartialEq)]
pub(super) struct Silencer {
    pub(super) lint: String,
    pub(super) item: String,
}

/// Every lint attribute in `src` that names a [`SILENCERS`] entry inside
/// an `allow(` or `expect(`, anywhere in the attribute (so `cfg_attr`
/// counts), with the item it applies to.
pub(super) fn silencers(src: &str) -> Vec<Silencer> {
    let t = tokens(src);
    let mut found = Vec::new();
    let mut i = 0;
    while i < t.len() {
        if t[i] != Tok::Punct("#") {
            i += 1;
            continue;
        }
        let inner = t.get(i + 1) == Some(&Tok::Punct("!"));
        let open = i + 1 + usize::from(inner);
        if t.get(open) != Some(&Tok::Punct("[")) {
            i += 1;
            continue;
        }
        let Some(close) = matching(&t, open) else {
            break;
        };
        let body = &t[open + 1..close];
        let silences = body
            .windows(2)
            .any(|w| matches!(w, [Tok::Ident("allow" | "expect"), Tok::Punct("(")]));
        if silences {
            for lint in SILENCERS {
                let hit = body.windows(lint.len()).any(|w| {
                    w.iter().zip(lint.iter()).all(
                        |(tok, want)| matches!(tok, Tok::Ident(s) | Tok::Punct(s) if s == want),
                    )
                });
                if hit {
                    let item = if inner {
                        "<inner attribute>".to_string()
                    } else {
                        item_after(&t, close + 1)
                    };
                    found.push(Silencer {
                        lint: lint.concat(),
                        item,
                    });
                }
            }
        }
        i = close + 1;
    }
    found
}

/// Index of the bracket closing the one at `open`.
fn matching(t: &[Tok<'_>], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (k, tok) in t.iter().enumerate().skip(open) {
        match tok {
            Tok::Punct("[" | "(" | "{") => depth += 1,
            Tok::Punct("]" | ")" | "}") => {
                depth -= 1;
                if depth == 0 {
                    return Some(k);
                }
            }
            _ => {}
        }
    }
    None
}

/// The item an outer attribute ending before `k` applies to, skipping
/// any further attributes and the visibility and qualifier keywords:
/// `fn name`, or the keyword and name of whatever else is there.
fn item_after(t: &[Tok<'_>], mut k: usize) -> String {
    while t.get(k) == Some(&Tok::Punct("#")) && t.get(k + 1) == Some(&Tok::Punct("[")) {
        match matching(t, k + 1) {
            Some(c) => k = c + 1,
            None => return "<end of file>".to_string(),
        }
    }
    if t.get(k) == Some(&Tok::Ident("pub")) {
        k += 1;
        if t.get(k) == Some(&Tok::Punct("(")) {
            k = matching(t, k).map_or(t.len(), |c| c + 1);
        }
    }
    // `const`/`unsafe`/`async`/`extern` qualify a fn only when the run of
    // them ends in `fn`; otherwise `const X` is the item. An `extern`
    // ABI string is already gone from the token stream.
    let mut q = k;
    while matches!(
        t.get(q),
        Some(Tok::Ident("async" | "const" | "unsafe" | "extern"))
    ) {
        q += 1;
    }
    if q > k && t.get(q) == Some(&Tok::Ident("fn")) {
        k = q;
    }
    match (t.get(k), t.get(k + 1)) {
        (Some(Tok::Ident(kw)), Some(Tok::Ident(name))) => format!("{kw} {name}"),
        (Some(Tok::Ident(kw)), _) => format!("<{kw} …>"),
        (Some(Tok::Punct(p)), _) => format!("<{p} …>"),
        (None, _) => "<end of file>".to_string(),
    }
}

/// Non-ASCII text inside a block comment is skipped with the comment and
/// never splits a char, so the silencer after it is still found.
#[test]
fn block_comments_may_hold_multibyte_text() {
    let src =
        "/* café — 😀 /* nested é */ ü */\n@[allow(clippy::all)]\nfn x() {}".replace('@', "#");
    let found: Vec<_> = silencers(&src)
        .into_iter()
        .map(|s| (s.lint, s.item))
        .collect();
    assert_eq!(found, vec![("clippy::all".to_string(), "fn x".to_string())]);
}

/// A char literal is told from a lifetime by its decoded first char, so
/// a multibyte char next to more multibyte text still closes where Rust
/// closes it.
#[test]
fn is_char_literal_decodes_whole_chars() {
    for lit in ["'a'", "'é',ü)", "'😀'", "'\\''", "'\\u{1F600}'", "'\"'"] {
        assert!(is_char_literal(lit), "{lit:?} is a char literal");
    }
    for lifetime in ["'a>", "'static str", "'é>", "'"] {
        assert!(!is_char_literal(lifetime), "{lifetime:?} is not");
    }
}

/// The scanner sees each silencer spelling, and nothing that only looks
/// like one. Fixtures are built at runtime so this file's own text holds
/// no attribute for `spawn_exemptions_are_pinned` to find.
#[test]
fn silencers_sees_every_spelling() {
    let attr = |s: &str| s.replace('@', "#");
    let lints = |src: &str| -> Vec<(String, String)> {
        silencers(&attr(src))
            .into_iter()
            .map(|s| (s.lint, s.item))
            .collect()
    };
    let one = |lint: &str, item: &str| vec![(lint.to_string(), item.to_string())];
    let dm = "clippy::disallowed_methods";

    assert_eq!(
        lints("@[allow(clippy::disallowed_methods)]\nfn a() {}"),
        one(dm, "fn a")
    );
    assert_eq!(
        lints("@ [ expect ( clippy :: disallowed_methods ) ]\npub(crate) fn b() {}"),
        one(dm, "fn b")
    );
    assert_eq!(
        lints("@[cfg_attr(test, allow(clippy::disallowed_methods))]\nfn c() {}"),
        one(dm, "fn c")
    );
    assert_eq!(
        lints("@[expect(reason = \"see @588 ]\", clippy::disallowed_methods)]\nfn d() {}"),
        one(dm, "fn d")
    );
    assert_eq!(
        lints("@[allow(clippy::disallowed_methods)]\n@[test]\n// note\nasync unsafe fn e() {}"),
        one(dm, "fn e")
    );
    assert_eq!(
        lints("@[allow(clippy::disallowed_methods)]\nconst X: u8 = 0;\nfn npx() {}"),
        one(dm, "const X")
    );
    assert_eq!(
        lints("@[allow(clippy::disallowed_methods)]\nmod m {}"),
        one(dm, "mod m")
    );
    assert_eq!(
        lints("@![allow(clippy::all)]"),
        one("clippy::all", "<inner attribute>")
    );
    assert_eq!(
        lints("@[allow(clippy::style)]\nfn f() {}"),
        one("clippy::style", "fn f")
    );
    assert_eq!(
        lints("@[expect(warnings)]\nfn g() {}"),
        one("warnings", "fn g")
    );
    assert_eq!(
        lints("@[allow(renamed_and_removed_lints, clippy::disallowed_method)]\nfn r() {}"),
        one("clippy::disallowed_method", "fn r")
    );

    // A literal that could swallow what follows it must end where Rust
    // ends it, so the real silencer after it is still found.
    for (lead, item) in [
        ("const Q: char = '\"';", "fn q"),
        ("const E: char = '\\'';", "fn q"),
        ("const U: char = 'é'; const V: char = '😀';", "fn q"),
        ("const B: &[u8] = b\"]\";", "fn q"),
        ("const R: &str = r@\"\"]\"@;", "fn q"),
        ("const X: &[u8] = br@\"\"]\"@;", "fn q"),
        ("const L: &str = \"\\\"]\";", "fn q"),
        ("fn lt<'a>(_: &'a str) {}", "fn q"),
    ] {
        assert_eq!(
            lints(&format!(
                "{lead}\n@[allow(clippy::disallowed_methods)]\nfn q() {{}}"
            )),
            one(dm, item),
            "missed the silencer after {lead:?}"
        );
    }

    // Not silencers: comments, strings, other lints, other attributes.
    for quiet in [
        "// @[allow(clippy::disallowed_methods)]\nfn h() {}",
        "/* @[allow(clippy::all)] */ fn i() {}",
        "const S: &str = \"@[allow(clippy::disallowed_methods)]\";",
        "const R: &str = r@\"@[allow(warnings)]\"@;",
        "@[allow(unused_warnings)]\nfn j() {}",
        "@[allow(clippy::all_the_things)]\nfn k() {}",
        "@[deny(clippy::disallowed_methods)]\nfn l() {}",
        "@[doc = \"allow(clippy::disallowed_methods)\"]\nfn m() {}",
        "fn n<'a>(x: &'a str) -> char { let _ = x; 'x' }",
        "/* outer /* @[allow(clippy::all)] */ still comment @[allow(warnings)] */ fn o() {}",
        "const B: &[u8] = b\"@[allow(clippy::all)]\";",
        "const H: &str = r@@\"\"@ @[allow(warnings)] \"@@;",
        "const Y: &[u8] = br@\"\" @[allow(warnings)] \"@;",
    ] {
        assert_eq!(lints(quiet), Vec::new(), "false positive on {quiet:?}");
    }
}
