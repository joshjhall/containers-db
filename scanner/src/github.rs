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
use reqwest::{Method, RequestBuilder, Response, StatusCode, header};
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
        Self::discover_with(env_getter(|k| std::env::var(k).ok()), repo_root)
    }

    /// [`Self::discover`] with every variable read through `get` (unset
    /// and empty both `None`), so tests can drive it without touching
    /// the process environment.
    fn discover_with(get: impl Fn(&str) -> Option<String>, repo_root: &Path) -> Result<Self> {
        let token = token_from(&get)?;
        let (owner, repo) = match get("GITHUB_REPOSITORY") {
            Some(nwo) => split_owner_repo(&nwo)?,
            None => split_owner_repo(&origin_url(repo_root)?)?,
        };
        let (api, graphql) = endpoints_from(&get)?;
        let client = http_client(TIMEOUT, TlsRoots::System)?;
        let mut gh = Self::with_base(client, &api, token, owner, repo);
        gh.graphql_url = graphql;
        Ok(gh)
    }

    /// A client against `api` (its GraphQL endpoint derived by
    /// [`default_graphql_url`]), for tests that point it at a mock server.
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
            graphql_url: default_graphql_url(&api),
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

/// A request GitHub answered with a non-2xx status. It travels inside the
/// `anyhow::Error` the client returns, so a caller that must tell a
/// permanent refusal from a retryable one can `downcast_ref` it rather than
/// parse the message (#550).
#[derive(Debug, thiserror::Error)]
#[error("{method} {url} returned {status}: {body}")]
pub struct HttpStatusError {
    pub method: Method,
    pub url: String,
    pub status: StatusCode,
    /// The response body, truncated to [`ERROR_BODY_CHARS`].
    pub body: String,
}

/// Send `req`, turning a transport failure or a non-2xx status into an
/// error naming the request; the latter is an [`HttpStatusError`]. The
/// token is a header, so it is never in the message; the body is truncated.
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
    Err(HttpStatusError {
        method: method.clone(),
        url: url.to_string(),
        status,
        body: body.chars().take(ERROR_BODY_CHARS).collect(),
    }
    .into())
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

/// The REST and GraphQL endpoints from `GITHUB_API_URL` and
/// `GITHUB_GRAPHQL_URL` (defaulting to github.com and
/// [`default_graphql_url`]),
/// refused unless the token can safely go to both: `https` (plain `http`
/// only to a loopback host, for tests), no userinfo, and one origin for
/// the pair. Errors name the variable (and at most the scheme), never any
/// part of its value, which may carry credentials. The pair comes back
/// normalized (default port dropped, host lowercased) so it matches the
/// canonical URLs the API echoes in pagination `Link` headers.
fn endpoints_from(get: impl Fn(&str) -> Option<String>) -> Result<(String, String)> {
    let api = get("GITHUB_API_URL").unwrap_or_else(|| DEFAULT_API_URL.to_string());
    let api = api.trim_end_matches('/').to_string();
    let api_url = checked_endpoint("GITHUB_API_URL", &api)?;
    let api = api_url.as_str().trim_end_matches('/').to_string();
    // Derive from the parsed form so the default sees the same normalized
    // path (`\` separators, dot segments) that is returned for the API.
    let graphql = get("GITHUB_GRAPHQL_URL").unwrap_or_else(|| default_graphql_url(&api));
    let graphql_url = checked_endpoint("GITHUB_GRAPHQL_URL", &graphql)?;
    ensure!(
        api_url.origin() == graphql_url.origin(),
        "GITHUB_GRAPHQL_URL is not on the same origin (scheme, host and port) as \
         GITHUB_API_URL; refusing to send the token to it"
    );
    Ok((api, graphql_url.into()))
}

/// The GraphQL endpoint for the REST base `api` when none is configured.
/// GitHub Enterprise Server serves REST at `/api/v3` but GraphQL at
/// `/api/graphql`, so a path ending in the two segments `/api/v3` has
/// its `v3` swapped for `graphql`. Any other base gets `{api}/graphql`:
/// github.com, a mock server, or a proxy that mounts the API at a bare
/// `/v3`. Only the path is inspected, so a host named `api` or `v3` is
/// left alone.
fn default_graphql_url(api: &str) -> String {
    let api = api.trim_end_matches('/');
    let authority = api.find("://").map_or(0, |i| i + 3);
    let path = api[authority..]
        .find('/')
        .map_or("", |i| &api[authority + i..]);
    match path.strip_suffix("/api/v3") {
        Some(_) => format!("{}graphql", &api[..api.len() - "v3".len()]),
        None => format!("{api}/graphql"),
    }
}

/// Parse `value` (from `var`) and check it is a safe place for the token.
fn checked_endpoint(var: &str, value: &str) -> Result<reqwest::Url> {
    let url =
        reqwest::Url::parse(value).map_err(|e| anyhow::anyhow!("{var} is not a valid URL: {e}"))?;
    let host = url
        .host_str()
        .with_context(|| format!("{var} has no host"))?
        .to_string();
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "{var} must not carry credentials in the URL"
    );
    // A query or fragment would be logged with every request error and
    // would swallow the paths built onto the base by string concatenation.
    ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "{var} must not carry a query or fragment"
    );
    match url.scheme() {
        "https" => {}
        "http" if is_loopback(&host) => {}
        scheme => bail!(
            "{var} must use https (plain http only to a loopback host), got {scheme}; \
             refusing to send the token to it"
        ),
    }
    Ok(url)
}

/// `localhost`, `127.0.0.0/8` or `::1` — the hosts a mock server binds.
fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// The getter [`GitHub::discover`] hands to `discover_with`: `raw` with an
/// empty value read as unset, so `GITHUB_REPOSITORY=` falls back to
/// `origin` instead of failing to split. Tests wrap a fixed env in it, so
/// they exercise the same filter production does.
fn env_getter(raw: impl Fn(&str) -> Option<String>) -> impl Fn(&str) -> Option<String> {
    move |k| raw(k).filter(|v| !v.is_empty())
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

    /// #550: only a status GitHub actually returned is an
    /// [`HttpStatusError`]. A request that never got an answer must not
    /// downcast to one, or a caller could read it as a permanent refusal.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_transport_failure_is_not_an_http_status_error() {
        // Bind then drop a listener: the port is free, so connecting is
        // refused rather than answered.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let gh = GitHub::with_base(
            http_client(TIMEOUT, TlsRoots::None).unwrap(),
            &format!("http://127.0.0.1:{port}"),
            TOKEN.to_string(),
            "o".to_string(),
            "r".to_string(),
        );
        let err = gh.list("pulls", 10).unwrap_err();
        assert!(
            err.downcast_ref::<reqwest::Error>().is_some(),
            "expected a transport error, got: {err:#}"
        );
        assert!(err.downcast_ref::<HttpStatusError>().is_none(), "{err:#}");
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
            let typed = err
                .downcast_ref::<HttpStatusError>()
                .expect("a non-2xx status is a typed HttpStatusError (#550)");
            assert_eq!(typed.status, StatusCode::FORBIDDEN);
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

    #[tokio::test(flavor = "multi_thread")]
    async fn graphql_returns_the_parsed_body_on_success() {
        let server = MockServer::start().await;
        let answer = json!({"data": {"repository": {"id": "R_1"}}});
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_json(json!({"query": "q"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(&answer))
            .expect(1)
            .mount(&server)
            .await;
        let v = client_for(&server).graphql(&json!({"query": "q"})).unwrap();
        assert_eq!(v, answer);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rest_invalid_json_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        let err = client_for(&server)
            .rest(Method::GET, "pulls", None)
            .unwrap_err();
        assert!(format!("{err:#}").contains("parsing JSON"), "{err:#}");
    }

    /// A page that comes back empty ends the listing even when it still
    /// carries a `next` link; following it could loop forever.
    #[tokio::test(flavor = "multi_thread")]
    async fn list_stops_on_an_empty_page() {
        let server = MockServer::start().await;
        let page = |n: u32| format!("{}/repos/o/r/pulls?page={n}", server.uri());
        let next = |n: u32| format!(r#"<{}>; rel="next""#, page(n));
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls"))
            .and(query_param("page", "3"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"n": 9}])))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls"))
            .and(query_param("page", "2"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!([]))
                    .insert_header("link", next(3)),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!([{"n": 1}]))
                    .insert_header("link", next(2)),
            )
            .mount(&server)
            .await;
        let rows = client_for(&server).list("pulls", 100).unwrap();
        assert_eq!(rows, [json!({"n": 1})]);
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2, "page 3 must not be fetched");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_stops_without_a_link_header() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"n": 1}, {"n": 2}])))
            .mount(&server)
            .await;
        let rows = client_for(&server).list("pulls", 100).unwrap();
        assert_eq!(rows, [json!({"n": 1}), json!({"n": 2})]);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
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

    /// An env getter over fixed pairs, so no test mutates the process env.
    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn endpoints_default_to_github_com() {
        let (api, graphql) = endpoints_from(env_of(&[])).unwrap();
        assert_eq!(api, "https://api.github.com");
        assert_eq!(graphql, "https://api.github.com/graphql");
    }

    #[test]
    fn endpoints_accept_github_com_and_ghes_pairs() {
        let (api, graphql) = endpoints_from(env_of(&[
            ("GITHUB_API_URL", "https://api.github.com"),
            ("GITHUB_GRAPHQL_URL", "https://api.github.com/graphql"),
        ]))
        .unwrap();
        assert_eq!(
            (api.as_str(), graphql.as_str()),
            ("https://api.github.com", "https://api.github.com/graphql")
        );

        let (api, graphql) = endpoints_from(env_of(&[
            ("GITHUB_API_URL", "https://ghe.example/api/v3/"),
            ("GITHUB_GRAPHQL_URL", "https://ghe.example/api/graphql"),
        ]))
        .unwrap();
        assert_eq!(
            (api.as_str(), graphql.as_str()),
            (
                "https://ghe.example/api/v3",
                "https://ghe.example/api/graphql"
            )
        );
    }

    #[test]
    fn endpoints_derive_the_ghes_graphql_endpoint() {
        for api in ["https://ghe.example/api/v3", "https://ghe.example/api/v3/"] {
            let (api_out, graphql) = endpoints_from(env_of(&[("GITHUB_API_URL", api)])).unwrap();
            assert_eq!(api_out, "https://ghe.example/api/v3", "{api}");
            assert_eq!(graphql, "https://ghe.example/api/graphql", "{api}");
        }
    }

    #[test]
    fn endpoints_derive_graphql_from_the_normalized_api_path() {
        for api in [
            "https://ghe.example/api/v3/.",
            "https://ghe.example/api/v3/x/..",
            "https://ghe.example\\api\\v3",
        ] {
            let (api_out, graphql) = endpoints_from(env_of(&[("GITHUB_API_URL", api)])).unwrap();
            assert_eq!(api_out, "https://ghe.example/api/v3", "{api}");
            assert_eq!(graphql, "https://ghe.example/api/graphql", "{api}");
        }
    }

    #[test]
    fn endpoints_keep_an_explicit_graphql_url_over_the_derived_one() {
        let (_, graphql) = endpoints_from(env_of(&[
            ("GITHUB_API_URL", "https://ghe.example/api/v3"),
            ("GITHUB_GRAPHQL_URL", "https://ghe.example/custom/graphql"),
        ]))
        .unwrap();
        assert_eq!(graphql, "https://ghe.example/custom/graphql");
    }

    /// An env that resolves the repository without running git, so only
    /// the endpoint variables decide whether `discover_with` succeeds.
    const DISCOVER_BASE: [(&str, &str); 2] = [("GH_TOKEN", TOKEN), ("GITHUB_REPOSITORY", "o/r")];

    fn discover_env<'a>(extra: &'a [(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
        DISCOVER_BASE.iter().chain(extra).copied().collect()
    }

    /// `repo_root` is never read when `GITHUB_REPOSITORY` is set; a path
    /// that does not exist makes any fallback to `git` fail loudly.
    const NO_REPO: &str = "/nonexistent/discover-test-repo";

    #[test]
    fn discover_refuses_a_plain_http_non_loopback_api_url() {
        let env = discover_env(&[("GITHUB_API_URL", "http://ghe.example/api/v3")]);
        let err = GitHub::discover_with(env_of(&env), Path::new(NO_REPO)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("GITHUB_API_URL"), "{msg}");
        assert!(!msg.contains("ghe.example"), "{msg}");

        // The same env with https constructs, so the scheme alone failed it.
        let env = discover_env(&[("GITHUB_API_URL", "https://ghe.example/api/v3")]);
        GitHub::discover_with(env_of(&env), Path::new(NO_REPO)).unwrap();
    }

    #[test]
    fn discover_wires_the_validated_ghes_endpoints_into_the_client() {
        let env = discover_env(&[
            ("GITHUB_API_URL", "https://GHE.example:443/api/v3/"),
            ("GITHUB_GRAPHQL_URL", "https://ghe.example/api/graphql"),
        ]);
        let gh = GitHub::discover_with(env_of(&env), Path::new(NO_REPO)).unwrap();
        // Normalized by `endpoints_from` (host lowercased, default port
        // and trailing slash dropped), not the raw env value.
        assert_eq!(gh.api, "https://ghe.example/api/v3");
        assert_eq!(gh.graphql_url, "https://ghe.example/api/graphql");
        assert_eq!((gh.owner(), gh.repo()), ("o", "r"));
        assert_eq!(gh.token, TOKEN);
    }

    #[test]
    fn discover_keeps_an_explicit_graphql_url_over_the_derived_one() {
        // `with_base` alone would derive `/api/graphql`; only the
        // validated pair from `endpoints_from` carries the custom path.
        let env = discover_env(&[
            ("GITHUB_API_URL", "https://ghe.example/api/v3"),
            ("GITHUB_GRAPHQL_URL", "https://ghe.example/custom/graphql"),
        ]);
        let gh = GitHub::discover_with(env_of(&env), Path::new(NO_REPO)).unwrap();
        assert_eq!(gh.graphql_url, "https://ghe.example/custom/graphql");
    }

    #[test]
    fn discover_refuses_a_cross_origin_graphql_url() {
        let env = discover_env(&[
            ("GITHUB_API_URL", "https://ghe.example/api/v3"),
            ("GITHUB_GRAPHQL_URL", "https://evil.example/api/graphql"),
        ]);
        let err = GitHub::discover_with(env_of(&env), Path::new(NO_REPO)).unwrap_err();
        assert!(format!("{err:#}").contains("same origin"), "{err:#}");
    }

    /// A fresh repo with `origin` set to `url`, or none when `url` is
    /// `None`. Panics if an inherited `GIT_DIR` (e.g. from a pre-push hook)
    /// would point `git` at a different repo.
    fn repo_with_origin(url: Option<&str>) -> tempfile::TempDir {
        assert!(
            std::env::var_os("GIT_DIR").is_none() && std::env::var_os("GIT_WORK_TREE").is_none(),
            "unset GIT_DIR/GIT_WORK_TREE: the temp-repo tests would act on another repo"
        );
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let st = Command::new("git")
                .current_dir(dir.path())
                .args(args)
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?}");
        };
        git(&["init", "-q"]);
        if let Some(url) = url {
            git(&["remote", "add", "origin", url]);
        }
        dir
    }

    #[test]
    fn origin_url_reads_the_origin_remote() {
        let dir = repo_with_origin(Some("git@github.com:a/b.git"));
        assert_eq!(origin_url(dir.path()).unwrap(), "git@github.com:a/b.git");
    }

    #[test]
    fn origin_url_without_an_origin_points_at_github_repository() {
        let dir = repo_with_origin(None);
        let err = origin_url(dir.path()).unwrap_err();
        assert!(format!("{err:#}").contains("GITHUB_REPOSITORY"), "{err:#}");
    }

    #[test]
    fn discover_falls_back_to_origin_without_github_repository() {
        let dir = repo_with_origin(Some("https://github.com/a/b.git"));
        let env = [("GH_TOKEN", TOKEN)];
        let gh = GitHub::discover_with(env_of(&env), dir.path()).unwrap();
        assert_eq!((gh.owner(), gh.repo()), ("a", "b"));
    }

    #[test]
    fn discover_prefers_github_repository_over_origin() {
        let dir = repo_with_origin(Some("https://github.com/a/b.git"));
        let gh = GitHub::discover_with(env_of(&DISCOVER_BASE), dir.path()).unwrap();
        assert_eq!((gh.owner(), gh.repo()), ("o", "r"));
    }

    /// `discover`'s getter reads an empty variable as unset, so an empty
    /// `GITHUB_REPOSITORY` falls back to `origin` rather than failing.
    /// Without the filter `discover_with` would split `""` and error.
    #[test]
    fn an_empty_env_value_reads_as_unset() {
        let dir = repo_with_origin(Some("https://github.com/a/b.git"));
        let env = [("GH_TOKEN", TOKEN), ("GITHUB_REPOSITORY", "")];
        let get = env_getter(env_of(&env));
        assert_eq!(get("GITHUB_REPOSITORY"), None);
        assert_eq!(get("GH_TOKEN").as_deref(), Some(TOKEN));
        let gh = GitHub::discover_with(get, dir.path()).unwrap();
        assert_eq!((gh.owner(), gh.repo()), ("a", "b"));
    }

    #[test]
    fn with_base_derives_the_ghes_graphql_endpoint() {
        let client = http_client(TIMEOUT, TlsRoots::None).unwrap();
        let gh = GitHub::with_base(
            client,
            "https://ghe.example/api/v3/",
            TOKEN.to_string(),
            "o".to_string(),
            "r".to_string(),
        );
        assert_eq!(gh.api, "https://ghe.example/api/v3");
        assert_eq!(gh.graphql_url, "https://ghe.example/api/graphql");
    }

    #[test]
    fn default_graphql_url_rewrites_only_a_trailing_api_v3_path() {
        for (api, want) in [
            (
                "https://ghe.example/api/v3",
                "https://ghe.example/api/graphql",
            ),
            (
                "https://ghe.example/api/v3/",
                "https://ghe.example/api/graphql",
            ),
            ("https://api.github.com", "https://api.github.com/graphql"),
            ("http://127.0.0.1:8080", "http://127.0.0.1:8080/graphql"),
            // A host named `v3` is not a path segment.
            ("https://v3", "https://v3/graphql"),
            ("https://v3/", "https://v3/graphql"),
            // A host named `api` is not a path segment either.
            ("https://api/v3", "https://api/v3/graphql"),
            // A bare `/v3` (a proxy or mock mounting the API there) is not
            // GHES, so it nests rather than becoming a sibling path.
            ("https://ghe.example/v3", "https://ghe.example/v3/graphql"),
            // GHES behind a path prefix still rewrites.
            (
                "https://proxy.example/ghe/api/v3",
                "https://proxy.example/ghe/api/graphql",
            ),
            // Only whole trailing segments count.
            (
                "https://ghe.example/api/xv3",
                "https://ghe.example/api/xv3/graphql",
            ),
            (
                "https://ghe.example/xapi/v3",
                "https://ghe.example/xapi/v3/graphql",
            ),
            (
                "https://ghe.example/v3/api",
                "https://ghe.example/v3/api/graphql",
            ),
        ] {
            assert_eq!(default_graphql_url(api), want, "{api}");
        }
    }

    #[test]
    fn endpoints_accept_plain_http_only_to_loopback() {
        for base in [
            "http://127.0.0.1:8080",
            "http://127.5.5.5:9",
            "http://localhost:9",
            "http://[::1]:9",
        ] {
            let pairs = [("GITHUB_API_URL", base)];
            let (api, graphql) = endpoints_from(env_of(&pairs)).unwrap();
            assert_eq!(graphql, format!("{api}/graphql"), "{base}");
        }
    }

    #[test]
    fn endpoints_refuse_plain_http_off_loopback() {
        let err = endpoints_from(env_of(&[("GITHUB_API_URL", "http://ghe.example/api/v3")]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("GITHUB_API_URL must use https"), "{err}");

        // An https API does not license a plain-http GraphQL endpoint.
        let err = endpoints_from(env_of(&[
            ("GITHUB_API_URL", "https://ghe.example/api/v3"),
            ("GITHUB_GRAPHQL_URL", "http://ghe.example/api/graphql"),
        ]))
        .unwrap_err()
        .to_string();
        assert!(err.contains("GITHUB_GRAPHQL_URL must use https"), "{err}");

        // Hosts that merely look loopback are not.
        for base in [
            "http://localhost.evil.example",
            "http://127.0.0.1.evil.example",
            "http://10.0.0.1",
            "http://0.0.0.0",
            // IPv4-mapped loopback is deliberately not accepted.
            "http://[::ffff:127.0.0.1]",
        ] {
            let pairs = [("GITHUB_API_URL", base)];
            let err = endpoints_from(env_of(&pairs)).unwrap_err().to_string();
            assert!(
                err.contains("GITHUB_API_URL must use https"),
                "{base}: {err}"
            );
        }

        // A token pasted into the host position is never echoed.
        let pairs = [("GITHUB_API_URL", "http://ghp_l3aked/api/v3")];
        let err = endpoints_from(env_of(&pairs)).unwrap_err().to_string();
        assert!(err.contains("got http;"), "{err}");
        assert!(!err.contains("ghp_l3aked"), "{err}");
    }

    #[test]
    fn endpoints_refuse_unparseable_hostless_and_other_schemes() {
        for var in ["GITHUB_API_URL", "GITHUB_GRAPHQL_URL"] {
            for (value, want) in [
                ("not-a-url", "is not a valid URL"),
                ("data:text/plain,x", "has no host"),
                ("ftp://ghe.example", "must use https"),
            ] {
                // The bad value on `var`, a valid API URL otherwise.
                let mut pairs = vec![("GITHUB_API_URL", "https://ghe.example/api/v3")];
                pairs.retain(|(n, _)| *n != var);
                pairs.push((var, value));
                let err = endpoints_from(env_of(&pairs)).unwrap_err().to_string();
                assert!(
                    err.contains(&format!("{var} {want}")),
                    "{var}={value}: {err}"
                );
            }
        }
    }

    #[test]
    fn endpoints_refuse_graphql_on_another_origin() {
        for (api, graphql) in [
            (
                "https://ghe.example/api/v3",
                "https://evil.example/api/graphql",
            ),
            (
                "https://ghe.example/api/v3",
                "https://ghe.example:8443/api/graphql",
            ),
            // Scheme alone differs (both legal on their own: loopback http).
            (
                "https://localhost:8443/api",
                "http://localhost:8443/graphql",
            ),
        ] {
            let pairs = [("GITHUB_API_URL", api), ("GITHUB_GRAPHQL_URL", graphql)];
            let err = endpoints_from(env_of(&pairs)).unwrap_err().to_string();
            assert!(err.contains("not on the same origin"), "{graphql}: {err}");
            // Neither origin is echoed: the host is part of the value.
            assert!(!err.contains("evil.example"), "{err}");
            assert!(!err.contains("8443"), "{err}");
        }

        // A GraphQL override alone is checked against the github.com default.
        let pairs = [("GITHUB_GRAPHQL_URL", "https://evil.example/graphql")];
        let err = endpoints_from(env_of(&pairs)).unwrap_err().to_string();
        assert!(err.contains("not on the same origin"), "{err}");
        assert!(!err.contains("evil.example"), "{err}");
    }

    #[test]
    fn endpoints_refuse_userinfo_and_never_echo_it() {
        let err = endpoints_from(env_of(&[(
            "GITHUB_API_URL",
            "https://user:s3cret@ghe.example/api/v3",
        )]))
        .unwrap_err()
        .to_string();
        assert!(err.contains("must not carry credentials"), "{err}");
        assert!(!err.contains("s3cret"), "{err}");

        // A bare token as username, and userinfo on the GraphQL side.
        for pairs in [
            [
                ("GITHUB_API_URL", "https://tok3n@ghe.example/api/v3"),
                ("GITHUB_GRAPHQL_URL", "https://ghe.example/api/graphql"),
            ],
            [
                ("GITHUB_API_URL", "https://ghe.example/api/v3"),
                (
                    "GITHUB_GRAPHQL_URL",
                    "https://tok3n@ghe.example/api/graphql",
                ),
            ],
        ] {
            let err = endpoints_from(env_of(&pairs)).unwrap_err().to_string();
            assert!(err.contains("must not carry credentials"), "{err}");
            assert!(!err.contains("tok3n"), "{err}");
        }

        // Password with no username.
        let err = endpoints_from(env_of(&[(
            "GITHUB_API_URL",
            "https://:pw0rd@ghe.example/api/v3",
        )]))
        .unwrap_err()
        .to_string();
        assert!(err.contains("must not carry credentials"), "{err}");
        assert!(!err.contains("pw0rd"), "{err}");
    }

    #[test]
    fn endpoints_refuse_query_or_fragment_and_never_echo_it() {
        for var in ["GITHUB_API_URL", "GITHUB_GRAPHQL_URL"] {
            for value in [
                "https://ghe.example/api/v3?access_token=s3cret",
                "https://ghe.example/api/v3#s3cret",
            ] {
                let mut pairs = vec![("GITHUB_API_URL", "https://ghe.example/api/v3")];
                pairs.retain(|(n, _)| *n != var);
                pairs.push((var, value));
                let err = endpoints_from(env_of(&pairs)).unwrap_err().to_string();
                assert!(
                    err.contains(&format!("{var} must not carry a query or fragment")),
                    "{var}={value}: {err}"
                );
                assert!(!err.contains("s3cret"), "{err}");
            }
        }
    }

    #[test]
    fn endpoints_come_back_normalized() {
        // An explicit default port and a mixed-case host are the same origin,
        // and come back canonical so `list()`'s pagination prefix check
        // matches the `Link` URLs the API echoes.
        // Both halves non-canonical, so returning either raw input would fail.
        let pairs = [
            ("GITHUB_API_URL", "https://GHE.example:443/api/v3"),
            ("GITHUB_GRAPHQL_URL", "https://Ghe.EXAMPLE:443/api/graphql"),
        ];
        let (api, graphql) = endpoints_from(env_of(&pairs)).unwrap();
        assert_eq!(api, "https://ghe.example/api/v3");
        assert_eq!(graphql, "https://ghe.example/api/graphql");

        // The derived default follows the normalized API URL.
        let pairs = [("GITHUB_API_URL", "https://GHE.example:443/api/v3")];
        let (api, graphql) = endpoints_from(env_of(&pairs)).unwrap();
        assert_eq!(api, "https://ghe.example/api/v3");
        assert_eq!(graphql, "https://ghe.example/api/graphql");
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
