//! The Project Brain's core objects (Phase 4 of the roadmap).
//!
//! Spec rule: "The Project Brain should distinguish: Facts, Constraints,
//! Decisions, Assumptions, Proposals, Evidence, Artifacts,
//! Relationships, Tasks, Ownership, History... This creates project
//! knowledge with provenance rather than a pile of chat messages."
//!
//! `KnowledgeItem` covers facts, constraints, and artifacts with one
//! shared shape (status + provenance + tags for scoping) since they
//! play the same structural role: a piece of project knowledge a task's
//! context can be scoped against. `Decision` is kept as its own type
//! because the spec gives decisions a distinctly richer shape (options,
//! evidence, an approver) that would just be dead fields on every fact
//! and constraint if merged in.

use crate::Uuid;
use serde::{Deserialize, Serialize};

/// Rule: "Every knowledge item should have a status... This prevents
/// uncertain AI output from becoming permanent truth."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeStatus {
    Unverified,
    Proposed,
    Accepted,
    Verified,
    Rejected,
    Superseded,
    Archived,
}

impl KnowledgeStatus {
    /// Only `Accepted` and `Verified` knowledge may be given to a model as
    /// fact. Everything else (uncertain, rejected, replaced or retired) must
    /// stay out of a prompt, "this prevents uncertain AI output from
    /// becoming permanent truth".
    pub fn is_authoritative(self) -> bool {
        matches!(self, KnowledgeStatus::Accepted | KnowledgeStatus::Verified)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeKind {
    Fact,
    Constraint,
    Artifact,
}

/// Rule: "Every authoritative fact should be traceable to: human input,
/// AI output, artifact, test, tool result, external source, decision,
/// imported data. Do not allow unexplained 'facts' to silently enter
/// the project brain." `source` is deliberately a free-form label
/// rather than a closed enum, new provenance kinds (a tool result, an
/// imported spec doc) shouldn't require a domain-crate release, only
/// `reference` needs a stable id when the source is itself a project
/// object (a task, a decision, another knowledge item).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provenance {
    pub source: String,
    pub reference: Option<Uuid>,
}

impl Provenance {
    pub fn human(who: impl Into<String>) -> Self {
        Self { source: format!("human:{}", who.into()), reference: None }
    }

    pub fn task_result(task_id: Uuid) -> Self {
        Self { source: "task_result".into(), reference: Some(task_id) }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeItem {
    pub id: Uuid,
    pub project_id: Uuid,
    pub kind: KnowledgeKind,
    pub title: String,
    pub content: String,
    pub status: KnowledgeStatus,
    pub provenance: Provenance,
    /// Free-form tags used by the context packager's keyword-matching
    /// heuristic (see solexa-brain) to decide relevance to a task, the
    /// Phase 4 stand-in for real semantic retrieval, same spirit as the
    /// Planner's capability-keyword heuristic in Phase 1/2.
    pub tags: Vec<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl KnowledgeItem {
    pub fn new(
        project_id: Uuid,
        kind: KnowledgeKind,
        title: impl Into<String>,
        content: impl Into<String>,
        provenance: Provenance,
        tags: Vec<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            project_id,
            kind,
            title: title.into(),
            content: content.into(),
            status: KnowledgeStatus::Accepted,
            provenance,
            tags,
            created_at: chrono::Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionStatus {
    Open,
    Resolved,
}

/// A structured decision record, spec section 10: "question, options,
/// constraints, evidence, participants, arguments, resolution,
/// approver, timestamp, status. Decisions can later be revisited."
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub id: Uuid,
    pub project_id: Uuid,
    pub question: String,
    pub options: Vec<String>,
    pub resolution: Option<String>,
    pub approved_by: Option<String>,
    pub status: DecisionStatus,
    /// Same purpose as `KnowledgeItem::tags`, lets the context
    /// packager (solexa-brain) scope a relevant decision into a task's
    /// context the same way it scopes facts/constraints/artifacts.
    pub tags: Vec<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Round 33: the GitHub Issue opened to mirror this decision for
    /// visibility, if `Coordinator::send_decision_to_github` has been
    /// called for it, `None` for every decision never sent to GitHub,
    /// which stays the default, unremarkable path.
    pub github_issue_url: Option<String>,
}

impl Decision {
    pub fn new(
        project_id: Uuid,
        question: impl Into<String>,
        options: Vec<String>,
        tags: Vec<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            project_id,
            question: question.into(),
            options,
            resolution: None,
            approved_by: None,
            status: DecisionStatus::Open,
            tags,
            created_at: chrono::Utc::now(),
            github_issue_url: None,
        }
    }

    pub fn resolve(&mut self, resolution: impl Into<String>, approved_by: impl Into<String>) {
        self.resolution = Some(resolution.into());
        self.approved_by = Some(approved_by.into());
        self.status = DecisionStatus::Resolved;
    }
}
