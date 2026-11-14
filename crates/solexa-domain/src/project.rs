//! ProjectSummary, a minimal read model for the `projects` table.
//!
//! Genuinely missing until round 22: `Storage::insert_project` has
//! existed since Phase 1, but nothing ever needed to read a project's
//! name/description back until `export_project_snapshot` needed them
//! for the snapshot header and commit messages. Deliberately minimal
//! rather than a full `Project` domain object with its own
//! coordinator CRUD surface, that's more than this round's actual
//! need, and would be exactly the "fake placeholder architecture" the
//! spec warns against building ahead of a real use.

use crate::Uuid;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSummary {
    pub id: Uuid,
    pub name: String,
    pub description: String,
}
