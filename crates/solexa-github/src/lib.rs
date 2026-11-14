//! solexa-github, round 30's GitHub-API half of "mirror a sync
//! Conflict as a real GitHub PR, resolvable via GitHub's own review
//! UI" (Phase 8, "Collaboration").
//!
//! Deliberately a separate crate from `solexa-sync`, whose own module
//! doc explicitly excludes "any GitHub API integration, any OAuth/PAT
//! handling" from its scope, this crate is where that boundary lives
//! instead. `solexa-sync` still does the actual git branch/commit/push
//! mechanics (round 30 added a couple of small functions there for
//! that); this crate only ever talks to GitHub itself.
//!
//! Round 22's git sync never gives SOLEXA a credential of its own: it
//! shells out to the system `git` binary inside a directory the user
//! already `git clone`d and configured themselves, whoever has push
//! access to that remote already *is* the team. This crate extends
//! that exact principle to GitHub specifically: everything here shells
//! out to the `gh` CLI, reusing whichever account the user already ran
//! `gh auth login` with. SOLEXA never stores, sees, or asks for a
//! GitHub token, the precondition is "install `gh` and `gh auth
//! login` yourself first", the same shape as git sync's own "clone the
//! repo and configure your own git credentials first".

use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::process::Command;

fn run_gh(repo_dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("gh")
        .current_dir(repo_dir)
        .args(args)
        .output()
        .with_context(|| format!("running `gh {}` in {}", args.join(" "), repo_dir.display()))?;
    if !output.status.success() {
        return Err(anyhow!(
            "`gh {}` failed in {}: {}",
            args.join(" "),
            repo_dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Whether the `gh` CLI is both installed *and* already authenticated
/// checked with `gh auth status` rather than just `gh --version`,
/// since `gh` itself refuses every real command (even read-only ones
/// against a public repo) until `gh auth login` has been run at least
/// once. Checked up front by the coordinator so a missing precondition
/// produces one clear error immediately, rather than a confusing
/// failure partway through creating a branch.
pub fn gh_available() -> bool {
    gh_available_from(Command::new("gh"))
}

/// The body of `gh_available`, split out so the mapping it implements,
/// "the command ran and exited successfully" is the *only* thing that
/// yields `true`, every other outcome (spawn failure, non-zero exit,
/// a killed process) yields `false`, can be tested deterministically on
/// any machine. Round 66 (T6): the previous test asserted
/// `!gh_available()` outright, which was only true in the build sandbox
/// where `gh` was installed but never authenticated; on a developer
/// machine that has run `gh auth login` that is a red suite for no
/// reason. A test must not depend on who is logged in on the machine
/// running it.
fn gh_available_from(mut command: Command) -> bool {
    command
        .args(["auth", "status"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub struct CreatePrParams<'a> {
    pub title: &'a str,
    pub body: &'a str,
    pub base_branch: &'a str,
    pub head_branch: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone)]
pub struct PrStatus {
    pub state: PrState,
    pub url: String,
}

/// The abstraction `solexa-coordinator` depends on, rather than
/// calling `run_gh` directly, so tests can substitute
/// `fake::FakeGithubClient` instead of needing a real, authenticated
/// GitHub account and a real repo to push branches to. Same shape as
/// `solexa-models`' `ModelProvider`: swap the implementation, keep the
/// caller's logic identical, and keep the test run deterministic and
/// offline (spec rule: "prefer deterministic verification wherever
/// possible" applies just as much to how *this* codebase is tested as
/// to what it verifies for the user).
pub trait GithubClient: Send + Sync {
    /// Whether this client is currently usable, for `GhCliClient`,
    /// whether `gh` is installed and already authenticated (see
    /// `gh_available`). Checked by the coordinator up front so a
    /// missing precondition produces one clear error immediately,
    /// rather than a confusing failure partway through creating a
    /// branch. Test doubles are always available, they don't shell
    /// out to anything real, so there's no precondition to check.
    fn is_available(&self) -> bool {
        true
    }

    /// Opens a PR in whichever repo `repo_dir` is a working copy of.
    /// Returns the PR's URL. `head_branch` must already exist and be
    /// pushed to the remote, this call only ever runs `gh pr create`,
    /// it never touches git itself (see `solexa_sync::checkout_new_branch`
    /// / `commit_and_push_new_branch` for that half).
    fn create_pr(&self, repo_dir: &Path, params: CreatePrParams) -> Result<String>;

    /// Looks up a PR's current state by its URL.
    fn get_pr_status(&self, repo_dir: &Path, pr_url: &str) -> Result<PrStatus>;

    /// Round 33: opens a plain GitHub Issue (not a PR), used to mirror
    /// a recorded `Decision` for visibility, not to propose a code
    /// change. Returns the new issue's URL.
    fn create_issue(&self, repo_dir: &Path, params: CreateIssueParams) -> Result<String>;

    /// Closes an issue and leaves a closing comment in one call, used
    /// when a mirrored Decision gets resolved in SOLEXA, to reflect
    /// that outcome back onto GitHub. SOLEXA stays the sole source of
    /// truth for what the decision *is*; this only ever reports a
    /// decision already made, never the other way around (unlike the
    /// PR-based Conflict mirroring, an issue's open/closed state can't
    /// cleanly represent "which of N options was chosen").
    fn close_issue_with_comment(&self, repo_dir: &Path, issue_url: &str, comment: &str) -> Result<()>;
}

pub struct CreateIssueParams<'a> {
    pub title: &'a str,
    pub body: &'a str,
}

/// The real implementation: shells out to the `gh` CLI exactly the way
/// `solexa_sync::run_git` shells out to `git`.
pub struct GhCliClient;

impl GithubClient for GhCliClient {
    fn is_available(&self) -> bool {
        gh_available()
    }

    fn create_pr(&self, repo_dir: &Path, params: CreatePrParams) -> Result<String> {
        run_gh(
            repo_dir,
            &[
                "pr",
                "create",
                "--title",
                params.title,
                "--body",
                params.body,
                "--base",
                params.base_branch,
                "--head",
                params.head_branch,
            ],
        )
    }

    fn get_pr_status(&self, repo_dir: &Path, pr_url: &str) -> Result<PrStatus> {
        let json = run_gh(repo_dir, &["pr", "view", pr_url, "--json", "state,url"])?;
        #[derive(serde::Deserialize)]
        struct Raw {
            state: String,
            url: String,
        }
        let raw: Raw = serde_json::from_str(&json)
            .with_context(|| format!("parsing `gh pr view` JSON output: {json}"))?;
        let state = match raw.state.as_str() {
            "OPEN" => PrState::Open,
            "MERGED" => PrState::Merged,
            "CLOSED" => PrState::Closed,
            other => return Err(anyhow!("unrecognized PR state from gh: {other}")),
        };
        Ok(PrStatus { state, url: raw.url })
    }

    fn create_issue(&self, repo_dir: &Path, params: CreateIssueParams) -> Result<String> {
        run_gh(repo_dir, &["issue", "create", "--title", params.title, "--body", params.body])
    }

    fn close_issue_with_comment(&self, repo_dir: &Path, issue_url: &str, comment: &str) -> Result<()> {
        run_gh(repo_dir, &["issue", "comment", issue_url, "--body", comment])?;
        run_gh(repo_dir, &["issue", "close", issue_url])?;
        Ok(())
    }
}

/// A test double, exported (not just `#[cfg(test)]`-gated) so
/// `solexa-coordinator`'s own test suite can depend on it without
/// duplicating it, same reasoning as why this isn't hidden behind a
/// feature flag: the whole point is other crates' tests substitute it
/// in place of `GhCliClient`.
pub mod fake {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Mutex;

    #[derive(Debug, Clone)]
    pub struct CreatedPr {
        pub repo_dir: PathBuf,
        pub title: String,
        pub body: String,
        pub base_branch: String,
        pub head_branch: String,
        pub url: String,
    }

    /// Records every PR it was asked to create and returns whatever
    /// status a test has set for a given URL, never shells out to
    /// `gh`, never touches the network. A freshly created PR starts
    /// `Open`, matching real GitHub's behavior for a brand new PR.
    #[derive(Default)]
    pub struct FakeGithubClient {
        pub created_prs: Mutex<Vec<CreatedPr>>,
        statuses: Mutex<HashMap<String, PrStatus>>,
        pub created_issues: Mutex<Vec<CreatedIssue>>,
        pub closed_issues: Mutex<Vec<ClosedIssue>>,
        next_id: Mutex<u32>,
    }

    #[derive(Debug, Clone)]
    pub struct CreatedIssue {
        pub repo_dir: PathBuf,
        pub title: String,
        pub body: String,
        pub url: String,
    }

    #[derive(Debug, Clone)]
    pub struct ClosedIssue {
        pub issue_url: String,
        pub comment: String,
    }

    impl FakeGithubClient {
        pub fn new() -> Self {
            Self::default()
        }

        /// Lets a test simulate a reviewer merging or closing the PR
        /// before the coordinator checks its status.
        pub fn set_status(&self, pr_url: &str, state: PrState) {
            self.statuses
                .lock()
                .unwrap()
                .insert(pr_url.to_string(), PrStatus { state, url: pr_url.to_string() });
        }

        pub fn created_pr_count(&self) -> usize {
            self.created_prs.lock().unwrap().len()
        }

        pub fn created_issue_count(&self) -> usize {
            self.created_issues.lock().unwrap().len()
        }
    }

    impl GithubClient for FakeGithubClient {
        fn create_pr(&self, repo_dir: &Path, params: CreatePrParams) -> Result<String> {
            let mut next_id = self.next_id.lock().unwrap();
            *next_id += 1;
            let url = format!("https://github.com/fake-org/fake-repo/pull/{next_id}");
            self.statuses.lock().unwrap().insert(url.clone(), PrStatus { state: PrState::Open, url: url.clone() });
            self.created_prs.lock().unwrap().push(CreatedPr {
                repo_dir: repo_dir.to_path_buf(),
                title: params.title.to_string(),
                body: params.body.to_string(),
                base_branch: params.base_branch.to_string(),
                head_branch: params.head_branch.to_string(),
                url: url.clone(),
            });
            Ok(url)
        }

        fn get_pr_status(&self, _repo_dir: &Path, pr_url: &str) -> Result<PrStatus> {
            self.statuses
                .lock()
                .unwrap()
                .get(pr_url)
                .cloned()
                .ok_or_else(|| anyhow!("fake: no such PR {pr_url}, was create_pr ever called for it?"))
        }

        fn create_issue(&self, repo_dir: &Path, params: CreateIssueParams) -> Result<String> {
            let mut next_id = self.next_id.lock().unwrap();
            *next_id += 1;
            let url = format!("https://github.com/fake-org/fake-repo/issues/{next_id}");
            self.created_issues.lock().unwrap().push(CreatedIssue {
                repo_dir: repo_dir.to_path_buf(),
                title: params.title.to_string(),
                body: params.body.to_string(),
                url: url.clone(),
            });
            Ok(url)
        }

        fn close_issue_with_comment(&self, _repo_dir: &Path, issue_url: &str, comment: &str) -> Result<()> {
            self.closed_issues
                .lock()
                .unwrap()
                .push(ClosedIssue { issue_url: issue_url.to_string(), comment: comment.to_string() });
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fake::FakeGithubClient;
    use std::path::PathBuf;

    // Round 66 (T6): the precondition check used to be tested by
    // asserting `!gh_available()` on the assumption that `gh` was
    // installed but never authenticated, true in the original build
    // sandbox, false on any machine that has run `gh auth login` (this
    // one), so the whole workspace suite went red on a normal dev box.
    // These two tests exercise the real logic (exit-status mapping,
    // missing binary) without depending on the machine's `gh` state.
    // What is *not* covered here: that a real installed-and-not-logged-in
    // `gh` makes it false, that needs an environment in that state.

    /// A `Command` that exits with `code` and ignores any extra args.
    fn command_that_exits(code: i32) -> Command {
        #[cfg(windows)]
        {
            let mut command = Command::new("cmd");
            command.args(["/c", &format!("exit {code}")]);
            command
        }
        #[cfg(unix)]
        {
            let mut command = Command::new("sh");
            command.args(["-c", &format!("exit {code}")]);
            command
        }
    }

    #[test]
    fn gh_available_is_true_only_when_the_command_succeeds() {
        assert!(gh_available_from(command_that_exits(0)));
        assert!(!gh_available_from(command_that_exits(1)));
    }

    #[test]
    fn gh_available_is_false_when_the_command_cannot_be_run() {
        assert!(!gh_available_from(Command::new("solexa-no-such-binary-xyz")));
    }

    #[test]
    fn gh_cli_client_is_available_matches_the_free_function() {
        assert_eq!(GhCliClient.is_available(), gh_available());
    }

    #[test]
    fn fake_client_records_the_pr_it_was_asked_to_create() {
        let client = FakeGithubClient::new();
        let url = client
            .create_pr(
                &PathBuf::from("/tmp/whatever"),
                CreatePrParams {
                    title: "Resolve conflict: Team mission",
                    body: "Accept the local device's proposed value.",
                    base_branch: "main",
                    head_branch: "solexa-conflict/team-abcd1234",
                },
            )
            .unwrap();
        assert_eq!(client.created_pr_count(), 1);
        let recorded = &client.created_prs.lock().unwrap()[0];
        assert_eq!(recorded.url, url);
        assert_eq!(recorded.base_branch, "main");
        assert_eq!(recorded.head_branch, "solexa-conflict/team-abcd1234");
    }

    #[test]
    fn fake_client_starts_a_new_pr_open_and_reflects_a_later_status_change() {
        let client = FakeGithubClient::new();
        let url = client
            .create_pr(
                &PathBuf::from("/tmp/whatever"),
                CreatePrParams { title: "t", body: "b", base_branch: "main", head_branch: "feature" },
            )
            .unwrap();

        let status = client.get_pr_status(&PathBuf::from("/tmp/whatever"), &url).unwrap();
        assert_eq!(status.state, PrState::Open);

        client.set_status(&url, PrState::Merged);
        let status = client.get_pr_status(&PathBuf::from("/tmp/whatever"), &url).unwrap();
        assert_eq!(status.state, PrState::Merged);
    }

    #[test]
    fn fake_client_errors_on_a_url_it_never_created() {
        let client = FakeGithubClient::new();
        let err = client.get_pr_status(&PathBuf::from("/tmp/whatever"), "https://github.com/x/y/pull/999");
        assert!(err.is_err());
    }

    #[test]
    fn fake_client_records_issues_it_creates_and_closes() {
        let client = FakeGithubClient::new();
        let url = client
            .create_issue(
                &PathBuf::from("/tmp/whatever"),
                CreateIssueParams { title: "Which database?", body: "Postgres or SQLite, see SOLEXA for the full context." },
            )
            .unwrap();
        assert_eq!(client.created_issue_count(), 1);
        assert!(url.contains("/issues/"));

        client.close_issue_with_comment(&PathBuf::from("/tmp/whatever"), &url, "Resolved in SOLEXA: Postgres").unwrap();
        let closed = client.closed_issues.lock().unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].issue_url, url);
        assert_eq!(closed[0].comment, "Resolved in SOLEXA: Postgres");
    }
}
