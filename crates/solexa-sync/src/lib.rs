//! solexa-sync, the low-level, pure I/O half of round 22's git-based
//! project sync (Phase 8, "Collaboration").
//!
//! Everything here does exactly two things: write/read a
//! `ProjectSnapshot` as files, and shell out to the system `git`
//! binary. Nothing here touches SQLite, nothing here knows what a
//! Member/Team/Specialist means beyond "a field inside a snapshot
//! file", and nothing here decides how to merge two diverging
//! snapshots, that's `solexa-coordinator`'s job, exactly the way
//! `solexa-resources` tracks hardware without knowing what a task is,
//! or `solexa-verification` runs checks without knowing what a
//! specialist is. This crate is the subsystem the coordinator composes.
//!
//! Deliberately **not** here: any GitHub API integration, any OAuth/PAT
//! handling, any cloning of a fresh repo from a URL. `repo_dir` is
//! always a directory the user already set up themselves (`git clone`,
//! their own credentials already configured), this crate only ever
//! runs `git` *inside* it. See `solexa_domain::sync` for the full
//! rationale.
//!
//! **Round 24**: rounds 22–23 wrote one combined `.solexa/project.json`.
//! This round splits it into one file per entity category
//! (`members.json`, `teams.json`, ...) so a git diff for "someone
//! edited a Team's mission" only ever touches `teams.json`, instead of
//! a five-category file where every sync touches the same line range
//! regardless of what actually changed. `write_snapshot` also cleans up
//! the old single-file layout if it finds one, so a repo that's been
//! synced since round 22 migrates cleanly on its next push rather than
//! ending up with both layouts committed side by side.

use anyhow::{anyhow, Context, Result};
use solexa_domain::{Decision, KnowledgeItem, Member, ProjectSnapshot, Specialist, SpecialistBenchmark, Team, Uuid};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;

/// Where a linked project's synced files live inside the repo, relative
/// to its root.
pub const SNAPSHOT_DIR: &str = ".solexa";

const MANIFEST_FILE: &str = "manifest.json";
const MEMBERS_FILE: &str = "members.json";
const TEAMS_FILE: &str = "teams.json";
const SPECIALISTS_FILE: &str = "specialists.json";
const BENCHMARKS_FILE: &str = "specialist_benchmarks.json";
const KNOWLEDGE_ITEMS_FILE: &str = "knowledge_items.json";
const DECISIONS_FILE: &str = "decisions.json";

/// Round 22–23's combined file, relative to the repo root, cleaned up
/// by `write_snapshot` once a repo has been pushed under the new
/// per-category layout at least once.
const LEGACY_SNAPSHOT_RELATIVE_PATH: &str = ".solexa/project.json";

/// Just the header fields, everything in `ProjectSnapshot` that isn't
/// one of the per-category collections. A private, file-layout-only
/// concept: nothing outside this crate needs to know the manifest
/// exists as its own file.
#[derive(Serialize, Deserialize)]
struct Manifest {
    format_version: u32,
    project_id: Uuid,
    project_name: String,
    project_description: String,
}

fn run_git(repo_dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(args)
        .output()
        .with_context(|| format!("running `git {}` in {}", args.join(" "), repo_dir.display()))?;
    if !output.status.success() {
        return Err(anyhow!(
            "`git {}` failed in {}: {}",
            args.join(" "),
            repo_dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Whether `dir` is inside a git working tree at all. Used by
/// `Coordinator::link_git_repo` to give an immediate, specific error
/// rather than only discovering the problem on the first push.
pub fn is_git_repo(dir: &Path) -> bool {
    dir.is_dir()
        && Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", "--is-inside-work-tree"])
            .output()
            .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true")
            .unwrap_or(false)
}

pub fn head_commit(repo_dir: &Path) -> Result<String> {
    run_git(repo_dir, &["rev-parse", "HEAD"])
}

fn write_json<T: Serialize>(dir: &Path, file_name: &str, value: &T) -> Result<()> {
    let path = dir.join(file_name);
    let json = serde_json::to_string_pretty(value).with_context(|| format!("serializing {file_name}"))?;
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))
}

/// Writes the snapshot as one file per category under `SNAPSHOT_DIR`,
/// pretty-printed for readable diffs. Does **not** canonicalize,
/// callers (`Coordinator::export_project_snapshot`) are expected to
/// have already sorted every collection, so two exports of identical
/// state produce byte-identical files. Also removes round 22–23's
/// single combined file if this repo still has one, staging that
/// removal isn't this function's job, `commit_and_push`'s `git add`
/// picks up deletions the same way it picks up new/changed files.
pub fn write_snapshot(repo_dir: &Path, snapshot: &ProjectSnapshot) -> Result<()> {
    let dir = repo_dir.join(SNAPSHOT_DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    write_json(&dir, MANIFEST_FILE, &Manifest {
        format_version: snapshot.format_version,
        project_id: snapshot.project_id,
        project_name: snapshot.project_name.clone(),
        project_description: snapshot.project_description.clone(),
    })?;
    write_json(&dir, MEMBERS_FILE, &snapshot.members)?;
    write_json(&dir, TEAMS_FILE, &snapshot.teams)?;
    write_json(&dir, SPECIALISTS_FILE, &snapshot.specialists)?;
    write_json(&dir, BENCHMARKS_FILE, &snapshot.specialist_benchmarks)?;
    write_json(&dir, KNOWLEDGE_ITEMS_FILE, &snapshot.knowledge_items)?;
    write_json(&dir, DECISIONS_FILE, &snapshot.decisions)?;

    let legacy_path = repo_dir.join(LEGACY_SNAPSHOT_RELATIVE_PATH);
    if legacy_path.is_file() {
        std::fs::remove_file(&legacy_path).with_context(|| format!("removing legacy {}", legacy_path.display()))?;
    }
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(dir: &Path, file_name: &str) -> Result<T> {
    let path = dir.join(file_name);
    let json = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&json).with_context(|| format!("parsing {}", path.display()))
}

/// Reads the per-category files under `SNAPSHOT_DIR` back into a
/// `ProjectSnapshot`. Gives a specific, distinct error for the two ways
/// this can fail: the repo was never synced at all, or it was last
/// synced under round 22–23's single-file layout and hasn't been
/// pushed since by an updated SOLEXA (one push under the new code
/// migrates it, see `write_snapshot`).
pub fn read_snapshot(repo_dir: &Path) -> Result<ProjectSnapshot> {
    let dir = repo_dir.join(SNAPSHOT_DIR);
    let manifest_path = dir.join(MANIFEST_FILE);
    if !manifest_path.is_file() {
        if repo_dir.join(LEGACY_SNAPSHOT_RELATIVE_PATH).is_file() {
            return Err(anyhow!(
                "{} was synced under SOLEXA's old single-file format and hasn't been pushed since, push once from an updated SOLEXA to migrate this repo, then pull again",
                repo_dir.join(LEGACY_SNAPSHOT_RELATIVE_PATH).display()
            ));
        }
        return Err(anyhow!("{} not found, is this repo actually linked and synced?", manifest_path.display()));
    }

    let manifest: Manifest = read_json(&dir, MANIFEST_FILE)?;
    if manifest.format_version != solexa_domain::CURRENT_SNAPSHOT_FORMAT_VERSION {
        return Err(anyhow!(
            "{} was written by a newer or older SOLEXA (format_version {}, this build understands {})",
            manifest_path.display(),
            manifest.format_version,
            solexa_domain::CURRENT_SNAPSHOT_FORMAT_VERSION
        ));
    }

    Ok(ProjectSnapshot {
        format_version: manifest.format_version,
        project_id: manifest.project_id,
        project_name: manifest.project_name,
        project_description: manifest.project_description,
        members: read_json::<Vec<Member>>(&dir, MEMBERS_FILE)?,
        teams: read_json::<Vec<Team>>(&dir, TEAMS_FILE)?,
        specialists: read_json::<Vec<Specialist>>(&dir, SPECIALISTS_FILE)?,
        specialist_benchmarks: read_json::<Vec<SpecialistBenchmark>>(&dir, BENCHMARKS_FILE)?,
        knowledge_items: read_json::<Vec<KnowledgeItem>>(&dir, KNOWLEDGE_ITEMS_FILE)?,
        decisions: read_json::<Vec<Decision>>(&dir, DECISIONS_FILE)?,
    })
}

#[derive(Debug, Clone)]
pub struct PushOutcome {
    /// `false` when the freshly-written snapshot was byte-identical to
    /// what's already committed, a legitimate, common "nothing to
    /// sync" outcome, not an error.
    pub pushed: bool,
    pub commit: String,
}

/// Stages, commits (if anything actually changed), and pushes
/// `SNAPSHOT_DIR`. Only ever touches that one directory, never
/// `git add .` or `git add -A`, so this can't accidentally sweep up
/// unrelated files the user has sitting in the same repo. Staging
/// `SNAPSHOT_DIR` (rather than each file individually) also picks up
/// `write_snapshot`'s removal of the legacy combined file, since `git
/// add <path>` stages deletions under `<path>` the same way it stages
/// additions and changes.
pub fn commit_and_push(repo_dir: &Path, commit_message: &str) -> Result<PushOutcome> {
    run_git(repo_dir, &["add", "--", SNAPSHOT_DIR])?;
    let staged = run_git(repo_dir, &["status", "--porcelain", "--", SNAPSHOT_DIR])?;
    if staged.is_empty() {
        return Ok(PushOutcome { pushed: false, commit: head_commit(repo_dir)? });
    }
    run_git(repo_dir, &["commit", "-m", commit_message])?;
    run_git(repo_dir, &["push"])?;
    Ok(PushOutcome { pushed: true, commit: head_commit(repo_dir)? })
}

#[derive(Debug, Clone)]
pub struct PullOutcome {
    pub commit: String,
}

/// `git pull --ff-only`. Deliberately refuses (returns `Err`) rather
/// than merging when the local branch has diverged commits of its own
/// the common case this round handles is "SOLEXA is the only writer
/// to these files, on however many machines"; a genuinely diverged git
/// history (e.g. someone hand-edited and committed a file outside
/// SOLEXA on two machines) is a real scope boundary, named in round
/// 22's README, not silently force-merged.
pub fn pull_ff_only(repo_dir: &Path) -> Result<PullOutcome> {
    run_git(repo_dir, &["pull", "--ff-only"])?;
    Ok(PullOutcome { commit: head_commit(repo_dir)? })
}

// ---- round 30: git mechanics for "mirror a Conflict as a GitHub PR" ----
//
// Everything below is still plain git, no GitHub API, no `gh` CLI, on
// purpose (see `solexa_github` for that half). A conflict's proposed
// resolution needs its own throwaway branch pushed to the remote
// before a PR can point at it; these four functions are exactly the
// git mechanics that takes, kept as small and boring as the functions
// above them.

/// The branch currently checked out, used so `send_conflict_to_github`
/// can switch back to it after building the throwaway branch,
/// regardless of what the user happened to have checked out (`main`,
/// `master`, or anything else).
pub fn current_branch(repo_dir: &Path) -> Result<String> {
    run_git(repo_dir, &["rev-parse", "--abbrev-ref", "HEAD"])
}

/// Creates and switches to a brand new branch off the current HEAD.
/// Errors if `branch_name` already exists, round 30 always derives it
/// from the conflict's own id, so a collision would mean calling this
/// twice for the same conflict without cleaning up first, which is
/// worth surfacing rather than silently resetting an existing branch.
pub fn checkout_new_branch(repo_dir: &Path, branch_name: &str) -> Result<()> {
    run_git(repo_dir, &["checkout", "-b", branch_name]).map(|_| ())
}

/// Switches back to a branch that already exists, the counterpart to
/// `checkout_new_branch`, used to restore the working tree to whatever
/// `current_branch` reported before the throwaway branch was created.
pub fn checkout_branch(repo_dir: &Path, branch_name: &str) -> Result<()> {
    run_git(repo_dir, &["checkout", branch_name]).map(|_| ())
}

/// Like `commit_and_push`, but for a brand-new local branch with no
/// upstream yet, plain `git push` refuses that until an upstream is
/// set once (`push -u`). `pushed: false` (no upstream ever set) means
/// the working tree's `SNAPSHOT_DIR` was already byte-identical to
/// what's already committed, a legitimate "there's nothing to open a
/// PR about" outcome, not an error, exactly like `commit_and_push`'s
/// own empty-diff case.
pub fn commit_and_push_new_branch(repo_dir: &Path, branch_name: &str, commit_message: &str) -> Result<PushOutcome> {
    run_git(repo_dir, &["add", "--", SNAPSHOT_DIR])?;
    let staged = run_git(repo_dir, &["status", "--porcelain", "--", SNAPSHOT_DIR])?;
    if staged.is_empty() {
        return Ok(PushOutcome { pushed: false, commit: head_commit(repo_dir)? });
    }
    run_git(repo_dir, &["commit", "-m", commit_message])?;
    run_git(repo_dir, &["push", "-u", "origin", branch_name])?;
    Ok(PushOutcome { pushed: true, commit: head_commit(repo_dir)? })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command as StdCommand;

    fn git(dir: &Path, args: &[&str]) {
        let status = StdCommand::new("git").arg("-C").arg(dir).args(args).status().unwrap();
        assert!(status.success(), "git {:?} failed in {}", args, dir.display());
    }

    fn init_bare_remote_and_clone(tmp: &Path) -> (PathBuf, PathBuf) {
        let remote = tmp.join("remote.git");
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, &["init", "--bare", "-b", "main"]);

        let clone_dir = tmp.join("clone_a");
        git(tmp, &["clone", remote.to_str().unwrap(), clone_dir.to_str().unwrap()]);
        git(&clone_dir, &["config", "user.email", "test@example.com"]);
        git(&clone_dir, &["config", "user.name", "Test"]);
        // A bare repo has no commits yet, an empty initial commit gives
        // clone_a's `main` something to push and later clones something
        // to check out, matching a real freshly-created GitHub repo.
        git(&clone_dir, &["commit", "--allow-empty", "-m", "initial commit"]);
        git(&clone_dir, &["push", "-u", "origin", "main"]);

        (remote, clone_dir)
    }

    fn sample_snapshot(project_id: Uuid) -> ProjectSnapshot {
        ProjectSnapshot {
            format_version: solexa_domain::CURRENT_SNAPSHOT_FORMAT_VERSION,
            project_id,
            project_name: "Test Project".into(),
            project_description: "".into(),
            members: vec![],
            teams: vec![],
            specialists: vec![],
            specialist_benchmarks: vec![],
            knowledge_items: vec![],
            decisions: vec![],
        }
    }

    #[test]
    fn is_git_repo_is_true_only_for_a_real_working_tree() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!is_git_repo(tmp.path()));
        assert!(!is_git_repo(&tmp.path().join("does-not-exist")));

        let (_remote, clone_dir) = init_bare_remote_and_clone(tmp.path());
        assert!(is_git_repo(&clone_dir));
    }

    #[test]
    fn round_trips_a_snapshot_through_a_real_push_and_pull() {
        // Two independent local clones of the same bare "remote",
        // exactly the shape of two teammates' machines, just without a
        // real GitHub in between (git doesn't know the difference).
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_a) = init_bare_remote_and_clone(tmp.path());
        let clone_b = tmp.path().join("clone_b");
        git(tmp.path(), &["clone", remote.to_str().unwrap(), clone_b.to_str().unwrap()]);
        git(&clone_b, &["config", "user.email", "test@example.com"]);
        git(&clone_b, &["config", "user.name", "Test"]);

        let project_id = Uuid::new_v4();
        let snapshot = sample_snapshot(project_id);

        write_snapshot(&clone_a, &snapshot).unwrap();
        let push = commit_and_push(&clone_a, "SOLEXA sync").unwrap();
        assert!(push.pushed);

        // A second push with no changes must be a genuine no-op.
        write_snapshot(&clone_a, &snapshot).unwrap();
        let noop = commit_and_push(&clone_a, "SOLEXA sync").unwrap();
        assert!(!noop.pushed);
        assert_eq!(noop.commit, push.commit);

        let pulled = pull_ff_only(&clone_b).unwrap();
        assert_eq!(pulled.commit, push.commit);
        let read_back = read_snapshot(&clone_b).unwrap();
        assert_eq!(read_back.project_id, project_id);
        assert_eq!(read_back.project_name, "Test Project");

        // The point of this round: separate files per category, not
        // one combined blob.
        for file in [MANIFEST_FILE, MEMBERS_FILE, TEAMS_FILE, SPECIALISTS_FILE, BENCHMARKS_FILE, KNOWLEDGE_ITEMS_FILE, DECISIONS_FILE] {
            assert!(clone_b.join(SNAPSHOT_DIR).join(file).is_file(), "{file} should exist after pulling");
        }
    }

    #[test]
    fn read_snapshot_gives_a_clear_error_when_never_synced() {
        let tmp = tempfile::tempdir().unwrap();
        let (_remote, clone_dir) = init_bare_remote_and_clone(tmp.path());
        let err = read_snapshot(&clone_dir).unwrap_err();
        assert!(err.to_string().contains("is this repo actually linked and synced"));
    }

    #[test]
    fn write_snapshot_migrates_a_repo_still_on_the_legacy_single_file_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let (_remote, clone_dir) = init_bare_remote_and_clone(tmp.path());

        // Simulate a repo last pushed by round 22/23's code: the old
        // combined file, committed, nothing in the new layout.
        let project_id = Uuid::new_v4();
        let legacy_snapshot = sample_snapshot(project_id);
        let legacy_path = clone_dir.join(LEGACY_SNAPSHOT_RELATIVE_PATH);
        std::fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        std::fs::write(&legacy_path, serde_json::to_string_pretty(&legacy_snapshot).unwrap()).unwrap();
        git(&clone_dir, &["add", "--", LEGACY_SNAPSHOT_RELATIVE_PATH]);
        git(&clone_dir, &["commit", "-m", "legacy single-file sync"]);
        git(&clone_dir, &["push"]);

        // Pulling it with round-24 code, before any push, must fail
        // with the specific "old format" message, not the generic
        // "never synced" one.
        let err = read_snapshot(&clone_dir).unwrap_err();
        assert!(err.to_string().contains("old single-file format"), "got: {err}");

        // The next push under the new code must clean the legacy file
        // up rather than leaving both layouts committed side by side.
        write_snapshot(&clone_dir, &legacy_snapshot).unwrap();
        assert!(!legacy_path.exists(), "legacy file should be removed from the working tree");
        let push = commit_and_push(&clone_dir, "migrate to per-category files").unwrap();
        assert!(push.pushed);

        let tracked = git_ls_files(&clone_dir);
        assert!(!tracked.contains(&LEGACY_SNAPSHOT_RELATIVE_PATH.to_string()), "legacy file should no longer be tracked: {tracked:?}");
        assert!(tracked.iter().any(|f| f.ends_with(MANIFEST_FILE)));

        // And it reads back cleanly now.
        let read_back = read_snapshot(&clone_dir).unwrap();
        assert_eq!(read_back.project_id, project_id);
    }

    fn git_ls_files(dir: &Path) -> Vec<String> {
        let output = StdCommand::new("git").arg("-C").arg(dir).args(["ls-files"]).output().unwrap();
        String::from_utf8_lossy(&output.stdout).lines().map(|s| s.to_string()).collect()
    }

    fn remote_branch_names(remote: &Path) -> Vec<String> {
        let output = StdCommand::new("git").arg("-C").arg(remote).args(["branch", "--list"]).output().unwrap();
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|l| l.trim_start_matches("* ").trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    }

    // ---- round 30: the new git-mechanics helpers ---------------------

    #[test]
    fn current_branch_reports_the_checked_out_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let (_remote, clone_dir) = init_bare_remote_and_clone(tmp.path());
        assert_eq!(current_branch(&clone_dir).unwrap(), "main");
    }

    #[test]
    fn checkout_new_branch_and_checkout_branch_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let (_remote, clone_dir) = init_bare_remote_and_clone(tmp.path());

        checkout_new_branch(&clone_dir, "solexa-conflict/team-abcd1234").unwrap();
        assert_eq!(current_branch(&clone_dir).unwrap(), "solexa-conflict/team-abcd1234");

        checkout_branch(&clone_dir, "main").unwrap();
        assert_eq!(current_branch(&clone_dir).unwrap(), "main");
    }

    #[test]
    fn commit_and_push_new_branch_pushes_a_real_branch_the_remote_actually_has() {
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_dir) = init_bare_remote_and_clone(tmp.path());
        let project_id = Uuid::new_v4();

        checkout_new_branch(&clone_dir, "solexa-conflict/team-abcd1234").unwrap();
        write_snapshot(&clone_dir, &sample_snapshot(project_id)).unwrap();
        let outcome = commit_and_push_new_branch(&clone_dir, "solexa-conflict/team-abcd1234", "propose local's value").unwrap();
        assert!(outcome.pushed);

        // Not just "the command exited 0", the bare remote itself, a
        // separate directory this test never wrote to directly, really
        // has the branch. This is what a `gh pr create --head
        // solexa-conflict/team-abcd1234` would depend on succeeding.
        let branches = remote_branch_names(&remote);
        assert!(branches.contains(&"solexa-conflict/team-abcd1234".to_string()), "remote branches: {branches:?}");
    }

    #[test]
    fn commit_and_push_new_branch_reports_pushed_false_and_pushes_nothing_when_there_is_no_change() {
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_dir) = init_bare_remote_and_clone(tmp.path());
        let project_id = Uuid::new_v4();

        // Establish a baseline on main first, so the new branch's
        // write_snapshot below produces byte-identical content, the
        // same "nothing actually changed" case `commit_and_push` itself
        // already handles for the normal (non-branch) push path.
        write_snapshot(&clone_dir, &sample_snapshot(project_id)).unwrap();
        commit_and_push(&clone_dir, "baseline").unwrap();

        checkout_new_branch(&clone_dir, "solexa-conflict/nothing-to-propose").unwrap();
        write_snapshot(&clone_dir, &sample_snapshot(project_id)).unwrap();
        let outcome = commit_and_push_new_branch(&clone_dir, "solexa-conflict/nothing-to-propose", "propose local's value").unwrap();
        assert!(!outcome.pushed);

        let branches = remote_branch_names(&remote);
        assert!(!branches.contains(&"solexa-conflict/nothing-to-propose".to_string()), "an empty-diff branch should never actually reach the remote: {branches:?}");
    }
}
