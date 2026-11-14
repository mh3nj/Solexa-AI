//! Round 58: proves, against a REAL llama-server, that the Project Brain's
//! context actually travels to the model, and that what must not travel
//! doesn't.
//!
//! A recording provider (what the unit tests use) can only prove what the
//! coordinator hands to *a* provider. This goes one step further: it runs the
//! real `LlamaCppProvider` and inspects the HTTP request bodies the daemon
//! receives, via `scripts/request_logging_proxy.py` (listens on 8090, logs
//! every POST body to /tmp/proxy_requests.jsonl, forwards to 127.0.0.1:8080).
//!
//! Three runs, each in a fresh project holding the same six knowledge items
//! (round 59 added a third run: a `read` specialist scoped to `auth`, which
//! must see the auth-tagged item and NOT the billing-tagged one):
//!   A. a specialist WITH `read`    -> its system prompt must carry the
//!      Accepted constraint and the Verified fact, and NOT the Rejected
//!      constraint or the Unverified fact.
//!   B. a specialist WITHOUT `read` -> its system prompt must carry none.
//! Each run is tagged with a marker in the task text so the two can be told
//! apart in the log. The check itself is done by the caller reading the log
//! (see the resume guide); this program only drives the runs.
//!
//! Usage (daemon on 8080, proxy on 8090):
//!   cargo run -p solexa-coordinator --example project_context_smoke_test

use std::sync::Arc;

use serde_json::json;
use solexa_coordinator::{Coordinator, UserRequest};
use solexa_domain::{KnowledgeItem, KnowledgeKind, KnowledgeStatus, Provenance, Specialist, Uuid};
use solexa_events::EventBus;
use solexa_models::llamacpp::LlamaCppProvider;
use solexa_models::{ModelProvider, ProviderRegistry};
use solexa_storage::Storage;

async fn run(marker: &str, permissions: serde_json::Value, scope: Vec<&str>) -> anyhow::Result<()> {
    let provider = LlamaCppProvider::new("127.0.0.1", 8090);
    provider.refresh().await?;
    let provider: Arc<dyn ModelProvider> = Arc::new(provider);

    let storage = Arc::new(Storage::in_memory()?);
    let mut bus = EventBus::new();
    bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
    let project_id = Uuid::new_v4();
    storage.insert_project(project_id, "Context smoke test", "")?;
    let model = provider.list_models().into_iter().next().ok_or_else(|| anyhow::anyhow!("daemon reports no models"))?;
    storage.insert_model(&model)?;

    for (kind, content, status, extra_tag) in [
        (KnowledgeKind::Constraint, "ACCEPTED-CONSTRAINT-7731", KnowledgeStatus::Accepted, None),
        (KnowledgeKind::Fact, "VERIFIED-FACT-2208", KnowledgeStatus::Verified, None),
        (KnowledgeKind::Constraint, "REJECTED-CONSTRAINT-4402", KnowledgeStatus::Rejected, None),
        (KnowledgeKind::Fact, "UNVERIFIED-FACT-9915", KnowledgeStatus::Unverified, None),
        // round 59: scope targets
        (KnowledgeKind::Constraint, "AUTH-ACCEPTED-3307", KnowledgeStatus::Accepted, Some("auth")),
        (KnowledgeKind::Constraint, "BILLING-ACCEPTED-6120", KnowledgeStatus::Accepted, Some("billing")),
    ] {
        let mut tags: Vec<String> = vec!["backend".into()];
        tags.extend(extra_tag.map(String::from));
        let mut item = KnowledgeItem::new(project_id, kind, content, content, Provenance::human("smoke"), tags);
        item.status = status;
        storage.insert_knowledge_item(&item)?;
    }

    let specialist: Specialist = serde_json::from_value(json!({
        "id": Uuid::new_v4(), "name": "Smoke Specialist", "role": "generalist", "domain": "general",
        "capabilities": ["general", "backend"], "preferred_model_ids": [model.id], "tools": [],
        "knowledge_scope": scope, "permissions": permissions, "output_schema": null,
        "escalation_policy": {"escalate_on_weak_confidence": false, "escalate_on_verification_failure": false,
                              "requires_model_review": false, "requires_specialist_review": false},
        "active": true, "role_ids": [], "max_tokens": 8, "temperature": 0.0
    }))?;
    storage.insert_specialist(project_id, &specialist)?;

    let mut registry = ProviderRegistry::new();
    registry.register(provider);
    let coordinator = Coordinator::new(storage.clone(), Arc::new(bus), registry);
    let submission = coordinator
        .submit_request(UserRequest { project_id, text: format!("Refactor the Rust backend auth module. [{marker}]"), requested_by: "smoke".into() })
        .await?;
    let trace = coordinator.events_for_correlation(submission.correlation_id)?;
    let invoked = trace.iter().find(|e| format!("{:?}", e.event_type) == "ModelInvoked").map(|e| e.payload.clone());
    println!("{marker}: ModelInvoked payload = {}", invoked.unwrap_or_default());
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    run("RUN-WITH-READ", json!(["read", "execute"]), vec![]).await?;
    run("RUN-WITHOUT-READ", json!(["execute"]), vec![]).await?;
    run("RUN-SCOPED-AUTH", json!(["read", "execute"]), vec!["auth"]).await?;
    Ok(())
}
