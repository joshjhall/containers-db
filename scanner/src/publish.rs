//! Publish a locally built scan commit as a GitHub-signed commit (#526).
//!
//! containers-db `main` requires signed commits, and the scanner holds no
//! signing key. GitHub signs commits it creates itself, so instead of
//! `git push` the scanner replays its local commit through the GraphQL
//! `createCommitOnBranch` mutation onto a throwaway staging branch, checks
//! the result has the local commit's exact tree, and only then points the
//! PR branch at it. The local commit stays the source of truth; the API is
//! a transport whose output is verified, not trusted.
//!
//! Regenerating an existing PR keeps `--force-with-lease` semantics:
//! `updateRefs` moves the branch only if it is still at the head seen when
//! the PRs were listed, so a push that raced in since then survives.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};
use base64::Engine as _;
use serde_json::{Value, json};

use crate::github::GitHub;

/// The file-level difference between a commit and its parent, as
/// `createCommitOnBranch` takes it: full contents for every added or
/// modified path, and the paths removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileChanges {
    pub additions: Vec<(String, Vec<u8>)>,
    pub deletions: Vec<String>,
}

/// A commit the remote created: its id and its tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCommit {
    pub oid: String,
    pub tree: String,
}

/// The branch operations publishing needs. [`ApiRemote`] is the GitHub
/// implementation; tests use a bare local repository.
pub trait Remote {
    /// Create `branch` at `oid`. Fails if it already exists.
    fn create_branch(&self, branch: &str, oid: &str) -> Result<()>;
    /// Commit `changes` on top of `branch`, which must be at
    /// `expected_head`, and advance the branch to the new commit.
    fn commit_on_branch(
        &self,
        branch: &str,
        expected_head: &str,
        message: &str,
        changes: &FileChanges,
    ) -> Result<RemoteCommit>;
    /// Atomically point `branch` at `after`, only if it is at `before`.
    fn move_branch(&self, branch: &str, before: &str, after: &str) -> Result<()>;
    fn delete_branch(&self, branch: &str) -> Result<()>;
    /// The branch's head, or `None` if it does not exist.
    fn branch_head(&self, branch: &str) -> Result<Option<String>>;
}

/// Where the published commit goes.
#[derive(Debug, Clone, Copy)]
pub enum Target<'a> {
    /// A branch that must not exist yet (a new scan PR).
    New(&'a str),
    /// An open PR's branch, replaced only if still at `lease`.
    Existing { branch: &'a str, lease: &'a str },
}

/// Publish the checked-out `HEAD` commit (built on `HEAD^`, which must
/// already exist on the remote) as a remote-created commit, and point the
/// target branch at it. Returns the published commit id.
pub fn publish_head(
    repo_root: &Path,
    remote: &dyn Remote,
    target: Target<'_>,
    message: &str,
) -> Result<String> {
    let local = rev(repo_root, "HEAD")?;
    let parent = rev(repo_root, "HEAD^")?;
    let local_tree = rev(repo_root, "HEAD^{tree}")?;
    let changes = changes_between(repo_root, &parent, &local)?;
    let branch = match target {
        Target::New(b) | Target::Existing { branch: b, .. } => b,
    };
    let staging = format!("scanner-staging/{branch}/{}", &local[..12]);

    remote
        .create_branch(&staging, &parent)
        .with_context(|| format!("creating staging branch {staging} at {parent}"))?;
    let published = commit_and_point(
        remote,
        target,
        branch,
        &staging,
        &parent,
        &local_tree,
        message,
        &changes,
    );
    // Best effort either way: a leftover staging branch is clutter, not
    // harm, and must not turn a published PR into a reported failure.
    if let Err(e) = remote.delete_branch(&staging) {
        eprintln!("scanner: could not delete staging branch {staging}: {e:#}");
    }
    published
}

#[allow(clippy::too_many_arguments)]
fn commit_and_point(
    remote: &dyn Remote,
    target: Target<'_>,
    branch: &str,
    staging: &str,
    parent: &str,
    local_tree: &str,
    message: &str,
    changes: &FileChanges,
) -> Result<String> {
    let signed = remote
        .commit_on_branch(staging, parent, message, changes)
        .context("creating the signed commit")?;
    ensure!(
        signed.tree == local_tree,
        "signed commit {} has tree {}, but the local commit's tree is {local_tree}; \
         refusing to publish content that differs from what the scanner built",
        signed.oid,
        signed.tree,
    );
    match target {
        Target::New(_) => remote
            .create_branch(branch, &signed.oid)
            .with_context(|| format!("creating branch {branch}"))?,
        Target::Existing { lease, .. } => {
            if let Err(e) = remote.move_branch(branch, lease, &signed.oid) {
                // GitHub reports a failed `beforeOid` check as a generic
                // error, so name the race from the branch's actual head.
                match remote.branch_head(branch) {
                    Ok(now) if now.as_deref() != Some(lease) => bail!(
                        "lease rejected: {branch} moved since the PRs were listed \
                         (expected {lease}, now {}); refusing to overwrite it",
                        now.as_deref().unwrap_or("deleted"),
                    ),
                    _ => return Err(e.context(format!("moving {branch} to {}", signed.oid))),
                }
            }
        }
    }
    Ok(signed.oid)
}

/// What `to` changes relative to `from`. Only plain adds, modifications
/// and deletions are representable through `createCommitOnBranch`; any
/// other status (type change, submodule, …) is an error rather than
/// something silently dropped. The tree check in [`publish_head`] is the
/// backstop for anything this misses (e.g. a mode change).
pub fn changes_between(repo_root: &Path, from: &str, to: &str) -> Result<FileChanges> {
    let out = git_bytes(
        repo_root,
        &["diff", "--name-status", "--no-renames", "-z", from, to],
    )?;
    let mut fields = out.split(|b| *b == 0).filter(|f| !f.is_empty());
    let mut changes = FileChanges::default();
    while let Some(status) = fields.next() {
        let path = fields
            .next()
            .context("git diff --name-status -z: status without a path")?;
        let path = std::str::from_utf8(path)
            .context("non-UTF-8 path in a scan commit")?
            .to_string();
        match status {
            b"A" | b"M" => {
                let contents =
                    git_bytes(repo_root, &["cat-file", "blob", &format!("{to}:{path}")])?;
                changes.additions.push((path, contents));
            }
            b"D" => changes.deletions.push(path),
            other => bail!(
                "{path}: change status {:?} cannot be published as a signed commit",
                String::from_utf8_lossy(other)
            ),
        }
    }
    Ok(changes)
}

/// [`Remote`] backed by the GitHub GraphQL API through [`GitHub`], using
/// its token (`GH_TOKEN` = `SCANNER_TOKEN` in CI).
pub struct ApiRemote<'a> {
    gh: &'a GitHub,
    repo_id: String,
}

impl<'a> ApiRemote<'a> {
    /// Look up the node id of the client's repository, which `createRef` and
    /// `updateRefs` take.
    pub fn discover(gh: &'a GitHub) -> Result<Self> {
        let v = gh.graphql(&json!({
            "query": "query($o:String!,$n:String!){repository(owner:$o,name:$n){id}}",
            "variables": {"o": gh.owner(), "n": gh.repo()},
        }))?;
        let id = v["data"]["repository"]["id"]
            .as_str()
            .with_context(|| format!("no repository id for {}/{}", gh.owner(), gh.repo()))?;
        Ok(ApiRemote {
            gh,
            repo_id: id.to_string(),
        })
    }

    fn graphql(&self, request: &Value) -> Result<Value> {
        self.gh.graphql(request)
    }
}

impl Remote for ApiRemote<'_> {
    fn create_branch(&self, branch: &str, oid: &str) -> Result<()> {
        self.graphql(&json!({
            "query": "mutation($i:CreateRefInput!){createRef(input:$i){ref{name}}}",
            "variables": {"i": {
                "repositoryId": self.repo_id,
                "name": format!("refs/heads/{branch}"),
                "oid": oid,
            }},
        }))?;
        Ok(())
    }

    fn commit_on_branch(
        &self,
        branch: &str,
        expected_head: &str,
        message: &str,
        changes: &FileChanges,
    ) -> Result<RemoteCommit> {
        let nwo = format!("{}/{}", self.gh.owner(), self.gh.repo());
        let v = self.graphql(&commit_request(
            &nwo,
            branch,
            expected_head,
            message,
            changes,
        ))?;
        parse_commit_response(&v)
    }

    fn move_branch(&self, branch: &str, before: &str, after: &str) -> Result<()> {
        self.graphql(&json!({
            "query": "mutation($i:UpdateRefsInput!){updateRefs(input:$i){clientMutationId}}",
            "variables": {"i": {
                "repositoryId": self.repo_id,
                "refUpdates": [{
                    "name": format!("refs/heads/{branch}"),
                    "beforeOid": before,
                    "afterOid": after,
                    "force": true,
                }],
            }},
        }))?;
        Ok(())
    }

    fn delete_branch(&self, branch: &str) -> Result<()> {
        // Percent-encoded per segment: `#`, `%` and `?` are legal in a ref
        // name and would otherwise send the DELETE to another ref.
        let path = format!("git/refs/heads/{}", crate::github::encode_ref(branch));
        self.gh.rest(reqwest::Method::DELETE, &path, None)?;
        Ok(())
    }

    fn branch_head(&self, branch: &str) -> Result<Option<String>> {
        let v = self.graphql(&json!({
            "query": "query($o:String!,$n:String!,$r:String!){repository(owner:$o,name:$n){ref(qualifiedName:$r){target{oid}}}}",
            "variables": {"o": self.gh.owner(), "n": self.gh.repo(), "r": format!("refs/heads/{branch}")},
        }))?;
        Ok(v["data"]["repository"]["ref"]["target"]["oid"]
            .as_str()
            .map(str::to_string))
    }
}

/// The `createCommitOnBranch` request. The first line of `message` is the
/// headline; anything after a blank line is the body.
pub fn commit_request(
    name_with_owner: &str,
    branch: &str,
    expected_head: &str,
    message: &str,
    changes: &FileChanges,
) -> Value {
    let b64 = base64::engine::general_purpose::STANDARD;
    let (headline, body) = match message.split_once('\n') {
        Some((h, rest)) => (h, rest.trim_start_matches('\n')),
        None => (message, ""),
    };
    let additions: Vec<Value> = changes
        .additions
        .iter()
        .map(|(path, contents)| json!({"path": path, "contents": b64.encode(contents)}))
        .collect();
    let deletions: Vec<Value> = changes
        .deletions
        .iter()
        .map(|path| json!({"path": path}))
        .collect();
    json!({
        "query": "mutation($i:CreateCommitOnBranchInput!){createCommitOnBranch(input:$i){commit{oid tree{oid}}}}",
        "variables": {"i": {
            "branch": {"repositoryNameWithOwner": name_with_owner, "branchName": branch},
            "message": {"headline": headline, "body": body},
            "expectedHeadOid": expected_head,
            "fileChanges": {"additions": additions, "deletions": deletions},
        }},
    })
}

/// Read the commit id and tree out of a `createCommitOnBranch` response.
pub fn parse_commit_response(v: &Value) -> Result<RemoteCommit> {
    let commit = &v["data"]["createCommitOnBranch"]["commit"];
    let oid = commit["oid"]
        .as_str()
        .with_context(|| format!("createCommitOnBranch: no commit oid in {v}"))?;
    let tree = commit["tree"]["oid"]
        .as_str()
        .with_context(|| format!("createCommitOnBranch: no tree oid in {v}"))?;
    Ok(RemoteCommit {
        oid: oid.to_string(),
        tree: tree.to_string(),
    })
}

fn rev(repo_root: &Path, spec: &str) -> Result<String> {
    let out = git_bytes(repo_root, &["rev-parse", "--verify", spec])?;
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}

fn git_bytes(repo_root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    run("git", repo_root, args, None)
}

fn run(program: &str, cwd: &Path, args: &[&str], stdin: Option<&[u8]>) -> Result<Vec<u8>> {
    let shown = || format!("`{program} {}`", args.join(" "));
    let mut child = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {}", shown()))?;
    if let Some(input) = stdin {
        use std::io::Write as _;
        // Taken so the pipe closes once written and the child sees EOF.
        let mut pipe = child.stdin.take().context("child stdin")?;
        pipe.write_all(input)
            .with_context(|| format!("writing stdin of {}", shown()))?;
    }
    let output = child
        .wait_with_output()
        .with_context(|| format!("waiting for {}", shown()))?;
    if !output.status.success() {
        bail!(
            "{} failed: {}",
            shown(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output.stdout)
}

/// A [`Remote`] over a bare local repository, built from git plumbing so it
/// derives the commit from [`FileChanges`] the way GitHub does rather than
/// copying the local tree.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::path::PathBuf;

    const ZERO: &str = "0000000000000000000000000000000000000000";

    pub struct BareRemote {
        pub git_dir: PathBuf,
        /// Drop the last addition before committing, to simulate a
        /// transport that loses a file.
        pub lose_a_file: bool,
    }

    impl BareRemote {
        pub fn new(git_dir: &Path) -> Self {
            BareRemote {
                git_dir: git_dir.to_path_buf(),
                lose_a_file: false,
            }
        }

        fn git(&self, args: &[&str], stdin: Option<&[u8]>, index: Option<&Path>) -> Result<String> {
            let mut cmd = Command::new("git");
            cmd.arg("--git-dir")
                .arg(&self.git_dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "GitHub")
                .env("GIT_AUTHOR_EMAIL", "noreply@github.com")
                .env("GIT_COMMITTER_NAME", "GitHub")
                .env("GIT_COMMITTER_EMAIL", "noreply@github.com")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if let Some(index) = index {
                // update-index refuses a bare repo without a work tree. The
                // index's directory stands in; nothing is checked out there.
                cmd.env("GIT_INDEX_FILE", index)
                    .env("GIT_WORK_TREE", index.parent().unwrap());
            }
            let mut child = cmd.spawn()?;
            {
                use std::io::Write as _;
                let mut pipe = child.stdin.take().unwrap();
                if let Some(input) = stdin {
                    pipe.write_all(input)?;
                }
            }
            let out = child.wait_with_output()?;
            if !out.status.success() {
                bail!(
                    "git {} failed: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        }

        pub fn head(&self, branch: &str) -> Option<String> {
            self.branch_head(branch).unwrap()
        }
    }

    impl Remote for BareRemote {
        fn create_branch(&self, branch: &str, oid: &str) -> Result<()> {
            let r = format!("refs/heads/{branch}");
            self.git(&["update-ref", &r, oid, ZERO], None, None)?;
            Ok(())
        }

        fn commit_on_branch(
            &self,
            branch: &str,
            expected_head: &str,
            message: &str,
            changes: &FileChanges,
        ) -> Result<RemoteCommit> {
            let r = format!("refs/heads/{branch}");
            let head = self.git(&["rev-parse", "--verify", &r], None, None)?;
            ensure!(head == expected_head, "expectedHeadOid mismatch");
            let dir = tempfile::tempdir()?;
            let index = dir.path().join("index");
            self.git(&["read-tree", &head], None, Some(&index))?;
            let mut additions = changes.additions.clone();
            if self.lose_a_file {
                additions.pop();
            }
            for (path, contents) in &additions {
                let blob = self.git(&["hash-object", "-w", "--stdin"], Some(contents), None)?;
                let info = format!("100644,{blob},{path}");
                self.git(
                    &["update-index", "--add", "--cacheinfo", &info],
                    None,
                    Some(&index),
                )?;
            }
            for path in &changes.deletions {
                self.git(
                    &["update-index", "--force-remove", path],
                    None,
                    Some(&index),
                )?;
            }
            let tree = self.git(&["write-tree"], None, Some(&index))?;
            let oid = self.git(
                &[
                    "commit-tree",
                    "--no-gpg-sign",
                    &tree,
                    "-p",
                    &head,
                    "-m",
                    message,
                ],
                None,
                None,
            )?;
            self.git(&["update-ref", &r, &oid, &head], None, None)?;
            Ok(RemoteCommit { oid, tree })
        }

        fn move_branch(&self, branch: &str, before: &str, after: &str) -> Result<()> {
            let r = format!("refs/heads/{branch}");
            self.git(&["update-ref", &r, after, before], None, None)?;
            Ok(())
        }

        fn delete_branch(&self, branch: &str) -> Result<()> {
            let r = format!("refs/heads/{branch}");
            self.git(&["update-ref", "-d", &r], None, None)?;
            Ok(())
        }

        fn branch_head(&self, branch: &str) -> Result<Option<String>> {
            let r = format!("refs/heads/{branch}");
            Ok(self
                .git(&["rev-parse", "--verify", "-q", &r], None, None)
                .ok())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::BareRemote;
    use super::*;

    fn git(p: &Path, args: &[&str]) -> String {
        String::from_utf8(git_bytes(p, args).unwrap())
            .unwrap()
            .trim()
            .to_string()
    }

    /// A clone whose `main` (holding `tools/rust/{index.json,old.json}`) is
    /// on a bare origin, with a local scan commit on top that modifies,
    /// adds and deletes one file each.
    fn repo_with_scan_commit() -> (tempfile::TempDir, tempfile::TempDir) {
        assert!(
            std::env::var_os("GIT_DIR").is_none(),
            "unset GIT_DIR: the temp-repo tests would act on another repo"
        );
        let origin = tempfile::tempdir().unwrap();
        git(origin.path(), &["init", "-q", "--bare", "-b", "main"]);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        git(p, &["config", "user.name", "t"]);
        git(p, &["config", "user.email", "t@example.com"]);
        git(p, &["config", "commit.gpgsign", "false"]);
        std::fs::create_dir_all(p.join("tools/rust/versions")).unwrap();
        std::fs::write(p.join("tools/rust/index.json"), "old").unwrap();
        std::fs::write(p.join("tools/rust/versions/1.0.0.json"), "{}").unwrap();
        git(p, &["add", "tools"]);
        git(p, &["commit", "-q", "-m", "base"]);
        git(
            p,
            &["remote", "add", "origin", origin.path().to_str().unwrap()],
        );
        git(p, &["push", "-q", "origin", "main"]);
        git(p, &["checkout", "-q", "-b", "scan/rust/2026-11-02"]);
        std::fs::write(p.join("tools/rust/index.json"), "new").unwrap();
        std::fs::write(p.join("tools/rust/versions/2.0.0.json"), "{\"v\":2}\n").unwrap();
        git(p, &["rm", "-q", "tools/rust/versions/1.0.0.json"]);
        git(p, &["add", "tools"]);
        git(
            p,
            &[
                "commit",
                "-q",
                "-m",
                "feat(rust): add upstream versions 2.0.0",
            ],
        );
        (dir, origin)
    }

    #[test]
    fn changes_cover_modify_add_and_delete() {
        let (dir, _origin) = repo_with_scan_commit();
        let c = changes_between(dir.path(), "HEAD^", "HEAD").unwrap();
        assert_eq!(
            c.additions,
            vec![
                ("tools/rust/index.json".to_string(), b"new".to_vec()),
                (
                    "tools/rust/versions/2.0.0.json".to_string(),
                    b"{\"v\":2}\n".to_vec()
                ),
            ]
        );
        assert_eq!(
            c.deletions,
            vec!["tools/rust/versions/1.0.0.json".to_string()]
        );
    }

    #[test]
    fn new_branch_gets_a_remote_commit_with_the_local_tree() {
        let (dir, origin) = repo_with_scan_commit();
        let p = dir.path();
        let remote = BareRemote::new(origin.path());
        let oid = publish_head(
            p,
            &remote,
            Target::New("scan/rust/2026-11-02"),
            "feat(rust): x",
        )
        .unwrap();
        let o = origin.path();
        assert_eq!(
            remote.head("scan/rust/2026-11-02").as_deref(),
            Some(oid.as_str())
        );
        assert_ne!(
            oid,
            git(p, &["rev-parse", "HEAD"]),
            "the remote built its own commit"
        );
        assert_eq!(
            git(o, &["rev-parse", &format!("{oid}^{{tree}}")]),
            git(p, &["rev-parse", "HEAD^{tree}"])
        );
        assert_eq!(
            git(o, &["rev-parse", &format!("{oid}^")]),
            git(p, &["rev-parse", "main"])
        );
        assert_eq!(
            git(
                o,
                &[
                    "for-each-ref",
                    "--format=%(refname)",
                    "refs/heads/scanner-staging"
                ]
            ),
            "",
            "the staging branch is cleaned up"
        );
    }

    #[test]
    fn new_branch_refuses_to_overwrite_an_existing_one() {
        let (dir, origin) = repo_with_scan_commit();
        let remote = BareRemote::new(origin.path());
        let main = git(dir.path(), &["rev-parse", "main"]);
        remote.create_branch("scan/rust/2026-11-02", &main).unwrap();
        let err = publish_head(
            dir.path(),
            &remote,
            Target::New("scan/rust/2026-11-02"),
            "m",
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("creating branch scan/rust/2026-11-02"),
            "{err:#}"
        );
        assert_eq!(remote.head("scan/rust/2026-11-02"), Some(main));
    }

    #[test]
    fn a_remote_tree_that_differs_is_never_published() {
        let (dir, origin) = repo_with_scan_commit();
        let remote = BareRemote {
            lose_a_file: true,
            ..BareRemote::new(origin.path())
        };
        let err = publish_head(
            dir.path(),
            &remote,
            Target::New("scan/rust/2026-11-02"),
            "m",
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("refusing to publish content"),
            "{err:#}"
        );
        assert_eq!(
            remote.head("scan/rust/2026-11-02"),
            None,
            "branch never created"
        );
    }

    #[test]
    fn existing_branch_moves_only_under_its_lease() {
        let (dir, origin) = repo_with_scan_commit();
        let p = dir.path();
        let remote = BareRemote::new(origin.path());
        let main = git(p, &["rev-parse", "main"]);
        remote.create_branch("scan/rust/old", &main).unwrap();

        let stale = "1111111111111111111111111111111111111111";
        let target = Target::Existing {
            branch: "scan/rust/old",
            lease: stale,
        };
        let err = publish_head(p, &remote, target, "m").unwrap_err();
        assert!(
            format!("{err:#}").contains("lease rejected: scan/rust/old moved"),
            "{err:#}"
        );
        assert_eq!(remote.head("scan/rust/old").as_deref(), Some(main.as_str()));

        let target = Target::Existing {
            branch: "scan/rust/old",
            lease: &main,
        };
        let oid = publish_head(p, &remote, target, "m").unwrap();
        assert_eq!(remote.head("scan/rust/old"), Some(oid));
    }

    mod api_remote {
        use super::super::*;
        use crate::github::testing::client_for;
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        async fn with_repo_id(server: &MockServer) {
            Mock::given(method("POST"))
                .and(path("/graphql"))
                .and(body_partial_json(
                    json!({"variables": {"o": "o", "n": "r"}}),
                ))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"data": {"repository": {"id": "R_1"}}})),
                )
                .up_to_n_times(1)
                .mount(server)
                .await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn branch_ops_send_the_repo_id_and_refs() {
            let server = MockServer::start().await;
            with_repo_id(&server).await;
            Mock::given(method("POST"))
                .and(path("/graphql"))
                .and(body_partial_json(json!({"variables": {"i": {
                    "repositoryId": "R_1", "name": "refs/heads/b", "oid": "abc",
                }}})))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/graphql"))
                .and(body_partial_json(json!({"variables": {"i": {
                    "repositoryId": "R_1",
                    "refUpdates": [{"name": "refs/heads/b", "beforeOid": "abc", "afterOid": "def", "force": true}],
                }}})))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("DELETE"))
                .and(path("/repos/o/r/git/refs/heads/b"))
                .respond_with(ResponseTemplate::new(204))
                .expect(1)
                .mount(&server)
                .await;
            let gh = client_for(&server);
            let remote = ApiRemote::discover(&gh).unwrap();
            remote.create_branch("b", "abc").unwrap();
            remote.move_branch("b", "abc", "def").unwrap();
            remote.delete_branch("b").unwrap();
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn branch_head_reads_a_missing_ref_as_none() {
            let server = MockServer::start().await;
            with_repo_id(&server).await;
            for (branch, oid) in [
                ("there", json!({"target": {"oid": "abc"}})),
                ("gone", Value::Null),
            ] {
                Mock::given(method("POST"))
                    .and(path("/graphql"))
                    .and(body_partial_json(
                        json!({"variables": {"r": format!("refs/heads/{branch}")}}),
                    ))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .set_body_json(json!({"data": {"repository": {"ref": oid}}})),
                    )
                    .mount(&server)
                    .await;
            }
            let gh = client_for(&server);
            let remote = ApiRemote::discover(&gh).unwrap();
            assert_eq!(remote.branch_head("there").unwrap().as_deref(), Some("abc"));
            assert_eq!(remote.branch_head("gone").unwrap(), None);
        }

        /// A `#` in the branch must reach GitHub encoded, not cut the path
        /// at a fragment and delete a different ref.
        #[tokio::test(flavor = "multi_thread")]
        async fn delete_branch_encodes_the_ref() {
            let server = MockServer::start().await;
            with_repo_id(&server).await;
            Mock::given(method("DELETE"))
                .and(path("/repos/o/r/git/refs/heads/scanner-staging/x%23y/abc"))
                .respond_with(ResponseTemplate::new(204))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("DELETE"))
                .and(path("/repos/o/r/git/refs/heads/scanner-staging/x"))
                .respond_with(ResponseTemplate::new(204))
                .expect(0)
                .mount(&server)
                .await;
            let gh = client_for(&server);
            let remote = ApiRemote::discover(&gh).unwrap();
            remote.delete_branch("scanner-staging/x#y/abc").unwrap();
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn discover_needs_a_repo_id() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({"data": {"repository": null}})),
                )
                .mount(&server)
                .await;
            let gh = client_for(&server);
            assert!(ApiRemote::discover(&gh).is_err());
        }
    }

    #[test]
    fn commit_request_splits_message_and_encodes_contents() {
        let changes = FileChanges {
            additions: vec![("tools/a.json".into(), b"{}\n".to_vec())],
            deletions: vec!["tools/b.json".into()],
        };
        let r = commit_request("o/r", "scan/x", "abc", "feat: t\n\nbody line", &changes);
        let i = &r["variables"]["i"];
        assert_eq!(i["branch"]["repositoryNameWithOwner"], "o/r");
        assert_eq!(i["branch"]["branchName"], "scan/x");
        assert_eq!(i["expectedHeadOid"], "abc");
        assert_eq!(i["message"]["headline"], "feat: t");
        assert_eq!(i["message"]["body"], "body line");
        assert_eq!(i["fileChanges"]["additions"][0]["contents"], "e30K");
        assert_eq!(i["fileChanges"]["deletions"][0]["path"], "tools/b.json");
        let single = commit_request("o/r", "b", "abc", "feat: only", &FileChanges::default());
        assert_eq!(single["variables"]["i"]["message"]["body"], "");
    }

    #[test]
    fn commit_response_needs_both_oids() {
        let ok =
            json!({"data":{"createCommitOnBranch":{"commit":{"oid":"c1","tree":{"oid":"t1"}}}}});
        assert_eq!(
            parse_commit_response(&ok).unwrap(),
            RemoteCommit {
                oid: "c1".into(),
                tree: "t1".into()
            }
        );
        let no_tree = json!({"data":{"createCommitOnBranch":{"commit":{"oid":"c1"}}}});
        assert!(parse_commit_response(&no_tree).is_err());
    }
}
