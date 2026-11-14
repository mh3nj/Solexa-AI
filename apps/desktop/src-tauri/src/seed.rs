//! First-run seeding: a single default project with one general-purpose
//! specialist backed by the mock model provider.
//!
//! IDs are fixed constants (not random) so a restart against the same
//! SQLite file lines back up correctly: the specialist's
//! `preferred_model_ids` stored on disk must still resolve to a model
//! id the freshly-built (in-memory, per-process) `ProviderRegistry`
//! actually registers on the next launch.

use anyhow::Result;
use solexa_domain::{
    EscalationPolicy, Modality, ModelMetadata, Permission, Specialist, Uuid,
};
use solexa_storage::Storage;

fn fixed_uuid(last_byte: u8) -> Uuid {
    let mut bytes = [0u8; 16];
    bytes[0] = 0x50; // 'S' for SOLEXA, arbitrary but stable
    bytes[15] = last_byte;
    Uuid::from_bytes(bytes)
}

pub fn default_project_id() -> Uuid {
    fixed_uuid(0x01)
}

fn default_model() -> ModelMetadata {
    ModelMetadata {
        id: fixed_uuid(0x02),
        name: "mock-general".into(),
        provider: "mock".into(),
        modalities: vec![Modality::Text],
        context_capacity_tokens: 8192,
        parameter_count_billions: None,
        quantization: None,
        estimated_vram_mb: 0,
        estimated_ram_mb: 256,
        tool_support: false,
        structured_output_support: false,
        streaming_support: false,
        local: true,
        license: None,
    }
}

fn general_specialist(model_id: Uuid) -> Specialist {
    Specialist {
        id: fixed_uuid(0x03),
        name: "General Specialist".into(),
        role: "generalist".into(),
        domain: "general project work".into(),
        capabilities: vec![
            "general".into(),
            "backend".into(),
            "design".into(),
            "security".into(),
            "audio".into(),
            "qa".into(),
        ],
        preferred_model_ids: vec![model_id],
        tools: vec![],
        knowledge_scope: vec![],
        permissions: vec![Permission::Read, Permission::Propose, Permission::Execute],
        output_schema: None,
        escalation_policy: EscalationPolicy::default(),
        active: true,
        role_ids: Vec::new(),
        max_tokens: None,
        temperature: None,
    }
}

/// Phase 2's second specialist: deliberately narrow (just "sql"), so a
/// request that implies both backend and database work gets decomposed
/// and its two subtasks genuinely land on two different specialists,
/// the same thing `solexa-coordinator`'s
/// `multi_capability_request_decomposes_and_routes_to_different_specialists`
/// test proves in isolation.
fn sql_specialist(model_id: Uuid) -> Specialist {
    Specialist {
        id: fixed_uuid(0x04),
        name: "SQL Specialist".into(),
        role: "database engineer".into(),
        domain: "sql and schema design".into(),
        capabilities: vec!["sql".into()],
        preferred_model_ids: vec![model_id],
        tools: vec![],
        knowledge_scope: vec![],
        permissions: vec![Permission::Read, Permission::Propose, Permission::Execute],
        output_schema: None,
        escalation_policy: EscalationPolicy::default(),
        active: true,
        role_ids: Vec::new(),
        max_tokens: None,
        temperature: None,
    }
}

/// Ensures the default project/model/specialists exist in storage.
/// Idempotent: a second run against the same database file is a no-op
/// beyond reading back what's already there. Returns the project id and
/// the model metadata the caller needs to register a matching
/// `MockLocalProvider` for this process.
pub fn ensure_seeded(storage: &Storage) -> Result<(Uuid, ModelMetadata)> {
    let project_id = default_project_id();
    let model = default_model();

    if storage.list_specialists(project_id)?.is_empty() {
        storage.insert_project(
            project_id,
            "SOLEXA Sandbox",
            "Default Phase 1-9 demo project, seeded on first launch.",
        )?;
        storage.insert_model(&model)?;
        let backend_specialist = general_specialist(model.id);
        let sql_specialist_obj = sql_specialist(model.id);
        storage.insert_specialist(project_id, &backend_specialist)?;
        storage.insert_specialist(project_id, &sql_specialist_obj)?;

        // A seeded Project Brain entry so a fresh install already
        // demonstrates Phase 4's context scoping: submit any
        // backend-tagged request and watch its ContextPackage actually
        // include this constraint (see solexa-brain).
        storage.insert_knowledge_item(&solexa_domain::KnowledgeItem::new(
            project_id,
            solexa_domain::KnowledgeKind::Constraint,
            "Database choice",
            "Must use PostgreSQL, not SQLite, for any production data path.",
            solexa_domain::Provenance::human("seed"),
            vec!["backend".into(), "sql".into()],
        ))?;

        // Phase 6: a human member and one AI member per specialist,
        // grouped into a single top-level team. Each AI member's
        // `specialist_ids` actually points at the Specialist it runs,
        // without that link the Team rail has no way to show which
        // capabilities/benchmark belong to which member.
        let human = solexa_domain::Member::new_human(project_id, "You");
        let mut backend_member =
            solexa_domain::Member::new_ai(project_id, "Backend Bot", "runs the General Specialist");
        backend_member.specialist_ids = vec![backend_specialist.id];
        let mut sql_member = solexa_domain::Member::new_ai(project_id, "SQL Bot", "runs the SQL Specialist");
        sql_member.specialist_ids = vec![sql_specialist_obj.id];
        storage.insert_member(&human)?;
        storage.insert_member(&backend_member)?;
        storage.insert_member(&sql_member)?;
        storage.insert_team(
            &solexa_domain::Team::new(project_id, "SOLEXA Sandbox Team", "handle whatever comes in")
                .with_members(vec![human.id, backend_member.id, sql_member.id]),
        )?;
    }

    Ok((project_id, model))
}
