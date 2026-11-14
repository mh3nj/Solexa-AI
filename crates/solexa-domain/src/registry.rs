//! RegistryEntry, a saved package sitting in the local, personal
//! registry (spec's "Specialist Discovery"/"Specialist Marketplace"
//! sections: "a registry where users can discover specialists based
//! on: domain, benchmark, ... capabilities... Creators can distribute
//! free or paid specialists"). Deliberately the local-first slice of
//! that: no hosting, no accounts, no browsing other people's
//! creations, a personal library of packages *this* installation has
//! saved, independent of which project is currently open, that any
//! project can install from. A hosted, shared registry other people
//! could publish to and discover from is real marketplace/cloud scope
//! this does not attempt (see spec Phase 8, "Collaboration").
//!
//! Unlike a live `Specialist`/`Team`, a registry entry carries no task
//! history or routing stake of its own, it is a saved copy of a
//! package, nothing more. That is why `Coordinator::remove_from_registry`
//! is a genuine, permanent delete rather than following the archive-
//! only philosophy `Specialist`/`Member`/`Team` hold: there is nothing
//! here that "never silently discard" was ever protecting.

use crate::Uuid;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryEntryKind {
    Specialist,
    Team,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub id: Uuid,
    pub kind: RegistryEntryKind,
    /// The specialist's or team's name at the time it was saved,
    /// purely descriptive, not kept in sync with the live object (if
    /// any) it came from; the two are independent from the moment of
    /// saving.
    pub name: String,
    /// A specialist's own domain, or empty for a team package (a team
    /// doesn't have a single domain the way one specialist does).
    pub domain: String,
    /// Every capability across the saved package, a specialist's own
    /// list, or the union of every AI member's specialist's
    /// capabilities for a team, so the registry can be filtered by
    /// capability without deserializing every entry's full package.
    pub capabilities: Vec<String>,
    /// The full `SpecialistPackage` or `TeamPackage`, already
    /// serialized, installing an entry is exactly deserializing this
    /// and handing it to `import_specialist_package`/
    /// `import_team_package`, the same functions a pasted or
    /// file-loaded package already goes through.
    pub package_json: String,
    pub added_at: chrono::DateTime<chrono::Utc>,
}
