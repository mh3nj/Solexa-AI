//! Round 53: proves, against a REAL daemon, that cancelling a task whose
//! model call is in flight actually STOPS the daemon generating, not just
//! that the coordinator stops waiting for it.
//!
//! The method is black-box and needs no daemon internals. Run the daemon
//! single-slot (`llama-server -np 1`, Ollama's default
//! `OLLAMA_NUM_PARALLEL=1` for a lone model), so a second request can only
//! be answered once the first generation has ended. Then:
//!
//!   1. FULL   , run a request to completion; time it. (If it's too short
//!                to cancel mid-way, the test says so and fails rather than
//!                "passing" vacuously.)
//!   2. CONTROL, start the request, don't cancel it, and time a tiny probe
//!                request to the daemon. It should be BLOCKED behind the
//!                running generation. This proves the probe can tell a busy
//!                daemon from an idle one.
//!   3. CANCEL , start the request, cancel the task, and time the same
//!                probe. It must be answered promptly: the daemon stopped.
//!
//! Needs a LONG-running generation: build the synthetic model with a big
//! context (`SOLEXA_SYNTH_CTX=65536 python3 scripts/build_synthetic_test_model.py`)
//!, the default 2048-token model finishes in ~300 ms, and this test says
//! INCONCLUSIVE rather than passing on it.
//!
//! Usage (daemon already running with the synthetic model loaded):
//!   cargo run -p solexa-coordinator --example cancel_inflight_smoke_test -- llamacpp
//!   cargo run -p solexa-coordinator --example cancel_inflight_smoke_test -- ollama

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use solexa_coordinator::{Coordinator, UserRequest};
use solexa_domain::{Specialist, TaskState, Uuid};
use solexa_events::EventBus;
use solexa_models::llamacpp::LlamaCppProvider;
use solexa_models::ollama::OllamaProvider;
use solexa_models::{GenerationRequest, ModelProvider, ProviderRegistry};
use solexa_storage::Storage;

const TEXT: &str = "Refactor the Rust backend auth module.";

struct Rig {
    coordinator: Arc<Coordinator>,
    storage: Arc<Storage>,
    project_id: Uuid,
    provider: Arc<dyn ModelProvider>,
    model_id: Uuid,
}

fn build(provider: Arc<dyn ModelProvider>, temperature: f32, max_tokens: u32) -> anyhow::Result<Rig> {
    let storage = Arc::new(Storage::in_memory()?);
    let mut bus = EventBus::new();
    bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
    let events = Arc::new(bus);
    let project_id = Uuid::new_v4();
    storage.insert_project(project_id, "Cancel smoke test", "")?;

    let model = provider.list_models().into_iter().next().ok_or_else(|| anyhow::anyhow!("daemon reports no models"))?;
    storage.insert_model(&model)?;
    // The generation has to run for seconds, and be repeatable. The limits
    // are the specialist's own (round 52), which also exercises them end to
    // end: the temperature picks greedy (steady, runs to the cap) or
    // sampled (see main()), and `max_tokens` bounds a full run so the
    // FULL and CONTROL phases finish in reasonable time.
    let specialist: Specialist = serde_json::from_value(json!({
        "id": Uuid::new_v4(), "name": "Smoke Specialist", "role": "generalist", "domain": "general",
        "capabilities": ["general", "backend"], "preferred_model_ids": [model.id], "tools": [],
        "knowledge_scope": [], "permissions": ["read", "propose", "execute"], "output_schema": null,
        "escalation_policy": {"escalate_on_weak_confidence": false, "escalate_on_verification_failure": false,
                              "requires_model_review": false, "requires_specialist_review": false},
        "active": true, "role_ids": [], "max_tokens": max_tokens, "temperature": temperature
    }))?;
    storage.insert_specialist(project_id, &specialist)?;

    let mut registry = ProviderRegistry::new();
    registry.register(provider.clone());
    let coordinator = Arc::new(Coordinator::new(storage.clone(), events, registry));
    Ok(Rig { coordinator, storage, project_id, provider, model_id: model.id })
}

impl Rig {
    fn start(&self) -> tokio::task::JoinHandle<anyhow::Result<solexa_coordinator::Submission>> {
        let coordinator = self.coordinator.clone();
        let project_id = self.project_id;
        tokio::spawn(async move {
            coordinator.submit_request(UserRequest { project_id, text: TEXT.into(), requested_by: "smoke".into() }).await
        })
    }

    /// The most recently created task (each phase starts a fresh one).
    fn newest_task(&self) -> Option<solexa_domain::Task> {
        let mut tasks = self.storage.list_tasks(self.project_id).ok()?;
        tasks.sort_by_key(|t| t.created_at);
        tasks.pop()
    }

    async fn wait_until_running(&self) -> anyhow::Result<solexa_domain::Task> {
        for _ in 0..500 {
            if let Some(t) = self.newest_task() {
                if t.state == TaskState::Running {
                    // Give the daemon a moment to actually be generating.
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    return Ok(t);
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        anyhow::bail!("the task never reached Running")
    }

    /// A one-token request straight to the daemon, timed. Only answerable
    /// once the daemon's single slot is free.
    async fn probe(&self) -> anyhow::Result<Duration> {
        let t0 = Instant::now();
        self.provider
            .generate(GenerationRequest {
                model_id: self.model_id,
                system_prompt: String::new(),
                prompt: "hi".into(),
                max_tokens: Some(1),
                temperature: Some(0.0),
            })
            .await?;
        Ok(t0.elapsed())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let which = std::env::args().nth(1).unwrap_or_default();
    let provider: Arc<dyn ModelProvider> = match which.as_str() {
        "llamacpp" => {
            let p = LlamaCppProvider::local();
            p.refresh().await?;
            Arc::new(p)
        }
        "ollama" => {
            let p = OllamaProvider::local();
            p.refresh().await?;
            Arc::new(p)
        }
        other => anyhow::bail!("usage: cancel_inflight_smoke_test -- <ollama|llamacpp> (got {other:?})"),
    };
    // llama.cpp: greedy decoding runs steadily to the cap (~4 s for 8000
    // tokens on the synthetic model). Ollama aborts degenerate GREEDY loops
    // itself (HTTP 500 "token repeat limit reached"), which would end the
    // generation early and make the timing meaningless, so it is sampled.
    let (temperature, max_tokens) = if which == "ollama" { (1.0, 8000) } else { (0.0, 8000) };
    let rig = build(provider, temperature, max_tokens)?;
    let mut failures: Vec<String> = Vec::new();
    let mut check = |name: &str, ok: bool| {
        println!("{} {name}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            failures.push(name.to_string());
        }
    };

    // 1. FULL
    println!("-- 1. FULL: an uncancelled run");
    let t0 = Instant::now();
    let submission = rig.start().await??;
    let full = t0.elapsed();
    let full_state = rig.coordinator.get_task(submission.task_id)?.map(|t| t.state);
    println!("    took {:.2?}, final state {:?}", full, full_state);
    // Round 52's end-to-end path, live: the specialist's max_tokens cuts the
    // run off, the daemon reports `length`, and the task is sent back for
    // revision instead of being verified as complete.
    let full_task = rig.coordinator.get_task(submission.task_id)?;
    let reason = full_task.as_ref().and_then(|t| t.verification.as_ref()).map(|v| v.details.clone()).unwrap_or_default();
    println!("    verification: {reason}");
    check("FULL: a run cut off by the specialist's own limit goes to Revision (round 52, live)", full_state == Some(TaskState::Revision));
    check("FULL: ...and the reason names the token limit", reason.contains("token limit"));
    if full < Duration::from_millis(2500) {
        println!("\nINCONCLUSIVE: a full run takes only {full:.2?}, too short to cancel mid-way and then tell");
        println!("'daemon stopped' from 'daemon finished'. Give the daemon a larger context (llama-server -c 32768,");
        println!("or `PARAMETER num_ctx` in the Modelfile) so generation runs longer, then re-run.");
        anyhow::bail!("inconclusive: generation too short");
    }

    // 2. CONTROL
    println!("-- 2. CONTROL: probe while a request is generating and NOT cancelled");
    let running = rig.start();
    rig.wait_until_running().await?;
    let blocked = rig.probe().await?;
    println!("    probe took {blocked:.2?} (blocked behind the running generation)");
    check("CONTROL: the probe is blocked while the daemon is busy", blocked > Duration::from_millis(1000));
    running.await??;

    // 3. CANCEL
    println!("-- 3. CANCEL: cancel the task mid-generation, then probe");
    let running = rig.start();
    let task = rig.wait_until_running().await?;
    let t_cancel = Instant::now();
    rig.coordinator.cancel_task(task.id)?;
    let _ = tokio::time::timeout(Duration::from_secs(5), running).await.map_err(|_| anyhow::anyhow!("the request did not end after cancel"))???;
    let returned = t_cancel.elapsed();
    let state = rig.coordinator.get_task(task.id)?.map(|t| t.state);
    println!("    cancel -> request returned in {returned:.2?}, task state {state:?}");
    check("CANCEL: the task is Cancelled", state == Some(TaskState::Cancelled));
    check("CANCEL: the request ended promptly (well before a full run)", returned < Duration::from_millis(1000));
    let freed = rig.probe().await?;
    println!("    probe after cancel took {freed:.2?}");
    check("CANCEL: the daemon is idle again, the probe is answered promptly", freed < Duration::from_millis(1500));
    check("CANCEL: ...and much faster than the blocked CONTROL probe", freed * 3 < blocked);

    if failures.is_empty() {
        println!("\nALL CHECKS PASSED for {which}");
        Ok(())
    } else {
        anyhow::bail!("{} check(s) failed: {failures:?}", failures.len())
    }
}
