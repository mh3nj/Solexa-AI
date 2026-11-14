//! GitSyncConfig, a project's link to a local git working directory
//! used as its "collaboration server" (round 22, Phase 8).
//!
//! Deliberately the local-first, no-infrastructure slice of
//! "Collaboration" the round's scoping conversation landed on: there is
//! no SOLEXA server, no SOLEXA account system, and no SOLEXA-specific
//! auth. `repo_path` points at a directory the *user* already cloned
//! themselves with their own git credentials, SOLEXA only ever shells
//! out to the system `git` binary inside it (see `solexa-sync`).
//! Whoever has push access to that remote (a GitHub repo, most likely,
//! but nothing here assumes that specifically) already forms the
//! "team", GitHub's own collaborator/invite system does the work a
//! bespoke auth model would otherwise need to.
//!
//! One config per project (v1), `project_id` is the natural key.
//! `last_synced_snapshot_json` is the exact canonical JSON that was
//! true immediately after the last successful push or pull; it's what
//! `Coordinator::pull_from_git` diffs a fresh local export against to
//! tell "local hasn't changed since we last agreed with the remote"
//! apart from "local has real unsynced edits", something git itself
//! can't answer, since git only ever sees the exported file, never the
//! live SQLite state it came from.
//!
//! Round 46 adds `require_review_for_incoming_changes` and
//! `PendingSyncChange`: an opt-in gate so a non-conflicting incoming
//! change can be held for a human to explicitly apply or deny, instead
//! of always fast-forwarding automatically. Deliberately local-first
//! like everything else here, it works whether or not the project is
//! even linked to GitHub, unlike rounds 30–33's PR/Issue mirroring,
//! which only ever covers genuine field-level conflicts.

use crate::conflict::SyncObjectKind;
use crate::Uuid;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitSyncConfig {
    pub project_id: Uuid,
    pub repo_path: String,
    pub linked_at: chrono::DateTime<chrono::Utc>,
    pub last_synced_commit: Option<String>,
    pub last_synced_snapshot_json: Option<String>,
    pub last_synced_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Round 46: when `true`, `Coordinator::pull_from_git` no longer
    /// applies a non-conflicting incoming change immediately, it
    /// queues a `PendingSyncChange` instead, and a registered Member
    /// must explicitly `apply_pending_sync_change` or
    /// `deny_pending_sync_change` it. Defaults to `false` so every
    /// project and test that predates this round keeps its existing
    /// fast-forward-on-pull behavior unchanged. This is deliberately
    /// independent of GitHub (rounds 30–33's PR/Issue mirroring, which
    /// only ever covers genuine *conflicts*): the point is a review
    /// gate that works with GitHub reachable, unreachable, or never
    /// configured at all, see solexa-domain's crate-level rule 10,
    /// local-first with no cloud dependency.
    pub require_review_for_incoming_changes: bool,
}

impl GitSyncConfig {
    pub fn new(project_id: Uuid, repo_path: impl Into<String>) -> Self {
        Self {
            project_id,
            repo_path: repo_path.into(),
            linked_at: chrono::Utc::now(),
            last_synced_commit: None,
            last_synced_snapshot_json: None,
            last_synced_at: None,
            require_review_for_incoming_changes: false,
        }
    }
}

/// Round 46: a non-conflicting incoming change from `pull_from_git`,
/// held for human review instead of being applied immediately, the
/// counterpart to `Conflict` for the *non*-colliding case. A `Conflict`
/// exists because two sides disagree; a `PendingSyncChange` exists
/// because nobody's reviewed a one-sided change yet, whether or not it
/// would have applied cleanly.
///
/// Deliberately reuses `SyncObjectKind` rather than inventing a
/// parallel enum, this is the same five kinds `pull_from_git` already
/// diffs (Member, Team, Specialist, KnowledgeItem, Decision), just a
/// different disposition for the "no collision" case when a project
/// opts in via `GitSyncConfig::require_review_for_incoming_changes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingSyncChangeStatus {
    Pending,
    Applied,
    Denied,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingSyncChange {
    pub id: Uuid,
    pub project_id: Uuid,
    pub kind: SyncObjectKind,
    pub object_id: Uuid,
    /// A human-readable label, same convention as
    /// `ConflictPosition::specialist_name`, a Member's display_name,
    /// a Team's name, etc., resolved once at queue time so the review
    /// UI never needs to separately look up "what is this object".
    pub label: String,
    /// The full incoming object, exactly as `pull_from_git`'s own
    /// `to_apply` would have upserted it verbatim.
    pub object_json: serde_json::Value,
    /// Round 46, Specialist kind only: the specialist's benchmark from
    /// the same remote snapshot, if any, carried alongside so
    /// approving a queued Specialist change applies its benchmark too,
    /// exactly matching what the non-reviewed path already does in one
    /// step. Always `None` for every other kind.
    pub benchmark_json: Option<serde_json::Value>,
    /// Whether this object doesn't exist locally at all yet (a create)
    /// versus already existing locally with a different value (an
    /// update), worth surfacing directly in the review UI rather than
    /// making a reviewer diff it themselves.
    pub is_new: bool,
    pub source_commit: String,
    pub discovered_at: chrono::DateTime<chrono::Utc>,
    pub status: PendingSyncChangeStatus,
    /// A human-readable label for whoever decided it, same
    /// `impl Into<String>` convention as `Conflict::approved_by`, not
    /// a hard foreign key, since a Member row can later be deleted
    /// without invalidating this record's own history.
    pub decided_by: Option<String>,
    pub decided_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Round 46: an optional note recorded on denial, deliberately
    /// something GitHub's own "close" action doesn't give you for
    /// free, one concrete way this is meant to be a *better* gate than
    /// mirroring through a PR, not just a local-only copy of one.
    pub denial_reason: Option<String>,
}

impl PendingSyncChange {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        project_id: Uuid,
        kind: SyncObjectKind,
        object_id: Uuid,
        label: impl Into<String>,
        object_json: serde_json::Value,
        benchmark_json: Option<serde_json::Value>,
        is_new: bool,
        source_commit: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            project_id,
            kind,
            object_id,
            label: label.into(),
            object_json,
            benchmark_json,
            is_new,
            source_commit: source_commit.into(),
            discovered_at: chrono::Utc::now(),
            status: PendingSyncChangeStatus::Pending,
            decided_by: None,
            decided_at: None,
            denial_reason: None,
        }
    }

    /// Refreshes an existing *pending* row with newer incoming content
    /// from a later pull, without losing its id or `discovered_at`,
    /// used instead of inserting a duplicate row when the same object
    /// is still undecided and the remote has moved again in the
    /// meantime.
    pub fn refresh_content(&mut self, label: impl Into<String>, object_json: serde_json::Value, benchmark_json: Option<serde_json::Value>, is_new: bool, source_commit: impl Into<String>) {
        self.label = label.into();
        self.object_json = object_json;
        self.benchmark_json = benchmark_json;
        self.is_new = is_new;
        self.source_commit = source_commit.into();
    }

    pub fn apply(&mut self, decided_by: impl Into<String>) {
        self.status = PendingSyncChangeStatus::Applied;
        self.decided_by = Some(decided_by.into());
        self.decided_at = Some(chrono::Utc::now());
    }

    pub fn deny(&mut self, decided_by: impl Into<String>, reason: Option<String>) {
        self.status = PendingSyncChangeStatus::Denied;
        self.decided_by = Some(decided_by.into());
        self.decided_at = Some(chrono::Utc::now());
        self.denial_reason = reason;
    }
}
