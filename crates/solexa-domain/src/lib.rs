//! solexa-domain, the core domain model shared by every other crate.
//!
//! This crate has no orchestration logic and no I/O. It exists purely to
//! define the shape of the objects the rest of the engine moves around:
//! `Task`, `Specialist`, `ModelMetadata`, `Member`, `Confidence`,
//! `ContextPackage`, `TaskResult`, `VerificationOutcome`.
//!
//! Keeping this dependency-free (beyond serde/uuid/chrono) means the
//! domain types can be shared by the coordinator, storage, models, and
//! verification crates, and eventually by the Tauri frontend via
//! generated bindings, without pulling in async runtimes or SQL drivers.

mod confidence;
mod conflict;
mod context;
mod benchmark;
mod knowledge;
mod member;
mod model;
mod package;
mod project;
mod registry;
mod result;
mod role;
mod snapshot;
mod specialist;
mod sync;
mod tag;
mod task;
mod team;
mod verification;

pub use benchmark::{BenchmarkOutcome, SpecialistBenchmark};
pub use confidence::{Confidence, ConfidenceLevel};
pub use conflict::{Conflict, ConflictPosition, ConflictStatus, SyncObjectKind, SyncObjectRef};
pub use context::{ContextPackage, DEFAULT_CONTEXT_CHARS, UNRESOLVED_SUFFIX};
pub use knowledge::{Decision, DecisionStatus, KnowledgeItem, KnowledgeKind, KnowledgeStatus, Provenance};
pub use member::{Member, MemberType};
pub use model::{Modality, ModelMetadata};
pub use package::{SpecialistPackage, TeamMemberPackage, TeamPackage, CURRENT_PACKAGE_FORMAT_VERSION};
pub use project::ProjectSummary;
pub use registry::{RegistryEntry, RegistryEntryKind};
pub use result::TaskResult;
pub use role::Role;
pub use snapshot::{ProjectSnapshot, CURRENT_SNAPSHOT_FORMAT_VERSION};
pub use specialist::{EscalationPolicy, Permission, Specialist};
pub use sync::{GitSyncConfig, PendingSyncChange, PendingSyncChangeStatus};
pub use tag::tags_match;
pub use task::{Task, TaskState, TaskStateError};
pub use team::Team;
pub use verification::{VerificationMethod, VerificationOutcome};

pub use uuid::Uuid;
