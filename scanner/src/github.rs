//! A thin GitHub API client over the crate's existing `reqwest`: REST with
//! `Link`-header pagination, and GraphQL. It replaces the `gh` CLI the
//! scanner used to shell out to (#532), so delivery needs no binary beyond
//! `git` on the runner.
//!
//! The delivery code that calls it is synchronous, so every request goes
//! through [`block`], which parks the calling worker thread of the ambient
//! multi-thread runtime (`#[tokio::main]` in production). The connection
//! pool then lives on that one runtime for the client's whole life.

use std::future::Future;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use reqwest::{Method, RequestBuilder, Response, header};
use serde_json::Value;

use crate::adapters::{TlsRoots, http_client};

/// Per-request ceiling. Longer than the adapters' default: a
/// `createCommitOnBranch` carries whole files.
const TIMEOUT: Duration = Duration::from_secs(60);

/// How much of an error response's body an error message keeps.
const ERROR_BODY_CHARS: usize = 500;

const DEFAULT_API_URL: &str = "https://api.github.com";

/// An authenticated client for one repository.
pub struct GitHub {
    client: reqwest::Client,
    /// REST base, no trailing slash. Pagination never leaves it, so the
    /// token is only ever sent here and to [`Self::graphql_url`].
    api: String,
    graphql_url: String,
    token: String,
    owner: String,
    repo: String,
}

/// Hand-written so the token never reaches a log through `{:?}`.
impl std::fmt::Debug for GitHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHub")
            .field("api", &self.api)
            .field("graphql_url", &self.graphql_url)
            .field("token", &"<redacted>")
            .field("owner", &self.owner)
            .field("repo", &self.repo)
            .finish()
    }
}

impl GitHub {
    /// The production client: the token from `GH_TOKEN`/`GITHUB_TOKEN`,
    /// the repository from `GITHUB_REPOSITORY` or `repo_root`'s `origin`,
    /// and the endpoints from `GITHUB_API_URL`/`GITHUB_GRAPHQL_URL`
    /// (Actions sets all three) or github.com.
    pub fn discover(repo_root: &Path) -> Result<Self> {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let token = token_from(env)?;
        let (owner, repo) = match env("GITHUB_REPOSITORY") {
            Some(nwo) => split_owner_repo(&nwo)?,
            None => split_owner_repo(&origin_url(repo_root)?)?,
        };
        let api = env("GITHUB_API_URL").unwrap_or_else(|| DEFAULT_API_URL.to_string());
        let client = http_client(TIMEOUT, TlsRoots::System)?;
        let mut gh = Self::with_base(client, &api, token, owner, repo);
        if let Some(graphql) = env("GITHUB_GRAPHQL_URL") {
            gh.graphql_url = graphql;
        }
        Ok(gh)
    }

    /// A client against `api` (its GraphQL endpoint at `{api}/graphql`),
    /// for tests that point it at a mock server.
    pub fn with_base(
        client: reqwest::Client,
        api: &str,
        token: String,
        owner: String,
        repo: String,
    ) -> Self {
        let api = api.trim_end_matches('/').to_string();
        GitHub {
            client,
            graphql_url: format!("{api}/graphql"),
            api,
            token,
            owner,
            repo,
        }
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn repo(&self) -> &str {
        &self.repo
    }

    /// `{api}/repos/{owner}/{repo}/{suffix}`.
    fn repo_url(&self, suffix: &str) -> String {
        format!("{}/repos/{}/{}/{suffix}", self.api, self.owner, self.repo)
    }

    fn authed(&self, method: Method, url: &str) -> RequestBuilder {
        self.client
            .request(method, url)
            .bearer_auth(&self.token)
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    /// Send a REST request against the repository and return its JSON
    /// body (`Null` for an empty one). A non-2xx status is an error.
    pub fn rest(&self, method: Method, suffix: &str, body: Option<&Value>) -> Result<Value> {
        let url = self.repo_url(suffix);
        let mut req = self.authed(method.clone(), &url);
        if let Some(body) = body {
            req = req.json(body);
        }
        block(async {
            let resp = send(req, &method, &url).await?;
            let bytes = resp
                .bytes()
                .await
                .with_context(|| format!("{method} {url}: reading the response"))?;
            if bytes.is_empty() {
                return Ok(Value::Null);
            }
            serde_json::from_slice(&bytes).with_context(|| format!("{method} {url}: parsing JSON"))
        })?
    }

    /// GET a list endpoint of the repository, following `Link: rel="next"`
    /// until it ends or `max_items` rows are in hand. Callers that must not
    /// act on a partial listing compare the length against `max_items`.
    pub fn list(&self, suffix: &str, max_items: usize) -> Result<Vec<Value>> {
        let mut next = Some(self.repo_url(suffix));
        let mut rows = Vec::new();
        while let Some(url) = next.take() {
            ensure!(
                url.starts_with(&format!("{}/", self.api)),
                "refusing to follow a pagination link off the API: {url}"
            );
            let req = self.authed(Method::GET, &url);
            let (page, link) = block(async {
                let resp = send(req, &Method::GET, &url).await?;
                let link = resp
                    .headers()
                    .get(header::LINK)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                let page: Vec<Value> = resp
                    .json()
                    .await
                    .with_context(|| format!("GET {url}: parsing a JSON array"))?;
                anyhow::Ok((page, link))
            })??;
            let empty = page.is_empty();
            rows.extend(page);
            if empty || rows.len() >= max_items {
                break;
            }
            next = link.as_deref().and_then(next_link);
        }
        Ok(rows)
    }

    /// POST a GraphQL request. A response carrying `errors` is an error.
    pub fn graphql(&self, request: &Value) -> Result<Value> {
        let url = self.graphql_url.clone();
        let req = self.authed(Method::POST, &url).json(request);
        let v: Value = block(async {
            let resp = send(req, &Method::POST, &url).await?;
            resp.json().await.context("parsing a GraphQL response")
        })??;
        if let Some(errors) = v.get("errors") {
            bail!("GraphQL errors: {errors}");
        }
        Ok(v)
    }
}

/// Send `req`, turning a transport failure or a non-2xx status into an
/// error naming the request. The token is a header, so it is never in
/// the message; the body is truncated.
async fn send(req: RequestBuilder, method: &Method, url: &str) -> Result<Response> {
    let resp = req
        .send()
        .await
        .with_context(|| format!("{method} {url}"))?;
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    let body: String = body.chars().take(ERROR_BODY_CHARS).collect();
    bail!("{method} {url} returned {status}: {body}")
}

/// Run `fut` to completion from synchronous code inside a multi-thread
/// tokio runtime. Any other context is an error rather than a panic: a
/// current-thread runtime cannot be blocked, and a runtime made here would
/// strand the client's pooled connections when it dropped.
fn block<F: Future>(fut: F) -> Result<F::Output> {
    use tokio::runtime::{Handle, RuntimeFlavor};
    let handle =
        Handle::try_current().context("the GitHub client must be called inside a tokio runtime")?;
    ensure!(
        handle.runtime_flavor() == RuntimeFlavor::MultiThread,
        "the GitHub client needs a multi-thread tokio runtime"
    );
    Ok(tokio::task::block_in_place(|| handle.block_on(fut)))
}

/// The first token variable that is set and non-empty, in `gh`'s order of
/// precedence.
fn token_from(get: impl Fn(&str) -> Option<String>) -> Result<String> {
    crate::validate::TOKEN_VARS
        .iter()
        .find_map(|var| get(var))
        .with_context(|| {
            format!(
                "no GitHub token: set one of {}",
                crate::validate::TOKEN_VARS.join(", ")
            )
        })
}

fn origin_url(repo_root: &Path) -> Result<String> {
    let out = Command::new("git")
        .current_dir(repo_root)
        .args(["remote", "get-url", "origin"])
        .output()
        .context("running `git remote get-url origin`")?;
    ensure!(
        out.status.success(),
        "`git remote get-url origin` failed (set GITHUB_REPOSITORY instead): {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `owner/repo` out of `owner/repo` or a remote URL in any of the
/// `https://host/o/r(.git)`, `ssh://git@host/o/r`, `git@host:o/r` forms.
fn split_owner_repo(s: &str) -> Result<(String, String)> {
    let trimmed = s.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let mut parts = trimmed.rsplit(['/', ':']);
    let repo = parts.next().unwrap_or_default();
    let owner = parts.next().unwrap_or_default();
    let valid = |p: &str| {
        // A whole `.`/`..` segment would be collapsed by URL parsing and
        // aim the token at another API path; a leading dot (`.github`) is fine.
        !matches!(p, "" | "." | "..")
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    ensure!(
        valid(owner) && valid(repo),
        "cannot read an owner/repo out of {s:?}"
    );
    Ok((owner.to_string(), repo.to_string()))
}

/// `ref` with each `/`-separated segment percent-encoded, for a REST path.
/// Only unreserved characters pass through, so no ref name can add a query
/// or a fragment.
///
/// An empty, `.` or `..` segment is refused rather than encoded: URL parsing
/// (WHATWG, as the `url` crate under reqwest does it) decodes `%2E%2E` back
/// into a dot segment and collapses it, so encoding cannot stop the request
/// naming another ref. Git forbids all three in a ref name anyway.
pub fn encode_ref(r: &str) -> Result<String> {
    r.split('/')
        .map(|seg| {
            ensure!(
                !matches!(seg, "" | "." | ".."),
                "refusing ref {r:?}: empty or dot path segment"
            );
            Ok(seg
                .bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                        (b as char).to_string()
                    }
                    _ => format!("%{b:02X}"),
                })
                .collect::<String>())
        })
        .collect::<Result<Vec<_>>>()
        .map(|segs| segs.join("/"))
}

/// The `rel="next"` target of a `Link` header.
fn next_link(link: &str) -> Option<String> {
    link.split(',').find_map(|part| {
        let (target, params) = part.split_once(';')?;
        params
            .split(';')
            .any(|p| p.trim() == r#"rel="next""#)
            .then(|| {
                target
                    .trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_string()
            })
    })
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use wiremock::MockServer;

    pub(crate) const TOKEN: &str = "test-token-s3cret";

    /// A client for `o/r` against `server`, trusting no TLS roots.
    pub(crate) fn client_for(server: &MockServer) -> GitHub {
        let client = http_client(TIMEOUT, TlsRoots::None).unwrap();
        GitHub::with_base(
            client,
            &server.uri(),
            TOKEN.to_string(),
            "o".to_string(),
            "r".to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{TOKEN, client_for};
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_json, header as h, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test(flavor = "multi_thread")]
    async fn rest_sends_auth_and_api_headers_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/repos/o/r/pulls/7"))
            .and(h("authorization", format!("Bearer {TOKEN}").as_str()))
            .and(h("accept", "application/vnd.github+json"))
            .and(h("x-github-api-version", "2022-11-28"))
            .and(body_json(json!({"title": "t"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"html_url": "u"})))
            .expect(1)
            .mount(&server)
            .await;
        let v = client_for(&server)
            .rest(Method::PATCH, "pulls/7", Some(&json!({"title": "t"})))
            .unwrap();
        assert_eq!(v["html_url"], "u");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rest_empty_body_is_null() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let v = client_for(&server)
            .rest(Method::DELETE, "git/refs/heads/x", None)
            .unwrap();
        assert_eq!(v, Value::Null);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn error_names_status_and_never_the_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403).set_body_string("Bad credentials"))
            .mount(&server)
            .await;
        let gh = client_for(&server);
        for err in [
            gh.rest(Method::GET, "pulls", None).unwrap_err(),
            gh.list("pulls", 10).unwrap_err(),
        ] {
            let msg = format!("{err:#}");
            assert!(msg.contains("403"), "{msg}");
            assert!(msg.contains("Bad credentials"), "{msg}");
            assert!(!msg.contains(TOKEN), "token leaked: {msg}");
        }
        assert!(!format!("{gh:?}").contains(TOKEN));
    }

    /// Two pages chained by `Link`, the second without a `next`.
    async fn two_pages(server: &MockServer) {
        let page2 = format!("{}/repos/o/r/pulls?page=2", server.uri());
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls"))
            .and(query_param("page", "2"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!([{"n": 3}]))
                    .insert_header("link", r#"<https://x.test/p1>; rel="prev""#),
            )
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!([{"n": 1}, {"n": 2}]))
                    .insert_header(
                        "link",
                        format!(r#"<{page2}>; rel="next", <{page2}>; rel="last""#),
                    ),
            )
            .mount(server)
            .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_follows_next_links_to_the_end() {
        let server = MockServer::start().await;
        two_pages(&server).await;
        let rows = client_for(&server).list("pulls", 100).unwrap();
        let ns: Vec<_> = rows.iter().map(|r| r["n"].as_u64().unwrap()).collect();
        assert_eq!(ns, [1, 2, 3]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_stops_at_max_items() {
        let server = MockServer::start().await;
        two_pages(&server).await;
        let rows = client_for(&server).list("pulls", 2).unwrap();
        assert_eq!(rows.len(), 2);
        // Not just truncated after the fact: page 2 is never requested.
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "the second page must not be fetched");
    }

    /// A `next` link to another host would carry the token there.
    #[tokio::test(flavor = "multi_thread")]
    async fn list_refuses_a_next_link_off_the_api() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!([{"n": 1}]))
                    .insert_header("link", r#"<https://evil.test/steal>; rel="next""#),
            )
            .mount(&server)
            .await;
        let err = client_for(&server).list("pulls", 100).unwrap_err();
        assert!(format!("{err:#}").contains("off the API"), "{err:#}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn graphql_errors_are_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_json(json!({"query": "q"})))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"errors": [{"message": "nope"}]})),
            )
            .mount(&server)
            .await;
        let err = client_for(&server)
            .graphql(&json!({"query": "q"}))
            .unwrap_err();
        assert!(format!("{err:#}").contains("nope"), "{err:#}");
    }

    #[test]
    fn calls_outside_a_runtime_error_instead_of_panicking() {
        let gh = GitHub::with_base(
            http_client(TIMEOUT, TlsRoots::None).unwrap(),
            "http://127.0.0.1:9",
            TOKEN.into(),
            "o".into(),
            "r".into(),
        );
        let err = gh.rest(Method::GET, "pulls", None).unwrap_err();
        assert!(format!("{err:#}").contains("tokio runtime"), "{err:#}");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt.block_on(async { gh.rest(Method::GET, "pulls", None).unwrap_err() });
        assert!(format!("{err:#}").contains("multi-thread"), "{err:#}");
    }

    #[test]
    fn token_prefers_gh_token() {
        let both = |k: &str| Some(format!("{k}-value"));
        assert_eq!(token_from(both).unwrap(), "GH_TOKEN-value");
        let only_github = |k: &str| (k == "GITHUB_TOKEN").then(|| "g".to_string());
        assert_eq!(token_from(only_github).unwrap(), "g");
        assert!(token_from(|_| None).is_err());
    }

    #[test]
    fn owner_repo_from_every_remote_form() {
        for s in [
            "o/r",
            "https://github.com/o/r",
            "https://github.com/o/r.git",
            "https://github.com/o/r/",
            "git@github.com:o/r.git",
            "ssh://git@github.com/o/r.git",
        ] {
            assert_eq!(
                split_owner_repo(s).unwrap(),
                ("o".to_string(), "r".to_string()),
                "{s}"
            );
        }
        assert_eq!(
            split_owner_repo("o/.github").unwrap(),
            ("o".to_string(), ".github".to_string())
        );
        for bad in [
            "",
            "r",
            "https://github.com/",
            "o/r?x=1",
            "../..",
            "o/..",
            "./r",
            "git@github.com:o/..git",
        ] {
            assert!(split_owner_repo(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn next_link_reads_only_rel_next() {
        assert_eq!(
            next_link(r#"<https://a/1>; rel="prev", <https://a/3>; rel="next""#).as_deref(),
            Some("https://a/3")
        );
        assert_eq!(next_link(r#"<https://a/1>; rel="last""#), None);
    }

    #[test]
    fn encode_ref_keeps_slashes_and_escapes_the_rest() {
        let enc = |r: &str| encode_ref(r).unwrap();
        assert_eq!(enc("scan/rust/2026-11-09"), "scan/rust/2026-11-09");
        assert_eq!(enc("scanner-staging/v1.2/abc"), "scanner-staging/v1.2/abc");
        assert_eq!(enc("scan/x#y"), "scan/x%23y");
        assert_eq!(enc("a%2Fb?c"), "a%252Fb%3Fc");
        assert_eq!(enc("a b"), "a%20b");
        assert_eq!(enc("\u{e9}"), "%C3%A9");
        // A dot inside a segment is fine; only whole dot segments collapse.
        assert_eq!(enc("a/.b/c."), "a/.b/c.");
    }

    /// Encoding cannot protect a dot segment: URL parsing decodes and
    /// collapses `%2E%2E`. So these are refused, never sent.
    #[test]
    fn encode_ref_refuses_dot_and_empty_segments() {
        for bad in ["a/../b", "a/./b", "..", ".", "a//b", "/a", "a/", ""] {
            let err = encode_ref(bad).unwrap_err();
            assert!(
                format!("{err:#}").contains("dot path segment"),
                "{bad:?}: {err:#}"
            );
        }
    }
}
