//! SpecialistPackage, a portable, self-contained export of a
//! specialist's configuration (spec section "Specialist Marketplace":
//! "Specialists can include: model, configuration, prompts, tools,
//! benchmark results, license, requirements, version, changelog...
//! Creators can distribute free or paid specialists").
//!
//! Deliberately the local-first slice of that: no license/pricing/
//! distribution infrastructure (that's real marketplace scope, not
//! attempted here), just the portable artifact itself, a specialist's
//! full configuration plus its measured track record, serializable to
//! a single JSON document a person can save, share, or hand to another
//! project. "Packaging" and "installation" (spec Phase 7) are exactly
//! export and import of this type; a registry/marketplace to discover
//! packages from is a different, larger piece of scope this does not
//! attempt.

use crate::{Specialist, SpecialistBenchmark};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecialistPackage {
    /// Format version of the package itself, not the specialist's own
    /// configuration, bumped only if this struct's shape changes in a
    /// way that breaks reading older packages. Lets `import` reject or
    /// migrate a package it doesn't understand instead of silently
    /// misreading one.
    pub package_format_version: u32,
    /// The specialist's full configuration. `id` and `active` inside
    /// this are not meaningful on import, `Coordinator::import_specialist_package`
    /// always assigns a fresh id and activates the imported copy
    /// regardless of what's stored here, the same way `edit_specialist`
    /// never lets a caller-supplied value override identity or
    /// lifecycle fields.
    pub specialist: Specialist,
    /// The specialist's measured track record at export time (spec:
    /// "benchmark results"). A specialist with real history carries
    /// proof of that history with it, rather than every import
    /// starting back at a neutral, unproven 0/0, the same reasoning
    /// `SpecialistBenchmark::success_rate`'s neutral-0.5-for-no-history
    /// default already applies to a specialist that's never run: an
    /// imported specialist should reflect what's actually known about
    /// it, not lose that information at the export/import boundary.
    pub benchmark: SpecialistBenchmark,
}

pub const CURRENT_PACKAGE_FORMAT_VERSION: u32 = 1;

/// One AI member's portable identity within a `TeamPackage`, its
/// display name and description, plus the full specialist it runs.
/// Deliberately excludes anything human: a `Member::Human` isn't a
/// portable configuration the way an AI member's capabilities are,
/// packaging "Alex" and creating a fresh copy of "Alex" in another
/// project doesn't mean anything the way installing a copy of a
/// typography specialist does. `TeamPackage::members` only ever
/// contains AI members for exactly this reason.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamMemberPackage {
    pub display_name: String,
    pub description: String,
    pub specialist: SpecialistPackage,
}

/// A portable export of a team's *AI* structure, its name, mission,
/// and AI members (each with its full specialist packaged the same
/// way `export_specialist_package` would). Deliberately excludes
/// human membership (see `TeamMemberPackage`) and hierarchy
/// (`parent_team_id`), a team's place in a *different* project's
/// hierarchy isn't something the exporting project could meaningfully
/// specify, the same reasoning `edit_team` already applies to why an
/// unrelated edit can't move a team in the hierarchy as a side effect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamPackage {
    pub package_format_version: u32,
    pub name: String,
    pub mission: String,
    pub members: Vec<TeamMemberPackage>,
}
