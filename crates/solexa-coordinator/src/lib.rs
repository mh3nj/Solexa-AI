//! solexa-coordinator, the core orchestration loop.
//!
//! This crate is where every other crate meets: it takes a raw human
//! request and drives it through
//!
//!   PLANNER -> TASK DECOMPOSITION -> CAPABILITY MATCHING ->
//!   SPECIALIST ASSIGNMENT -> CONTEXT PACKAGE -> MODEL EXECUTION ->
//!   RESULT -> VERIFICATION -> ACCEPT / REVISE / ESCALATE
//!
//! exactly as laid out in the master implementation directive. Every
//! step transitions the `Task` through its explicit state machine and
//! publishes an `Event`, so the whole run is replayable afterward from
//! `events_for_correlation` alone.
//!
//! A request needing one capability runs as one task (Phase 1). A
//! request needing several is decomposed by the `Planner` into a parent
//! task plus one subtask per capability, each independently routed to
//! whichever registered specialist(s) match it. When exactly one
//! specialist matches a task, its result is used directly (Phase 2).
//! When more than one matches, all of them are consulted concurrently
//! and their results compared (Phase 5): if they agree, the task
//! proceeds normally; if they genuinely disagree, a `Conflict` is
//! created preserving every position, and the task waits in `Review`
//! for `Coordinator::resolve_conflict`, never silently picking one
//! specialist's answer over another's.
//!
//! A specialist can additionally opt into `requires_model_review`: even
//! after deterministic verification passes, its result gets sent to an
//! independent reviewing model before being accepted. A rejection there
//! sends the task to `Revision` exactly like a failed deterministic
//! check would, this is an *extra* gate specialists opt into, not a
//! replacement for the deterministic checks (spec: "prefer deterministic
//! verification... never use another LLM as the only possible
//! verification mechanism").

mod planner;

pub use planner::Planner;

use anyhow::{anyhow, Context, Result};
use futures::future::join_all;
use serde_json::json;
use solexa_domain::{
    Confidence, ConfidenceLevel, Conflict, ConflictPosition, Decision, GitSyncConfig,
    KnowledgeItem, Member, ModelMetadata, PendingSyncChange, PendingSyncChangeStatus, Permission,
    ProjectSnapshot, RegistryEntry,
    RegistryEntryKind, Role, Specialist, SpecialistBenchmark, SpecialistPackage, SyncObjectKind,
    SyncObjectRef, Task, TaskResult, TaskState, Team, Uuid, CURRENT_PACKAGE_FORMAT_VERSION,
};
use solexa_events::{ActorType, Event, EventBus, EventType};
use solexa_github::{CreateIssueParams, CreatePrParams, PrState};
use solexa_models::{FinishReason, GenerationRequest, GenerationResult, ModelProvider, ProviderRegistry};
use solexa_resources::{ResourceBudget, ResourceManager};
use solexa_storage::Storage;
use solexa_verification::CompositeVerifier;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct UserRequest {
    pub project_id: Uuid,
    pub text: String,
    /// Actor id of whoever is asking, a human member's id in later
    /// phases; a free-form string for Phase 1 ("human" is fine).
    pub requested_by: String,
}

/// What `Coordinator::submit_request` hands back: enough to look the
/// task up and pull its full execution trace.
#[derive(Debug, Clone, Copy)]
pub struct Submission {
    pub task_id: Uuid,
    pub correlation_id: Uuid,
}

/// Round 49: returned (wrapped in `anyhow`) by `run_specialist` when
/// `cancel_task` ended its hardware wait; `execute_task` recognizes it
/// and finishes the task cleanly as `Cancelled` instead of failing the
/// whole request.
#[derive(Debug)]
struct TaskCancelled(Uuid);

impl std::fmt::Display for TaskCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "task {} was cancelled", self.0)
    }
}

impl std::error::Error for TaskCancelled {}

/// Round 53: registers a per-task cancel signal for the lifetime of one
/// `execute_task` call and removes it on drop, so an early `?` return can
/// never leave a stale signal behind. The signal is only a *wake-up* for
/// awaits that need interrupting (the in-flight model call); the source of
/// truth for "is this task cancelled" is always its stored state, see
/// `Coordinator::persist_task`.
struct SignalRegistration<'a> {
    signals: &'a std::sync::Mutex<HashMap<Uuid, tokio::sync::watch::Sender<bool>>>,
    resources: &'a ResourceManager,
    task_id: Uuid,
}

impl<'a> SignalRegistration<'a> {
    fn new(
        signals: &'a std::sync::Mutex<HashMap<Uuid, tokio::sync::watch::Sender<bool>>>,
        resources: &'a ResourceManager,
        task_id: Uuid,
    ) -> Self {
        signals.lock().unwrap().insert(task_id, tokio::sync::watch::channel(false).0);
        Self { signals, resources, task_id }
    }
}

impl Drop for SignalRegistration<'_> {
    fn drop(&mut self) {
        self.signals.lock().unwrap().remove(&self.task_id);
        // Round 54: the task's pipeline has ended, so a cancel marker for
        // it that nothing consumed (it was past the hardware wait, or
        // finished) would otherwise live forever.
        self.resources.forget_cancel(self.task_id);
    }
}

/// Round 53: `Ok(true)` when a guarded parent-task write was refused
/// because the parent was cancelled (the caller then returns normally,
/// a cancelled request is not an error), `Ok(false)` when it was written,
/// `Err` for a real failure.
fn cancelled_while_planning(write: Result<()>) -> Result<bool> {
    match write {
        Ok(()) => Ok(false),
        Err(err) if err.downcast_ref::<TaskCancelled>().is_some() => Ok(true),
        Err(err) => Err(err),
    }
}

/// Round 53: aborts a spawned task when dropped. `stream_generation` runs
/// the provider call inside `tokio::spawn`, and dropping a `JoinHandle`
/// only *detaches* the task, without this, abandoning the wait would
/// leave the model call running and its socket open, so a "cancelled" task
/// would keep burning the GPU. Aborting drops the provider future, which
/// drops its `TcpStream`, which is what tells the daemon to stop.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Resolves when a task's cancel signal fires. If the signal's sender is
/// gone (the task finished and unregistered) it never resolves, rather than
/// reporting a cancellation that didn't happen.
async fn wait_for_cancel(mut rx: tokio::sync::watch::Receiver<bool>) {
    // `wait_for` returns a `Ref` that holds a (non-`Send`) lock guard, so
    // reduce it to a bool before any further `.await`.
    let fired = rx.wait_for(|cancelled| *cancelled).await.is_ok();
    if !fired {
        std::future::pending::<()>().await;
    }
}

pub struct Coordinator {
    storage: Arc<Storage>,
    events: Arc<EventBus>,
    providers: ProviderRegistry,
    verifier: CompositeVerifier,
    planner: Planner,
    resources: ResourceManager,
    github: Arc<dyn solexa_github::GithubClient>,
    /// Round 53: serializes `cancel_task`'s persist against every
    /// pipeline write to a task (`persist_task`). Held only across a few
    /// synchronous storage calls, never across an `.await`.
    cancel_gate: std::sync::Mutex<()>,
    /// Round 65: serializes every whole-row read-modify-write of a member,
    /// team, specialist, role, decision, conflict, knowledge item or pending
    /// sync change (see `entity_gate`). Separate from `cancel_gate`, task rows
    /// have their own, finer protocol. Lock order, where both are ever held:
    /// `entity_gate` first, then `cancel_gate`; no path takes them the other
    /// way round.
    entity_gate: std::sync::Mutex<()>,
    /// Round 53: one wake-up channel per task currently inside
    /// `execute_task`. `cancel_task` fires it to interrupt the in-flight
    /// model call; see `SignalRegistration`.
    signals: std::sync::Mutex<HashMap<Uuid, tokio::sync::watch::Sender<bool>>>,
}

thread_local! {
    /// How many `EntityGuard`s the current thread holds (re-entrancy depth).
    static ENTITY_GATE_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Held for the duration of one UI-initiated whole-row mutation; see
/// `Coordinator::entity_gate`.
struct EntityGuard<'a> {
    _lock: Option<std::sync::MutexGuard<'a, ()>>,
}

impl Drop for EntityGuard<'_> {
    fn drop(&mut self) {
        ENTITY_GATE_DEPTH.with(|d| d.set(d.get() - 1));
        // `_lock` (if this was the outermost guard) is released after this body.
    }
}

/// Storage key under which the resized resource budget is remembered.
const SETTING_RESOURCE_BUDGET: &str = "resource_budget";

fn budget_to_json(b: ResourceBudget) -> serde_json::Value {
    json!({ "vram_mb": b.vram_mb, "ram_mb": b.ram_mb, "max_concurrent": b.max_concurrent })
}

/// Round 61: the budget saved by a previous session's resize, or `None` if
/// there is none or it can't be trusted. Startup must never fail or misbehave
/// because of what is in a settings row (hand-edited, truncated, from a future
/// version), so *anything* wrong, unreadable, not JSON, a missing or
/// mistyped or out-of-range field, a budget `validate` rejects, means "use the
/// default", never an error and never a half-applied value.
fn load_saved_budget(storage: &Storage) -> Option<ResourceBudget> {
    let raw = storage.get_setting(SETTING_RESOURCE_BUDGET).ok().flatten()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let budget = ResourceBudget {
        vram_mb: u32::try_from(value.get("vram_mb")?.as_u64()?).ok()?,
        ram_mb: u32::try_from(value.get("ram_mb")?.as_u64()?).ok()?,
        max_concurrent: usize::try_from(value.get("max_concurrent")?.as_u64()?).ok()?,
    };
    budget.validate().ok()?;
    Some(budget)
}

impl Coordinator {
    /// Round 65: the lock that makes the coordinator's member / team /
    /// specialist / role / decision / conflict / knowledge / pending-change
    /// mutators safe to run concurrently. Each of them reads a whole row,
    /// changes one field and writes the whole row back; without this, two UI
    /// actions on the same row (or a sync apply racing a UI edit) can each read
    /// the same starting copy and the second write silently reverts the first.
    ///
    /// **Re-entrant per thread** these mutators call one another (the sync
    /// upserts are reached from several entry points; `import_team_package`
    /// calls `add_member_to_team`), so a nested acquire on the same thread is
    /// a no-op rather than a deadlock. Never poisoned: a panicking holder is
    /// released on unwind and later callers carry on. Must never be held across
    /// an `.await` (everything it guards is synchronous). The lint test
    /// `every_function_that_rewrites_a_whole_row_takes_the_entity_gate` keeps
    /// new mutators honest.
    ///
    /// The depth counter is per *thread*, not per coordinator: holding this
    /// gate on one `Coordinator` and calling a mutator on a *different* one
    /// from the same thread would skip the second lock. Nothing does that.
    fn entity_gate(&self) -> EntityGuard<'_> {
        let depth = ENTITY_GATE_DEPTH.with(|d| d.get());
        let lock = (depth == 0).then(|| self.entity_gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
        ENTITY_GATE_DEPTH.with(|d| d.set(depth + 1));
        EntityGuard { _lock: lock }
    }

    /// The default constructor: the resource budget is whatever the last
    /// session saved via `set_resource_budget` (round 61), or
    /// `ResourceBudget::default()` if nothing valid was saved. A caller that
    /// wants a specific budget uses `with_resource_budget`, which is never
    /// overridden by a saved one.
    pub fn new(storage: Arc<Storage>, events: Arc<EventBus>, providers: ProviderRegistry) -> Self {
        let budget = load_saved_budget(&storage).unwrap_or_default();
        Self::with_resource_budget(storage, events, providers, budget)
    }

    /// Same as `new`, but with an explicit hardware budget instead of
    /// the generous default, mainly for tests and for a real desktop
    /// app that wants to report its actual detected VRAM/RAM instead of
    /// a made-up number.
    pub fn with_resource_budget(
        storage: Arc<Storage>,
        events: Arc<EventBus>,
        providers: ProviderRegistry,
        budget: ResourceBudget,
    ) -> Self {
        Self {
            storage,
            events,
            providers,
            verifier: CompositeVerifier::deterministic_default(),
            planner: Planner::default(),
            resources: ResourceManager::new(budget),
            github: Arc::new(solexa_github::GhCliClient),
            cancel_gate: std::sync::Mutex::new(()),
            entity_gate: std::sync::Mutex::new(()),
            signals: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Round 30: swaps in a different `GithubClient`, exclusively for
    /// tests. Real callers (the Tauri app) always get `GhCliClient` from
    /// `new`/`with_resource_budget`, which shells out to the user's own
    /// authenticated `gh` CLI; a test substitutes
    /// `solexa_github::fake::FakeGithubClient` instead, so
    /// `send_conflict_to_github`/`check_conflict_github_pr_status` can
    /// be exercised deterministically and offline, the same way
    /// `solexa-verification`'s deterministic checks avoid needing a
    /// live model for most of this crate's other tests.
    #[cfg(test)]
    fn with_github_client(mut self, client: Arc<dyn solexa_github::GithubClient>) -> Self {
        self.github = client;
        self
    }

    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    pub fn events_for_correlation(&self, correlation_id: Uuid) -> Result<Vec<Event>> {
        self.storage.list_events_for_correlation(correlation_id)
    }

    pub fn get_task(&self, task_id: Uuid) -> Result<Option<Task>> {
        self.storage.get_task(task_id)
    }

    pub fn list_tasks(&self, project_id: Uuid) -> Result<Vec<Task>> {
        self.storage.list_tasks(project_id)
    }

    pub fn resource_snapshot(&self) -> solexa_resources::ResourceSnapshot {
        self.resources.snapshot()
    }

    /// Adopted from a parallel round: the configured ceiling
    /// `resource_snapshot()`'s numbers are measured against, a "used
    /// 6000MB VRAM" reading means nothing on its own to a Resource
    /// Center view without also knowing the budget it's being weighed
    /// against.
    /// Round 64: the member accountable for work done by `specialist_id`, a
    /// live (not soft-deleted) member whose `specialist_ids` includes it, or
    /// `None` when no member runs that specialist (the task is then simply
    /// unowned; a wrong-typed id is never recorded instead). If several
    /// members run the same specialist the smallest member id wins:
    /// `list_members` has no defined order, and ownership must not depend on
    /// row order.
    fn owner_member_for(&self, project_id: Uuid, specialist_id: Uuid) -> Result<Option<Uuid>> {
        Ok(self
            .storage
            .list_members(project_id)?
            .into_iter()
            .filter(|m| !m.is_deleted() && m.specialist_ids.contains(&specialist_id))
            .map(|m| m.id)
            .min())
    }

    pub fn resource_budget(&self) -> solexa_resources::ResourceBudget {
        self.resources.budget()
    }

    /// Round 60: resizes the engine's resource budget while it is running
    /// (the Resource Center's controls), returning the budget it replaced.
    /// Running work is never evicted; a shrink that would strand an
    /// already-queued task is refused (see `ResourceManager::set_budget`).
    /// Each accepted change is recorded as a `ResourceBudgetChanged` event.
    ///
    /// The budget is **process-wide** (there is one `ResourceManager`) while
    /// events belong to a project, so `project_id` only says where the audit
    /// event is filed. Round 61: an accepted change is **saved** and
    /// `Coordinator::new` restores it next start. If saving fails the change
    /// still applies for this session (the audit event says `persisted:
    /// false`) and an error says so.
    pub fn set_resource_budget(
        &self,
        project_id: Uuid,
        budget: solexa_resources::ResourceBudget,
    ) -> Result<solexa_resources::ResourceBudget> {
        self.apply_resource_budget(project_id, budget, false)
    }

    /// Round 62: puts the resource budget back to `ResourceBudget::default()`
    /// and **forgets the saved override**, so a future change to the default
    /// takes effect (typing the default numbers in by hand would instead save
    /// them as an explicit choice). Goes through the same safe path as a
    /// resize: if the default is too small for a task already queued, it is
    /// refused and nothing, neither the live budget nor the saved value,
    /// changes. Returns the budget it replaced.
    pub fn reset_resource_budget(&self, project_id: Uuid) -> Result<solexa_resources::ResourceBudget> {
        self.apply_resource_budget(project_id, ResourceBudget::default(), true)
    }

    fn apply_resource_budget(
        &self,
        project_id: Uuid,
        budget: solexa_resources::ResourceBudget,
        reset: bool,
    ) -> Result<solexa_resources::ResourceBudget> {
        let previous = self.resources.set_budget(budget).map_err(|e| anyhow!(e.to_string()))?;
        let saved = if reset {
            self.storage.delete_setting(SETTING_RESOURCE_BUDGET)
        } else {
            self.storage.set_setting(SETTING_RESOURCE_BUDGET, &budget_to_json(budget).to_string())
        };
        self.emit(
            project_id,
            "human",
            ActorType::Human,
            EventType::ResourceBudgetChanged,
            json!({ "previous": budget_to_json(previous), "current": budget_to_json(budget), "persisted": saved.is_ok(), "reset": reset }),
            Uuid::new_v4(),
            None,
        )?;
        saved.map_err(|e| {
            if reset {
                anyhow!("the budget was reset for this session but the saved override could not be cleared, so the old budget will come back on restart: {e}")
            } else {
                anyhow!("the budget was changed for this session but could not be saved, so it will revert on restart: {e}")
            }
        })?;
        Ok(previous)
    }

    /// Round 60: `set_resource_budget` taking plain numbers, for callers (the
    /// desktop shell) that read the budget through `resource_budget()` and
    /// never name `ResourceBudget` themselves.
    pub fn set_resource_limits(&self, project_id: Uuid, vram_mb: u32, ram_mb: u32, max_concurrent: usize) -> Result<()> {
        self.set_resource_budget(project_id, solexa_resources::ResourceBudget { vram_mb, ram_mb, max_concurrent }).map(|_| ())
    }

    /// Adds a fact/constraint/artifact to the project's Project Brain
    /// (Phase 4). Not part of any task's execution flow, so it gets its
    /// own correlation id, a project's knowledge growing over time is
    /// its own kind of event, not something caused by a task.
    pub fn record_knowledge(&self, item: KnowledgeItem) -> Result<()> {
        self.storage.insert_knowledge_item(&item)?;
        self.emit(
            item.project_id,
            "human",
            ActorType::Human,
            EventType::KnowledgeItemAdded,
            json!({
                "knowledge_item_id": item.id,
                "kind": item.kind,
                "title": item.title,
                "tags": item.tags,
            }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    /// Same idea for a resolved (or open) decision record.
    pub fn record_decision(&self, decision: Decision) -> Result<()> {
        self.storage.insert_decision(&decision)?;
        self.emit(
            decision.project_id,
            "human",
            ActorType::Human,
            EventType::DecisionRecorded,
            json!({
                "decision_id": decision.id,
                "question": decision.question,
                "resolution": decision.resolution,
                "status": decision.status,
                "tags": decision.tags,
            }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    /// Resolves an open `Decision`, same accountability rule as
    /// `resolve_conflict`: the approver must be a `Member` already
    /// registered in the project, not a free-form string. Unlike a
    /// conflict, resolving a decision never touches a `Task`, a
    /// decision can exist (and be revisited) independent of any one
    /// task's execution.
    pub fn resolve_decision(
        &self,
        decision_id: Uuid,
        resolution: impl Into<String>,
        approving_member_id: Uuid,
    ) -> Result<()> {
        let _entity_gate = self.entity_gate();
        let approver = self
            .storage
            .get_member(approving_member_id)?
            .ok_or_else(|| anyhow!("member {approving_member_id} is not registered in this project, only a registered Member can resolve a decision"))?;
        let mut decision = self
            .storage
            .get_decision(decision_id)?
            .ok_or_else(|| anyhow!("decision {decision_id} not found"))?;
        self.require_approver_permission(decision.project_id, approving_member_id, "resolve a decision")?;
        let resolution = resolution.into();
        if !decision.options.iter().any(|opt| opt == &resolution) {
            return Err(anyhow!(
                "\"{resolution}\" is not one of this decision's declared options ({options}), a resolution must be one of the options the decision was recorded with",
                options = decision.options.join(", "),
            ));
        }
        decision.resolve(resolution, approver.display_name.clone());
        self.storage.update_decision(&decision)?;
        self.emit(
            decision.project_id,
            &approver.display_name,
            ActorType::Human,
            EventType::DecisionResolved,
            json!({
                "decision_id": decision.id,
                "resolution": decision.resolution,
                "approved_by_member_id": approver.id,
            }),
            Uuid::new_v4(),
            None,
        )?;

        // Round 33: if this decision was mirrored to GitHub as an
        // Issue, reflect the resolution back onto it, a closing
        // comment naming what was decided, then closing the issue.
        // Deliberately best-effort: SOLEXA is already the sole source
        // of truth for what the decision *is* (recorded above,
        // unconditionally); a failure to reach GitHub here must never
        // undo or block a resolution that has already happened.
        if let Some(issue_url) = decision.github_issue_url.as_deref() {
            if let Ok(Some(config)) = self.storage.get_git_sync_config(decision.project_id) {
                let repo_path = std::path::Path::new(&config.repo_path);
                let comment = format!(
                    "Resolved in SOLEXA: **{}** (approved by {}).",
                    decision.resolution.as_deref().unwrap_or(""),
                    approver.display_name,
                );
                let _ = self.github.close_issue_with_comment(repo_path, issue_url, &comment);
            }
        }

        Ok(())
    }

    /// Round 33: mirrors a recorded `Decision` as a plain GitHub Issue
    /// visibility for a team that already lives in GitHub
    /// notifications, not a second place to resolve it. Unlike round
    /// 30's PR-mirrored Conflicts (a clean binary merge/close choice),
    /// a `Decision` can have any number of options, so an issue's
    /// open/closed state can't cleanly represent which one was picked
    /// SOLEXA stays the sole source of truth for the resolution
    /// itself (see `resolve_decision`, which reflects the outcome back
    /// onto this issue as a closing comment once one is actually
    /// made). Requires the project to have a linked git repo and the
    /// `gh` CLI installed and already authenticated, the same
    /// precondition round 30 established for Conflicts. Returns the
    /// new issue's URL.
    pub fn send_decision_to_github(&self, decision_id: Uuid) -> Result<String> {
        let decision = self
            .storage
            .get_decision(decision_id)?
            .ok_or_else(|| anyhow!("decision {decision_id} not found"))?;
        if decision.github_issue_url.is_some() {
            return Err(anyhow!(
                "decision {decision_id} already has a GitHub issue ({}), no need to open another",
                decision.github_issue_url.unwrap()
            ));
        }
        if !self.github.is_available() {
            return Err(anyhow!("the `gh` CLI isn't installed or isn't authenticated, install it and run `gh auth login` first, then try again"));
        }
        let config = self
            .storage
            .get_git_sync_config(decision.project_id)?
            .ok_or_else(|| anyhow!("project {} has no linked git repo, link one with link_git_repo first", decision.project_id))?;
        let repo_path = std::path::Path::new(&config.repo_path);

        let options_list = decision.options.iter().map(|o| format!("- {o}")).collect::<Vec<_>>().join("\n");
        let body = format!(
            "SOLEXA has an open decision that needs input.\n\n**Options:**\n{options_list}\n\n\
             This issue is for visibility only, resolve the decision in SOLEXA itself, not here. \
             It'll be closed automatically, with a comment naming the outcome, once that happens.",
        );
        let url = self.github.create_issue(repo_path, CreateIssueParams { title: &decision.question, body: &body })?;
        self.storage.set_decision_github_issue_url(decision.id, &url)?;
        Ok(url)
    }

    pub fn list_knowledge(&self, project_id: Uuid) -> Result<Vec<KnowledgeItem>> {
        self.storage.list_knowledge_items(project_id)
    }

    /// Retires a `KnowledgeItem` by changing its status to `Archived`
    /// never deletes the row. What the UI presents as "remove" is
    /// this: the item drops out of the active/default view, but its
    /// content and provenance stay queryable, consistent with the
    /// spec's rule that project knowledge must never silently vanish.
    pub fn archive_knowledge_item(&self, item_id: Uuid) -> Result<()> {
        let _entity_gate = self.entity_gate();
        let item = self
            .storage
            .get_knowledge_item(item_id)?
            .ok_or_else(|| anyhow!("knowledge item {item_id} not found"))?;
        self.storage.update_knowledge_item_status(item_id, solexa_domain::KnowledgeStatus::Archived)?;
        self.emit(
            item.project_id,
            "human",
            ActorType::Human,
            EventType::KnowledgeItemArchived,
            json!({ "knowledge_item_id": item_id, "title": item.title }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    pub fn list_decisions(&self, project_id: Uuid) -> Result<Vec<Decision>> {
        self.storage.list_decisions(project_id)
    }

    /// Changes a task's priority after it's already been created.
    ///
    /// Round 36: this finally has teeth. `Task.priority` is now read by
    /// `ResourceManager::acquire` (see `execute_task`), so raising a
    /// task's priority genuinely moves it ahead of lower-priority work
    /// queued on the same hardware. For many rounds this was an honest
    /// no-op, a field write plus an event, with nothing in the engine
    /// reading the value, and the README said so plainly rather than
    /// implying it scheduled anything.
    ///
    /// Note the scope: priority orders tasks *waiting on resources*. A
    /// task already running is never preempted, and a task whose model
    /// fits immediately never queues at all.
    pub fn set_task_priority(&self, task_id: Uuid, priority: u8) -> Result<()> {
        if !(1..=10).contains(&priority) {
            return Err(anyhow!("priority must be between 1 and 10, got {priority}"));
        }
        let task = self
            .storage
            .get_task(task_id)?
            .ok_or_else(|| anyhow!("task {task_id} not found"))?;
        let previous = task.priority;
        // Round 57: a single-column write, NOT `update_task`, see
        // `Storage::set_task_priority`. `task` above is only read for the
        // event's `previous_priority` and project id.
        if !self.storage.set_task_priority(task_id, priority)? {
            return Err(anyhow!("task {task_id} not found"));
        }
        self.emit(
            task.project_id,
            "human",
            ActorType::Human,
            EventType::TaskPriorityChanged,
            json!({ "task_id": task.id, "previous_priority": previous, "new_priority": priority }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    /// Round 49: cancels a task, the `TaskState::Cancelled` variant
    /// (and the state machine's explicit "work can be cancelled from
    /// any non-terminal state" rule) existed from Phase 1 but nothing
    /// anywhere ever produced it, the same declared-but-never-referenced
    /// shape `Permission::Approve` had before round 46.
    ///
    /// What happens depends on where the task is, and since round 53 none
    /// of the cases is a refusal:
    /// - **Waiting on hardware capacity** (what the Resource Center's
    ///   `queued` count shows): the wait ends, the task's place in the
    ///   queue and its concurrency permit are released.
    /// - **Not yet at the hardware wait**: marked Cancelled, and the task
    ///   is stopped the moment it would write.
    /// - **Already generating** (round 53, this used to be refused): the
    ///   in-flight model call is interrupted. `stream_generation` races
    ///   the call against the task's cancel signal and, on a cancel,
    ///   aborts the spawned provider call so its socket closes and the
    ///   daemon stops generating. This covers the task's main call and a
    ///   model/specialist review call alike.
    ///
    /// Why it's safe to do now: round 49 refused because a pipeline write
    /// landing after `Cancelled` would overwrite it (a "resurrection").
    /// Every pipeline write now goes through `persist_task`, which refuses
    /// to overwrite a stored `Cancelled`, and `cancel_task`'s own persist
    /// is serialized against those writes by `cancel_gate`. A task that
    /// has already reached a terminal state is still reported as
    /// "can't be cancelled", it finished first.
    ///
    /// One narrow, named limit: a side effect that is not a task write
    /// (events already emitted, a benchmark attempt already recorded for a
    /// model call that did complete, a `Conflict` row inserted just before
    /// its guarded task write) is not rolled back by a cancel that lands a
    /// moment later.
    ///
    /// Cancelling a decomposed parent also cancels its still-active
    /// subtasks; cancelling a subtask leaves its siblings alone, and
    /// the parent then escalates to Review the way any subtask that
    /// "did not complete cleanly" already does.
    pub fn cancel_task(&self, task_id: Uuid) -> Result<()> {
        // Everything from validation to persisting happens under the gate,
        // so no pipeline write (`persist_task`) can land between "this task
        // is cancellable" and "it is now Cancelled".
        let targets = {
            let _gate = self.cancel_gate.lock().unwrap();
            let task = self
                .storage
                .get_task(task_id)?
                .ok_or_else(|| anyhow!("task {task_id} not found"))?;
            if !task.state.can_transition_to(TaskState::Cancelled) {
                return Err(anyhow!("task {task_id} can't be cancelled, it's already {}", task.state));
            }
            let children: Vec<Task> = self
                .storage
                .list_tasks(task.project_id)?
                .into_iter()
                .filter(|t| t.parent_task_id == Some(task_id) && t.state.can_transition_to(TaskState::Cancelled))
                .collect();
            let mut cancelled = Vec::new();
            for target in std::iter::once(task).chain(children) {
                let mut target = target;
                let previous = target.state;
                // Only a task inside `execute_task` can be waiting on
                // hardware, and only it will ever clear the marker (see
                // `SignalRegistration::drop`). A task that hasn't started
                // needs no marker: its stored state is now `Cancelled`, so
                // its first pipeline write is refused (`persist_task`). A
                // parent never waits at all.
                if self.signals.lock().unwrap().contains_key(&target.id) {
                    self.resources.cancel(target.id);
                }
                target.transition_to(TaskState::Cancelled)?;
                self.storage.update_task(&target)?;
                cancelled.push((target, previous));
            }
            cancelled
        };
        // Outside the gate: wake any in-flight model call. Its stored state
        // is already Cancelled, so even if it were mid-way to a write, that
        // write would now be refused.
        for (target, previous) in targets {
            let interrupted = self.fire_cancel_signal(target.id);
            self.emit(
                target.project_id,
                "human",
                ActorType::Human,
                EventType::TaskCancelled,
                json!({ "task_id": target.id, "previous_state": previous, "interrupted": interrupted }),
                Uuid::new_v4(),
                None,
            )?;
        }
        Ok(())
    }

    /// Round 53: wakes whatever is awaiting this task's cancel signal.
    /// Returns whether the task was inside `execute_task` at all (i.e.
    /// whether there was anything to interrupt).
    fn fire_cancel_signal(&self, task_id: Uuid) -> bool {
        match self.signals.lock().unwrap().get(&task_id) {
            Some(sender) => {
                sender.send_replace(true);
                true
            }
            None => false,
        }
    }

    /// Round 53: THE write path for a task from inside the pipeline. A
    /// task whose stored state is `Cancelled` is never overwritten by a
    /// pipeline write, that overwrite ("resurrection") is exactly why
    /// round 49 had to refuse to cancel a task that was generating. The
    /// one exception is a write that is itself the `Cancelled` state
    /// (`execute_task` reconciling its in-memory copy). Returns
    /// `TaskCancelled` when the write is refused, which `execute_task`
    /// turns into a clean `Cancelled` finish.
    fn persist_task(&self, task: &Task) -> Result<()> {
        let _gate = self.cancel_gate.lock().unwrap();
        if task.state != TaskState::Cancelled {
            if let Some(stored) = self.storage.get_task(task.id)? {
                if stored.state == TaskState::Cancelled {
                    return Err(anyhow::Error::new(TaskCancelled(task.id)));
                }
            }
        }
        self.storage.update_task(task)?;
        Ok(())
    }

    /// Round 44: sets or clears a task's deadline, `None` clears it,
    /// matching `Task.deadline`'s own `Option` shape rather than
    /// needing a separate "unset" method. No range check the way
    /// `set_task_priority` has one: any timestamp is a legitimate
    /// deadline, including one already in the past (a human might be
    /// recording that something was already overdue when they created
    /// the task, not necessarily scheduling the future).
    pub fn set_task_deadline(&self, task_id: Uuid, deadline: Option<chrono::DateTime<chrono::Utc>>) -> Result<()> {
        let task = self
            .storage
            .get_task(task_id)?
            .ok_or_else(|| anyhow!("task {task_id} not found"))?;
        let previous = task.deadline;
        // Round 57: single-column write, see `Storage::set_task_priority`.
        if !self.storage.set_task_deadline(task_id, deadline)? {
            return Err(anyhow!("task {task_id} not found"));
        }
        self.emit(
            task.project_id,
            "human",
            ActorType::Human,
            EventType::TaskDeadlineChanged,
            json!({ "task_id": task.id, "previous_deadline": previous, "new_deadline": deadline }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    /// Registers a human, AI, or hybrid member of a project (Phase 6:
    /// "Make humans first-class participants. Make AI members
    /// first-class participants.").
    pub fn register_member(&self, member: Member) -> Result<()> {
        self.storage.insert_member(&member)?;
        self.emit(
            member.project_id,
            "human",
            ActorType::Human,
            EventType::MemberRegistered,
            json!({ "member_id": member.id, "display_name": member.display_name, "member_type": member.member_type }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    /// Round 35: the project-level accessors the desktop shell needed
    /// to stop being pinned to one project. Thin pass-throughs by
    /// design, projects have no orchestration logic of their own, they
    /// are the scoping key everything else already carries.
    pub fn list_projects(&self) -> Result<Vec<solexa_domain::ProjectSummary>> {
        self.storage.list_projects()
    }

    pub fn get_project(&self, project_id: Uuid) -> Result<Option<solexa_domain::ProjectSummary>> {
        self.storage.get_project(project_id)
    }

    /// Creates a project and returns its new id. A brand-new project is
    /// deliberately empty, no seeded team, specialist, or member.
    /// `seed.rs` only bootstraps the very first project at first launch
    /// so the app isn't a blank wall on install; past that, what goes
    /// into a project is the user's call, and quietly inventing a
    /// "Sandbox Team" inside every new one would be exactly the fake
    /// scaffolding the spec warns against.
    pub fn create_project(&self, name: impl Into<String>, description: impl Into<String>) -> Result<Uuid> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(anyhow!("project name must not be empty"));
        }
        let project_id = Uuid::new_v4();
        self.storage.insert_project(project_id, &name, &description.into())?;
        Ok(project_id)
    }

    pub fn list_members(&self, project_id: Uuid) -> Result<Vec<Member>> {
        self.storage.list_members(project_id)
    }

    pub fn get_member(&self, member_id: Uuid) -> Result<Option<Member>> {
        self.storage.get_member(member_id)
    }

    /// Rewrites an existing member's `display_name`/`description`/
    /// `specialist_ids`, spec section 11's "Model Swapping": "The user
    /// should be able to replace an AI member's model without
    /// destroying: member identity, history, responsibilities,
    /// permissions, team relationships. This is essential." Renaming a
    /// human member (previously impossible, a member could be
    /// registered but never renamed) uses this same path.
    ///
    /// `updated.id`, `updated.project_id`, and `updated.member_type` are
    /// all ignored in favor of the existing stored values, same
    /// defensive shape `edit_specialist` uses for `id`/`active`. A
    /// member's type (human vs. AI vs. hybrid) is identity, not
    /// something an edit changes; a caller who wants to change *what*
    /// an AI member runs edits `specialist_ids`, not `member_type`.
    pub fn edit_member(&self, project_id: Uuid, member_id: Uuid, mut updated: Member) -> Result<()> {
        let _entity_gate = self.entity_gate();
        let existing = self
            .storage
            .get_member(member_id)?
            .ok_or_else(|| anyhow!("member {member_id} not found"))?;
        if updated.display_name.trim().is_empty() {
            return Err(anyhow!("member display name must not be empty"));
        }
        updated.id = member_id;
        updated.project_id = existing.project_id;
        updated.member_type = existing.member_type;
        updated.deleted_at = existing.deleted_at;
        // Round 46: role_ids is exclusively managed by
        // assign_role/unassign_role, the same defensive override
        // member_type and deleted_at already get, so a generic rename
        // through this method can never silently wipe a member's roles.
        updated.role_ids = existing.role_ids;
        self.storage.update_member(&updated)?;
        self.emit(
            project_id,
            "human",
            ActorType::Human,
            EventType::MemberUpdated,
            json!({
                "member_id": member_id,
                "display_name": updated.display_name,
                "previous_display_name": existing.display_name,
            }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    /// Registers a team, optionally nested under `team.parent_team_id`.
    pub fn register_team(&self, team: Team) -> Result<()> {
        if team.name.trim().is_empty() {
            return Err(anyhow!("team name must not be empty"));
        }
        self.storage.insert_team(&team)?;
        self.emit(
            team.project_id,
            "human",
            ActorType::Human,
            EventType::TeamRegistered,
            json!({ "team_id": team.id, "name": team.name, "parent_team_id": team.parent_team_id, "member_ids": team.member_ids }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    pub fn list_teams(&self, project_id: Uuid) -> Result<Vec<Team>> {
        self.storage.list_teams(project_id)
    }

    /// Rewrites an existing team's `name`/`mission`, a team could
    /// previously be registered but never renamed, and its mission
    /// statement was fixed at creation time forever. `updated.id`,
    /// `updated.member_ids`, and `updated.parent_team_id` are all
    /// ignored in favor of the existing stored values: membership
    /// changes go through `add_member_to_team` (a deliberate, single-
    /// purpose action), and re-parenting a team's place in the
    /// hierarchy is a bigger structural decision than a rename, same
    /// defensive shape `edit_specialist` uses for `active`, applied
    /// here to the two fields with their own dedicated mutation paths.
    pub fn edit_team(&self, project_id: Uuid, team_id: Uuid, mut updated: Team) -> Result<()> {
        let _entity_gate = self.entity_gate();
        let existing = self
            .storage
            .get_team(team_id)?
            .ok_or_else(|| anyhow!("team {team_id} not found"))?;
        if updated.name.trim().is_empty() {
            return Err(anyhow!("team name must not be empty"));
        }
        updated.id = team_id;
        updated.project_id = existing.project_id;
        updated.member_ids = existing.member_ids.clone();
        updated.parent_team_id = existing.parent_team_id;
        updated.deleted_at = existing.deleted_at;
        self.storage.update_team(&updated)?;
        self.emit(
            project_id,
            "human",
            ActorType::Human,
            EventType::TeamUpdated,
            json!({
                "team_id": team_id,
                "name": updated.name,
                "previous_name": existing.name,
            }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    /// Every team id nested under `team_id`, at any depth, the
    /// building block `set_team_parent` uses for cycle detection, and
    /// a natural companion to `team_members_recursive`'s "who's on
    /// this team including delegated sub-teams" with the team-shaped
    /// version of the same question.
    fn descendant_team_ids(&self, team_id: Uuid) -> Result<Vec<Uuid>> {
        let mut ids = Vec::new();
        for child in self.storage.list_child_teams(team_id)? {
            ids.push(child.id);
            ids.extend(self.descendant_team_ids(child.id)?);
        }
        Ok(ids)
    }

    /// Moves a team to a new place in the hierarchy, attaches it
    /// under `new_parent_id`, or detaches it to the top level if
    /// `None`. `edit_team` deliberately never touches `parent_team_id`
    /// (spec section 3: "Support hierarchy... do not require all teams
    /// to use the same orchestration strategy" implies re-parenting is
    /// a real, deliberate structural move, not a rename side effect),
    /// this is that dedicated action.
    ///
    /// Refuses to create a cycle: a team can't become its own parent,
    /// and can't be moved to be a descendant of one of its own
    /// descendants (which would make it its own ancestor once you
    /// follow the chain around). Both checks happen before anything is
    /// written, a rejected re-parent leaves the hierarchy exactly as
    /// it was.
    pub fn set_team_parent(&self, project_id: Uuid, team_id: Uuid, new_parent_id: Option<Uuid>) -> Result<()> {
        let _entity_gate = self.entity_gate();
        let mut team = self
            .storage
            .get_team(team_id)?
            .ok_or_else(|| anyhow!("team {team_id} not found"))?;

        if let Some(parent_id) = new_parent_id {
            if parent_id == team_id {
                return Err(anyhow!("a team cannot be its own parent"));
            }
            self.storage
                .get_team(parent_id)?
                .ok_or_else(|| anyhow!("parent team {parent_id} not found"))?;

            let descendants = self.descendant_team_ids(team_id)?;
            if descendants.contains(&parent_id) {
                return Err(anyhow!(
                    "cannot move team {team_id} under {parent_id}, {parent_id} is already a descendant of {team_id}, so this would create a cycle"
                ));
            }
        }

        let previous_parent_id = team.parent_team_id;
        team.parent_team_id = new_parent_id;
        self.storage.update_team(&team)?;
        self.emit(
            project_id,
            "human",
            ActorType::Human,
            EventType::TeamReparented,
            json!({
                "team_id": team_id,
                "new_parent_id": new_parent_id,
                "previous_parent_id": previous_parent_id,
            }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    /// Resolves a team's full membership, including every member of
    /// every team nested (at any depth) under it, spec: "Support
    /// hierarchy. A team can delegate to another team. Do not require
    /// all teams to use the same orchestration strategy," which starts
    /// with being able to answer "who's actually on this team?" across
    /// the whole delegated structure, not just its direct roster.
    pub fn team_members_recursive(&self, team_id: Uuid) -> Result<Vec<Member>> {
        let team = self
            .storage
            .get_team(team_id)?
            .ok_or_else(|| anyhow!("team {team_id} not found"))?;

        let mut member_ids: Vec<Uuid> = team.member_ids.clone();
        for child in self.storage.list_child_teams(team_id)? {
            for member in self.team_members_recursive(child.id)? {
                member_ids.push(member.id);
            }
        }
        member_ids.sort();
        member_ids.dedup();

        let mut members = Vec::with_capacity(member_ids.len());
        for id in member_ids {
            if let Some(member) = self.storage.get_member(id)? {
                members.push(member);
            }
        }
        Ok(members)
    }

    /// Adds an existing member to an existing team's roster (idempotent
    /// adding a member already present is a no-op, not an error).
    /// This is what lets a specialist created *after* the seed team
    /// exists actually join it, instead of only ever being wireable at
    /// `Team::with_members` construction time.
    pub fn add_member_to_team(&self, team_id: Uuid, member_id: Uuid) -> Result<()> {
        let _entity_gate = self.entity_gate();
        let mut team = self
            .storage
            .get_team(team_id)?
            .ok_or_else(|| anyhow!("team {team_id} not found"))?;
        if team.member_ids.contains(&member_id) {
            return Ok(());
        }
        team.member_ids.push(member_id);
        self.storage.update_team(&team)?;
        self.emit(
            team.project_id,
            "human",
            ActorType::Human,
            EventType::TeamMemberAdded,
            json!({ "team_id": team.id, "member_id": member_id }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    /// The inverse of `add_member_to_team`, removes a member from a
    /// team's roster without deleting the member itself (removing
    /// someone from a team isn't removing them from the project; they
    /// simply stop being one of this team's members). Same ergonomics
    /// as `add_member_to_team` deliberately: no `project_id` parameter
    /// (derived from the fetched team), and idempotent in the mirrored
    /// direction, removing a member who isn't on the roster is a
    /// no-op success, not an error, the same way adding an already-
    /// present member is.
    pub fn remove_member_from_team(&self, team_id: Uuid, member_id: Uuid) -> Result<()> {
        let _entity_gate = self.entity_gate();
        let mut team = self
            .storage
            .get_team(team_id)?
            .ok_or_else(|| anyhow!("team {team_id} not found"))?;
        if !team.member_ids.contains(&member_id) {
            return Ok(());
        }
        team.member_ids.retain(|id| *id != member_id);
        self.storage.update_team(&team)?;
        self.emit(
            team.project_id,
            "human",
            ActorType::Human,
            EventType::TeamMemberRemoved,
            json!({ "team_id": team.id, "member_id": member_id }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    // ---- soft delete (round 27) ---------------------------------------
    //
    // Never a hard delete: the row (and everything referencing it,
    // task history, other teams' member lists, etc.) stays exactly as
    // it was. `deleted_at` is a plain field on Member/Team specifically
    // so a delete or restore propagates through git sync via the exact
    // same per-field merge every other field already gets (round 25),
    // no new sync logic needed. See round 27's README for the full
    // scoping conversation this came out of, including why Specialist
    // (which already has `active`) and KnowledgeItem/Decision (which
    // already have their own status models) aren't touched here.

    pub fn delete_member(&self, member_id: Uuid) -> Result<()> {
        let _entity_gate = self.entity_gate();
        let mut member = self
            .storage
            .get_member(member_id)?
            .ok_or_else(|| anyhow!("member {member_id} not found"))?;
        if member.is_deleted() {
            return Ok(());
        }
        member.deleted_at = Some(chrono::Utc::now());
        self.storage.update_member(&member)?;
        self.emit(
            member.project_id,
            "human",
            ActorType::Human,
            EventType::MemberDeleted,
            json!({ "member_id": member_id, "deleted_at": member.deleted_at }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    pub fn restore_member(&self, member_id: Uuid) -> Result<()> {
        let _entity_gate = self.entity_gate();
        let mut member = self
            .storage
            .get_member(member_id)?
            .ok_or_else(|| anyhow!("member {member_id} not found"))?;
        if !member.is_deleted() {
            return Ok(());
        }
        member.deleted_at = None;
        self.storage.update_member(&member)?;
        self.emit(
            member.project_id,
            "human",
            ActorType::Human,
            EventType::MemberRestored,
            json!({ "member_id": member_id }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    // ---- roles (round 46) ---------------------------------------------
    //
    // "Discord roles" for SOLEXA, a named, reusable permission bundle a
    // Member can hold zero or more of, reusing `Permission`
    // (Read/Propose/Edit/Review/Approve/Execute), the same enum
    // `Specialist` has carried since Phase 3. See solexa-domain::role's
    // doc comment for the full rationale and this round's deliberate
    // scope boundary (Members get roles; Specialist keeps its own
    // separate, already-working permission field untouched).

    pub fn create_role(&self, project_id: Uuid, name: impl Into<String>, permissions: Vec<Permission>) -> Result<Role> {
        let role = Role::new(project_id, name, permissions);
        self.storage.insert_role(&role)?;
        self.emit(
            project_id,
            "human",
            ActorType::Human,
            EventType::RoleCreated,
            json!({ "role_id": role.id, "name": role.name, "permissions": role.permissions }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(role)
    }

    /// Renames a role or changes its permission set. Every member
    /// currently holding it picks up the change immediately, nothing
    /// about a member's own row needs to change, the entire point of a
    /// reusable role over an ad-hoc per-member permission list.
    pub fn update_role(&self, role_id: Uuid, name: impl Into<String>, permissions: Vec<Permission>) -> Result<()> {
        let _entity_gate = self.entity_gate();
        let mut role = self.storage.get_role(role_id)?.ok_or_else(|| anyhow!("role {role_id} not found"))?;
        role.name = name.into();
        role.permissions = permissions;
        self.storage.update_role(&role)?;
        self.emit(
            role.project_id,
            "human",
            ActorType::Human,
            EventType::RoleUpdated,
            json!({ "role_id": role.id, "name": role.name, "permissions": role.permissions }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    pub fn list_roles(&self, project_id: Uuid) -> Result<Vec<Role>> {
        self.storage.list_roles(project_id)
    }

    /// Idempotent, assigning a role a member already holds is a no-op,
    /// not an error, matching `disabling_and_enabling_the_same_model_
    /// twice_is_not_an_error`'s established convention for this kind of
    /// toggle.
    pub fn assign_role(&self, member_id: Uuid, role_id: Uuid) -> Result<()> {
        let _entity_gate = self.entity_gate();
        let mut member = self.storage.get_member(member_id)?.ok_or_else(|| anyhow!("member {member_id} not found"))?;
        let role = self
            .storage
            .get_role(role_id)?
            .ok_or_else(|| anyhow!("role {role_id} not found"))?;
        if role.project_id != member.project_id {
            return Err(anyhow!("role {role_id} belongs to a different project than member {member_id}"));
        }
        if member.role_ids.contains(&role_id) {
            return Ok(());
        }
        member.role_ids.push(role_id);
        self.storage.update_member(&member)?;
        self.emit(
            member.project_id,
            "human",
            ActorType::Human,
            EventType::MemberRoleAssigned,
            json!({ "member_id": member_id, "role_id": role_id, "role_name": role.name }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    pub fn unassign_role(&self, member_id: Uuid, role_id: Uuid) -> Result<()> {
        let _entity_gate = self.entity_gate();
        let mut member = self.storage.get_member(member_id)?.ok_or_else(|| anyhow!("member {member_id} not found"))?;
        if !member.role_ids.contains(&role_id) {
            return Ok(());
        }
        member.role_ids.retain(|id| *id != role_id);
        self.storage.update_member(&member)?;
        self.emit(
            member.project_id,
            "human",
            ActorType::Human,
            EventType::MemberRoleUnassigned,
            json!({ "member_id": member_id, "role_id": role_id }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    /// A member's effective permission set is the union across every
    /// role it holds, no hierarchy, no "highest role wins": holding
    /// any role that grants a permission is enough, the same flat
    /// union Discord itself uses across a member's roles.
    pub fn member_has_permission(&self, member_id: Uuid, permission: Permission) -> Result<bool> {
        let member = self.storage.get_member(member_id)?.ok_or_else(|| anyhow!("member {member_id} not found"))?;
        for role_id in &member.role_ids {
            if let Some(role) = self.storage.get_role(*role_id)? {
                if role.has_permission(permission) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    // ---- specialists and roles (round 47) ------------------------------
    //
    // Extends round 46's Role/permission mechanism to Specialist, the
    // "everything" half of the original "give roles to everyone and
    // everything" ask that round 46 deliberately deferred. See
    // solexa-domain::specialist's doc comment on `role_ids` for why
    // this is additive to, never a replacement for, a specialist's own
    // `permissions` field.

    /// Same union `member_has_permission` computes, plus the
    /// specialist's own direct `permissions` (round-3-era, still the
    /// primary grant, a role only ever adds to it).
    pub fn specialist_has_permission(&self, specialist_id: Uuid, permission: Permission) -> Result<bool> {
        let specialist = self.storage.get_specialist(specialist_id)?.ok_or_else(|| anyhow!("specialist {specialist_id} not found"))?;
        if specialist.permissions.contains(&permission) {
            return Ok(true);
        }
        for role_id in &specialist.role_ids {
            if let Some(role) = self.storage.get_role(*role_id)? {
                if role.has_permission(permission) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Idempotent, same shape as `assign_role`.
    pub fn assign_role_to_specialist(&self, specialist_id: Uuid, role_id: Uuid) -> Result<()> {
        let _entity_gate = self.entity_gate();
        let mut specialist = self.storage.get_specialist(specialist_id)?.ok_or_else(|| anyhow!("specialist {specialist_id} not found"))?;
        let role = self.storage.get_role(role_id)?.ok_or_else(|| anyhow!("role {role_id} not found"))?;
        // `Specialist` carries no `project_id` field of its own (it's
        // always looked up scoped to a project already), the only way
        // to check it belongs to the role's project is to look it up
        // within that project's own specialist list.
        let belongs_to_role_project = self.storage.list_specialists(role.project_id)?.iter().any(|s| s.id == specialist_id);
        if !belongs_to_role_project {
            return Err(anyhow!("role {role_id} belongs to a different project than specialist {specialist_id}"));
        }
        if specialist.role_ids.contains(&role_id) {
            return Ok(());
        }
        specialist.role_ids.push(role_id);
        self.storage.update_specialist(&specialist)?;
        self.emit(
            role.project_id,
            "human",
            ActorType::Human,
            EventType::MemberRoleAssigned,
            json!({ "specialist_id": specialist_id, "role_id": role_id, "role_name": role.name }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    pub fn unassign_role_from_specialist(&self, specialist_id: Uuid, role_id: Uuid) -> Result<()> {
        let _entity_gate = self.entity_gate();
        let mut specialist = self.storage.get_specialist(specialist_id)?.ok_or_else(|| anyhow!("specialist {specialist_id} not found"))?;
        if !specialist.role_ids.contains(&role_id) {
            return Ok(());
        }
        specialist.role_ids.retain(|id| *id != role_id);
        // project_id for the event: specialist rows don't carry their
        // own project_id field (they're always looked up scoped to a
        // project already), so pull it from the role being removed,
        // valid regardless of cross-project mistakes since a specialist
        // could only ever have gained this role_id via the
        // cross-project check in assign_role_to_specialist above.
        let role = self.storage.get_role(role_id)?;
        self.storage.update_specialist(&specialist)?;
        if let Some(role) = role {
            self.emit(
                role.project_id,
                "human",
                ActorType::Human,
                EventType::MemberRoleUnassigned,
                json!({ "specialist_id": specialist_id, "role_id": role_id }),
                Uuid::new_v4(),
                None,
            )?;
        }
        Ok(())
    }

    pub fn delete_team(&self, team_id: Uuid) -> Result<()> {
        let _entity_gate = self.entity_gate();
        let mut team = self
            .storage
            .get_team(team_id)?
            .ok_or_else(|| anyhow!("team {team_id} not found"))?;
        if team.is_deleted() {
            return Ok(());
        }
        team.deleted_at = Some(chrono::Utc::now());
        self.storage.update_team(&team)?;
        self.emit(
            team.project_id,
            "human",
            ActorType::Human,
            EventType::TeamDeleted,
            json!({ "team_id": team_id, "deleted_at": team.deleted_at }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    pub fn restore_team(&self, team_id: Uuid) -> Result<()> {
        let _entity_gate = self.entity_gate();
        let mut team = self
            .storage
            .get_team(team_id)?
            .ok_or_else(|| anyhow!("team {team_id} not found"))?;
        if !team.is_deleted() {
            return Ok(());
        }
        team.deleted_at = None;
        self.storage.update_team(&team)?;
        self.emit(
            team.project_id,
            "human",
            ActorType::Human,
            EventType::TeamRestored,
            json!({ "team_id": team_id }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    /// Registers a new `Specialist` configuration (spec section 13,
    /// "Specialist Creation": a structured flow, not a giant
    /// system-prompt textbox). Up to this round every specialist that
    /// ever existed came from `seed.rs`, this is the first path for
    /// one to enter a project any other way.
    ///
    /// Deliberately light validation: a specialist needs a name and at
    /// least one declared capability to ever be matchable by anything
    /// (an empty-capability specialist could never be routed a task,
    /// see `Planner`/capability matching). `preferred_model_ids` is
    /// intentionally *not* validated against currently-registered
    /// models here: `run_specialist` already falls back gracefully to
    /// any available model if none of a specialist's preferences
    /// resolve, exactly the "fallback models" behavior spec section 3
    /// describes, so rejecting an unresolvable preference at creation
    /// time would be stricter than the engine actually needs to be.
    pub fn register_specialist(&self, project_id: Uuid, specialist: Specialist) -> Result<()> {
        if specialist.name.trim().is_empty() {
            return Err(anyhow!("specialist name must not be empty"));
        }
        if specialist.capabilities.is_empty() {
            return Err(anyhow!(
                "specialist must declare at least one capability, or it can never be matched to a task"
            ));
        }
        specialist.validate_generation_limits().map_err(|e| anyhow!(e))?;
        self.storage.insert_specialist(project_id, &specialist)?;
        self.emit(
            project_id,
            "human",
            ActorType::Human,
            EventType::SpecialistRegistered,
            json!({
                "specialist_id": specialist.id,
                "name": specialist.name,
                "domain": specialist.domain,
                "capabilities": specialist.capabilities,
                "requires_model_review": specialist.escalation_policy.requires_model_review,
            }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    pub fn list_specialists(&self, project_id: Uuid) -> Result<Vec<Specialist>> {
        self.storage.list_specialists(project_id)
    }

    /// Rewrites an existing specialist's configuration in place, same
    /// validation as `register_specialist` (a name and at least one
    /// capability, or it's unroutable), but for a specialist that
    /// already exists rather than a new one. `updated.id` is ignored in
    /// favor of `specialist_id`: the caller supplies which specialist
    /// they're editing separately from the new field values, so a
    /// caller can't accidentally retarget the edit at a different
    /// specialist by mismatching the two. `updated.active` is likewise
    /// ignored in favor of the existing stored value, archiving is a
    /// deliberate act via `set_specialist_active`, never a side effect
    /// of an unrelated field edit (a UI form that doesn't even display
    /// the archived flag shouldn't be able to silently undo it).
    /// `updated.role_ids` is ignored the same way (round 52 fix, it was
    /// not, since round 47): roles are managed only by
    /// `assign_role_to_specialist` / `unassign_role_from_specialist`, and
    /// the edit form has no role control, so honoring the caller's list
    /// meant every ordinary edit silently stripped the specialist's roles
    /// and the permissions they granted. `max_tokens` / `temperature` ARE
    /// taken from `updated`, they're ordinary editable fields.
    pub fn edit_specialist(&self, project_id: Uuid, specialist_id: Uuid, mut updated: Specialist) -> Result<()> {
        let _entity_gate = self.entity_gate();
        let existing = self
            .storage
            .get_specialist(specialist_id)?
            .ok_or_else(|| anyhow!("specialist {specialist_id} not found"))?;
        if updated.name.trim().is_empty() {
            return Err(anyhow!("specialist name must not be empty"));
        }
        if updated.capabilities.is_empty() {
            return Err(anyhow!(
                "specialist must declare at least one capability, or it can never be matched to a task"
            ));
        }
        updated.validate_generation_limits().map_err(|e| anyhow!(e))?;
        updated.id = specialist_id;
        updated.active = existing.active;
        updated.role_ids = existing.role_ids.clone();
        self.storage.update_specialist(&updated)?;
        self.emit(
            project_id,
            "human",
            ActorType::Human,
            EventType::SpecialistUpdated,
            json!({
                "specialist_id": specialist_id,
                "name": updated.name,
                "previous_name": existing.name,
            }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    /// Archives (`active: false`) or reactivates (`active: true`) a
    /// specialist without deleting it, spec section 9's "never
    /// silently discard" philosophy applied to specialists the same
    /// way `KnowledgeStatus::Archived` applies it to facts and
    /// constraints. An archived specialist is excluded from task
    /// routing (see the capability-matching filter in
    /// `submit_request`) and from `rank_specialists_for_capability`'s
    /// discovery results, but keeps its full history, benchmark, past
    /// assignments, identity, and stays visible via `list_specialists`.
    /// Idempotent: setting the same value twice is a no-op, not an
    /// error, matching `add_member_to_team`'s style.
    pub fn set_specialist_active(&self, project_id: Uuid, specialist_id: Uuid, active: bool) -> Result<()> {
        let _entity_gate = self.entity_gate();
        let mut specialist = self
            .storage
            .get_specialist(specialist_id)?
            .ok_or_else(|| anyhow!("specialist {specialist_id} not found"))?;
        if specialist.active == active {
            return Ok(());
        }
        specialist.active = active;
        self.storage.update_specialist(&specialist)?;
        self.emit(
            project_id,
            "human",
            ActorType::Human,
            EventType::SpecialistActiveChanged,
            json!({ "specialist_id": specialist_id, "active": active }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    /// Produces a portable `SpecialistPackage` for an existing
    /// specialist, the "packaging" half of spec Phase 7's "specialist
    /// packaging, registry, installation." Includes the specialist's
    /// measured benchmark at export time, not just its configuration,
    /// so a proven specialist's track record travels with it rather
    /// than resetting to neutral on import.
    pub fn export_specialist_package(&self, specialist_id: Uuid) -> Result<SpecialistPackage> {
        let specialist = self
            .storage
            .get_specialist(specialist_id)?
            .ok_or_else(|| anyhow!("specialist {specialist_id} not found"))?;
        let benchmark = self.storage.get_benchmark(specialist_id)?;
        Ok(SpecialistPackage {
            package_format_version: CURRENT_PACKAGE_FORMAT_VERSION,
            specialist,
            benchmark,
        })
    }

    /// The "installation" half, registers a new specialist from a
    /// `SpecialistPackage`, the same way a human filling out the
    /// specialist-creation form would, just sourced from a portable
    /// document instead. Always assigns a fresh id and activates the
    /// result regardless of what the package's `specialist.id`/`active`
    /// happen to contain, an imported specialist is a new instance in
    /// this project, not a claim to be the literal same object as
    /// whatever it was exported from, and installing something
    /// shouldn't default to it being disabled. Rejects an unrecognized
    /// `package_format_version` rather than guessing how to read a
    /// package shape this build doesn't understand.
    ///
    /// If the package carries real benchmark history (`attempts() >
    /// 0`), that history is seeded onto the new specialist via
    /// `Storage::set_benchmark`, the imported copy starts with the
    /// proof of its track record already attached, not back at a
    /// neutral, unproven 0/0.
    pub fn import_specialist_package(&self, project_id: Uuid, package: SpecialistPackage) -> Result<Uuid> {
        if package.package_format_version != CURRENT_PACKAGE_FORMAT_VERSION {
            return Err(anyhow!(
                "unrecognized specialist package format version {} (this build understands version {})",
                package.package_format_version,
                CURRENT_PACKAGE_FORMAT_VERSION
            ));
        }
        let mut specialist = package.specialist;
        let new_id = Uuid::new_v4();
        specialist.id = new_id;
        specialist.active = true;
        self.register_specialist(project_id, specialist)?;

        if package.benchmark.attempts() > 0 {
            self.storage.set_benchmark(&SpecialistBenchmark {
                specialist_id: new_id,
                ..package.benchmark
            })?;
        }

        self.emit(
            project_id,
            "human",
            ActorType::Human,
            EventType::SpecialistImported,
            json!({ "specialist_id": new_id, "package_format_version": package.package_format_version }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(new_id)
    }

    /// Produces a portable `TeamPackage` for an existing team, every
    /// AI member's display name, description, and full specialist
    /// (packaged the same way `export_specialist_package` would,
    /// benchmark included). Human members and the team's place in the
    /// hierarchy are both deliberately excluded, see
    /// `TeamPackage`/`TeamMemberPackage`'s doc comments for why.
    pub fn export_team_package(&self, team_id: Uuid) -> Result<solexa_domain::TeamPackage> {
        let team = self
            .storage
            .get_team(team_id)?
            .ok_or_else(|| anyhow!("team {team_id} not found"))?;

        let mut members = Vec::new();
        for member_id in &team.member_ids {
            let Some(member) = self.storage.get_member(*member_id)? else { continue };
            if member.member_type != solexa_domain::MemberType::Ai {
                continue;
            }
            for specialist_id in &member.specialist_ids {
                let Some(specialist) = self.storage.get_specialist(*specialist_id)? else { continue };
                let benchmark = self.storage.get_benchmark(*specialist_id)?;
                members.push(solexa_domain::TeamMemberPackage {
                    display_name: member.display_name.clone(),
                    description: member.description.clone(),
                    specialist: solexa_domain::SpecialistPackage {
                        package_format_version: CURRENT_PACKAGE_FORMAT_VERSION,
                        specialist,
                        benchmark,
                    },
                });
            }
        }

        Ok(solexa_domain::TeamPackage {
            package_format_version: CURRENT_PACKAGE_FORMAT_VERSION,
            name: team.name,
            mission: team.mission,
            members,
        })
    }

    /// Installs a new team from a `TeamPackage`, registers a fresh
    /// team, then for each packaged AI member: imports its specialist
    /// via `import_specialist_package` (fresh id, always active, same
    /// rules as importing a specialist standalone), registers a fresh
    /// AI member running it, and adds that member to the new team.
    /// Reuses `register_team`/`import_specialist_package`/
    /// `register_member`/`add_member_to_team` rather than duplicating
    /// any of their validation, installing a team is exactly those
    /// four actions run in sequence, not a parallel code path.
    pub fn import_team_package(&self, project_id: Uuid, package: solexa_domain::TeamPackage) -> Result<Uuid> {
        if package.package_format_version != CURRENT_PACKAGE_FORMAT_VERSION {
            return Err(anyhow!(
                "unrecognized team package format version {} (this build understands version {})",
                package.package_format_version,
                CURRENT_PACKAGE_FORMAT_VERSION
            ));
        }

        let team = Team::new(project_id, package.name, package.mission);
        let team_id = team.id;
        self.register_team(team)?;

        for member_package in package.members {
            let specialist_id = self.import_specialist_package(project_id, member_package.specialist)?;
            let mut member = Member::new_ai(project_id, member_package.display_name, member_package.description);
            member.specialist_ids = vec![specialist_id];
            let member_id = member.id;
            self.register_member(member)?;
            self.add_member_to_team(team_id, member_id)?;
        }

        self.emit(
            project_id,
            "human",
            ActorType::Human,
            EventType::TeamImported,
            json!({ "team_id": team_id, "package_format_version": package.package_format_version }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(team_id)
    }

    /// Saves a copy of a specialist into the local registry, a
    /// personal library independent of any one project (spec's
    /// "Specialist Discovery"/"Specialist Marketplace", local-first
    /// slice). Reuses `export_specialist_package` rather than
    /// duplicating its logic: saving to the registry is exactly
    /// exporting, then keeping the result. `project_id` attributes the
    /// audit event to the project the specialist was saved *from* 
    /// the registry entry itself carries no project of its own.
    pub fn save_specialist_to_registry(&self, project_id: Uuid, specialist_id: Uuid) -> Result<Uuid> {
        let package = self.export_specialist_package(specialist_id)?;
        let entry = RegistryEntry {
            id: Uuid::new_v4(),
            kind: RegistryEntryKind::Specialist,
            name: package.specialist.name.clone(),
            domain: package.specialist.domain.clone(),
            capabilities: package.specialist.capabilities.clone(),
            package_json: serde_json::to_string(&package)?,
            added_at: chrono::Utc::now(),
        };
        let entry_id = entry.id;
        self.storage.insert_registry_entry(&entry)?;
        self.emit(
            project_id,
            "human",
            ActorType::Human,
            EventType::SavedToRegistry,
            json!({ "registry_entry_id": entry_id, "kind": "specialist", "name": entry.name }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(entry_id)
    }

    /// Saves a copy of a team's AI structure into the local registry.
    /// `capabilities` is the union across every packaged AI member's
    /// specialist, so the registry can be filtered by capability
    /// without deserializing every entry's full package.
    pub fn save_team_to_registry(&self, project_id: Uuid, team_id: Uuid) -> Result<Uuid> {
        let package = self.export_team_package(team_id)?;
        let mut capabilities: Vec<String> = Vec::new();
        for member in &package.members {
            for cap in &member.specialist.specialist.capabilities {
                if !capabilities.contains(cap) {
                    capabilities.push(cap.clone());
                }
            }
        }
        let entry = RegistryEntry {
            id: Uuid::new_v4(),
            kind: RegistryEntryKind::Team,
            name: package.name.clone(),
            domain: String::new(),
            capabilities,
            package_json: serde_json::to_string(&package)?,
            added_at: chrono::Utc::now(),
        };
        let entry_id = entry.id;
        self.storage.insert_registry_entry(&entry)?;
        self.emit(
            project_id,
            "human",
            ActorType::Human,
            EventType::SavedToRegistry,
            json!({ "registry_entry_id": entry_id, "kind": "team", "name": entry.name }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(entry_id)
    }

    /// Every entry in the local registry, newest first. Global, not
    /// scoped to any project, matching how the registry itself works.
    pub fn list_registry_entries(&self) -> Result<Vec<RegistryEntry>> {
        self.storage.list_registry_entries()
    }

    /// Installs a saved registry entry into `project_id`, deserializes
    /// its package and hands it to `import_specialist_package` or
    /// `import_team_package` depending on `entry.kind`, the same
    /// functions a pasted or file-loaded package already goes through.
    /// Returns the id of the newly created specialist or team.
    pub fn install_from_registry(&self, project_id: Uuid, entry_id: Uuid) -> Result<Uuid> {
        let entry = self
            .storage
            .get_registry_entry(entry_id)?
            .ok_or_else(|| anyhow!("registry entry {entry_id} not found"))?;
        let new_id = match entry.kind {
            RegistryEntryKind::Specialist => {
                let package: SpecialistPackage = serde_json::from_str(&entry.package_json)
                    .context("stored registry entry's package_json is not a valid SpecialistPackage")?;
                self.import_specialist_package(project_id, package)?
            }
            RegistryEntryKind::Team => {
                let package: solexa_domain::TeamPackage = serde_json::from_str(&entry.package_json)
                    .context("stored registry entry's package_json is not a valid TeamPackage")?;
                self.import_team_package(project_id, package)?
            }
        };
        self.emit(
            project_id,
            "human",
            ActorType::Human,
            EventType::InstalledFromRegistry,
            json!({ "registry_entry_id": entry_id, "kind": entry.kind, "installed_id": new_id }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(new_id)
    }

    /// Permanently removes an entry from the local registry, a
    /// genuine delete, not an archive (see `solexa_domain::registry`'s
    /// doc comment on why a registry entry doesn't carry the same
    /// never-silently-discard stakes a live `Specialist`/`Member`/
    /// `Team` does). Deliberately does not emit a project-scoped event:
    /// managing a personal, project-independent library isn't the kind
    /// of project work the event trace exists to make replayable,
    /// there is no project this action belongs to.
    pub fn remove_from_registry(&self, entry_id: Uuid) -> Result<()> {
        if self.storage.get_registry_entry(entry_id)?.is_none() {
            return Err(anyhow!("registry entry {entry_id} not found"));
        }
        self.storage.delete_registry_entry(entry_id)
    }

    // ---- git sync (round 22, start of Phase 8 "Collaboration";
    // round 23 extended it to the Project Brain) ----------------------
    //
    // No SOLEXA server, no SOLEXA accounts: `repo_path` is a directory
    // the user already `git clone`d themselves, and everything here
    // just shells out to the system `git` binary inside it (via
    // `solexa-sync`). Whoever has push access to that remote already
    // forms the "team", see `solexa_domain::sync` for the full
    // rationale. Scoped to the team roster plus the Project Brain
    // (Members/Teams/Specialists/KnowledgeItems/Decisions), see
    // `solexa_domain::snapshot` for what's deliberately excluded and
    // why.

    /// Bundles this project's team roster and Project Brain into a
    /// canonicalized `ProjectSnapshot`, what `push_to_git` writes to
    /// the linked repo and `pull_from_git` diffs a freshly-pulled
    /// remote copy against.
    pub fn export_project_snapshot(&self, project_id: Uuid) -> Result<ProjectSnapshot> {
        let project = self
            .storage
            .get_project(project_id)?
            .ok_or_else(|| anyhow!("project {project_id} not found"))?;
        let specialists = self.storage.list_specialists(project_id)?;
        let mut specialist_benchmarks = Vec::with_capacity(specialists.len());
        for specialist in &specialists {
            specialist_benchmarks.push(self.storage.get_benchmark(specialist.id)?);
        }
        Ok(ProjectSnapshot {
            format_version: solexa_domain::CURRENT_SNAPSHOT_FORMAT_VERSION,
            project_id,
            project_name: project.name,
            project_description: project.description,
            members: self.storage.list_members(project_id)?,
            teams: self.storage.list_teams(project_id)?,
            specialists,
            specialist_benchmarks,
            knowledge_items: self.storage.list_knowledge_items(project_id)?,
            decisions: self.storage.list_decisions(project_id)?,
        }
        .canonicalize())
    }

    /// Round 56: the snapshot the *sync* paths use, `export_project_snapshot`
    /// with role assignments blanked (see `ProjectSnapshot::
    /// without_role_assignments`). `export_project_snapshot` itself stays
    /// complete, since it is a faithful export of local state.
    fn export_sync_snapshot(&self, project_id: Uuid) -> Result<ProjectSnapshot> {
        Ok(self.export_project_snapshot(project_id)?.without_role_assignments())
    }

    /// Links `project_id` to a local git working directory. Fails fast
    /// with a specific error if `repo_path` doesn't look like a real
    /// git working tree, rather than only discovering the problem on
    /// the first push. Linking again (e.g. pointing at a different
    /// clone) simply replaces the existing link.
    pub fn link_git_repo(&self, project_id: Uuid, repo_path: String) -> Result<()> {
        let path = std::path::Path::new(&repo_path);
        if !solexa_sync::is_git_repo(path) {
            return Err(anyhow!(
                "{repo_path} doesn't look like a git working directory, clone the team's repo yourself first (e.g. `git clone <url> {repo_path}`), then link it here"
            ));
        }
        self.storage.set_git_sync_config(&GitSyncConfig::new(project_id, repo_path.clone()))?;
        self.emit(
            project_id,
            "human",
            ActorType::Human,
            EventType::GitRepoLinked,
            json!({ "repo_path": repo_path }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    /// A genuine delete, like `remove_from_registry`, this row is a
    /// pointer/config, not project substance. Unlinking never touches
    /// any already-synced Member/Team/Specialist data.
    pub fn unlink_git_repo(&self, project_id: Uuid) -> Result<()> {
        if self.storage.get_git_sync_config(project_id)?.is_none() {
            return Err(anyhow!("project {project_id} has no linked git repo"));
        }
        self.storage.delete_git_sync_config(project_id)?;
        self.emit(
            project_id,
            "human",
            ActorType::Human,
            EventType::GitRepoUnlinked,
            json!({}),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    /// Round 46: toggles the review gate on `pull_from_git`. Defaults
    /// to `false` for every project, turning it on is an explicit,
    /// deliberate choice, not something a project is opted into
    /// silently.
    pub fn set_require_review_for_incoming_changes(&self, project_id: Uuid, enabled: bool) -> Result<()> {
        let mut config = self
            .storage
            .get_git_sync_config(project_id)?
            .ok_or_else(|| anyhow!("project {project_id} has no linked git repo, link one with link_git_repo first"))?;
        config.require_review_for_incoming_changes = enabled;
        self.storage.set_git_sync_config(&config)?;
        self.emit(
            project_id,
            "human",
            ActorType::Human,
            EventType::RequireReviewForIncomingChangesChanged,
            json!({ "enabled": enabled }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    /// Round 55: whether this project makes deciding a Decision and
    /// resolving a Conflict require `Permission::Approve`. Defaults to
    /// `false` for every project, see `set_enforce_approver_roles`.
    pub fn enforce_approver_roles(&self, project_id: Uuid) -> Result<bool> {
        self.storage.get_enforce_approver_roles(project_id)
    }

    /// Round 55: turns on (or off) role enforcement for the approver-style
    /// actions, `resolve_decision`, `resolve_conflict`,
    /// `resolve_sync_conflict` (and so `check_conflict_github_pr_status`,
    /// which resolves through it). Until now those only required the
    /// approver to be a *registered Member*, while deciding a pending sync
    /// change already required `Permission::Approve`; with no roles held
    /// by default, a member could overrule a model conflict but not approve
    /// a sync change. Opt-in and default-off, like round 46's review gate,
    /// so no existing project changes behavior.
    ///
    /// **Enabling is refused when no member holds `Permission::Approve`**:
    /// that would leave every pending decision and conflict unresolvable
    /// the instant it took effect. Create a role with Approve and assign it
    /// first. Disabling is always allowed, this call takes no actor (the
    /// human at the keyboard is the owner of a local-first project), so
    /// turning it off is always the way out of an unwanted state.
    pub fn set_enforce_approver_roles(&self, project_id: Uuid, enabled: bool) -> Result<()> {
        if enabled {
            let mut anyone_can_approve = false;
            for member in self.storage.list_members(project_id)? {
                if self.member_has_permission(member.id, Permission::Approve)? {
                    anyone_can_approve = true;
                    break;
                }
            }
            if !anyone_can_approve {
                return Err(anyhow!(
                    "no member of this project holds Permission::Approve, so turning on role enforcement would leave every decision and conflict unresolvable, create a role that grants Approve and assign it to a member first"
                ));
            }
        }
        self.storage.set_enforce_approver_roles(project_id, enabled)?;
        self.emit(
            project_id,
            "human",
            ActorType::Human,
            EventType::EnforceApproverRolesChanged,
            json!({ "enabled": enabled }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    /// Round 55: the gate the approver-style actions call, BEFORE they
    /// write anything. A no-op when the project hasn't opted in.
    fn require_approver_permission(&self, project_id: Uuid, member_id: Uuid, action: &str) -> Result<()> {
        if !self.storage.get_enforce_approver_roles(project_id)? {
            return Ok(());
        }
        if !self.member_has_permission(member_id, Permission::Approve)? {
            return Err(anyhow!(
                "member {member_id} isn't authorized to {action}, this project enforces roles, so it needs a role with Permission::Approve"
            ));
        }
        Ok(())
    }

    /// Every pending-sync-change row for a project, pending, applied,
    /// and denied alike, oldest first, the same "keep decided ones as
    /// history" shape `list_conflicts` already uses.
    pub fn list_pending_sync_changes(&self, project_id: Uuid) -> Result<Vec<PendingSyncChange>> {
        self.storage.list_pending_sync_changes(project_id)
    }

    /// Applies a queued incoming change, the human-in-the-loop
    /// counterpart to what `pull_from_git` would otherwise have done
    /// immediately. Requires `deciding_member_id` to hold
    /// `Permission::Approve` via one of its roles; this is the one
    /// concrete enforcement point this round wires the whole Role
    /// system up to (see solexa-domain::role's doc comment for why
    /// that's the deliberate scope boundary, not an oversight).
    pub fn apply_pending_sync_change(&self, pending_change_id: Uuid, deciding_member_id: Uuid) -> Result<()> {
        let _entity_gate = self.entity_gate();
        let mut change = self
            .storage
            .get_pending_sync_change(pending_change_id)?
            .ok_or_else(|| anyhow!("pending sync change {pending_change_id} not found"))?;
        if change.status != PendingSyncChangeStatus::Pending {
            return Err(anyhow!("pending sync change {pending_change_id} was already decided ({:?})", change.status));
        }
        if !self.member_has_permission(deciding_member_id, Permission::Approve)? {
            return Err(anyhow!(
                "member {deciding_member_id} isn't authorized to decide pending sync changes, it needs a role with Permission::Approve"
            ));
        }
        let deciding_member = self
            .storage
            .get_member(deciding_member_id)?
            .ok_or_else(|| anyhow!("member {deciding_member_id} not found"))?;

        match change.kind {
            SyncObjectKind::Member => {
                let member: Member = serde_json::from_value(change.object_json.clone()).context("deserializing a queued Member")?;
                self.upsert_synced_member(&member)?;
            }
            SyncObjectKind::Team => {
                let team: Team = serde_json::from_value(change.object_json.clone()).context("deserializing a queued Team")?;
                self.upsert_synced_team(&team)?;
            }
            SyncObjectKind::Specialist => {
                let specialist: Specialist = serde_json::from_value(change.object_json.clone()).context("deserializing a queued Specialist")?;
                self.upsert_synced_specialist(change.project_id, &specialist)?;
                if let Some(benchmark_json) = &change.benchmark_json {
                    let benchmark: SpecialistBenchmark = serde_json::from_value(benchmark_json.clone()).context("deserializing a queued Specialist's benchmark")?;
                    self.storage.set_benchmark(&benchmark)?;
                }
            }
            SyncObjectKind::KnowledgeItem => {
                let item: KnowledgeItem = serde_json::from_value(change.object_json.clone()).context("deserializing a queued KnowledgeItem")?;
                self.upsert_synced_knowledge_item(&item)?;
            }
            SyncObjectKind::Decision => {
                let decision: Decision = serde_json::from_value(change.object_json.clone()).context("deserializing a queued Decision")?;
                self.upsert_synced_decision(&decision)?;
            }
        }

        change.apply(deciding_member.display_name.clone());
        self.storage.update_pending_sync_change(&change)?;
        self.emit(
            change.project_id,
            &deciding_member.display_name,
            ActorType::Human,
            EventType::PendingSyncChangeApplied,
            json!({ "pending_change_id": pending_change_id, "kind": change.kind, "object_id": change.object_id }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    /// Denies a queued incoming change, it's marked `Denied` and
    /// never applied. Same authorization requirement as
    /// `apply_pending_sync_change`. `reason` is optional but, unlike
    /// closing a GitHub PR, gives whoever denied it a real place to
    /// say why, one concrete way this is meant to be a better gate
    /// than mirroring through GitHub, not just a local copy of one.
    pub fn deny_pending_sync_change(&self, pending_change_id: Uuid, deciding_member_id: Uuid, reason: Option<String>) -> Result<()> {
        let _entity_gate = self.entity_gate();
        let mut change = self
            .storage
            .get_pending_sync_change(pending_change_id)?
            .ok_or_else(|| anyhow!("pending sync change {pending_change_id} not found"))?;
        if change.status != PendingSyncChangeStatus::Pending {
            return Err(anyhow!("pending sync change {pending_change_id} was already decided ({:?})", change.status));
        }
        if !self.member_has_permission(deciding_member_id, Permission::Approve)? {
            return Err(anyhow!(
                "member {deciding_member_id} isn't authorized to decide pending sync changes, it needs a role with Permission::Approve"
            ));
        }
        let deciding_member = self
            .storage
            .get_member(deciding_member_id)?
            .ok_or_else(|| anyhow!("member {deciding_member_id} not found"))?;

        change.deny(deciding_member.display_name.clone(), reason.clone());
        self.storage.update_pending_sync_change(&change)?;
        self.emit(
            change.project_id,
            &deciding_member.display_name,
            ActorType::Human,
            EventType::PendingSyncChangeDenied,
            json!({ "pending_change_id": pending_change_id, "kind": change.kind, "object_id": change.object_id, "reason": reason }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    /// `None` if the project has never been linked. `local_dirty` is
    /// computed by re-exporting the current local state and comparing
    /// it to the last-synced baseline, something git itself can't
    /// answer, since git only ever sees the file `push_to_git` writes,
    /// never the live SQLite state it came from.
    pub fn git_sync_status(&self, project_id: Uuid) -> Result<Option<GitSyncStatus>> {
        let Some(config) = self.storage.get_git_sync_config(project_id)? else {
            return Ok(None);
        };
        let current_json = serde_json::to_string(&self.export_sync_snapshot(project_id)?)?;
        // A baseline stored by an older version still carries role
        // assignments; compare it in its sanitized form so upgrading
        // doesn't make every project look spuriously "dirty".
        let baseline_json = config.last_synced_snapshot_json.as_deref().map(|raw| {
            serde_json::from_str::<ProjectSnapshot>(raw)
                .ok()
                .and_then(|s| serde_json::to_string(&s.without_role_assignments()).ok())
                .unwrap_or_else(|| raw.to_string())
        });
        let local_dirty = baseline_json.as_deref() != Some(current_json.as_str());
        Ok(Some(GitSyncStatus { config, local_dirty }))
    }

    /// Writes the current team roster to the linked repo and pushes it.
    /// A `pushed: false` result (nothing was actually different from
    /// what's already committed) is a normal, common outcome, not an
    /// error.
    pub fn push_to_git(&self, project_id: Uuid) -> Result<GitPushResult> {
        let mut config = self
            .storage
            .get_git_sync_config(project_id)?
            .ok_or_else(|| anyhow!("project {project_id} has no linked git repo, link one with link_git_repo first"))?;
        let repo_path = std::path::Path::new(&config.repo_path);

        let snapshot = self.export_sync_snapshot(project_id)?;
        let snapshot_json = serde_json::to_string(&snapshot)?;

        solexa_sync::write_snapshot(repo_path, &snapshot)?;
        let outcome = solexa_sync::commit_and_push(
            repo_path,
            &format!("SOLEXA sync: {} @ {}", snapshot.project_name, chrono::Utc::now().to_rfc3339()),
        )?;

        config.last_synced_commit = Some(outcome.commit.clone());
        config.last_synced_snapshot_json = Some(snapshot_json);
        config.last_synced_at = Some(chrono::Utc::now());
        self.storage.set_git_sync_config(&config)?;

        self.emit(
            project_id,
            "human",
            ActorType::Human,
            EventType::PushedToGit,
            json!({ "pushed": outcome.pushed, "commit": outcome.commit }),
            Uuid::new_v4(),
            None,
        )?;

        Ok(GitPushResult { pushed: outcome.pushed, commit: outcome.commit })
    }

    /// Pulls the linked repo, then reconciles the remote snapshot
    /// against local state, category by category (Members, Teams,
    /// Specialists): an object that only changed on one side since the
    /// last sync is fast-forwarded automatically; an object that
    /// changed differently on both sides becomes a `Conflict`, the
    /// same preserved-disagreement object Phase 5 uses for specialists
    /// disagreeing on a task, resolved with `resolve_sync_conflict`.
    /// Never silently overwrites a local edit, and never deletes a
    /// local object the remote no longer lists (see the round's README
    /// for why deletion propagation isn't attempted this round).
    /// Round 46: routes what would otherwise be an immediate `to_apply`
    /// fast-forward into a `PendingSyncChange` instead, when
    /// `config.require_review_for_incoming_changes` is set. Refreshes
    /// an existing *Pending* row for the same object in place rather
    /// than inserting a duplicate, so a still-undecided object doesn't
    /// pile up one row per pull while nobody's reviewed it yet.
    /// Deliberately doesn't touch the sync baseline at all (same as
    /// `create_sync_conflict` doesn't), see solexa-domain::sync's
    /// module doc for why that's safe: the next pull's 3-way diff
    /// simply sees "remote unchanged since baseline" and skips,
    /// exactly like an unresolved conflict already does today.
    #[allow(clippy::too_many_arguments)]
    fn queue_pending_sync_change(
        &self,
        project_id: Uuid,
        kind: SyncObjectKind,
        object_id: Uuid,
        label: impl Into<String>,
        object_json: serde_json::Value,
        benchmark_json: Option<serde_json::Value>,
        is_new: bool,
        source_commit: &str,
    ) -> Result<Uuid> {
        let _entity_gate = self.entity_gate();
        let label = label.into();
        if let Some(mut existing) = self.storage.get_pending_sync_change_for_object(project_id, kind, object_id)? {
            existing.refresh_content(label, object_json, benchmark_json, is_new, source_commit);
            self.storage.update_pending_sync_change(&existing)?;
            self.emit(
                project_id,
                "human",
                ActorType::Human,
                EventType::PendingSyncChangeQueued,
                json!({ "pending_change_id": existing.id, "kind": kind, "object_id": object_id, "refreshed": true }),
                Uuid::new_v4(),
                None,
            )?;
            Ok(existing.id)
        } else {
            let change = PendingSyncChange::new(project_id, kind, object_id, label, object_json, benchmark_json, is_new, source_commit);
            self.storage.insert_pending_sync_change(&change)?;
            self.emit(
                project_id,
                "human",
                ActorType::Human,
                EventType::PendingSyncChangeQueued,
                json!({ "pending_change_id": change.id, "kind": kind, "object_id": object_id, "refreshed": false }),
                Uuid::new_v4(),
                None,
            )?;
            Ok(change.id)
        }
    }

    pub fn pull_from_git(&self, project_id: Uuid) -> Result<GitPullResult> {
        let mut config = self
            .storage
            .get_git_sync_config(project_id)?
            .ok_or_else(|| anyhow!("project {project_id} has no linked git repo, link one with link_git_repo first"))?;
        let repo_path = std::path::Path::new(&config.repo_path);

        let commit_before = solexa_sync::head_commit(repo_path).unwrap_or_default();
        let pulled = solexa_sync::pull_ff_only(repo_path)?;
        let had_remote_changes = pulled.commit != commit_before;

        // Round 56: whatever the repo says about role assignments is
        // ignored, see `ProjectSnapshot::without_role_assignments`.
        let remote = solexa_sync::read_snapshot(repo_path)?.without_role_assignments();
        if remote.project_id != project_id {
            return Err(anyhow!(
                "the linked repo's snapshot is for a different project ({}) than the one you're pulling into ({project_id}), this looks like the wrong repo",
                remote.project_id
            ));
        }

        let baseline: Option<ProjectSnapshot> = config
            .last_synced_snapshot_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .context("parsing this project's stored sync baseline")?
            // A baseline stored by an older version still carries role
            // assignments (round 56): compare it in sanitized form.
            .map(ProjectSnapshot::without_role_assignments);
        let local = self.export_sync_snapshot(project_id)?;

        let mut result = GitPullResult { commit: pulled.commit.clone(), had_remote_changes, ..Default::default() };

        // Members
        let local_index = index_by_id(&local.members, |m| m.id);
        let diff = diff_sync_objects(
            &local_index,
            &index_by_id(&remote.members, |m| m.id),
            &baseline.as_ref().map(|b| index_by_id(&b.members, |m| m.id)).unwrap_or_default(),
        );
        for (id, value) in &diff.to_apply {
            if config.require_review_for_incoming_changes {
                let name = label_from_value(value, "display_name", "(unnamed)");
                let is_new = !local_index.contains_key(id);
                let pending_id = self.queue_pending_sync_change(project_id, SyncObjectKind::Member, *id, name, value.clone(), None, is_new, &pulled.commit)?;
                result.pending_changes_created.push(pending_id);
                continue;
            }
            let member: Member = serde_json::from_value(value.clone()).context("deserializing a synced Member")?;
            self.upsert_synced_member(&member)?;
            result.members_applied += 1;
        }
        for (id, field, local_value, remote_value) in diff.to_conflict {
            let name = display_label(&local_index, id, "display_name", "(unnamed)");
            result.conflicts_created.push(self.create_sync_conflict(
                project_id,
                SyncObjectKind::Member,
                id,
                field.clone(),
                format!("Member \"{name}\"'s \"{field}\" field was edited differently locally and on the linked repo"),
                local_value,
                remote_value,
                &pulled.commit,
            )?);
        }

        // Teams
        let local_index = index_by_id(&local.teams, |t| t.id);
        let diff = diff_sync_objects(
            &local_index,
            &index_by_id(&remote.teams, |t| t.id),
            &baseline.as_ref().map(|b| index_by_id(&b.teams, |t| t.id)).unwrap_or_default(),
        );
        for (id, value) in &diff.to_apply {
            if config.require_review_for_incoming_changes {
                let name = label_from_value(value, "name", "(unnamed)");
                let is_new = !local_index.contains_key(id);
                let pending_id = self.queue_pending_sync_change(project_id, SyncObjectKind::Team, *id, name, value.clone(), None, is_new, &pulled.commit)?;
                result.pending_changes_created.push(pending_id);
                continue;
            }
            let team: Team = serde_json::from_value(value.clone()).context("deserializing a synced Team")?;
            self.upsert_synced_team(&team)?;
            result.teams_applied += 1;
        }
        for (id, field, local_value, remote_value) in diff.to_conflict {
            let name = display_label(&local_index, id, "name", "(unnamed)");
            result.conflicts_created.push(self.create_sync_conflict(
                project_id,
                SyncObjectKind::Team,
                id,
                field.clone(),
                format!("Team \"{name}\"'s \"{field}\" field was edited differently locally and on the linked repo"),
                local_value,
                remote_value,
                &pulled.commit,
            )?);
        }

        // Specialists (+ carry benchmarks along for whichever specialists
        // were actually fast-forwarded, never for a conflicted one).
        let local_index = index_by_id(&local.specialists, |s| s.id);
        let diff = diff_sync_objects(
            &local_index,
            &index_by_id(&remote.specialists, |s| s.id),
            &baseline.as_ref().map(|b| index_by_id(&b.specialists, |s| s.id)).unwrap_or_default(),
        );
        let remote_benchmarks: HashMap<Uuid, &SpecialistBenchmark> =
            remote.specialist_benchmarks.iter().map(|b| (b.specialist_id, b)).collect();
        for (id, value) in &diff.to_apply {
            if config.require_review_for_incoming_changes {
                let name = label_from_value(value, "name", "(unnamed)");
                let is_new = !local_index.contains_key(id);
                let benchmark_json = remote_benchmarks.get(id).map(|b| serde_json::to_value(b)).transpose()?;
                let pending_id = self.queue_pending_sync_change(project_id, SyncObjectKind::Specialist, *id, name, value.clone(), benchmark_json, is_new, &pulled.commit)?;
                result.pending_changes_created.push(pending_id);
                continue;
            }
            let specialist: Specialist = serde_json::from_value(value.clone()).context("deserializing a synced Specialist")?;
            self.upsert_synced_specialist(project_id, &specialist)?;
            if let Some(benchmark) = remote_benchmarks.get(id) {
                self.storage.set_benchmark(benchmark)?;
            }
            result.specialists_applied += 1;
        }
        for (id, field, local_value, remote_value) in diff.to_conflict {
            let name = display_label(&local_index, id, "name", "(unnamed)");
            result.conflicts_created.push(self.create_sync_conflict(
                project_id,
                SyncObjectKind::Specialist,
                id,
                field.clone(),
                format!("Specialist \"{name}\"'s \"{field}\" field was edited differently locally and on the linked repo"),
                local_value,
                remote_value,
                &pulled.commit,
            )?);
        }

        // Knowledge items (facts/constraints/artifacts, round 23). Only
        // `status` can ever actually diverge: nothing in this app edits
        // a knowledge item's title/content/tags/kind after creation
        // (see `archive_knowledge_item`), so `upsert_synced_knowledge_item`
        // only ever needs to apply a status change.
        let local_index = index_by_id(&local.knowledge_items, |k| k.id);
        let diff = diff_sync_objects(
            &local_index,
            &index_by_id(&remote.knowledge_items, |k| k.id),
            &baseline.as_ref().map(|b| index_by_id(&b.knowledge_items, |k| k.id)).unwrap_or_default(),
        );
        for (id, value) in &diff.to_apply {
            if config.require_review_for_incoming_changes {
                let title = label_from_value(value, "title", "(untitled)");
                let is_new = !local_index.contains_key(id);
                let pending_id = self.queue_pending_sync_change(project_id, SyncObjectKind::KnowledgeItem, *id, title, value.clone(), None, is_new, &pulled.commit)?;
                result.pending_changes_created.push(pending_id);
                continue;
            }
            let item: KnowledgeItem = serde_json::from_value(value.clone()).context("deserializing a synced KnowledgeItem")?;
            self.upsert_synced_knowledge_item(&item)?;
            result.knowledge_items_applied += 1;
        }
        for (id, field, local_value, remote_value) in diff.to_conflict {
            let title = display_label(&local_index, id, "title", "(untitled)");
            result.conflicts_created.push(self.create_sync_conflict(
                project_id,
                SyncObjectKind::KnowledgeItem,
                id,
                field.clone(),
                format!("Knowledge item \"{title}\"'s \"{field}\" field was changed differently locally and on the linked repo"),
                local_value,
                remote_value,
                &pulled.commit,
            )?);
        }

        // Decisions (round 23). Only resolution/approved_by/status can
        // ever diverge post-creation (see `resolve_decision`), the
        // interesting real-world case is two people resolving the same
        // open question two different ways while both offline.
        let local_index = index_by_id(&local.decisions, |d| d.id);
        let diff = diff_sync_objects(
            &local_index,
            &index_by_id(&remote.decisions, |d| d.id),
            &baseline.as_ref().map(|b| index_by_id(&b.decisions, |d| d.id)).unwrap_or_default(),
        );
        for (id, value) in &diff.to_apply {
            if config.require_review_for_incoming_changes {
                let question = label_from_value(value, "question", "(no question)");
                let is_new = !local_index.contains_key(id);
                let pending_id = self.queue_pending_sync_change(project_id, SyncObjectKind::Decision, *id, question, value.clone(), None, is_new, &pulled.commit)?;
                result.pending_changes_created.push(pending_id);
                continue;
            }
            let decision: Decision = serde_json::from_value(value.clone()).context("deserializing a synced Decision")?;
            self.upsert_synced_decision(&decision)?;
            result.decisions_applied += 1;
        }
        for (id, field, local_value, remote_value) in diff.to_conflict {
            let question = display_label(&local_index, id, "question", "(no question)");
            result.conflicts_created.push(self.create_sync_conflict(
                project_id,
                SyncObjectKind::Decision,
                id,
                field.clone(),
                format!("Decision \"{question}\"'s \"{field}\" field was resolved differently locally and on the linked repo"),
                local_value,
                remote_value,
                &pulled.commit,
            )?);
        }

        config.last_synced_commit = Some(pulled.commit.clone());
        config.last_synced_snapshot_json = Some(serde_json::to_string(&remote)?);
        config.last_synced_at = Some(chrono::Utc::now());
        self.storage.set_git_sync_config(&config)?;

        // Round 31: every conflict this pull just created gets an
        // automatic attempt at `send_conflict_to_github`, the explicit
        // per-conflict button from round 30 still exists (for conflicts
        // that predate this round, or a retry after a failure below),
        // but nothing about *creating* one now waits for a human to
        // notice it first. Never lets a GitHub failure fail the pull
        // itself: the conflict is already safely created and fully
        // resolvable locally either way.
        for &conflict_id in &result.conflicts_created {
            match self.send_conflict_to_github(conflict_id) {
                Ok(url) => result.github_prs_created.push((conflict_id, url)),
                Err(err) => result.github_pr_errors.push((conflict_id, err.to_string())),
            }
        }

        self.emit(
            project_id,
            "human",
            ActorType::Human,
            EventType::PulledFromGit,
            json!({
                "commit": pulled.commit,
                "members_applied": result.members_applied,
                "teams_applied": result.teams_applied,
                "specialists_applied": result.specialists_applied,
                "knowledge_items_applied": result.knowledge_items_applied,
                "decisions_applied": result.decisions_applied,
                "conflicts_created": result.conflicts_created,
                "pending_changes_created": result.pending_changes_created,
                "github_prs_created": result.github_prs_created,
            }),
            Uuid::new_v4(),
            None,
        )?;

        Ok(result)
    }

    /// Resolves a sync conflict (one whose `sync_object` is set) by
    /// applying the chosen position's content back into local storage.
    /// The counterpart to `resolve_conflict`, which handles a
    /// task-execution conflict instead, each rejects the other's kind
    /// of conflict with a clear error pointing at the right function.
    pub fn resolve_sync_conflict(&self, conflict_id: Uuid, chosen_specialist_id: Uuid, approving_member_id: Uuid) -> Result<()> {
        let _entity_gate = self.entity_gate();
        let approver = self
            .storage
            .get_member(approving_member_id)?
            .ok_or_else(|| anyhow!("member {approving_member_id} is not registered in this project, only a registered Member can resolve a conflict"))?;

        let mut conflict = self
            .storage
            .get_conflict(conflict_id)?
            .ok_or_else(|| anyhow!("conflict {conflict_id} not found"))?;
        self.require_approver_permission(conflict.project_id, approving_member_id, "resolve a sync conflict")?;
        let sync_object = conflict.sync_object.clone().ok_or_else(|| {
            anyhow!("conflict {conflict_id} has no sync_object, this is a task-execution conflict; resolve it with resolve_conflict instead")
        })?;
        let position = conflict
            .position_for(chosen_specialist_id)
            .cloned()
            .ok_or_else(|| anyhow!("position {chosen_specialist_id} did not participate in conflict {conflict_id}"))?;

        match &sync_object.field {
            Some(field) => self.apply_synced_field_patch(conflict.project_id, sync_object.kind, sync_object.object_id, field, position.content.clone())?,
            None => {
                // Pre-round-25 conflict: content is the whole object, not
                // just one field (see `SyncObjectRef::field`'s doc comment).
                match sync_object.kind {
                    SyncObjectKind::Member => {
                        let member: Member = serde_json::from_value(position.content.clone())
                            .context("deserializing the accepted position's Member content")?;
                        self.upsert_synced_member(&member)?;
                    }
                    SyncObjectKind::Team => {
                        let team: Team = serde_json::from_value(position.content.clone())
                            .context("deserializing the accepted position's Team content")?;
                        self.upsert_synced_team(&team)?;
                    }
                    SyncObjectKind::Specialist => {
                        let specialist: Specialist = serde_json::from_value(position.content.clone())
                            .context("deserializing the accepted position's Specialist content")?;
                        self.upsert_synced_specialist(conflict.project_id, &specialist)?;
                    }
                    SyncObjectKind::KnowledgeItem => {
                        let item: KnowledgeItem = serde_json::from_value(position.content.clone())
                            .context("deserializing the accepted position's KnowledgeItem content")?;
                        self.upsert_synced_knowledge_item(&item)?;
                    }
                    SyncObjectKind::Decision => {
                        let decision: Decision = serde_json::from_value(position.content.clone())
                            .context("deserializing the accepted position's Decision content")?;
                        self.upsert_synced_decision(&decision)?;
                    }
                }
            }
        }

        conflict.resolve(format!("Accepted {} version", position.specialist_name), approver.display_name.clone());
        self.storage.update_conflict(&conflict)?;

        self.emit(
            conflict.project_id,
            &approver.display_name,
            ActorType::Human,
            EventType::ConflictResolved,
            json!({
                "conflict_id": conflict.id,
                "sync_object_kind": sync_object.kind,
                "sync_object_id": sync_object.object_id,
                "approved_by_member_id": approver.id,
            }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(())
    }

    /// Round 30: opens a real GitHub PR proposing the *local* side's
    /// value for a still-open sync Conflict, so a teammate can accept
    /// or reject it through GitHub's own review UI instead of (or as
    /// well as) SOLEXA's local Conflict Center. Requires the project to
    /// have a linked git repo and the `gh` CLI installed and already
    /// authenticated (`gh auth login`), SOLEXA never stores or asks
    /// for a GitHub credential of its own, the same principle round
    /// 22's plain git sync already applies to `git` itself.
    ///
    /// The PR's *base* branch is whatever's currently checked out
    /// (normally the just-pulled remote state, what stays in place if
    /// the PR is closed without merging); its *head* branch is a new
    /// throwaway branch containing the local DB's current merged
    /// snapshot, which for the one disputed field still holds local's
    /// value (round 25's per-field merge already fast-forwarded
    /// everything else about this object). Merging the PR therefore
    /// means "accept local's value"; closing it without merging means
    /// "keep what's already there". Returns the new PR's URL.
    pub fn send_conflict_to_github(&self, conflict_id: Uuid) -> Result<String> {
        let conflict = self
            .storage
            .get_conflict(conflict_id)?
            .ok_or_else(|| anyhow!("conflict {conflict_id} not found"))?;
        if conflict.status == solexa_domain::ConflictStatus::Resolved {
            return Err(anyhow!("conflict {conflict_id} is already resolved, nothing to send to GitHub"));
        }
        if conflict.github_pr_url.is_some() {
            return Err(anyhow!(
                "conflict {conflict_id} already has a GitHub PR ({}), check its status instead of opening another",
                conflict.github_pr_url.unwrap()
            ));
        }
        let sync_object = conflict
            .sync_object
            .clone()
            .ok_or_else(|| anyhow!("conflict {conflict_id} has no sync_object, GitHub PR mirroring only applies to sync conflicts, not task-execution conflicts"))?;
        let field = sync_object.field.clone().ok_or_else(|| {
            anyhow!("conflict {conflict_id} predates round 25's field-level conflicts and can't be sent to GitHub as a single-field PR")
        })?;
        let local_position = conflict
            .positions
            .iter()
            .find(|p| p.specialist_name == "Local")
            .ok_or_else(|| anyhow!("conflict {conflict_id} has no \"Local\" position, this doesn't look like a sync conflict"))?
            .clone();

        if !self.github.is_available() {
            return Err(anyhow!("the `gh` CLI isn't installed or isn't authenticated, install it and run `gh auth login` first, then try again"));
        }

        let config = self
            .storage
            .get_git_sync_config(conflict.project_id)?
            .ok_or_else(|| anyhow!("project {} has no linked git repo, link one with link_git_repo first", conflict.project_id))?;
        let repo_path = std::path::Path::new(&config.repo_path);

        let original_branch = solexa_sync::current_branch(repo_path)?;
        let branch_name = format!("solexa-conflict/{:?}-{}", sync_object.kind, &conflict.id.to_string()[..8]).to_lowercase();
        let local_snapshot = self.export_sync_snapshot(conflict.project_id)?;

        solexa_sync::checkout_new_branch(repo_path, &branch_name)?;
        // Whatever happens next, the working tree must end up back on
        // whatever branch the user actually had checked out, an
        // interrupted branch switch left behind would silently break
        // the *next* push/pull this project does.
        let push_result: Result<solexa_sync::PushOutcome> = (|| {
            solexa_sync::write_snapshot(repo_path, &local_snapshot)?;
            solexa_sync::commit_and_push_new_branch(
                repo_path,
                &branch_name,
                &format!("SOLEXA: propose local's value for conflict {}", conflict.id),
            )
        })();
        solexa_sync::checkout_branch(repo_path, &original_branch)?;
        let push = push_result?;
        if !push.pushed {
            return Err(anyhow!("the local and remote values for this conflict are already identical, nothing to open a PR about"));
        }

        let title = format!("Resolve conflict: {}", conflict.question);
        let body = format!(
            "SOLEXA sync conflict on **{:?} {}**, field `{}`.\n\n\
             - Merging this PR accepts the **local** device's value.\n\
             - Closing it without merging keeps what's already on `{original_branch}`.\n\n\
             Local's proposed value:\n```json\n{}\n```",
            sync_object.kind,
            sync_object.object_id,
            field,
            serde_json::to_string_pretty(&local_position.content).unwrap_or_default(),
        );

        let url = self.github.create_pr(
            repo_path,
            CreatePrParams { title: &title, body: &body, base_branch: &original_branch, head_branch: &branch_name },
        )?;

        self.storage.set_conflict_github_pr_url(conflict.id, &url)?;
        self.emit(
            conflict.project_id,
            "human",
            ActorType::Human,
            EventType::ConflictSentToGithub,
            json!({ "conflict_id": conflict.id, "pr_url": url }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(url)
    }

    /// Round 30's other half: checks whether a conflict's GitHub PR has
    /// been merged, closed, or is still open, and, for the two
    /// terminal states, resolves the conflict automatically by calling
    /// `resolve_sync_conflict` with whichever position the PR's outcome
    /// implies. `checking_member_id` is recorded as the approver: the
    /// decision was actually made on GitHub, but pulling that decision
    /// back into SOLEXA is itself an action a specific member takes
    /// (pressing "Check GitHub PR status"), and every resolution in
    /// this app is accountable to a registered Member, see
    /// `resolve_sync_conflict`'s own doc comment.
    pub fn check_conflict_github_pr_status(&self, conflict_id: Uuid, checking_member_id: Uuid) -> Result<GithubPrCheckOutcome> {
        let conflict = self
            .storage
            .get_conflict(conflict_id)?
            .ok_or_else(|| anyhow!("conflict {conflict_id} not found"))?;
        if conflict.status == solexa_domain::ConflictStatus::Resolved {
            return Ok(GithubPrCheckOutcome::AlreadyResolved);
        }
        let pr_url = conflict
            .github_pr_url
            .clone()
            .ok_or_else(|| anyhow!("conflict {conflict_id} was never sent to GitHub, call send_conflict_to_github first"))?;

        let config = self
            .storage
            .get_git_sync_config(conflict.project_id)?
            .ok_or_else(|| anyhow!("project {} has no linked git repo", conflict.project_id))?;
        let repo_path = std::path::Path::new(&config.repo_path);

        let status = self.github.get_pr_status(repo_path, &pr_url)?;
        match status.state {
            PrState::Open => Ok(GithubPrCheckOutcome::StillOpen),
            PrState::Merged => {
                let local_position = conflict
                    .positions
                    .iter()
                    .find(|p| p.specialist_name == "Local")
                    .ok_or_else(|| anyhow!("conflict {conflict_id} has no \"Local\" position"))?;
                self.resolve_sync_conflict(conflict_id, local_position.specialist_id, checking_member_id)?;
                Ok(GithubPrCheckOutcome::ResolvedAcceptingLocal)
            }
            PrState::Closed => {
                let remote_position = conflict
                    .positions
                    .iter()
                    .find(|p| p.specialist_name != "Local")
                    .ok_or_else(|| anyhow!("conflict {conflict_id} has no non-local position"))?;
                self.resolve_sync_conflict(conflict_id, remote_position.specialist_id, checking_member_id)?;
                Ok(GithubPrCheckOutcome::ResolvedAcceptingRemote)
            }
        }
    }

    /// Round 32: the automatic counterpart to clicking "Check PR
    /// status" on every conflict one at a time, sweeps every conflict
    /// in the project that is still `Open` *and* already has a GitHub
    /// PR (from round 30's manual send or round 31's automatic one),
    /// and checks each. Reuses `check_conflict_github_pr_status`
    /// entirely; this function is nothing but the loop and the
    /// eligibility filter around it. One conflict's check failing
    /// (most commonly: `gh` no longer authenticated) never stops the
    /// rest of the sweep, each outcome or error is recorded against
    /// its own conflict id.
    pub fn check_all_conflict_github_pr_statuses(&self, project_id: Uuid, checking_member_id: Uuid) -> Result<GithubPrStatusSweepResult> {
        let mut result = GithubPrStatusSweepResult::default();
        let conflicts = self.storage.list_conflicts(project_id)?;
        for conflict in conflicts
            .into_iter()
            .filter(|c| c.status == solexa_domain::ConflictStatus::Open && c.github_pr_url.is_some())
        {
            match self.check_conflict_github_pr_status(conflict.id, checking_member_id) {
                Ok(outcome) => result.checked.push((conflict.id, outcome)),
                Err(err) => result.errors.push((conflict.id, err.to_string())),
            }
        }
        Ok(result)
    }

    /// Round 56: an existing member keeps ITS role assignments, they are
    /// device-local (see `ProjectSnapshot::without_role_assignments`), and
    /// the incoming value carries none. A member new to this device starts
    /// with no roles.
    fn upsert_synced_member(&self, member: &Member) -> Result<()> {
        let _entity_gate = self.entity_gate();
        if let Some(existing) = self.storage.get_member(member.id)? {
            let mut incoming = member.clone();
            incoming.role_ids = existing.role_ids;
            self.storage.update_member(&incoming)
        } else {
            let mut incoming = member.clone();
            incoming.role_ids.clear();
            self.storage.insert_member(&incoming)
        }
    }

    fn upsert_synced_team(&self, team: &Team) -> Result<()> {
        let _entity_gate = self.entity_gate();
        if self.storage.get_team(team.id)?.is_some() {
            self.storage.update_team(team)
        } else {
            self.storage.insert_team(team)
        }
    }

    /// Round 56: same rule as `upsert_synced_member`.
    fn upsert_synced_specialist(&self, project_id: Uuid, specialist: &Specialist) -> Result<()> {
        let _entity_gate = self.entity_gate();
        if let Some(existing) = self.storage.get_specialist(specialist.id)? {
            let mut incoming = specialist.clone();
            incoming.role_ids = existing.role_ids;
            self.storage.update_specialist(&incoming)
        } else {
            let mut incoming = specialist.clone();
            incoming.role_ids.clear();
            self.storage.insert_specialist(project_id, &incoming)
        }
    }

    /// Only ever applies a status change, see the comment on this
    /// function's call site in `pull_from_git` for why that's the only
    /// field a KnowledgeItem can actually diverge on in this app.
    fn upsert_synced_knowledge_item(&self, item: &KnowledgeItem) -> Result<()> {
        let _entity_gate = self.entity_gate();
        match self.storage.get_knowledge_item(item.id)? {
            Some(existing) if existing.status != item.status => {
                self.storage.update_knowledge_item_status(item.id, item.status)
            }
            Some(_) => Ok(()),
            None => self.storage.insert_knowledge_item(item),
        }
    }

    fn upsert_synced_decision(&self, decision: &Decision) -> Result<()> {
        let _entity_gate = self.entity_gate();
        if self.storage.get_decision(decision.id)?.is_some() {
            self.storage.update_decision(decision)
        } else {
            self.storage.insert_decision(decision)
        }
    }

    /// Patches one field of an object's *current* stored state (round
    /// 25), used when resolving a field-level sync conflict, where the
    /// accepted position's content is just the disputed field's value,
    /// not a whole object. Patching the object's current state (rather
    /// than replacing it outright with a stale full snapshot) preserves
    /// whatever other fields the pull that created this conflict had
    /// already safely merged in.
    fn apply_synced_field_patch(&self, project_id: Uuid, kind: SyncObjectKind, object_id: Uuid, field: &str, value: serde_json::Value) -> Result<()> {
        match kind {
            SyncObjectKind::Member => {
                let current = self.storage.get_member(object_id)?.ok_or_else(|| anyhow!("member {object_id} not found"))?;
                let mut json = serde_json::to_value(&current)?;
                json.as_object_mut().expect("Member serializes to a JSON object").insert(field.to_string(), value);
                let patched: Member = serde_json::from_value(json).context("applying field patch to Member")?;
                self.upsert_synced_member(&patched)
            }
            SyncObjectKind::Team => {
                let current = self.storage.get_team(object_id)?.ok_or_else(|| anyhow!("team {object_id} not found"))?;
                let mut json = serde_json::to_value(&current)?;
                json.as_object_mut().expect("Team serializes to a JSON object").insert(field.to_string(), value);
                let patched: Team = serde_json::from_value(json).context("applying field patch to Team")?;
                self.upsert_synced_team(&patched)
            }
            SyncObjectKind::Specialist => {
                let current = self.storage.get_specialist(object_id)?.ok_or_else(|| anyhow!("specialist {object_id} not found"))?;
                let mut json = serde_json::to_value(&current)?;
                json.as_object_mut().expect("Specialist serializes to a JSON object").insert(field.to_string(), value);
                let patched: Specialist = serde_json::from_value(json).context("applying field patch to Specialist")?;
                self.upsert_synced_specialist(project_id, &patched)
            }
            SyncObjectKind::KnowledgeItem => {
                let current = self.storage.get_knowledge_item(object_id)?.ok_or_else(|| anyhow!("knowledge item {object_id} not found"))?;
                let mut json = serde_json::to_value(&current)?;
                json.as_object_mut().expect("KnowledgeItem serializes to a JSON object").insert(field.to_string(), value);
                let patched: KnowledgeItem = serde_json::from_value(json).context("applying field patch to KnowledgeItem")?;
                self.upsert_synced_knowledge_item(&patched)
            }
            SyncObjectKind::Decision => {
                let current = self.storage.get_decision(object_id)?.ok_or_else(|| anyhow!("decision {object_id} not found"))?;
                let mut json = serde_json::to_value(&current)?;
                json.as_object_mut().expect("Decision serializes to a JSON object").insert(field.to_string(), value);
                let patched: Decision = serde_json::from_value(json).context("applying field patch to Decision")?;
                self.upsert_synced_decision(&patched)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn create_sync_conflict(
        &self,
        project_id: Uuid,
        kind: SyncObjectKind,
        object_id: Uuid,
        field: String,
        question: String,
        local_value: serde_json::Value,
        remote_value: serde_json::Value,
        remote_commit: &str,
    ) -> Result<Uuid> {
        let conflict = Conflict::new_sync(
            project_id,
            SyncObjectRef { kind, object_id, field: Some(field.clone()) },
            question.clone(),
            vec![
                ConflictPosition {
                    specialist_id: Uuid::new_v4(),
                    specialist_name: "Local".into(),
                    model_id: None,
                    content: local_value,
                    verification_passed: None,
                },
                ConflictPosition {
                    specialist_id: Uuid::new_v4(),
                    specialist_name: format!("Remote ({})", &remote_commit[..remote_commit.len().min(7)]),
                    model_id: None,
                    content: remote_value,
                    verification_passed: None,
                },
            ],
        );
        let conflict_id = conflict.id;
        self.storage.insert_conflict(&conflict)?;
        self.emit(
            project_id,
            "coordinator",
            ActorType::System,
            EventType::ConflictDetected,
            json!({ "conflict_id": conflict_id, "sync_object_kind": kind, "sync_object_id": object_id, "sync_object_field": field, "question": question }),
            Uuid::new_v4(),
            None,
        )?;
        Ok(conflict_id)
    }

    /// Every model any registered `ModelProvider` currently advertises,
    /// what a specialist-creation UI would offer as `preferred_model_ids`
    /// choices. Read-only; registering new model providers/metadata
    /// still happens at `Coordinator` construction time (spec section 5
    /// scoped this crate to routing/using models, not installing them).
    pub fn list_models(&self) -> Vec<ModelMetadata> {
        self.providers.all_models()
    }

    /// Round 41: `list_models()` filtered to what `project_id` hasn't
    /// explicitly disabled, the view every model-selection call site
    /// (`run_specialist`, `model_review`, `specialist_review`) actually
    /// routes against, and what a specialist-creation UI should offer
    /// as choices for *this* project specifically, rather than every
    /// model any project in the install happens to share a daemon with.
    pub fn models_available_to(&self, project_id: Uuid) -> Result<Vec<ModelMetadata>> {
        let disabled: HashSet<Uuid> = self.storage.disabled_model_ids_for_project(project_id)?.into_iter().collect();
        Ok(self.providers.all_models().into_iter().filter(|m| !disabled.contains(&m.id)).collect())
    }

    /// Which of `list_models()`'s ids `project_id` has turned off, the
    /// complement of `models_available_to`, for a settings UI that
    /// needs to render every model with its current per-project
    /// enabled/disabled state rather than just the ones still allowed.
    pub fn disabled_model_ids(&self, project_id: Uuid) -> Result<Vec<Uuid>> {
        self.storage.disabled_model_ids_for_project(project_id)
    }

    /// Disables or re-enables `model_id` for `project_id` specifically.
    /// Doesn't touch any task, specialist, or `preferred_model_ids`
    /// already on record, a specialist that still lists a
    /// newly-disabled model as preferred simply stops being able to use
    /// it the next time it's assigned work (every selection call site
    /// checks `models_available_to`, not just "does a provider exist"),
    /// rather than being silently reassigned or blocked outright.
    pub fn set_model_enabled(&self, project_id: Uuid, model_id: Uuid, enabled: bool) -> Result<()> {
        self.storage.set_model_enabled_for_project(project_id, model_id, enabled)
    }

    /// The "registry" query (spec section 13, Specialist Discovery):
    /// every specialist advertising `capability`, ranked by measured
    /// verification pass rate, best first. Independent of task
    /// execution entirely; this is for a human (or a future UI) asking
    /// "who's my best SQL specialist?", not part of the routing path.
    pub fn rank_specialists_for_capability(
        &self,
        project_id: Uuid,
        capability: &str,
    ) -> Result<Vec<(Specialist, SpecialistBenchmark)>> {
        let mut ranked: Vec<(Specialist, SpecialistBenchmark)> = self
            .storage
            .list_specialists(project_id)?
            .into_iter()
            .filter(|s| s.active && s.has_capability(capability))
            .map(|s| {
                let benchmark = self.storage.get_benchmark(s.id)?;
                Ok((s, benchmark))
            })
            .collect::<Result<Vec<_>>>()?;
        ranked.sort_by(|(_, a), (_, b)| {
            b.success_rate().partial_cmp(&a.success_rate()).unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(ranked)
    }

    /// Runs the full core loop for one request, synchronously with
    /// respect to the caller (no queueing/scheduling yet, that's
    /// Phase 3). A request that implies one capability gets one task
    /// end to end, unchanged from Phase 1. A request implying more than
    /// one capability is decomposed by the planner into a parent task
    /// plus one subtask per capability (Phase 2); each subtask is
    /// independently routed to whichever specialist matches it, and the
    /// parent's result is assembled from all of them once they resolve.
    /// `Submission.task_id` is always the top-level task the caller
    /// should watch: the single task in the simple case, the parent in
    /// the decomposed case.
    ///
    /// This never panics on a business-logic failure (missing
    /// specialist, failed verification, ...), those become visible
    /// task states and events instead.
    pub async fn submit_request(&self, request: UserRequest) -> Result<Submission> {
        let correlation_id = Uuid::new_v4();
        let mut last_event_id = self.emit(
            request.project_id,
            &request.requested_by,
            ActorType::Human,
            EventType::RequestReceived,
            json!({ "text": request.text }),
            correlation_id,
            None,
        )?;

        last_event_id = self.emit_after(
            &request,
            EventType::PlannerStarted,
            json!({}),
            correlation_id,
            last_event_id,
        )?;

        // ---- PLANNER / TASK DECOMPOSITION ----------------------------
        let mut planned = self.planner.plan(&request);
        if planned.is_empty() {
            return Err(anyhow!("planner returned no tasks for a non-empty request"));
        }

        if planned.len() == 1 {
            // Single-capability request: exactly the Phase 1 path.
            let mut task = planned.pop().unwrap();
            task.transition_to(TaskState::Planned)
                .context("planner produced an already-invalid task state")?;
            self.storage.insert_task(&task)?;

            last_event_id = self.emit_after(
                &request,
                EventType::TaskCreated,
                json!({
                    "task_id": task.id,
                    "title": task.title,
                    "required_capabilities": task.required_capabilities,
                }),
                correlation_id,
                last_event_id,
            )?;
            last_event_id = self.emit_after(
                &request,
                EventType::PlannerCompleted,
                json!({ "task_id": task.id }),
                correlation_id,
                last_event_id,
            )?;

            self.execute_task(&request, &mut task, correlation_id, last_event_id)
                .await?;
            return Ok(Submission { task_id: task.id, correlation_id });
        }

        // ---- MULTI-CAPABILITY DECOMPOSITION (Phase 2) ------------------
        let mut tasks = planned.into_iter();
        let mut parent = tasks.next().unwrap();
        let mut subtasks: Vec<Task> = tasks.collect();

        parent.transition_to(TaskState::Planned)?;
        self.storage.insert_task(&parent)?;
        last_event_id = self.emit_after(
            &request,
            EventType::TaskCreated,
            json!({
                "task_id": parent.id,
                "title": parent.title,
                "kind": "parent",
                "subtask_count": subtasks.len(),
                "required_capabilities": parent.required_capabilities,
            }),
            correlation_id,
            last_event_id,
        )?;
        last_event_id = self.emit_after(
            &request,
            EventType::PlannerCompleted,
            json!({ "task_id": parent.id, "subtask_ids": subtasks.iter().map(|t| t.id).collect::<Vec<_>>() }),
            correlation_id,
            last_event_id,
        )?;

        parent.transition_to(TaskState::Queued)?;
        parent.transition_to(TaskState::Running)?;
        if cancelled_while_planning(self.persist_task(&parent))? {
            return Ok(Submission { task_id: parent.id, correlation_id });
        }

        // The last point with one well-defined predecessor before
        // subtasks branch off and run independently/concurrently, used
        // to root each subtask's own event chain, and later cited as
        // the parent's completion event's cause, since no single
        // subtask chain can honestly claim to be *the* cause once
        // there's more than one of them running at once.
        let fan_out_event_id = last_event_id;
        let mut subtask_roots: Vec<Uuid> = Vec::with_capacity(subtasks.len());
        let mut creation_cursor = fan_out_event_id;
        for subtask in subtasks.iter_mut() {
            subtask.transition_to(TaskState::Planned)?;
            self.storage.insert_task(subtask)?;
            creation_cursor = self.emit_after(
                &request,
                EventType::TaskCreated,
                json!({
                    "task_id": subtask.id,
                    "title": subtask.title,
                    "parent_task_id": parent.id,
                    "required_capabilities": subtask.required_capabilities,
                }),
                correlation_id,
                creation_cursor,
            )?;
            subtask_roots.push(creation_cursor);
        }

        // Run every subtask's pipeline concurrently from here, this is
        // what actually exercises the resource-aware scheduler (see
        // solexa-resources): two subtasks whose models don't both fit
        // the configured budget genuinely serialize on
        // `ResourceManager::acquire`, rather than just happening to run
        // one after another because the coordinator looped over them in
        // order. No OS-thread parallelism is implied or required here,
        // this is cooperative concurrency within one task, which is
        // exactly what's needed to prove the scheduling logic (queuing,
        // blocking, waking) actually works.
        let outcomes = join_all(
            subtasks
                .iter_mut()
                .zip(subtask_roots)
                .map(|(subtask, root_event_id)| self.execute_task(&request, subtask, correlation_id, root_event_id)),
        )
        .await;
        for outcome in outcomes {
            outcome?;
        }

        // ---- COMBINE SUBTASK RESULTS INTO THE PARENT -------------------
        let all_completed = subtasks.iter().all(|t| t.state == TaskState::Completed);
        let combined = serde_json::Value::Array(
            subtasks
                .iter()
                .map(|t| {
                    json!({
                        "subtask_id": t.id,
                        "capabilities": t.required_capabilities,
                        "state": t.state,
                        "specialist_id": t.assigned_specialist_id,
                        "result": t.result.as_ref().map(|r| r.content.clone()),
                    })
                })
                .collect(),
        );
        // Sentinel ids: this result was assembled from N different
        // specialists/models, not produced by a single one, so there is
        // no single id that honestly belongs here. `Uuid::nil()` marks
        // that explicitly rather than picking one subtask's id and
        // implying it did all the work.
        parent.result = Some(TaskResult {
            content: combined,
            produced_by_specialist_id: Uuid::nil(),
            produced_by_model_id: Uuid::nil(),
            tool_calls: Vec::new(),
        });

        if all_completed {
            parent.transition_to(TaskState::Review)?;
            parent.transition_to(TaskState::Verified)?;
            parent.transition_to(TaskState::Completed)?;
            if cancelled_while_planning(self.persist_task(&parent))? {
                return Ok(Submission { task_id: parent.id, correlation_id });
            }
            self.emit_after(
                &request,
                EventType::TaskCompleted,
                json!({ "task_id": parent.id }),
                correlation_id,
                fan_out_event_id,
            )?;
        } else {
            parent.transition_to(TaskState::Review)?;
            if cancelled_while_planning(self.persist_task(&parent))? {
                return Ok(Submission { task_id: parent.id, correlation_id });
            }
            self.emit_after(
                &request,
                EventType::Escalated,
                json!({
                    "task_id": parent.id,
                    "reason": "one or more subtasks did not complete cleanly",
                    "subtask_states": subtasks.iter().map(|t| (t.id, t.state)).collect::<std::collections::HashMap<_, _>>(),
                }),
                correlation_id,
                fan_out_event_id,
            )?;
        }

        Ok(Submission { task_id: parent.id, correlation_id })
    }

    /// The per-task pipeline: capability matching -> specialist
    /// assignment -> context package -> model execution (once per
    /// matching specialist) -> agreement check -> verification ->
    /// accept/revise/escalate, or a `Conflict` if matching specialists
    /// disagree. Used directly for a single-capability request, and
    /// once per subtask when the planner decomposes a request (Phase
    /// 2). Mutates `task` in place and returns the id of the last event
    /// emitted, so callers can keep chaining causation after it returns.
    async fn execute_task(
        &self,
        request: &UserRequest,
        task: &mut Task,
        correlation_id: Uuid,
        last_event_id: Uuid,
    ) -> Result<Uuid> {
        // Round 53: from here to the end of this call, `cancel_task` can
        // interrupt this task's in-flight model call.
        let _signal = SignalRegistration::new(&self.signals, &self.resources, task.id);
        match self.execute_task_inner(request, task, correlation_id, last_event_id).await {
            Err(err) if err.downcast_ref::<TaskCancelled>().is_some() => {
                // `cancel_task` already persisted Cancelled; make the
                // in-memory copy this call chain holds agree, so no
                // later write in the caller can resurrect it.
                if task.state != TaskState::Cancelled {
                    task.transition_to(TaskState::Cancelled)?;
                }
                self.persist_task(task)?;
                Ok(last_event_id)
            }
            other => other,
        }
    }

    async fn execute_task_inner(
        &self,
        request: &UserRequest,
        task: &mut Task,
        correlation_id: Uuid,
        mut last_event_id: Uuid,
    ) -> Result<Uuid> {
        // ---- CAPABILITY MATCHING / PERMISSION CHECK / SPECIALIST ASSIGNMENT --
        // Spec rule (section 15, security): "Never give every model
        // unrestricted... access. Tools must be capabilities granted by
        // policy." A specialist advertising a capability isn't
        // automatically authorized to act on it, `Permission::Execute`
        // is the gate. Distinguishing "nobody has this capability" from
        // "someone has it but isn't authorized" matters for the same
        // reason the rest of this pipeline insists on saying *why*
        // something happened, not just *that* it did.
        let specialists = self.storage.list_specialists(request.project_id)?;
        let roles_by_id: HashMap<Uuid, Role> = self.storage.list_roles(request.project_id)?.into_iter().map(|r| (r.id, r)).collect();
        let capable: Vec<&Specialist> = specialists
            .iter()
            .filter(|s| s.active && task.required_capabilities.iter().any(|cap| s.has_capability(cap)))
            .collect();
        let mut matching: Vec<&Specialist> = capable
            .iter()
            .filter(|s| specialist_grants(s, &roles_by_id, Permission::Execute))
            .copied()
            .collect();

        // Phase 8: rank eligible specialists by measured track record
        // (spec section 13: "must not rank purely by model size",
        // verification pass rate is exactly the "measurable
        // performance" it calls for instead). This only changes which
        // one is picked as `primary` (the task's owner-of-record) when
        // there's a choice, it does NOT change who gets consulted in
        // a Phase 5 consensus round; every eligible specialist is still
        // consulted regardless of rank. A stable sort keeps ties (e.g.
        // two specialists with no history yet, both defaulting to 0.5)
        // in their original order, so this stays deterministic.
        let benchmark_scores: std::collections::HashMap<Uuid, f32> = matching
            .iter()
            .map(|s| Ok((s.id, self.storage.get_benchmark(s.id)?.success_rate())))
            .collect::<Result<_>>()?;
        matching.sort_by(|a, b| {
            benchmark_scores[&b.id]
                .partial_cmp(&benchmark_scores[&a.id])
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let Some(primary) = matching.first().copied() else {
            // No eligible, authorized specialist: make this a visible,
            // inspectable stopping point rather than an error the
            // caller has to catch. A human can register a matching
            // specialist (or grant Execute to an existing one) and
            // re-run.
            task.transition_to(TaskState::Queued)?;
            task.transition_to(TaskState::Blocked)?;
            self.persist_task(task)?;
            let reason = if capable.is_empty() {
                "no registered specialist advertises any required capability".to_string()
            } else {
                format!(
                    "{} registered specialist(s) advertise a required capability but lack Permission::Execute",
                    capable.len()
                )
            };
            return self.emit_after(
                request,
                EventType::TaskBlocked,
                json!({
                    "task_id": task.id,
                    "reason": reason,
                    "required_capabilities": task.required_capabilities,
                    "capable_but_unauthorized_specialist_ids": capable.iter().map(|s| s.id).collect::<Vec<_>>(),
                }),
                correlation_id,
                last_event_id,
            );
        };

        // `primary` (the first match) decides the task's nominal owner
        // regardless of mode, Phase 6's ownership model needs exactly one
        // accountable member even when several specialists were consulted
        // for a second opinion. Round 64: the owner is that specialist's
        // *member* (`Task::owner_id` is documented as a member id); this used
        // to store the specialist's own id there, which no member row matches.
        let owner_member_id = self.owner_member_for(request.project_id, primary.id)?;
        task.transition_to(TaskState::Queued)?;
        task.transition_to(TaskState::Assigned)?;
        task.assigned_specialist_id = Some(primary.id);
        task.owner_id = owner_member_id;
        self.persist_task(task)?;
        last_event_id = self.emit_after(
            request,
            EventType::TaskAssigned,
            json!({
                "task_id": task.id,
                "specialist_id": primary.id,
                "specialist_name": primary.name,
                "owner_member_id": owner_member_id,
                "mode": if matching.len() > 1 { "consensus" } else { "single" },
                "candidate_specialist_ids": matching.iter().map(|s| s.id).collect::<Vec<_>>(),
            }),
            correlation_id,
            last_event_id,
        )?;

        // ---- CONTEXT PACKAGE -------------------------------------------
        // Phase 4: scope in whatever the Project Brain actually has
        // that's tagged relevant to this task's required capabilities
        // (see solexa-brain). Computed once, every consulted specialist
        // in a consensus round shares the same context package.
        let knowledge = self.storage.list_knowledge_items(request.project_id)?;
        let decisions = self.storage.list_decisions(request.project_id)?;
        let context = solexa_brain::package_context(&knowledge, &decisions, &task.required_capabilities);
        // Round 59: each specialist re-packages from these with its own
        // `knowledge_scope`; the package stored on the task stays the
        // unscoped one (everything the task could be given).
        let required_capabilities = task.required_capabilities.clone();
        let brain = BrainView { knowledge: &knowledge, decisions: &decisions, required_capabilities: &required_capabilities };
        task.context_package = Some(context.clone());
        self.persist_task(task)?;
        last_event_id = self.emit_after(
            request,
            EventType::ContextPackaged,
            json!({
                "task_id": task.id,
                "excluded_summary": context.excluded_summary,
                "included_facts": context.included_facts.len(),
                "included_constraints": context.included_constraints.len(),
                "included_artifacts": context.included_artifacts.len(),
                "included_decisions": context.included_decisions.len(),
            }),
            correlation_id,
            last_event_id,
        )?;

        task.transition_to(TaskState::Running)?;
        self.persist_task(task)?;

        if matching.len() == 1 {
            // ---- SINGLE SPECIALIST (Phases 1-4, unchanged) --------------
            let (result, verification, confidence, next_event_id) = self
                .run_specialist(request, task.id, &task.description, task.priority, task.deadline, primary, &brain, &roles_by_id, correlation_id, last_event_id)
                .await?;
            last_event_id = next_event_id;
            return self
                .finalize_task(request, task, primary, result, verification, confidence, correlation_id, last_event_id)
                .await;
        }

        // ---- CONSENSUS: consult every matching specialist (Phase 5) ---
        // Run concurrently for the same reason Phase 3's subtasks do,
        // it's what actually exercises the resource-aware scheduler
        // when two candidate specialists share a model, and there's no
        // reason to make a human wait for N sequential model calls when
        // N concurrent ones tell the same story.
        let outcomes: Vec<(TaskResult, solexa_domain::VerificationOutcome, Confidence, Uuid)> = join_all(
            matching
                .iter()
                .map(|specialist| self.run_specialist(request, task.id, &task.description, task.priority, task.deadline, specialist, &brain, &roles_by_id, correlation_id, last_event_id)),
        )
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()?;

        let first_content = &outcomes[0].0.content;
        let all_agree = outcomes.iter().all(|(result, _, _, _)| &result.content == first_content);

        if all_agree {
            // Every specialist reached the same answer, Phase 5's
            // "consensus" case. Proceed exactly as the single-specialist
            // path would, using the first (arbitrary among identical)
            // outcome.
            let (result, verification, confidence, _) = outcomes.into_iter().next().unwrap();
            return self
                .finalize_task(request, task, primary, result, verification, confidence, correlation_id, last_event_id)
                .await;
        }

        // Genuine disagreement: per spec section 9 ("Disagreement Is A
        // Feature"), never silently overwrite one position with
        // another. Preserve all of them and stop for a human.
        let positions: Vec<ConflictPosition> = outcomes
            .iter()
            .zip(matching.iter())
            .map(|((result, verification, _, _), specialist)| ConflictPosition {
                specialist_id: specialist.id,
                specialist_name: specialist.name.clone(),
                model_id: Some(result.produced_by_model_id),
                content: result.content.clone(),
                verification_passed: Some(verification.passed),
            })
            .collect();
        let conflict = Conflict::new(
            request.project_id,
            task.id,
            format!("Specialists disagree on the result for: {}", task.title),
            positions,
        );
        self.storage.insert_conflict(&conflict)?;

        task.transition_to(TaskState::Review)?;
        self.persist_task(task)?;
        last_event_id = self.emit_after(
            request,
            EventType::ConflictDetected,
            json!({
                "task_id": task.id,
                "conflict_id": conflict.id,
                "specialist_ids": matching.iter().map(|s| s.id).collect::<Vec<_>>(),
            }),
            correlation_id,
            last_event_id,
        )?;

        Ok(last_event_id)
    }

    /// One specialist's full attempt at a task: model selection,
    /// resource acquisition, generation, release, and verification.
    /// Deliberately takes `task_id`/`task_description` by value rather
    /// than `&mut Task`, when called once per candidate specialist in
    /// a consensus round (Phase 5), several of these run concurrently
    /// against the *same* task, which a shared `&mut Task` can't allow.
    /// Callers decide what to do with the result; this never mutates
    /// task state itself.
    async fn run_specialist(
        &self,
        request: &UserRequest,
        task_id: Uuid,
        task_description: &str,
        // Round 36: passed by value alongside the other task scalars
        // (same reason, two concurrent attempts against one task
        // can't share a `&mut Task`) so `ResourceManager::acquire` can
        // order hardware waits by it.
        task_priority: u8,
        // Round 44: same reasoning, for the same call, `DateTime<Utc>`
        // here since that's what's stored; converted to an `Instant`
        // right before the `acquire` call below, not any earlier,
        // since that conversion is only meaningful "as of now."
        task_deadline: Option<chrono::DateTime<chrono::Utc>>,
        specialist: &Specialist,
        // Round 58: the Project Brain's contents for this task and the
        // project's roles (to resolve role-derived `Read`). The context
        // reaches the model ONLY through the system prompt, ONLY when the
        // specialist holds `Permission::Read`. Round 59: packaged here, per
        // specialist, so its `knowledge_scope` can narrow it.
        brain: &BrainView<'_>,
        roles_by_id: &HashMap<Uuid, Role>,
        correlation_id: Uuid,
        mut last_event_id: Uuid,
    ) -> Result<(TaskResult, solexa_domain::VerificationOutcome, Confidence, Uuid)> {
        // Round 41: routed against this project's available models, not
        // every model any provider in the install happens to report,
        // a model this project has explicitly disabled is skipped even
        // if it's still in the specialist's own `preferred_model_ids`.
        let available = self.models_available_to(request.project_id)?;
        let available_ids: HashSet<Uuid> = available.iter().map(|m| m.id).collect();
        let model_id = specialist
            .preferred_model_ids
            .iter()
            .find(|id| available_ids.contains(id) && self.providers.provider_for(**id).is_some())
            .copied()
            .or_else(|| available.first().map(|m| m.id))
            .ok_or_else(|| anyhow!("no model available for specialist {}", specialist.name))?;
        let provider = self
            .providers
            .provider_for(model_id)
            .ok_or_else(|| anyhow!("model {} has no registered provider", model_id))?;
        let model_meta = available
            .into_iter()
            .find(|m| m.id == model_id)
            .ok_or_else(|| anyhow!("model {} has no registered metadata", model_id))?;

        if !self.resources.fits_in_budget(&model_meta) {
            // Not a normal business-flow outcome (unlike "no matching
            // specialist"), this means the configured hardware budget
            // is smaller than a model's own footprint, so no amount of
            // queuing would ever let it run. That's a configuration
            // problem to surface loudly, not a task state to leave
            // sitting in Blocked forever.
            return Err(anyhow!(
                "model {} requires {}MB VRAM / {}MB RAM, which exceeds the configured resource budget ({:?}) and can never be scheduled",
                model_meta.name,
                model_meta.estimated_vram_mb,
                model_meta.estimated_ram_mb,
                self.resources.budget(),
            ));
        }

        let wait_started = std::time::Instant::now();
        // Round 36: the task's own priority decides who gets freed
        // capacity when several subtasks are queued on hardware at
        // once, this is the call that finally makes `Task.priority`
        // mean something, after many rounds of it being stored and
        // settable but read by nothing.
        //
        // Round 44: `task_deadline` (wall-clock, `DateTime<Utc>`)
        // converts to a monotonic `Instant` right here, "as of now",
        // converting any earlier would attach a stale offset if this
        // task waited a while just to reach this line. A deadline
        // already in the past converts to "now" (maximally urgent)
        // rather than being dropped to `None` (which `acquire` treats
        // as *least* urgent), `to_std()` only fails on a negative
        // duration, i.e. exactly the overdue case.
        let deadline_instant = task_deadline.map(|d| {
            let now_utc = chrono::Utc::now();
            match (d - now_utc).to_std() {
                Ok(remaining) => wait_started + remaining,
                Err(_) => wait_started,
            }
        });
        let guard = self
            .resources
            .acquire_cancellable(&model_meta, task_priority, deadline_instant, Some(task_id))
            .await
            .map_err(|e| match e {
                solexa_resources::AcquireError::Cancelled => anyhow::Error::new(TaskCancelled(task_id)),
                // Round 60: the budget was resized while this task waited. A
                // failure to surface, not a cancellation to swallow.
                solexa_resources::AcquireError::ExceedsBudget => anyhow!(
                    "model {} requires {}MB VRAM / {}MB RAM, which exceeds the resource budget ({:?}), the budget was resized while this task was waiting, so it can never be scheduled",
                    model_meta.name,
                    model_meta.estimated_vram_mb,
                    model_meta.estimated_ram_mb,
                    self.resources.budget(),
                ),
            })?;
        let waited_ms = wait_started.elapsed().as_millis() as u64;
        let snapshot = self.resources.snapshot();
        last_event_id = self.emit_after(
            request,
            EventType::ResourceAcquired,
            json!({
                "task_id": task_id,
                "specialist_id": specialist.id,
                "model_id": model_id,
                "vram_reserved_mb": model_meta.estimated_vram_mb,
                "ram_reserved_mb": model_meta.estimated_ram_mb,
                "waited_ms": waited_ms,
                "budget_used_vram_mb": snapshot.used_vram_mb,
                "budget_used_ram_mb": snapshot.used_ram_mb,
                "budget_running": snapshot.running,
            }),
            correlation_id,
            last_event_id,
        )?;

        // Round 58: render the Project Brain context into the system prompt.
        // `Permission::Read` finally gates something: a specialist without it
        // (directly or through a role) is given no project knowledge. With no
        // relevant knowledge the prompt is byte-identical to before.
        let context_text = if specialist_grants(specialist, roles_by_id, Permission::Read) {
            solexa_brain::package_context_scoped(brain.knowledge, brain.decisions, brain.required_capabilities, &specialist.knowledge_scope)
                .render_for_prompt(solexa_domain::DEFAULT_CONTEXT_CHARS)
        } else {
            None
        };
        let mut system_prompt = format!(
            "You are the {}, {} specialist. Domain: {}.",
            specialist.role, specialist.name, specialist.domain
        );
        if let Some(text) = &context_text {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(text);
        }

        last_event_id = self.emit_after(
            request,
            EventType::ModelInvoked,
            json!({
                "task_id": task_id,
                "specialist_id": specialist.id,
                "model_id": model_id,
                "context_delivered": context_text.is_some(),
                "context_scoped": specialist.knowledge_scope.iter().any(|t| !t.trim().is_empty()),
                "context_chars": context_text.as_ref().map(|t| t.chars().count()).unwrap_or(0),
            }),
            correlation_id,
            last_event_id,
        )?;

        let generation = self
            .stream_generation(
                &provider,
                GenerationRequest {
                    model_id,
                    system_prompt,
                    prompt: task_description.to_string(),
                    // Round 52: the specialist's own limits. `None` (the
                    // default) is exactly the pre-round-52 request.
                    max_tokens: specialist.effective_max_tokens(),
                    temperature: specialist.effective_temperature(),
                },
                request,
                task_id,
                json!({ "specialist_id": specialist.id, "model_id": model_id }),
                correlation_id,
                last_event_id,
            )
            .await
            .context("model provider generation failed")?;

        // Round 52: cut off by THIS specialist's own limit. Deliberately
        // requires the limit to be set: a runtime hitting its own context
        // ceiling with no configured limit still reports `Length`, but
        // flagging that would change behavior for every specialist that
        // never opted in, this stays opt-in.
        let truncated_by_limit = specialist.effective_max_tokens().is_some() && generation.finish_reason == FinishReason::Length;

        // Release right away, the whole point of a resource-aware
        // scheduler is not holding hardware longer than the generation
        // itself needs.
        drop(guard);
        last_event_id = self.emit_after(
            request,
            EventType::ResourceReleased,
            json!({ "task_id": task_id, "specialist_id": specialist.id, "model_id": model_id }),
            correlation_id,
            last_event_id,
        )?;
        last_event_id = self.emit_after(
            request,
            EventType::ModelCompleted,
            json!({
                "task_id": task_id,
                "specialist_id": specialist.id,
                "estimated_tokens": generation.estimated_tokens,
                "finish_reason": generation.finish_reason,
                "truncated_by_limit": truncated_by_limit,
            }),
            correlation_id,
            last_event_id,
        )?;

        let result = TaskResult {
            content: serde_json::Value::String(generation.text),
            produced_by_specialist_id: specialist.id,
            produced_by_model_id: model_id,
            tool_calls: Vec::new(),
        };

        last_event_id = self.emit_after(
            request,
            EventType::ResultSubmitted,
            json!({ "task_id": task_id, "specialist_id": specialist.id }),
            correlation_id,
            last_event_id,
        )?;

        // ---- VERIFICATION -------------------------------------------------
        last_event_id = self.emit_after(
            request,
            EventType::VerificationStarted,
            json!({ "task_id": task_id, "specialist_id": specialist.id }),
            correlation_id,
            last_event_id,
        )?;
        // A partial result is not checked as if it were complete: running
        // the verifier on cut-off output would be meaningless at best and
        // a false pass at worst (a truncated string is still non-empty).
        let verification = if truncated_by_limit {
            solexa_domain::VerificationOutcome {
                method: solexa_domain::VerificationMethod::DeterministicRule,
                passed: false,
                details: format!(
                    "output was cut off at this specialist's {} token limit before it finished, so it was not accepted as complete",
                    specialist.max_tokens.unwrap_or(0)
                ),
                verified_at: chrono::Utc::now(),
            }
        } else {
            self.verifier.verify(specialist, &result)
        };
        // Phase 8: every real attempt updates this specialist's
        // measured track record, win or lose, recorded here (once per
        // specialist per attempt) rather than in `finalize_task` (once
        // per task overall), so a specialist's history reflects its own
        // performance even when it was one of several consulted in a
        // consensus round and didn't end up as the accepted result.
        self.storage.record_benchmark_outcome(
            specialist.id,
            if verification.passed {
                solexa_domain::BenchmarkOutcome::VerificationPassed
            } else {
                solexa_domain::BenchmarkOutcome::VerificationFailed
            },
        )?;
        last_event_id = self.emit_after(
            request,
            EventType::Verified,
            json!({
                "task_id": task_id,
                "specialist_id": specialist.id,
                "method": verification.method,
                "passed": verification.passed,
                "details": verification.details,
            }),
            correlation_id,
            last_event_id,
        )?;

        // Phase 1 has no specialist self-reported confidence (that
        // requires a real model capable of introspecting its own
        // answer). Deriving it from the verification outcome is an
        // honest, explicit stand-in: it's a signal, not a claim that a
        // model assessed itself.
        let confidence = Confidence {
            knowledge: ConfidenceLevel::Medium,
            reasoning: ConfidenceLevel::Medium,
            evidence: if verification.passed { ConfidenceLevel::Medium } else { ConfidenceLevel::Low },
            requirement_coverage: ConfidenceLevel::Medium,
            tool_reliability: ConfidenceLevel::High,
            completeness: if verification.passed { ConfidenceLevel::High } else { ConfidenceLevel::Low },
        };

        Ok((result, verification, confidence, last_event_id))
    }

    /// Applies one specialist's already-computed result to `task` and
    /// runs accept/revise/escalate. Shared by the single-specialist path
    /// and the consensus path once it has settled on one outcome
    /// (either because only one specialist was consulted, or because
    /// every consulted specialist agreed).
    #[allow(clippy::too_many_arguments)]
    async fn finalize_task(
        &self,
        request: &UserRequest,
        task: &mut Task,
        specialist: &Specialist,
        result: TaskResult,
        verification: solexa_domain::VerificationOutcome,
        confidence: Confidence,
        correlation_id: Uuid,
        mut last_event_id: Uuid,
    ) -> Result<Uuid> {
        task.result = Some(result);
        task.transition_to(TaskState::Review)?;
        task.verification = Some(verification.clone());
        task.confidence = Some(confidence.clone());
        self.persist_task(task)?;

        if !verification.passed {
            task.transition_to(TaskState::Revision)?;
            self.persist_task(task)?;
            last_event_id = self.emit_after(
                request,
                EventType::RevisionRequested,
                json!({ "task_id": task.id, "reason": verification.details }),
                correlation_id,
                last_event_id,
            )?;
            if specialist.escalation_policy.escalate_on_verification_failure {
                last_event_id = self.emit_after(
                    request,
                    EventType::Escalated,
                    json!({ "task_id": task.id, "reason": "verification failed" }),
                    correlation_id,
                    last_event_id,
                )?;
            }
        } else if confidence.has_weak_dimension() && specialist.escalation_policy.escalate_on_weak_confidence {
            last_event_id = self.emit_after(
                request,
                EventType::Escalated,
                json!({ "task_id": task.id, "reason": "weak confidence dimension after verification" }),
                correlation_id,
                last_event_id,
            )?;
        } else if specialist.escalation_policy.requires_model_review
            || specialist.escalation_policy.requires_specialist_review
        {
            // Deterministic verification passed and confidence looks
            // fine, but this specialist opted into one or both extra,
            // independent review passes. Spec's verification priority
            // list ranks tier 6 (specialist review) as stronger than
            // tier 7 (model review), so when both are opted into,
            // specialist review runs first; either can reject on its
            // own and send the task to Revision without the other ever
            // running.
            let mut approved = true;
            let mut rejection_reason = String::new();

            if specialist.escalation_policy.requires_specialist_review {
                let (verdict, review_text, next_event_id) = self
                    .specialist_review(request, task, specialist, task.result.as_ref().unwrap(), correlation_id, last_event_id)
                    .await?;
                last_event_id = next_event_id;
                if verdict != ReviewVerdict::Approved {
                    approved = false;
                    rejection_reason = review_rejection_reason("specialist review", verdict, &review_text);
                }
            }

            if approved && specialist.escalation_policy.requires_model_review {
                let (verdict, review_text, next_event_id) = self
                    .model_review(request, task.id, task.result.as_ref().unwrap(), correlation_id, last_event_id)
                    .await?;
                last_event_id = next_event_id;
                if verdict != ReviewVerdict::Approved {
                    approved = false;
                    rejection_reason = review_rejection_reason("model review", verdict, &review_text);
                }
            }

            if approved {
                task.transition_to(TaskState::Verified)?;
                task.transition_to(TaskState::Completed)?;
                self.persist_task(task)?;
                last_event_id = self.emit_after(
                    request,
                    EventType::TaskCompleted,
                    json!({ "task_id": task.id }),
                    correlation_id,
                    last_event_id,
                )?;
            } else {
                task.transition_to(TaskState::Revision)?;
                self.persist_task(task)?;
                last_event_id = self.emit_after(
                    request,
                    EventType::RevisionRequested,
                    json!({ "task_id": task.id, "reason": rejection_reason }),
                    correlation_id,
                    last_event_id,
                )?;
            }
        } else {
            task.transition_to(TaskState::Verified)?;
            task.transition_to(TaskState::Completed)?;
            self.persist_task(task)?;
            last_event_id = self.emit_after(
                request,
                EventType::TaskCompleted,
                json!({ "task_id": task.id }),
                correlation_id,
                last_event_id,
            )?;
        }

        Ok(last_event_id)
    }

    /// Sends an already-accepted (deterministic-check-passed) result to
    /// an independent reviewing model, per a specialist's
    /// `requires_model_review` opt-in. Picks any registered model
    /// *different* from the one that produced the result when one
    /// exists, an honest limitation, not a silent one: falling back to
    /// reviewing with the same model is noted in the emitted event's
    /// `same_model_as_generation` field rather than hidden.
    async fn model_review(
        &self,
        request: &UserRequest,
        task_id: Uuid,
        result: &TaskResult,
        correlation_id: Uuid,
        mut last_event_id: Uuid,
    ) -> Result<(ReviewVerdict, String, Uuid)> {
        // Round 41: routed against this project's available models, a
        // model this project has disabled is never picked as a
        // reviewer, even as the "reuse the generation model" fallback.
        let all_models = self.models_available_to(request.project_id)?;
        let reviewer_model = all_models
            .iter()
            .find(|m| m.id != result.produced_by_model_id)
            .or_else(|| all_models.iter().find(|m| m.id == result.produced_by_model_id))
            .ok_or_else(|| anyhow!("no model available to perform model review"))?;
        let reviewer_model_id = reviewer_model.id;
        let same_model_as_generation = reviewer_model_id == result.produced_by_model_id;
        let provider = self
            .providers
            .provider_for(reviewer_model_id)
            .ok_or_else(|| anyhow!("model {} has no registered provider", reviewer_model_id))?;

        last_event_id = self.emit_after(
            request,
            EventType::ModelReviewStarted,
            json!({ "reviewer_model_id": reviewer_model_id, "same_model_as_generation": same_model_as_generation }),
            correlation_id,
            last_event_id,
        )?;

        let content_text = match &result.content {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let review = {
            // Same spawn-and-drain shape as run_specialist (round 39),
            // factored into a shared helper (`stream_generation`) rather
            // than copy-pasted a third time.
            self.stream_generation(
                &provider,
                GenerationRequest {
                    model_id: reviewer_model_id,
                    system_prompt: "You are a review specialist checking another specialist's completed work for correctness.".into(),
                    prompt: format!(
                        "Task: {}\n\nResult to review:\n{}\n\nRespond with VERDICT: APPROVED or VERDICT: REJECTED plus a one-line reason.",
                        request.text, content_text,
                    ),
                                    max_tokens: Some(REVIEW_MAX_TOKENS),
                    temperature: None,
                },
                request,
                task_id,
                json!({ "reviewer_model_id": reviewer_model_id }),
                correlation_id,
                last_event_id,
            )
            .await
            .context("model review generation failed")?
        };

        let verdict = review_verdict(&review.text, review.finish_reason);
        last_event_id = self.emit_after(
            request,
            EventType::ModelReviewCompleted,
            json!({
                "approved": verdict == ReviewVerdict::Approved,
                "inconclusive": verdict == ReviewVerdict::Inconclusive,
                "finish_reason": review.finish_reason,
                "review_text": review.text,
            }),
            correlation_id,
            last_event_id,
        )?;

        Ok((verdict, review.text, last_event_id))
    }

    /// Sends an already-accepted result to a second, independent
    /// *specialist* for review, per a specialist's
    /// `requires_specialist_review` opt-in (spec's verification
    /// priority tier 6, one tier stronger than `model_review`'s tier
    /// 7, since the reviewer brings its own domain capabilities,
    /// `Permission::Review`, and track record rather than being an
    /// arbitrary model).
    ///
    /// The reviewer must: be `active` (an archived specialist is never
    /// a candidate, see `set_specialist_active`), not be the same
    /// specialist that produced the result (self-review isn't
    /// independent review), hold `Permission::Review`, and share at
    /// least one of the task's required capabilities (a domain-relevant
    /// reviewer, not an arbitrary one, an SQL specialist has no
    /// standing to review typography work). Among eligible candidates,
    /// the best-benchmarked one is picked, same preference `finalize_task`'s
    /// initial specialist selection uses elsewhere.
    ///
    /// Unlike `model_review`, which always has *some* model to fall
    /// back to (even the same one, honestly flagged), a genuinely
    /// unavailable specialist reviewer has no meaningful fallback, a
    /// specialist reviewing its own work isn't independent verification
    /// the way a different model still is. When no eligible reviewer
    /// exists, this skips the extra check entirely rather than blocking
    /// the task or silently promoting a same-specialist "review" as if
    /// it meant something: it emits `SpecialistReviewSkipped` and
    /// returns approved, leaving deterministic verification (which
    /// already passed, or this would never have been called) as the
    /// standing result, the same honesty standard `model_review`'s
    /// `same_model_as_generation` flag holds, adapted to a case where
    /// there truly is no fallback worth taking.
    #[allow(clippy::too_many_arguments)]
    async fn specialist_review(
        &self,
        request: &UserRequest,
        task: &Task,
        producing_specialist: &Specialist,
        result: &TaskResult,
        correlation_id: Uuid,
        mut last_event_id: Uuid,
    ) -> Result<(ReviewVerdict, String, Uuid)> {
        let roles_by_id: HashMap<Uuid, Role> = self.storage.list_roles(request.project_id)?.into_iter().map(|r| (r.id, r)).collect();
        let mut candidates: Vec<(Specialist, SpecialistBenchmark)> = self
            .storage
            .list_specialists(request.project_id)?
            .into_iter()
            .filter(|s| {
                s.active
                    && s.id != producing_specialist.id
                    && specialist_grants(s, &roles_by_id, Permission::Review)
                    && task.required_capabilities.iter().any(|cap| s.has_capability(cap))
            })
            .map(|s| {
                let benchmark = self
                    .storage
                    .get_benchmark(s.id)
                    .unwrap_or_else(|_| SpecialistBenchmark::empty(s.id));
                (s, benchmark)
            })
            .collect();
        candidates.sort_by(|a, b| b.1.success_rate().partial_cmp(&a.1.success_rate()).unwrap());

        let Some((reviewer_specialist, _)) = candidates.into_iter().next() else {
            last_event_id = self.emit_after(
                request,
                EventType::SpecialistReviewSkipped,
                json!({
                    "reason": "no other active specialist with Permission::Review and an overlapping capability was available",
                    "required_capabilities": task.required_capabilities,
                }),
                correlation_id,
                last_event_id,
            )?;
            return Ok((ReviewVerdict::Approved, "specialist review skipped, no eligible reviewer available; deterministic verification stands".into(), last_event_id));
        };

        // Round 41: same project-availability filter as run_specialist.
        let available = self.models_available_to(request.project_id)?;
        let available_ids: HashSet<Uuid> = available.iter().map(|m| m.id).collect();
        let model_id = reviewer_specialist
            .preferred_model_ids
            .iter()
            .find(|id| available_ids.contains(id) && self.providers.provider_for(**id).is_some())
            .copied()
            .or_else(|| available.first().map(|m| m.id))
            .ok_or_else(|| anyhow!("no model available for reviewer specialist {}", reviewer_specialist.name))?;
        let provider = self
            .providers
            .provider_for(model_id)
            .ok_or_else(|| anyhow!("model {} has no registered provider", model_id))?;

        last_event_id = self.emit_after(
            request,
            EventType::SpecialistReviewStarted,
            json!({
                "reviewer_specialist_id": reviewer_specialist.id,
                "reviewer_specialist_name": reviewer_specialist.name,
                "reviewer_model_id": model_id,
            }),
            correlation_id,
            last_event_id,
        )?;

        let content_text = match &result.content {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let review = self
            .stream_generation(
                &provider,
                GenerationRequest {
                    model_id,
                    system_prompt: format!(
                        "You are {}, a {} reviewing another specialist's completed work in your domain of {} for correctness.",
                        reviewer_specialist.name, reviewer_specialist.role, reviewer_specialist.domain,
                    ),
                    prompt: format!(
                        "Task: {}\n\nResult to review:\n{}\n\nRespond with VERDICT: APPROVED or VERDICT: REJECTED plus a one-line reason.",
                        request.text, content_text,
                    ),
                                    max_tokens: Some(REVIEW_MAX_TOKENS),
                    temperature: None,
                },
                request,
                task.id,
                json!({ "reviewer_specialist_id": reviewer_specialist.id, "reviewer_model_id": model_id }),
                correlation_id,
                last_event_id,
            )
            .await
            .context("specialist review generation failed")?;

        let verdict = review_verdict(&review.text, review.finish_reason);
        last_event_id = self.emit_after(
            request,
            EventType::SpecialistReviewCompleted,
            json!({
                "reviewer_specialist_id": reviewer_specialist.id,
                "approved": verdict == ReviewVerdict::Approved,
                "inconclusive": verdict == ReviewVerdict::Inconclusive,
                "finish_reason": review.finish_reason,
                "review_text": review.text,
            }),
            correlation_id,
            last_event_id,
        )?;

        Ok((verdict, review.text, last_event_id))
    }

    /// Resolves an open `Conflict` by accepting one participating
    /// specialist's position as the task's final result. `approving_member_id`
    /// must be a `Member` already registered in the project (Phase 6:
    /// approval is tracked as a real accountable participant, not a
    /// free-form string anyone could type). The losing position(s) are
    /// never deleted, they stay on the (now Resolved) conflict record,
    /// preserved exactly as the spec requires.
    pub fn resolve_conflict(
        &self,
        conflict_id: Uuid,
        chosen_specialist_id: Uuid,
        approving_member_id: Uuid,
    ) -> Result<()> {
        let _entity_gate = self.entity_gate();
        let approver = self
            .storage
            .get_member(approving_member_id)?
            .ok_or_else(|| anyhow!("member {approving_member_id} is not registered in this project, only a registered Member can resolve a conflict"))?;

        let mut conflict = self
            .storage
            .get_conflict(conflict_id)?
            .ok_or_else(|| anyhow!("conflict {conflict_id} not found"))?;
        self.require_approver_permission(conflict.project_id, approving_member_id, "resolve a conflict")?;
        let position = conflict
            .position_for(chosen_specialist_id)
            .cloned()
            .ok_or_else(|| anyhow!("specialist {chosen_specialist_id} did not participate in conflict {conflict_id}"))?;

        let task_id = conflict.task_id.ok_or_else(|| {
            anyhow!("conflict {conflict_id} has no task_id, this is a sync conflict; resolve it with resolve_sync_conflict instead")
        })?;
        let mut task = self
            .storage
            .get_task(task_id)?
            .ok_or_else(|| anyhow!("task {} not found", task_id))?;
        // Round 57: everything that can fail is checked BEFORE the first
        // write. This used to mark the conflict Resolved first and only then
        // discover (on the task's state transition) that the task had been
        // cancelled, an error to the user and a half-applied change on disk.
        if task.state == TaskState::Cancelled {
            return Err(anyhow!("task {task_id} was cancelled, so conflict {conflict_id} can't be resolved into a result"));
        }
        task.result = Some(TaskResult {
            content: position.content.clone(),
            produced_by_specialist_id: position.specialist_id,
            produced_by_model_id: position.model_id.expect("task-execution conflict positions always carry a model_id"),
            tool_calls: Vec::new(),
        });
        conflict.resolve(
            format!("Accepted {}'s result", position.specialist_name),
            approver.display_name.clone(),
        );
        task.verification = Some(solexa_domain::VerificationOutcome {
            method: solexa_domain::VerificationMethod::HumanReview,
            passed: position.verification_passed.expect("task-execution conflict positions always carry a verification_passed value"),
            details: format!("Resolved via human conflict resolution: {}", conflict.resolution.clone().unwrap_or_default()),
            verified_at: chrono::Utc::now(),
        });
        task.transition_to(TaskState::Verified)?;
        task.transition_to(TaskState::Completed)?;
        // The task goes through `persist_task` (round 53's guarded write),
        // so a cancel that landed after the read above is refused rather
        // than overwritten, and, being first, leaves the conflict Open.
        if let Err(err) = self.persist_task(&task) {
            return Err(if err.downcast_ref::<TaskCancelled>().is_some() {
                anyhow!("task {task_id} was cancelled, so conflict {conflict_id} can't be resolved into a result")
            } else {
                err
            });
        }
        self.storage.update_conflict(&conflict)?;

        let correlation_id = Uuid::new_v4();
        let resolved_event_id = self.emit(
            task.project_id,
            &approver.display_name,
            ActorType::Human,
            EventType::ConflictResolved,
            json!({
                "conflict_id": conflict.id,
                "task_id": task.id,
                "chosen_specialist_id": chosen_specialist_id,
                "approved_by_member_id": approver.id,
            }),
            correlation_id,
            None,
        )?;
        self.emit(
            task.project_id,
            "coordinator",
            ActorType::System,
            EventType::TaskCompleted,
            json!({ "task_id": task.id }),
            correlation_id,
            Some(resolved_event_id),
        )?;

        Ok(())
    }

    pub fn list_conflicts(&self, project_id: Uuid) -> Result<Vec<Conflict>> {
        self.storage.list_conflicts(project_id)
    }

    pub fn get_conflict(&self, conflict_id: Uuid) -> Result<Option<Conflict>> {
        self.storage.get_conflict(conflict_id)
    }


    fn emit(
        &self,
        project_id: Uuid,
        actor_id: &str,
        actor_type: ActorType,
        event_type: EventType,
        payload: serde_json::Value,
        correlation_id: Uuid,
        causation_id: Option<Uuid>,
    ) -> Result<Uuid> {
        let event = Event::new(
            project_id,
            actor_id,
            actor_type,
            event_type,
            payload,
            correlation_id,
            causation_id,
        );
        let event_id = event.event_id;
        self.events.publish(event)?;
        Ok(event_id)
    }

    /// Convenience wrapper: every step after the first is a `System`
    /// actor caused by the previous step, which is the common case in
    /// this loop.
    fn emit_after(
        &self,
        request: &UserRequest,
        event_type: EventType,
        payload: serde_json::Value,
        correlation_id: Uuid,
        caused_by: Uuid,
    ) -> Result<Uuid> {
        self.emit(
            request.project_id,
            "coordinator",
            ActorType::System,
            event_type,
            payload,
            correlation_id,
            Some(caused_by),
        )
    }

    /// Round 39's ephemeral counterpart to `emit_after`: publishes via
    /// `EventBus::publish_ephemeral` instead of `publish`, so it never
    /// reaches a durable sink (see that method's own doc comment for
    /// why). Deliberately returns `Result<()>`, not `Result<Uuid>`,
    /// an ephemeral event's id has nothing to be `caused_by` later,
    /// since it's never durable history a future event could point
    /// back to. `caused_by` here is always the *last durable* event
    /// (e.g. `ModelInvoked`), and stays that same value across every
    /// ephemeral event this call site emits, rather than chaining
    /// causation from one ephemeral event to the next.
    fn emit_ephemeral(
        &self,
        request: &UserRequest,
        event_type: EventType,
        payload: serde_json::Value,
        correlation_id: Uuid,
        caused_by: Uuid,
    ) -> Result<()> {
        self.events.publish_ephemeral(Event::new(
            request.project_id,
            "coordinator",
            ActorType::System,
            event_type,
            payload,
            correlation_id,
            Some(caused_by),
        ))
    }

    /// Shared by `run_specialist`, `model_review`, and
    /// `specialist_review` (round 40 factored this out of `
    /// run_specialist`'s own round-39 body, which was on its way to
    /// being duplicated a third time): spawns
    /// `provider.generate_streaming`, draining the resulting channel of
    /// chunks into one ephemeral `ModelTokenGenerated` event per chunk
    /// *while generation is still in progress* spawned rather than
    /// plainly awaited, since a caller can only ever see chunks after
    /// the fact otherwise, which would make "streaming" pointless for a
    /// live trace view, then awaits the final `GenerationResult`.
    ///
    /// `extra_payload` must be a JSON object; it supplies whatever
    /// identifies *what's generating* to the caller (a specialist and
    /// model for `run_specialist`; a reviewer model or reviewer
    /// specialist for the two review paths), merged with the
    /// `task_id`/`chunk`/`chunk_index` fields common to all three
    /// callers' token events. Every token event shares `caused_by`,
    /// the caller's own just-emitted `*Started` event, rather than
    /// chaining off one another, since none of them is durable history
    /// a later event could legitimately cite (see `emit_ephemeral`).
    ///
    /// Returns the underlying generation's own error (or a spawned
    /// task's panic, wrapped) unadorned, each caller wraps this call
    /// in its own `.context(...)` naming which of the three call sites
    /// failed, exactly as each did with the plain `generate()` call
    /// this replaced.
    async fn stream_generation(
        &self,
        provider: &Arc<dyn ModelProvider>,
        gen_request: GenerationRequest,
        request: &UserRequest,
        task_id: Uuid,
        extra_payload: serde_json::Value,
        correlation_id: Uuid,
        caused_by: Uuid,
    ) -> Result<GenerationResult> {
        let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let provider = provider.clone();
        // Round 53: aborted if this future is dropped or we bail out on a
        // cancel, see `AbortOnDrop`.
        let mut gen_handle = AbortOnDrop(tokio::spawn(async move { provider.generate_streaming(gen_request, chunk_tx).await }));
        // `None` for a task outside `execute_task` (nothing can cancel it).
        let cancel_rx = self.signals.lock().unwrap().get(&task_id).map(|sender| sender.subscribe());

        let mut chunk_index: u32 = 0;
        loop {
            let next = match &cancel_rx {
                Some(rx) => tokio::select! {
                    chunk = chunk_rx.recv() => chunk,
                    _ = wait_for_cancel(rx.clone()) => return Err(anyhow::Error::new(TaskCancelled(task_id))),
                },
                None => chunk_rx.recv().await,
            };
            let Some(chunk) = next else { break };
            chunk_index += 1;
            let mut payload = extra_payload.clone();
            if let serde_json::Value::Object(map) = &mut payload {
                map.insert("task_id".into(), json!(task_id));
                map.insert("chunk".into(), json!(chunk));
                map.insert("chunk_index".into(), json!(chunk_index));
            }
            self.emit_ephemeral(request, EventType::ModelTokenGenerated, payload, correlation_id, caused_by)?;
        }

        // The chunk channel closing means the provider is finishing, not
        // that it has returned, a cancel can still land in this gap.
        let joined = match &cancel_rx {
            Some(rx) => tokio::select! {
                joined = &mut gen_handle.0 => joined,
                _ = wait_for_cancel(rx.clone()) => return Err(anyhow::Error::new(TaskCancelled(task_id))),
            },
            None => (&mut gen_handle.0).await,
        };
        joined.context("model generation task panicked")?
    }
}

/// The status of a project's git link, `None` from `git_sync_status`
/// if it's never been linked at all.
#[derive(Debug, Clone)]
pub struct GitSyncStatus {
    pub config: GitSyncConfig,
    /// Whether the local team roster has changed since the last
    /// successful push or pull, i.e. whether `push_to_git` would
    /// actually have something to commit.
    pub local_dirty: bool,
}

#[derive(Debug, Clone)]
pub struct GitPushResult {
    /// `false` when the freshly-exported snapshot was identical to
    /// what's already committed, a normal "nothing to sync" outcome.
    pub pushed: bool,
    pub commit: String,
}

#[derive(Debug, Clone, Default)]
pub struct GitPullResult {
    pub commit: String,
    /// Whether `git pull` actually moved HEAD, `false` means the
    /// remote had nothing new, though local objects can still have
    /// been "applied" in that case (a no-op re-application of
    /// already-matching state).
    pub had_remote_changes: bool,
    pub members_applied: usize,
    pub teams_applied: usize,
    pub specialists_applied: usize,
    /// Round 23: Project Brain categories, same fast-forward/conflict
    /// treatment as the team roster.
    pub knowledge_items_applied: usize,
    pub decisions_applied: usize,
    /// Ids of new `Conflict` rows created by this pull, each needs
    /// `resolve_sync_conflict` before that object's local value will
    /// change again.
    pub conflicts_created: Vec<Uuid>,
    /// Round 46: ids of `PendingSyncChange` rows created or refreshed
    /// by this pull, only ever non-empty when the project has
    /// `require_review_for_incoming_changes` set. Each needs
    /// `apply_pending_sync_change` or `deny_pending_sync_change` from a
    /// Member with `Permission::Approve` before it affects anything.
    pub pending_changes_created: Vec<Uuid>,
    /// Round 31: for each conflict in `conflicts_created`, this pull
    /// also tried `send_conflict_to_github` automatically, no more
    /// manual "Send to GitHub as PR" click per conflict. `(conflict_id,
    /// pr_url)` for each one that succeeded.
    pub github_prs_created: Vec<(Uuid, String)>,
    /// The same attempt's failures, most commonly "the `gh` CLI isn't
    /// installed or isn't authenticated". Deliberately non-fatal: a
    /// failed PR-mirroring attempt never fails the pull itself, since
    /// the conflict is still there, still resolvable locally in the
    /// Conflict Center, with or without GitHub. `(conflict_id, error
    /// message)`.
    pub github_pr_errors: Vec<(Uuid, String)>,
}

/// Round 30: the outcome of checking a conflict's mirrored GitHub PR.
/// `StillOpen` means nothing changed locally; the other three variants
/// each correspond to exactly one call (or zero, for `AlreadyResolved`)
/// to `resolve_sync_conflict` having already happened by the time this
/// returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubPrCheckOutcome {
    StillOpen,
    ResolvedAcceptingLocal,
    ResolvedAcceptingRemote,
    /// The conflict was already resolved (locally, or by an earlier
    /// call to this same method) before this check even looked at
    /// GitHub, a no-op, not an error.
    AlreadyResolved,
}

/// Round 32: the result of `check_all_conflict_github_pr_statuses`,
/// one sweep over every open, GitHub-mirrored conflict in a project,
/// the automatic counterpart to clicking "Check PR status" on each one
/// by hand. Never fails as a whole because one conflict's check
/// failed; each is recorded independently, the same non-fatal-per-item
/// shape round 31 already uses for `pull_from_git`'s own GitHub loop.
#[derive(Debug, Clone, Default)]
pub struct GithubPrStatusSweepResult {
    pub checked: Vec<(Uuid, GithubPrCheckOutcome)>,
    pub errors: Vec<(Uuid, String)>,
}

/// Serializes each item to JSON, keyed by id, the common shape
/// `diff_sync_objects` compares across local/remote/baseline. Panics
/// only if a domain type somehow fails to serialize to JSON, which
/// none of ours do (all-owned data, no non-string map keys).
fn index_by_id<T: serde::Serialize>(items: &[T], id_of: impl Fn(&T) -> Uuid) -> HashMap<Uuid, serde_json::Value> {
    items
        .iter()
        .map(|item| (id_of(item), serde_json::to_value(item).expect("domain types always serialize to JSON")))
        .collect()
}

/// Pulls a display label for an object out of a full-object index (not
/// a field value), used to build a sync conflict's human-readable
/// question, e.g. `Team "Engineering"'s "mission" field was...`. Every
/// id in `diff.to_conflict` is guaranteed to exist in `local_map` (see
/// `diff_sync_objects`: a conflict only ever arises when both sides
/// have the object), so this always finds something once given the
/// right key.
fn display_label(local_map: &HashMap<Uuid, serde_json::Value>, id: Uuid, name_field: &str, fallback: &str) -> String {
    local_map
        .get(&id)
        .and_then(|v| v.get(name_field))
        .and_then(|v| v.as_str())
        .unwrap_or(fallback)
        .to_string()
}

/// Round 46: labels a `to_apply` item straight from the incoming value
/// itself, not `local_map`, unlike a conflict (which by definition
/// only ever happens to an object that already exists locally), a
/// queued pending change is very often a brand-new object with
/// nothing in `local_map` to look up at all, and even for an existing
/// one, showing the reviewer what's actually about to be applied is
/// more useful than showing what's there today.
fn label_from_value(value: &serde_json::Value, name_field: &str, fallback: &str) -> String {
    value.get(name_field).and_then(|v| v.as_str()).unwrap_or(fallback).to_string()
}

/// Round 47: whether `specialist` is authorized for `permission`,
/// either directly (its own `permissions`, the original, still-active
/// grant since Phase 3) or via any `Role` it holds (round 46's
/// mechanism, extended here to a second entity type). Purely additive,
/// same union `member_has_permission` uses, a role can only ever ADD
/// a permission a specialist doesn't already have, never take one away
/// that direct assignment already granted.
/// What the Project Brain holds for one task, borrowed so each consulted
/// specialist can package its own (scope-narrowed) view of it.
struct BrainView<'a> {
    knowledge: &'a [solexa_domain::KnowledgeItem],
    decisions: &'a [solexa_domain::Decision],
    required_capabilities: &'a [String],
}

fn specialist_grants(specialist: &Specialist, roles_by_id: &HashMap<Uuid, Role>, permission: Permission) -> bool {
    specialist.permissions.contains(&permission)
        || specialist
            .role_ids
            .iter()
            .any(|rid| roles_by_id.get(rid).is_some_and(|r| r.has_permission(permission)))
}

struct ObjectDiff {
    /// (id, merged value) pairs to insert/update locally. Either a
    /// brand new object that only exists on the remote, a whole object
    /// fast-forwarded because local hadn't touched it, or an existing
    /// object with zero or more non-colliding fields merged in from
    /// remote (see `merge_object_fields`), in the last case, whatever
    /// field(s) genuinely collided are *not* reflected here; they show
    /// up in `to_conflict` instead, and the merged value here still
    /// carries their *local* value unchanged.
    to_apply: Vec<(Uuid, serde_json::Value)>,
    /// (id, field name, local field value, remote field value), one
    /// entry per field that genuinely collided (round 25: field-level,
    /// not whole-object). The same id can appear here more than once if
    /// more than one field collided, and can appear in both
    /// `to_conflict` and `to_apply` at once if some of its fields
    /// merged safely while others didn't.
    to_conflict: Vec<(Uuid, String, serde_json::Value, serde_json::Value)>,
}

/// The core 3-way merge decision for one object category (Members,
/// Teams, Specialists, KnowledgeItems, or Decisions) during
/// `pull_from_git`. For each id present locally, remotely, or both:
///
/// - remote already matches local → nothing to do.
/// - remote matches the last-synced baseline (i.e. remote hasn't
///   changed) → local's value, whatever it is, wins as-is.
/// - local matches the baseline (i.e. local hasn't changed) → fast-
///   forward the whole object from remote.
/// - neither matches the baseline (both sides changed the object) →
///   `merge_object_fields` decides *per field*, so two devices editing
///   different fields of the same object merge automatically instead
///   of the whole object becoming one all-or-nothing conflict.
///
/// A remote-side "deletion" (present in baseline/local, absent from
/// remote) is deliberately never propagated as a local delete, see
/// round 22's README; the object is left as-is and neither counted as
/// applied nor conflicted.
fn diff_sync_objects(
    local: &HashMap<Uuid, serde_json::Value>,
    remote: &HashMap<Uuid, serde_json::Value>,
    baseline: &HashMap<Uuid, serde_json::Value>,
) -> ObjectDiff {
    let mut diff = ObjectDiff { to_apply: Vec::new(), to_conflict: Vec::new() };
    let mut ids: Vec<Uuid> = local.keys().chain(remote.keys()).copied().collect();
    ids.sort();
    ids.dedup();
    for id in ids {
        let local_val = local.get(&id);
        let remote_val = remote.get(&id);
        let base_val = baseline.get(&id);

        if remote_val == local_val {
            continue; // already in sync
        }
        if remote_val == base_val {
            continue; // remote unchanged since baseline; local's value wins as-is
        }
        if local_val == base_val {
            // local unchanged since baseline; fast-forward the whole object
            if let Some(r) = remote_val {
                diff.to_apply.push((id, r.clone()));
            }
            continue;
        }
        // both sides changed since baseline
        match (local_val, remote_val) {
            (Some(l), Some(r)) => {
                let (merged, conflicts) = merge_object_fields(l, r, base_val);
                if let Some(merged) = merged {
                    diff.to_apply.push((id, merged));
                }
                diff.to_conflict.extend(conflicts.into_iter().map(|(field, lv, rv)| (id, field, lv, rv)));
            }
            (None, Some(r)) => diff.to_apply.push((id, r.clone())), // nothing local to conflict with
            (Some(_), None) => {} // remote deleted, local changed, not propagated this round
            (None, None) => unreachable!("id came from local.keys() or remote.keys()"),
        }
    }
    diff
}

/// Round 25's field-level merge for a single object that changed on
/// both sides since the last sync. Compares every top-level JSON key
/// (skipping `id`/`project_id`, which are identity and can never
/// meaningfully diverge for the same id) using the same 3-way logic as
/// `diff_sync_objects` itself, just one level down:
///
/// - a field neither side changed, or only remote changed → resolved
///   automatically (kept as local's value, or fast-forwarded).
/// - a field only local changed → kept as local's value, as-is.
/// - a field **both sides changed** round 26: if it's a JSON array on
///   both sides, `merge_list_field` unions the add/remove operations
///   each side made since baseline (see its own doc comment for why
///   this never needs a conflict). Otherwise (a scalar or nested
///   object), returned as a conflict for that specific field; the
///   field's value in the returned merged object (if one is returned)
///   stays at local's current value until the conflict is resolved.
///
/// Returns `(None, conflicts)` if nothing was safe to merge (every
/// differing field collided), so callers don't write a no-op "merged"
/// object that's actually identical to what's already stored.
fn merge_object_fields(
    local: &serde_json::Value,
    remote: &serde_json::Value,
    baseline: Option<&serde_json::Value>,
) -> (Option<serde_json::Value>, Vec<(String, serde_json::Value, serde_json::Value)>) {
    let (Some(local_obj), Some(remote_obj)) = (local.as_object(), remote.as_object()) else {
        // Not JSON objects (shouldn't happen for our domain types),
        // fall back to treating the whole value as one field.
        return (None, vec![("*".to_string(), local.clone(), remote.clone())]);
    };
    let baseline_obj = baseline.and_then(|b| b.as_object());

    let mut merged = local_obj.clone();
    let mut conflicts = Vec::new();
    let mut changed_any = false;

    let mut keys: Vec<&String> = local_obj.keys().chain(remote_obj.keys()).collect();
    keys.sort();
    keys.dedup();

    for key in keys {
        if key == "id" || key == "project_id" {
            continue;
        }
        let local_field = local_obj.get(key);
        let remote_field = remote_obj.get(key);
        let base_field = baseline_obj.and_then(|b| b.get(key));

        if remote_field == local_field {
            continue; // this field already agrees
        }
        if remote_field == base_field {
            continue; // remote didn't change this field; local's value wins as-is
        }
        if local_field == base_field {
            // local didn't change this field; fast-forward it
            if let Some(r) = remote_field {
                merged.insert(key.clone(), r.clone());
                changed_any = true;
            }
            continue;
        }
        // this field genuinely changed on both sides, a list field
        // merges via union (round 26); anything else is a real collision
        if let (Some(serde_json::Value::Array(l)), Some(serde_json::Value::Array(r))) = (local_field, remote_field) {
            let b: &[serde_json::Value] = match base_field {
                Some(serde_json::Value::Array(b)) => b,
                _ => &[],
            };
            merged.insert(key.clone(), serde_json::Value::Array(merge_list_field(b, l, r)));
            changed_any = true;
            continue;
        }
        conflicts.push((
            key.clone(),
            local_field.cloned().unwrap_or(serde_json::Value::Null),
            remote_field.cloned().unwrap_or(serde_json::Value::Null),
        ));
    }

    (if changed_any { Some(serde_json::Value::Object(merged)) } else { None }, conflicts)
}

/// Round 26: a 3-way merge for a list field, treating it as an
/// unordered set rather than a single comparable value. Computes what
/// each side added and removed relative to `baseline`, then applies
/// both sides' operations together, the union of every addition,
/// minus anything either side removed.
///
/// Deliberately never produces a conflict, unlike a scalar field: a
/// plain value list (of ids, capability strings, etc., everything
/// this app actually has) has no notion of one element's value
/// "colliding" with another's the way two different new strings for a
/// `Team.mission` would. Two people adding *different* new elements
/// independently is exactly the common, unambiguous case this exists
/// for; the only genuinely ambiguous list scenario, one side removing
/// an element while the other simultaneously edits *that specific
/// element's own identity* can't arise here, because nothing in this
/// app edits a list element's value in place; every change is a whole-
/// element add or remove.
///
/// Result order isn't preserved from either side, it's baseline's
/// surviving order, then local's new elements, then remote's (skipping
/// duplicates), since order was never semantically meaningful for any
/// of this app's list fields (a set of member ids, a bag of
/// capabilities) to begin with.
fn merge_list_field(baseline: &[serde_json::Value], local: &[serde_json::Value], remote: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let removed_by_either = |v: &serde_json::Value| !local.contains(v) || !remote.contains(v);
    let mut merged: Vec<serde_json::Value> = baseline.iter().filter(|v| !removed_by_either(v)).cloned().collect();
    for v in local.iter().chain(remote.iter()) {
        if !baseline.contains(v) && !merged.contains(v) {
            merged.push(v.clone());
        }
    }
    merged
}

/// Round 50: the output bound for the two review calls (`model_review`,
/// `specialist_review`), NOT the main task-execution call, which stays
/// unbounded exactly as before. A review is asked for "VERDICT: ... plus
/// a one-line reason," so a reviewer that keeps generating is runaway
/// output holding a hardware guard for no benefit.
///
/// The cap is still deliberately generous, though since round 51 hitting
/// it is no longer quiet: providers report a `FinishReason`, and
/// `review_verdict` treats a review cut off by the limit *before* its
/// verdict line as `Inconclusive` (the task goes to revision) instead of
/// defaulting it to approved. A reasoning model that deliberates past
/// 1024 tokens before answering will now hit that path visibly, the
/// remaining cost of bounding at all is a needless revision, not a
/// silently wrong approval.
const REVIEW_MAX_TOKENS: u32 = 1024;

/// Round 51: the outcome of a review call. `Inconclusive` exists so that
/// "the reviewer ran out of room before saying anything" is distinguishable
/// from "the reviewer approved", the two used to be the same `true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewVerdict {
    Approved,
    Rejected,
    Inconclusive,
}

/// The `RevisionRequested` reason for a review that did not approve.
fn review_rejection_reason(kind: &str, verdict: ReviewVerdict, review_text: &str) -> String {
    match verdict {
        ReviewVerdict::Inconclusive => format!(
            "{kind} was inconclusive: the reviewer hit its output limit before giving a verdict, so the result was not approved. Partial review: {review_text}"
        ),
        _ => format!("{kind} rejected the result: {review_text}"),
    }
}

/// Parses a reviewer's verdict from a line that starts with "VERDICT:"
/// anchored to a line start rather than a whole-text substring
/// search, because `MockLocalProvider` (an honest stand-in, not a real
/// reviewer) echoes its input prompt verbatim, and the review prompt's
/// own instructions mention both "APPROVED" and "REJECTED" as format
/// guidance. A naive `.contains("REJECTED")` check would misfire on
/// that echoed instruction text; anchoring to a dedicated verdict line
/// is what a real reviewing model's structured output would look like
/// anyway, and it happens to also be immune to this specific mock
/// artifact. Defaults to approved when no verdict line is found at all
/// an ambiguous/unparseable review should not silently block a task
/// that otherwise passed deterministic verification.
///
/// Round 51 narrows that default: a missing verdict line is only waved
/// through when the review *finished* (or the provider didn't say). If the
/// provider reports `FinishReason::Length`, the missing verdict is the
/// truncation's doing, not ambiguity in the reviewer's answer, so it is
/// `Inconclusive`. A verdict line that did make it out before a cutoff
/// still stands.
fn review_verdict(review_text: &str, finish_reason: FinishReason) -> ReviewVerdict {
    let verdict_line = review_text.lines().find(|line| line.trim_start().to_uppercase().starts_with("VERDICT:"));
    match verdict_line {
        Some(line) if line.to_uppercase().contains("REJECTED") => ReviewVerdict::Rejected,
        Some(_) => ReviewVerdict::Approved,
        None if finish_reason == FinishReason::Length => ReviewVerdict::Inconclusive,
        None => ReviewVerdict::Approved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solexa_domain::{EscalationPolicy, Modality, ModelMetadata, Permission, Specialist};
    use solexa_models::{MockLocalProvider, MockReviewerProvider};
    use std::sync::Arc as StdArc;

    fn general_specialist(model_id: Uuid) -> Specialist {
        Specialist {
            id: Uuid::new_v4(),
            name: "General Specialist".into(),
            role: "generalist".into(),
            domain: "general".into(),
            capabilities: vec!["general".into(), "backend".into()],
            preferred_model_ids: vec![model_id],
            tools: vec![],
            knowledge_scope: vec![],
            permissions: vec![Permission::Read, Permission::Propose, Permission::Execute],
            output_schema: None,
            escalation_policy: EscalationPolicy::default(),
            active: true,
                    role_ids: Vec::new(), max_tokens: None, temperature: None,
        }
    }

    /// Deliberately narrow, the point of Phase 2's second fixture
    /// specialist is to prove capability matching actually routes
    /// different subtasks to different specialists, not just that a
    /// single generalist happens to handle everything.
    fn sql_specialist(model_id: Uuid) -> Specialist {
        Specialist {
            id: Uuid::new_v4(),
            name: "SQL Specialist".into(),
            role: "database engineer".into(),
            domain: "sql".into(),
            capabilities: vec!["sql".into()],
            preferred_model_ids: vec![model_id],
            tools: vec![],
            knowledge_scope: vec![],
            permissions: vec![Permission::Read, Permission::Propose, Permission::Execute],
            output_schema: None,
            escalation_policy: EscalationPolicy::default(),
            active: true,
                    role_ids: Vec::new(), max_tokens: None, temperature: None,
        }
    }

    /// A specialist whose only job in these tests is to review another
    /// specialist's work, `Permission::Review` but deliberately no
    /// `Permission::Execute`, proving specialist-review candidacy is
    /// gated on Review specifically, not on being a general-purpose
    /// executor. Shares the "backend" capability with `general_specialist`
    /// so it's actually a domain-relevant reviewer for the same requests.
    fn reviewer_specialist(model_id: Uuid) -> Specialist {
        Specialist {
            id: Uuid::new_v4(),
            name: "Backend Review Specialist".into(),
            role: "senior backend reviewer".into(),
            domain: "backend".into(),
            capabilities: vec!["backend".into()],
            preferred_model_ids: vec![model_id],
            tools: vec![],
            knowledge_scope: vec![],
            permissions: vec![Permission::Read, Permission::Review],
            output_schema: None,
            escalation_policy: EscalationPolicy::default(),
            active: true,
                    role_ids: Vec::new(), max_tokens: None, temperature: None,
        }
    }

    fn mock_model() -> ModelMetadata {
        ModelMetadata {
            id: Uuid::new_v4(),
            name: "mock-general-7b".into(),
            provider: "mock".into(),
            modalities: vec![Modality::Text],
            context_capacity_tokens: 8192,
            parameter_count_billions: Some(7.0),
            quantization: Some("Q4_K_M".into()),
            estimated_vram_mb: 6000,
            estimated_ram_mb: 8000,
            tool_support: false,
            structured_output_support: false,
            streaming_support: false,
            local: true,
            license: Some("apache-2.0".into()),
        }
    }

    async fn build_coordinator() -> (Coordinator, Uuid) {
        let storage = StdArc::new(Storage::in_memory().unwrap());

        // Wire storage in as a durable event sink, exactly as the
        // desktop app does, this is what makes a run's trace queryable
        // afterward via `events_for_correlation`.
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let events = StdArc::new(bus);

        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let model = mock_model();
        let model_id = model.id;
        storage.insert_model(&model).unwrap();

        let specialist = general_specialist(model_id);
        storage.insert_specialist(project_id, &specialist).unwrap();
        storage
            .insert_specialist(project_id, &sql_specialist(model_id))
            .unwrap();

        let mut providers = ProviderRegistry::new();
        providers.register(StdArc::new(MockLocalProvider::new(vec![model])));

        (Coordinator::new(storage, events, providers), project_id)
    }

    #[tokio::test]
    async fn full_core_loop_completes_and_is_fully_traceable() {
        let (coordinator, project_id) = build_coordinator().await;

        let submission = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Refactor the Rust backend auth module.".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();

        let task = coordinator.get_task(submission.task_id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Completed);
        assert!(task.result.is_some());
        assert!(task.verification.unwrap().passed);
        assert_eq!(task.assigned_specialist_id.is_some(), true);

        // The whole run must be replayable from events alone.
        let trace = coordinator
            .events_for_correlation(submission.correlation_id)
            .unwrap();
        let event_types: Vec<_> = trace.iter().map(|e| e.event_type).collect();
        assert_eq!(
            event_types,
            vec![
                EventType::RequestReceived,
                EventType::PlannerStarted,
                EventType::TaskCreated,
                EventType::PlannerCompleted,
                EventType::TaskAssigned,
                EventType::ContextPackaged,
                EventType::ResourceAcquired,
                EventType::ModelInvoked,
                EventType::ResourceReleased,
                EventType::ModelCompleted,
                EventType::ResultSubmitted,
                EventType::VerificationStarted,
                EventType::Verified,
                EventType::TaskCompleted,
            ]
        );
        // Every event but the root must cite what caused it.
        assert!(trace[0].causation_id.is_none());
        assert!(trace[1..].iter().all(|e| e.causation_id.is_some()));
    }

    #[tokio::test]
    async fn model_token_generated_events_are_live_only_and_never_reach_durable_history() {
        // Built by hand rather than via `build_coordinator()` so a
        // subscriber can be grabbed before `events` moves into
        // `Coordinator::new`, `build_coordinator()` has no reason to
        // expose that plumbing for every other test that doesn't need it.
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let events = StdArc::new(bus);
        let mut live = events.subscribe();

        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();
        let model = mock_model();
        let model_id = model.id;
        storage.insert_model(&model).unwrap();
        storage.insert_specialist(project_id, &general_specialist(model_id)).unwrap();
        let mut providers = ProviderRegistry::new();
        providers.register(StdArc::new(MockLocalProvider::new(vec![model])));
        let coordinator = Coordinator::new(storage, events, providers);

        let submission = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Refactor the Rust backend auth module.".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();

        // By the time submit_request returns, run_specialist has
        // already awaited the spawned generation task to completion,
        // so every live event this run will ever produce has already
        // been sent; try_recv (non-blocking) is enough to collect them.
        let mut token_events = Vec::new();
        while let Ok(event) = live.try_recv() {
            if event.event_type == EventType::ModelTokenGenerated {
                token_events.push(event);
            }
        }

        // MockLocalProvider has no real partial output to send
        // progressively, the honest default `generate_streaming`
        // behavior (round 39) is exactly one chunk carrying the whole
        // text, not a fabricated per-token split.
        let task = coordinator.get_task(submission.task_id).unwrap().unwrap();
        let full_text = task.result.as_ref().unwrap().content.as_str().unwrap().to_string();
        assert_eq!(token_events.len(), 1, "{token_events:?}");
        assert_eq!(token_events[0].payload["chunk"], full_text);
        assert_eq!(token_events[0].payload["chunk_index"], 1);
        assert_eq!(token_events[0].correlation_id, submission.correlation_id);

        // ...and, being ephemeral, none of it reached durable storage,
        // checked directly here rather than only inferred from
        // full_core_loop_completes_and_is_fully_traceable's exact
        // sequence not mentioning it.
        let trace = coordinator.events_for_correlation(submission.correlation_id).unwrap();
        assert!(
            trace.iter().all(|e| e.event_type != EventType::ModelTokenGenerated),
            "a ModelTokenGenerated event reached durable storage, publish_ephemeral isn't ephemeral"
        );
    }

    #[tokio::test]
    async fn request_with_no_matching_specialist_blocks_visibly_instead_of_erroring() {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let events = StdArc::new(bus);
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Empty Project", "").unwrap();

        let coordinator = Coordinator::new(storage, events, ProviderRegistry::new());
        let submission = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Do literally anything.".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();

        let task = coordinator.get_task(submission.task_id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Blocked);
    }

    #[tokio::test]
    async fn multi_capability_request_decomposes_and_routes_to_different_specialists() {
        let (coordinator, project_id) = build_coordinator().await;

        let submission = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Refactor the Rust backend and write the SQL migration for the new auth table."
                    .into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();

        let parent = coordinator.get_task(submission.task_id).unwrap().unwrap();
        assert_eq!(parent.state, TaskState::Completed);
        assert!(parent.parent_task_id.is_none());
        // The parent's result was assembled from its subtasks, not
        // produced by one specialist/model, the nil sentinel says so.
        assert_eq!(parent.result.as_ref().unwrap().produced_by_specialist_id, Uuid::nil());

        let all_tasks = coordinator.list_tasks(project_id).unwrap();
        let subtasks: Vec<_> = all_tasks
            .iter()
            .filter(|t| t.parent_task_id == Some(parent.id))
            .collect();
        assert_eq!(subtasks.len(), 2, "one subtask per inferred capability");
        assert!(subtasks.iter().all(|t| t.state == TaskState::Completed));

        // The whole point of the second fixture specialist: the backend
        // subtask and the sql subtask must land on *different*
        // specialists, proving capability-based routing actually
        // discriminates rather than one generalist absorbing everything.
        let assigned_specialists: std::collections::HashSet<_> = subtasks
            .iter()
            .map(|t| t.assigned_specialist_id.unwrap())
            .collect();
        assert_eq!(
            assigned_specialists.len(),
            2,
            "backend and sql subtasks should be routed to two distinct specialists"
        );

        // Every subtask's events are chained under the same
        // correlation id as the parent's, the whole decomposed run is
        // still one replayable trace.
        let trace = coordinator
            .events_for_correlation(submission.correlation_id)
            .unwrap();
        let task_created_count = trace
            .iter()
            .filter(|e| e.event_type == EventType::TaskCreated)
            .count();
        assert_eq!(task_created_count, 3, "parent + 2 subtasks");
        let completed_count = trace
            .iter()
            .filter(|e| e.event_type == EventType::TaskCompleted)
            .count();
        assert_eq!(completed_count, 3, "2 subtasks + the parent");
    }

    /// A custom `ModelProvider` used only by the test below: it counts
    /// how many `generate()` calls are in flight at once (tracking a
    /// running peak) and sleeps briefly inside each call, so two
    /// concurrent subtasks have a real chance to overlap *if nothing
    /// stops them*. `MockLocalProvider` returns instantly, which would
    /// make a serialization test meaningless, there'd be no window in
    /// which two calls could race even if the resource guard were
    /// broken.
    struct SlowCountingProvider {
        model: ModelMetadata,
        active: StdArc<std::sync::atomic::AtomicUsize>,
        peak: StdArc<std::sync::atomic::AtomicUsize>,
        delay: std::time::Duration,
    }

    #[async_trait::async_trait]
    impl solexa_models::ModelProvider for SlowCountingProvider {
        fn list_models(&self) -> Vec<ModelMetadata> {
            vec![self.model.clone()]
        }

        async fn generate(
            &self,
            request: solexa_models::GenerationRequest,
        ) -> anyhow::Result<solexa_models::GenerationResult> {
            use std::sync::atomic::Ordering;
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(solexa_models::GenerationResult {
                finish_reason: solexa_models::FinishReason::Unknown,
                text: format!("handled: {}", request.prompt),
                estimated_tokens: 1,
            })
        }
    }

    #[tokio::test]
    async fn decomposed_subtasks_serialize_when_the_resource_budget_fits_only_one_model() {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let events = StdArc::new(bus);
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let model = mock_model();
        let model_id = model.id;
        storage.insert_model(&model).unwrap();
        storage
            .insert_specialist(project_id, &general_specialist(model_id))
            .unwrap();
        storage
            .insert_specialist(project_id, &sql_specialist(model_id))
            .unwrap();

        let active = StdArc::new(std::sync::atomic::AtomicUsize::new(0));
        let peak = StdArc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut providers = ProviderRegistry::new();
        providers.register(StdArc::new(SlowCountingProvider {
            model: model.clone(),
            active: active.clone(),
            peak: peak.clone(),
            delay: std::time::Duration::from_millis(60),
        }));

        // Exactly one model's worth of VRAM, generous RAM headroom, and
        // a concurrency cap of 4, so if the two subtasks' model calls
        // DO serialize, it's provably because of the VRAM budget check
        // in solexa-resources, not the unrelated concurrency semaphore.
        let budget = solexa_resources::ResourceBudget {
            vram_mb: model.estimated_vram_mb,
            ram_mb: model.estimated_ram_mb * 4,
            max_concurrent: 4,
        };
        let coordinator = Coordinator::with_resource_budget(storage, events, providers, budget);

        let submission = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Refactor the Rust backend and write the SQL migration for the new auth table."
                    .into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();

        let parent = coordinator.get_task(submission.task_id).unwrap().unwrap();
        assert_eq!(parent.state, TaskState::Completed);
        assert_eq!(
            peak.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the two subtasks' model calls should never have overlapped, the VRAM budget only fits one"
        );
    }

    /// Adopted from a parallel round: proves
    /// `Coordinator::resource_snapshot().queued` reports real
    /// contention, not just `solexa-resources`' own internal view of
    /// it, the same setup as the test just above (one VRAM slot, two
    /// subtasks, `SlowCountingProvider` to hold it open long enough to
    /// observe), but read through the coordinator's own public API,
    /// which is what the Tauri command a live Resource Center calls
    /// actually goes through.
    #[tokio::test]
    async fn resource_snapshot_reflects_a_subtask_genuinely_queued_behind_another() {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let events = StdArc::new(bus);
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let model = mock_model();
        let model_id = model.id;
        storage.insert_model(&model).unwrap();
        storage.insert_specialist(project_id, &general_specialist(model_id)).unwrap();
        storage.insert_specialist(project_id, &sql_specialist(model_id)).unwrap();

        let mut providers = ProviderRegistry::new();
        providers.register(StdArc::new(SlowCountingProvider {
            model: model.clone(),
            active: StdArc::new(std::sync::atomic::AtomicUsize::new(0)),
            peak: StdArc::new(std::sync::atomic::AtomicUsize::new(0)),
            // Long enough that the test's own 60ms check below lands
            // reliably inside the window where the first subtask is
            // still holding the budget and the second is genuinely
            // blocked, short enough not to make the test slow.
            delay: std::time::Duration::from_millis(150),
        }));

        let budget = solexa_resources::ResourceBudget {
            vram_mb: model.estimated_vram_mb,
            ram_mb: model.estimated_ram_mb * 4,
            max_concurrent: 4,
        };
        let coordinator = StdArc::new(Coordinator::with_resource_budget(storage, events, providers, budget));

        assert_eq!(coordinator.resource_snapshot().queued, 0, "nothing has been submitted yet");

        let submit_handle = tokio::spawn({
            let coordinator = coordinator.clone();
            async move {
                coordinator
                    .submit_request(UserRequest {
                        project_id,
                        text: "Refactor the Rust backend and write the SQL migration for the new auth table."
                            .into(),
                        requested_by: "human".into(),
                    })
                    .await
                    .unwrap()
            }
        });

        // Give the planner time to decompose and the first subtask time
        // to actually acquire the budget's one VRAM slot, so the second
        // subtask is provably queued behind it rather than just not
        // having started yet.
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        let mid_flight = coordinator.resource_snapshot();
        // Real finding, not a bug this round introduces or fixes:
        // `ResourceManager::acquire` takes its semaphore permit *before*
        // checking the VRAM/RAM budget (see its own doc comment), so
        // `running` counts every caller currently inside `acquire`'s
        // wait loop, up to `max_concurrent`, not only the ones that
        // have actually cleared the budget gate into `generate()`. With
        // `max_concurrent: 4` (this crate's standing convention for a
        // budget-contention test, matching the round-36+ tests above),
        // both subtasks grab a permit almost immediately here, so
        // `running` reads 2 even though only one of them is genuinely
        // executing, the other is double-counted, appearing in both
        // `running` and `queued` at once. `queued` itself is unaffected
        // by this (it only ever reflects the VRAM/RAM wait queue) and
        // is the field this test exists to prove. Left named rather
        // than "fixed" here: changing `acquire`'s permit-then-budget
        // ordering is a real scheduling-semantics change with its own
        // blast radius, out of scope for what this round asked for.
        assert_eq!(mid_flight.running, 2, "both subtasks hold a concurrency permit even though only one has cleared the VRAM gate");
        assert_eq!(mid_flight.queued, 1, "exactly one subtask is genuinely blocked on the VRAM budget, not just holding a permit");

        submit_handle.await.unwrap();

        let after = coordinator.resource_snapshot();
        assert_eq!(after.running, 0, "both subtasks have finished and released");
        assert_eq!(after.queued, 0, "nothing should still be waiting once the whole request has completed");
    }

    /// Round 49: the real proof for `cancel_task`, same one-VRAM-slot,
    /// two-subtask contention setup as the Resource Center test above,
    /// but this time the subtask genuinely queued behind the other gets
    /// cancelled mid-wait. Proves: the generating subtask is refused
    /// (not half-cancelled), the queued one really stops and the
    /// request still finishes cleanly, the survivor completes
    /// untouched, nothing is left stuck in the resource manager, and
    /// the parent escalates the way any not-cleanly-completed subtask
    /// already makes it.
    /// Like `SlowCountingProvider`, but records the prompt of every call
    /// in the order calls START, so a test can tell which of two
    /// concurrent subtasks got the hardware first without any internal
    /// bookkeeping of the coordinator's.
    struct StartOrderProvider {
        model: ModelMetadata,
        delay: std::time::Duration,
        started: StdArc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl solexa_models::ModelProvider for StartOrderProvider {
        fn list_models(&self) -> Vec<ModelMetadata> {
            vec![self.model.clone()]
        }

        async fn generate(
            &self,
            request: solexa_models::GenerationRequest,
        ) -> anyhow::Result<solexa_models::GenerationResult> {
            self.started.lock().unwrap().push(request.prompt.clone());
            tokio::time::sleep(self.delay).await;
            Ok(solexa_models::GenerationResult {
                finish_reason: solexa_models::FinishReason::Unknown,
                text: format!("handled: {}", request.prompt),
                estimated_tokens: 1,
            })
        }
    }

    #[tokio::test]
    async fn cancelling_a_subtask_queued_on_hardware_ends_its_wait_and_leaves_its_sibling_untouched() {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let events = StdArc::new(bus);
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let model = mock_model();
        let model_id = model.id;
        storage.insert_model(&model).unwrap();
        storage.insert_specialist(project_id, &general_specialist(model_id)).unwrap();
        storage.insert_specialist(project_id, &sql_specialist(model_id)).unwrap();

        let mut providers = ProviderRegistry::new();
        let started = StdArc::new(std::sync::Mutex::new(Vec::new()));
        providers.register(StdArc::new(StartOrderProvider {
            model: model.clone(),
            delay: std::time::Duration::from_millis(300),
            started: started.clone(),
        }));
        let budget = solexa_resources::ResourceBudget {
            vram_mb: model.estimated_vram_mb,
            ram_mb: model.estimated_ram_mb * 4,
            max_concurrent: 4,
        };
        let coordinator = StdArc::new(Coordinator::with_resource_budget(storage.clone(), events, providers, budget));

        let submit_handle = tokio::spawn({
            let coordinator = coordinator.clone();
            async move {
                coordinator
                    .submit_request(UserRequest {
                        project_id,
                        text: "Refactor the Rust backend and write the SQL migration for the new auth table.".into(),
                        requested_by: "human".into(),
                    })
                    .await
            }
        });

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(coordinator.resource_snapshot().queued, 1, "one subtask must be genuinely blocked on the VRAM budget before we cancel anything");

        let subtasks: Vec<Task> = storage.list_tasks(project_id).unwrap().into_iter().filter(|t| t.parent_task_id.is_some()).collect();
        assert_eq!(subtasks.len(), 2);
        // Exactly one call has started: that subtask holds the hardware.
        let first_prompt = {
            let started = started.lock().unwrap();
            assert_eq!(started.len(), 1, "only one subtask can be generating while the other is queued");
            started[0].clone()
        };
        let generating_id = subtasks.iter().find(|t| t.description == first_prompt).expect("the started prompt is a subtask's description").id;
        let queued_id = subtasks.iter().find(|t| t.id != generating_id).unwrap().id;

        // The queued one is genuinely interrupted (the generating sibling
        // is left alone simply because we don't cancel it, since round 53
        // it could be cancelled too; see the in-flight tests).
        coordinator.cancel_task(queued_id).unwrap();
        let submission = tokio::time::timeout(std::time::Duration::from_secs(5), submit_handle)
            .await
            .expect("cancelling a queued subtask must not leave the whole request hanging")
            .unwrap()
            .expect("a cancelled subtask must not fail the whole request");

        assert_eq!(coordinator.get_task(queued_id).unwrap().unwrap().state, TaskState::Cancelled, "cancelled must win, nothing may resurrect it");
        assert_eq!(coordinator.get_task(generating_id).unwrap().unwrap().state, TaskState::Completed, "the sibling must finish untouched");
        let parent = coordinator.get_task(submission.task_id).unwrap().unwrap();
        assert_eq!(parent.state, TaskState::Review, "a parent with a cancelled subtask escalates like any not-cleanly-completed one");

        let after = coordinator.resource_snapshot();
        assert_eq!((after.running, after.queued), (0, 0), "nothing may be left stuck in the resource manager");
        assert_eq!(coordinator.resources.pending_cancellations(), 0, "the queued subtask's marker was consumed by its wait ending");
        // Already-terminal tasks can't be cancelled.
        let err = coordinator.cancel_task(generating_id).unwrap_err();
        assert!(err.to_string().contains("can't be cancelled"), "{err}");
    }

    #[tokio::test]
    async fn a_registered_constraint_relevant_to_the_task_is_scoped_into_its_context() {
        let (coordinator, project_id) = build_coordinator().await;

        let relevant = solexa_domain::KnowledgeItem::new(
            project_id,
            solexa_domain::KnowledgeKind::Constraint,
            "DB choice",
            "Must use PostgreSQL, not SQLite, in production.",
            solexa_domain::Provenance::human("alice"),
            vec!["backend".into()],
        );
        let irrelevant = solexa_domain::KnowledgeItem::new(
            project_id,
            solexa_domain::KnowledgeKind::Fact,
            "Brand color",
            "The brand accent color is #E3A857.",
            solexa_domain::Provenance::human("alice"),
            vec!["design".into()],
        );
        coordinator.record_knowledge(relevant.clone()).unwrap();
        coordinator.record_knowledge(irrelevant).unwrap();

        let submission = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Refactor the Rust backend auth module.".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();

        let task = coordinator.get_task(submission.task_id).unwrap().unwrap();
        let context = task.context_package.expect("a context package should always be attached");
        assert_eq!(context.included_constraints, vec![relevant.content]);
        assert!(
            context.included_facts.is_empty(),
            "the design-tagged fact must not leak into a backend task's context"
        );
        assert!(context.excluded_summary.contains('1'), "should mention exactly one item was excluded");
    }

    /// Two specialists sharing the "backend" capability but with
    /// different names/roles/domains, since `MockLocalProvider`'s
    /// output now includes the system prompt (which is built from the
    /// specialist's own role/name/domain), these two genuinely produce
    /// different text for the same task, even sharing one model. That's
    /// what gives the conflict-detection tests something real to catch.
    async fn build_coordinator_with_disagreeing_backend_specialists() -> (Coordinator, Uuid, Uuid, Uuid) {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let events = StdArc::new(bus);

        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let model = mock_model();
        let model_id = model.id;
        storage.insert_model(&model).unwrap();

        let specialist_a = Specialist {
            id: Uuid::new_v4(),
            name: "Backend Specialist A".into(),
            role: "backend engineer".into(),
            domain: "backend services".into(),
            capabilities: vec!["backend".into()],
            preferred_model_ids: vec![model_id],
            tools: vec![],
            knowledge_scope: vec![],
            permissions: vec![Permission::Read, Permission::Propose, Permission::Execute],
            output_schema: None,
            escalation_policy: EscalationPolicy::default(),
            active: true,
                    role_ids: Vec::new(), max_tokens: None, temperature: None,
        };
        let specialist_b = Specialist {
            id: Uuid::new_v4(),
            name: "Backend Specialist B".into(),
            role: "systems architect".into(),
            domain: "distributed backend architecture".into(),
            capabilities: vec!["backend".into()],
            preferred_model_ids: vec![model_id],
            tools: vec![],
            knowledge_scope: vec![],
            permissions: vec![Permission::Read, Permission::Propose, Permission::Execute],
            output_schema: None,
            escalation_policy: EscalationPolicy::default(),
            active: true,
                    role_ids: Vec::new(), max_tokens: None, temperature: None,
        };
        storage.insert_specialist(project_id, &specialist_a).unwrap();
        storage.insert_specialist(project_id, &specialist_b).unwrap();

        let mut providers = ProviderRegistry::new();
        providers.register(StdArc::new(MockLocalProvider::new(vec![model])));

        (
            Coordinator::new(storage, events, providers),
            project_id,
            specialist_a.id,
            specialist_b.id,
        )
    }

    #[tokio::test]
    async fn two_disagreeing_specialists_create_a_conflict_instead_of_picking_one() {
        let (coordinator, project_id, specialist_a_id, specialist_b_id) =
            build_coordinator_with_disagreeing_backend_specialists().await;

        let submission = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Refactor the Rust backend auth module.".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();

        let task = coordinator.get_task(submission.task_id).unwrap().unwrap();
        // Never silently picked one specialist's answer, the task
        // waits, visibly, rather than completing.
        assert_eq!(task.state, TaskState::Review);
        assert!(task.result.is_none(), "no result should be committed while the conflict is unresolved");

        let conflicts = coordinator.list_conflicts(project_id).unwrap();
        assert_eq!(conflicts.len(), 1);
        let conflict = &conflicts[0];
        assert_eq!(conflict.task_id, Some(task.id));
        assert_eq!(conflict.status, solexa_domain::ConflictStatus::Open);
        assert_eq!(conflict.positions.len(), 2);
        let participant_ids: std::collections::HashSet<_> =
            conflict.positions.iter().map(|p| p.specialist_id).collect();
        assert_eq!(participant_ids, [specialist_a_id, specialist_b_id].into_iter().collect());
        // The two positions must actually differ, proving this isn't
        // an artifact of always creating a conflict regardless of
        // agreement.
        assert_ne!(conflict.positions[0].content, conflict.positions[1].content);

        let trace = coordinator.events_for_correlation(submission.correlation_id).unwrap();
        assert!(trace.iter().any(|e| e.event_type == EventType::ConflictDetected));
        assert!(!trace.iter().any(|e| e.event_type == EventType::TaskCompleted));
    }

    #[tokio::test]
    async fn resolving_a_conflict_completes_the_task_with_the_chosen_position_and_preserves_the_other() {
        let (coordinator, project_id, specialist_a_id, _specialist_b_id) =
            build_coordinator_with_disagreeing_backend_specialists().await;

        let submission = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Refactor the Rust backend auth module.".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();

        let conflict = coordinator.list_conflicts(project_id).unwrap().into_iter().next().unwrap();
        let chosen_position = conflict.position_for(specialist_a_id).unwrap().clone();

        let reviewer = solexa_domain::Member::new_human(project_id, "human-reviewer");
        coordinator.register_member(reviewer.clone()).unwrap();

        coordinator.resolve_conflict(conflict.id, specialist_a_id, reviewer.id).unwrap();

        let task = coordinator.get_task(submission.task_id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Completed);
        assert_eq!(task.result.unwrap().content, chosen_position.content);

        let resolved = coordinator.get_conflict(conflict.id).unwrap().unwrap();
        assert_eq!(resolved.status, solexa_domain::ConflictStatus::Resolved);
        assert_eq!(resolved.approved_by.as_deref(), Some("human-reviewer"));
        // The losing position must still be there, resolving a
        // conflict is never destructive.
        assert_eq!(resolved.positions.len(), 2);
    }

    // ---- round 58: the Project Brain context actually reaches the model --

    /// Records the (system_prompt, prompt) of every NON-review call, i.e.
    /// exactly what each specialist's main task call sent to the model.
    struct PromptRecordingProvider {
        inner: MockLocalProvider,
        seen: StdArc<std::sync::Mutex<Vec<(String, String)>>>,
    }

    #[async_trait::async_trait]
    impl solexa_models::ModelProvider for PromptRecordingProvider {
        fn list_models(&self) -> Vec<ModelMetadata> {
            self.inner.list_models()
        }
        async fn generate(&self, request: solexa_models::GenerationRequest) -> anyhow::Result<solexa_models::GenerationResult> {
            if !request.prompt.contains("Respond with VERDICT:") {
                self.seen.lock().unwrap().push((request.system_prompt.clone(), request.prompt.clone()));
            }
            self.inner.generate(request).await
        }
    }

    fn knowledge(project_id: Uuid, kind: solexa_domain::KnowledgeKind, content: &str, status: solexa_domain::KnowledgeStatus) -> solexa_domain::KnowledgeItem {
        let mut item = solexa_domain::KnowledgeItem::new(project_id, kind, content, content, solexa_domain::Provenance::human("test"), vec!["backend".into()]);
        item.status = status;
        item
    }

    /// Runs one task through `specialist_count` specialists holding
    /// `permissions` (and optionally one role), in a project holding
    /// `items`; returns what the model was sent for each main call.
    async fn prompts_sent_for(
        items: Vec<solexa_domain::KnowledgeItem>,
        permissions: Vec<Permission>,
        specialist_count: usize,
        with_read_role: bool,
    ) -> Vec<(String, String)> {
        prompts_sent_scoped(items, permissions, specialist_count, with_read_role, Vec::new()).await
    }

    /// Same, with every specialist's `knowledge_scope` set (round 59).
    async fn prompts_sent_scoped(
        items: Vec<solexa_domain::KnowledgeItem>,
        permissions: Vec<Permission>,
        specialist_count: usize,
        with_read_role: bool,
        scope: Vec<String>,
    ) -> Vec<(String, String)> {
        prompts_sent_with_scopes(items, permissions, specialist_count, with_read_role, vec![scope]).await
    }

    /// Specialist `n` gets `scopes[n % scopes.len()]`.
    async fn prompts_sent_with_scopes(
        items: Vec<solexa_domain::KnowledgeItem>,
        permissions: Vec<Permission>,
        specialist_count: usize,
        with_read_role: bool,
        scopes: Vec<Vec<String>>,
    ) -> Vec<(String, String)> {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let events = StdArc::new(bus);
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();
        let model = mock_model();
        storage.insert_model(&model).unwrap();
        for item in &items {
            let mut item = item.clone();
            item.project_id = project_id;
            storage.insert_knowledge_item(&item).unwrap();
        }
        let seen = StdArc::new(std::sync::Mutex::new(Vec::new()));
        let mut providers = ProviderRegistry::new();
        providers.register(StdArc::new(PromptRecordingProvider { inner: MockLocalProvider::new(vec![model.clone()]), seen: seen.clone() }));
        let coordinator = Coordinator::new(storage.clone(), events, providers);
        let read_role = coordinator.create_role(project_id, "Reader", vec![Permission::Read]).unwrap();
        for n in 0..specialist_count {
            let mut specialist = general_specialist(model.id);
            specialist.id = Uuid::new_v4();
            specialist.name = format!("General {n}");
            specialist.permissions = permissions.clone();
            specialist.knowledge_scope = scopes[n % scopes.len()].clone();
            if with_read_role {
                specialist.role_ids = vec![read_role.id];
            }
            storage.insert_specialist(project_id, &specialist).unwrap();
        }
        coordinator
            .submit_request(UserRequest { project_id, text: "Refactor the Rust backend auth module.".into(), requested_by: "human".into() })
            .await
            .unwrap();
        let sent = seen.lock().unwrap().clone();
        sent
    }

    #[tokio::test]
    async fn an_accepted_project_constraint_is_in_the_system_prompt_the_model_receives() {
        let items = vec![knowledge(Uuid::nil(), solexa_domain::KnowledgeKind::Constraint, "Never use unsafe Rust in the auth module.", solexa_domain::KnowledgeStatus::Accepted)];
        let sent = prompts_sent_for(items, vec![Permission::Read, Permission::Execute], 1, false).await;
        assert_eq!(sent.len(), 1, "one main call");
        let (system_prompt, _prompt) = &sent[0];
        assert!(
            system_prompt.contains("Never use unsafe Rust in the auth module."),
            "the Project Brain's packaged context must reach the model; the system prompt was only: {system_prompt:?}"
        );
    }

    const ROUND58_CONSTRAINT: &str = "Never use unsafe Rust in the auth module.";

    fn accepted_constraint() -> Vec<solexa_domain::KnowledgeItem> {
        vec![knowledge(Uuid::nil(), solexa_domain::KnowledgeKind::Constraint, ROUND58_CONSTRAINT, solexa_domain::KnowledgeStatus::Accepted)]
    }

    #[tokio::test]
    async fn knowledge_that_is_not_accepted_or_verified_never_reaches_the_model() {
        use solexa_domain::KnowledgeStatus as S;
        for status in [S::Unverified, S::Proposed, S::Rejected, S::Superseded, S::Archived] {
            let items = vec![knowledge(Uuid::nil(), solexa_domain::KnowledgeKind::Constraint, ROUND58_CONSTRAINT, status)];
            let sent = prompts_sent_for(items, vec![Permission::Read, Permission::Execute], 1, false).await;
            assert!(!sent[0].0.contains(ROUND58_CONSTRAINT), "{status:?} knowledge reached the model: {:?}", sent[0].0);
        }
        // positive controls: the same item DOES arrive once it is authoritative
        for status in [S::Accepted, S::Verified] {
            let items = vec![knowledge(Uuid::nil(), solexa_domain::KnowledgeKind::Constraint, ROUND58_CONSTRAINT, status)];
            let sent = prompts_sent_for(items, vec![Permission::Read, Permission::Execute], 1, false).await;
            assert!(sent[0].0.contains(ROUND58_CONSTRAINT), "{status:?} should be delivered: {:?}", sent[0].0);
        }
    }

    #[tokio::test]
    async fn a_specialist_without_read_is_given_no_project_knowledge() {
        let sent = prompts_sent_for(accepted_constraint(), vec![Permission::Execute], 1, false).await;
        assert_eq!(sent.len(), 1, "it still runs the task");
        assert!(!sent[0].0.contains(ROUND58_CONSTRAINT), "{:?}", sent[0].0);
        assert_eq!(sent[0].0, "You are the generalist, General 0 specialist. Domain: general.");
        // positive control: the same project with Read delivers it
        let with_read = prompts_sent_for(accepted_constraint(), vec![Permission::Read, Permission::Execute], 1, false).await;
        assert!(with_read[0].0.contains(ROUND58_CONSTRAINT));
    }

    #[tokio::test]
    async fn read_granted_only_through_a_role_delivers_the_knowledge() {
        let sent = prompts_sent_for(accepted_constraint(), vec![Permission::Execute], 1, true).await;
        assert!(sent[0].0.contains(ROUND58_CONSTRAINT), "role-derived Read must count: {:?}", sent[0].0);
    }

    #[tokio::test]
    async fn with_no_relevant_knowledge_the_system_prompt_is_exactly_what_it_was_before() {
        let irrelevant = {
            let mut item = knowledge(Uuid::nil(), solexa_domain::KnowledgeKind::Constraint, "Brand color is #E3A857.", solexa_domain::KnowledgeStatus::Accepted);
            item.tags = vec!["design".into()];
            vec![item]
        };
        let sent = prompts_sent_for(irrelevant, vec![Permission::Read, Permission::Execute], 1, false).await;
        assert_eq!(sent[0].0, "You are the generalist, General 0 specialist. Domain: general.");
        let empty = prompts_sent_for(Vec::new(), vec![Permission::Read, Permission::Execute], 1, false).await;
        assert_eq!(empty[0].0, sent[0].0);
    }

    #[tokio::test]
    async fn the_task_prompt_itself_is_left_untouched() {
        let sent = prompts_sent_for(accepted_constraint(), vec![Permission::Read, Permission::Execute], 1, false).await;
        assert_eq!(sent[0].1, "Refactor the Rust backend auth module.");
    }

    #[tokio::test]
    async fn every_specialist_in_a_consensus_round_receives_the_same_context() {
        let sent = prompts_sent_for(accepted_constraint(), vec![Permission::Read, Permission::Execute], 2, false).await;
        assert_eq!(sent.len(), 2, "two specialists consulted");
        for (system_prompt, _) in &sent {
            assert!(system_prompt.contains(ROUND58_CONSTRAINT), "{system_prompt:?}");
        }
    }

    #[tokio::test]
    async fn the_model_invoked_event_records_whether_context_was_delivered() {
        let (coordinator, project_id) = build_coordinator().await;
        let mut item = knowledge(project_id, solexa_domain::KnowledgeKind::Constraint, ROUND58_CONSTRAINT, solexa_domain::KnowledgeStatus::Accepted);
        item.project_id = project_id;
        coordinator.storage.insert_knowledge_item(&item).unwrap();
        let submission = coordinator
            .submit_request(UserRequest { project_id, text: "Refactor the Rust backend auth module.".into(), requested_by: "human".into() })
            .await
            .unwrap();
        let trace = coordinator.events_for_correlation(submission.correlation_id).unwrap();
        let invoked = trace.iter().find(|e| e.event_type == EventType::ModelInvoked).unwrap();
        assert_eq!(invoked.payload["context_delivered"], json!(true), "{:?}", invoked.payload);
        assert!(invoked.payload["context_chars"].as_u64().unwrap() as usize >= ROUND58_CONSTRAINT.len());
    }

    // ---- round 59: Specialist.knowledge_scope narrows what a specialist is given --

    fn scoped_items() -> Vec<solexa_domain::KnowledgeItem> {
        let mut auth = knowledge(Uuid::nil(), solexa_domain::KnowledgeKind::Constraint, "AUTH-RULE-5521", solexa_domain::KnowledgeStatus::Accepted);
        auth.tags = vec!["backend".into(), "auth".into()];
        let mut billing = knowledge(Uuid::nil(), solexa_domain::KnowledgeKind::Constraint, "BILLING-RULE-8832", solexa_domain::KnowledgeStatus::Accepted);
        billing.tags = vec!["backend".into(), "billing".into()];
        vec![auth, billing]
    }

    const READ_EXEC: fn() -> Vec<Permission> = || vec![Permission::Read, Permission::Execute];

    #[tokio::test]
    async fn a_knowledge_scope_limits_which_project_knowledge_the_specialist_is_given() {
        let unscoped = prompts_sent_scoped(scoped_items(), READ_EXEC(), 1, false, vec![]).await;
        assert!(unscoped[0].0.contains("AUTH-RULE-5521") && unscoped[0].0.contains("BILLING-RULE-8832"), "positive control: no scope = both: {:?}", unscoped[0].0);

        let scoped = prompts_sent_scoped(scoped_items(), READ_EXEC(), 1, false, vec!["auth".into()]).await;
        assert!(scoped[0].0.contains("AUTH-RULE-5521"), "{:?}", scoped[0].0);
        assert!(!scoped[0].0.contains("BILLING-RULE-8832"), "out-of-scope knowledge reached the model: {:?}", scoped[0].0);
    }

    #[tokio::test]
    async fn in_a_consensus_round_each_specialist_is_given_its_own_scope() {
        let sent = prompts_sent_with_scopes(scoped_items(), READ_EXEC(), 2, false, vec![vec!["auth".into()], vec!["billing".into()]]).await;
        assert_eq!(sent.len(), 2);
        let general0 = sent.iter().find(|(sp, _)| sp.contains("General 0")).unwrap();
        let general1 = sent.iter().find(|(sp, _)| sp.contains("General 1")).unwrap();
        assert!(general0.0.contains("AUTH-RULE-5521") && !general0.0.contains("BILLING-RULE-8832"), "{:?}", general0.0);
        assert!(general1.0.contains("BILLING-RULE-8832") && !general1.0.contains("AUTH-RULE-5521"), "{:?}", general1.0);
    }

    #[tokio::test]
    async fn the_model_invoked_event_records_whether_a_scope_was_applied() {
        for (scope, expected) in [(vec!["auth".to_string()], true), (vec![], false), (vec!["  ".to_string()], false)] {
            let (coordinator, project_id) = build_coordinator().await;
            for specialist in coordinator.storage.list_specialists(project_id).unwrap() {
                let mut specialist = specialist;
                specialist.knowledge_scope = scope.clone();
                coordinator.storage.update_specialist(&specialist).unwrap();
            }
            let submission = coordinator
                .submit_request(UserRequest { project_id, text: "Refactor the Rust backend auth module.".into(), requested_by: "human".into() })
                .await
                .unwrap();
            let trace = coordinator.events_for_correlation(submission.correlation_id).unwrap();
            let invoked = trace.iter().find(|e| e.event_type == EventType::ModelInvoked).unwrap();
            assert_eq!(invoked.payload["context_scoped"], json!(expected), "scope {scope:?}: {:?}", invoked.payload);
        }
    }

    #[tokio::test]
    async fn a_scope_matching_nothing_gives_the_original_prompt_and_blank_entries_mean_unrestricted() {
        let none = prompts_sent_scoped(scoped_items(), READ_EXEC(), 1, false, vec!["typography".into()]).await;
        assert_eq!(none[0].0, "You are the generalist, General 0 specialist. Domain: general.");
        let blank = prompts_sent_scoped(scoped_items(), READ_EXEC(), 1, false, vec!["".into(), "  ".into()]).await;
        assert!(blank[0].0.contains("AUTH-RULE-5521") && blank[0].0.contains("BILLING-RULE-8832"), "{:?}", blank[0].0);
    }

    #[tokio::test]
    async fn a_scope_cannot_widen_past_the_task_relevance_match_or_the_read_gate() {
        // tagged for the scope but NOT for the task's capability
        let mut item = knowledge(Uuid::nil(), solexa_domain::KnowledgeKind::Constraint, "SCOPE-ONLY-4410", solexa_domain::KnowledgeStatus::Accepted);
        item.tags = vec!["auth".into()];
        let sent = prompts_sent_scoped(vec![item], READ_EXEC(), 1, false, vec!["auth".into()]).await;
        assert!(!sent[0].0.contains("SCOPE-ONLY-4410"), "{:?}", sent[0].0);
        // a scope never grants Read
        let no_read = prompts_sent_scoped(scoped_items(), vec![Permission::Execute], 1, false, vec!["auth".into()]).await;
        assert_eq!(no_read[0].0, "You are the generalist, General 0 specialist. Domain: general.");
    }

    // ---- round 60: the resource budget can be resized while running --------

    struct CollectSink(StdArc<std::sync::Mutex<Vec<solexa_events::Event>>>);
    impl solexa_events::EventSink for CollectSink {
        fn record(&self, event: &solexa_events::Event) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(event.clone());
            Ok(())
        }
    }

    struct ResizeRig {
        coordinator: StdArc<Coordinator>,
        storage: StdArc<Storage>,
        project_id: Uuid,
        model: ModelMetadata,
        started: StdArc<std::sync::Mutex<Vec<String>>>,
        collected: StdArc<std::sync::Mutex<Vec<solexa_events::Event>>>,
    }

    impl ResizeRig {
        /// Two subtasks (backend + sql) that each take 300ms to generate, so
        /// the contention a test creates is real. `vram_mb` / `max_concurrent`
        /// choose which resource they contend on.
        fn new(vram_mb: u32, max_concurrent: usize) -> Self {
            Self::with_model(mock_model(), vram_mb, max_concurrent)
        }

        /// Same, but with a chosen model (e.g. one larger than the default budget).
        fn with_model(model: ModelMetadata, vram_mb: u32, max_concurrent: usize) -> Self {
            let storage = StdArc::new(Storage::in_memory().unwrap());
            let collected = StdArc::new(std::sync::Mutex::new(Vec::new()));
            let mut bus = EventBus::new();
            bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
            bus.add_sink(Box::new(CollectSink(collected.clone())));
            let events = StdArc::new(bus);
            let project_id = Uuid::new_v4();
            storage.insert_project(project_id, "Test Project", "").unwrap();
            storage.insert_model(&model).unwrap();
            storage.insert_specialist(project_id, &general_specialist(model.id)).unwrap();
            storage.insert_specialist(project_id, &sql_specialist(model.id)).unwrap();
            let started = StdArc::new(std::sync::Mutex::new(Vec::new()));
            let mut providers = ProviderRegistry::new();
            providers.register(StdArc::new(StartOrderProvider {
                model: model.clone(),
                delay: std::time::Duration::from_millis(300),
                started: started.clone(),
            }));
            let budget = solexa_resources::ResourceBudget { vram_mb, ram_mb: model.estimated_ram_mb * 8, max_concurrent };
            let coordinator = StdArc::new(Coordinator::with_resource_budget(storage.clone(), events, providers, budget));
            Self { coordinator, storage, project_id, model, started, collected }
        }

        fn submit(&self) -> tokio::task::JoinHandle<Result<Submission>> {
            let (coordinator, project_id) = (self.coordinator.clone(), self.project_id);
            tokio::spawn(async move {
                coordinator
                    .submit_request(UserRequest {
                        project_id,
                        text: "Refactor the Rust backend and write the SQL migration for the new auth table.".into(),
                        requested_by: "human".into(),
                    })
                    .await
            })
        }

        fn budget_events(&self) -> Vec<solexa_events::Event> {
            self.collected.lock().unwrap().iter().filter(|e| e.event_type == EventType::ResourceBudgetChanged).cloned().collect()
        }
    }

    fn budget_of(vram_mb: u32, ram_mb: u32, max_concurrent: usize) -> solexa_resources::ResourceBudget {
        solexa_resources::ResourceBudget { vram_mb, ram_mb, max_concurrent }
    }

    #[tokio::test]
    async fn growing_the_budget_live_lets_a_queued_subtask_start_now_and_records_the_change() {
        let rig = ResizeRig::new(mock_model().estimated_vram_mb, 4);
        let submit = rig.submit();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(rig.coordinator.resource_snapshot().queued, 1, "premise: one subtask is blocked on VRAM");
        assert_eq!(rig.started.lock().unwrap().len(), 1);

        let ram = rig.model.estimated_ram_mb * 8;
        let previous = rig.coordinator.set_resource_budget(rig.project_id, budget_of(rig.model.estimated_vram_mb * 2, ram, 4)).unwrap();
        assert_eq!(previous.vram_mb, rig.model.estimated_vram_mb);
        assert_eq!(rig.coordinator.resource_budget().vram_mb, rig.model.estimated_vram_mb * 2);

        // the queued subtask starts while the first is still generating (300ms delay)
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        assert_eq!(rig.started.lock().unwrap().len(), 2, "the grown budget must start the queued subtask immediately");
        assert_eq!(rig.coordinator.resource_snapshot().queued, 0);

        let submission = tokio::time::timeout(std::time::Duration::from_secs(5), submit).await.unwrap().unwrap().unwrap();
        let subtasks: Vec<Task> = rig.storage.list_tasks(rig.project_id).unwrap().into_iter().filter(|t| t.parent_task_id.is_some()).collect();
        assert!(subtasks.iter().all(|t| t.state == TaskState::Completed), "{:?}", subtasks.iter().map(|t| t.state).collect::<Vec<_>>());
        let _ = submission;

        let changes = rig.budget_events();
        assert_eq!(changes.len(), 1, "exactly one audit event per accepted resize");
        let payload = &changes[0].payload;
        assert_eq!(payload["previous"]["vram_mb"], json!(rig.model.estimated_vram_mb));
        assert_eq!(payload["current"]["vram_mb"], json!(rig.model.estimated_vram_mb * 2));
        assert_eq!(payload["current"]["max_concurrent"], json!(4));
    }

    #[tokio::test]
    async fn a_resize_that_would_strand_a_queued_task_is_refused_changes_nothing_and_is_not_recorded() {
        let rig = ResizeRig::new(mock_model().estimated_vram_mb, 4);
        let submit = rig.submit();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(rig.coordinator.resource_snapshot().queued, 1);

        let err = rig
            .coordinator
            .set_resource_budget(rig.project_id, budget_of(rig.model.estimated_vram_mb - 1, rig.model.estimated_ram_mb * 8, 4))
            .unwrap_err();
        assert!(err.to_string().contains("queued task"), "{err}");
        assert_eq!(rig.coordinator.resource_budget().vram_mb, rig.model.estimated_vram_mb, "a refused resize changes nothing");
        assert!(rig.budget_events().is_empty(), "a refused resize must not be recorded as a change");

        // clean up: cancel the queued subtask so the request can finish. (Subtask
        // descriptions are identical, so, like the round-49 test, the first
        // listed one is the one that started and the other is the queued one.)
        let subtasks: Vec<Task> = rig.storage.list_tasks(rig.project_id).unwrap().into_iter().filter(|t| t.parent_task_id.is_some()).collect();
        assert_eq!(subtasks.len(), 2);
        let queued_id = subtasks[1].id;
        rig.coordinator.cancel_task(queued_id).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), submit).await.unwrap().unwrap().unwrap();
    }

    #[tokio::test]
    async fn an_invalid_budget_is_refused_with_the_reason() {
        let (coordinator, project_id) = build_coordinator().await;
        let before = coordinator.resource_budget();
        let err = coordinator.set_resource_budget(project_id, budget_of(1_000, 1_000, 0)).unwrap_err();
        assert!(err.to_string().contains("at least 1"), "{err}");
        assert_eq!(coordinator.resource_budget().max_concurrent, before.max_concurrent);
    }

    #[tokio::test]
    async fn a_task_the_resize_could_not_see_fails_with_a_clear_message_not_as_cancelled() {
        // max_concurrent = 1: the second subtask waits on the concurrency permit,
        // invisible to the strand check. Shrinking VRAM under the running one
        // is allowed; when the second finally runs it can never fit.
        let rig = ResizeRig::new(1_000_000, 1);
        let submit = rig.submit();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(rig.started.lock().unwrap().len(), 1);
        assert_eq!(rig.coordinator.resource_snapshot().queued, 0, "premise: the second subtask is not in the visible queue");

        rig.coordinator
            .set_resource_budget(rig.project_id, budget_of(rig.model.estimated_vram_mb - 1, rig.model.estimated_ram_mb * 8, 1))
            .expect("nobody visible is stranded, so the shrink is accepted");

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), submit).await.expect("must not hang").unwrap();
        let message = outcome.expect_err("the stranded subtask must fail the request loudly").to_string();
        assert!(message.contains("resized") && message.contains("resource budget"), "{message}");
        assert!(!message.to_lowercase().contains("cancel"), "an over-budget model is a failure, not a cancellation: {message}");
        let after = rig.coordinator.resource_snapshot();
        assert_eq!((after.running, after.queued), (0, 0), "nothing may be left stuck");
    }

    #[tokio::test]
    async fn set_resource_limits_is_the_plain_number_form_of_set_resource_budget() {
        let rig = ResizeRig::new(8_000, 2);
        rig.coordinator.set_resource_limits(rig.project_id, 12_000, 40_000, 3).unwrap();
        let b = rig.coordinator.resource_budget();
        assert_eq!((b.vram_mb, b.ram_mb, b.max_concurrent), (12_000, 40_000, 3));
        assert_eq!(rig.budget_events().len(), 1);
        assert!(rig.coordinator.set_resource_limits(rig.project_id, 12_000, 40_000, 0).is_err());
        assert_eq!(rig.coordinator.resource_budget().max_concurrent, 3, "a refused call changes nothing");
    }

    // ---- round 61: the resource budget is remembered across restarts --------

    /// A coordinator over `storage` built the way the desktop shell builds it
    /// (`Coordinator::new`, no explicit budget).
    fn restart_over(storage: &StdArc<Storage>) -> Coordinator {
        Coordinator::new(storage.clone(), StdArc::new(EventBus::new()), ProviderRegistry::new())
    }

    #[tokio::test]
    async fn a_resized_budget_is_remembered_by_the_next_coordinator_over_the_same_storage() {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "P", "").unwrap();
        let first = restart_over(&storage);
        assert_eq!(first.resource_budget().max_concurrent, solexa_resources::ResourceBudget::default().max_concurrent, "premise: starts at the default");

        first.set_resource_limits(project_id, 12_345, 23_456, 7).unwrap();
        drop(first);

        let second = restart_over(&storage);
        let b = second.resource_budget();
        assert_eq!((b.vram_mb, b.ram_mb, b.max_concurrent), (12_345, 23_456, 7), "a restart must come back with the resized budget");
    }

    #[tokio::test]
    async fn an_explicit_budget_wins_over_a_saved_one() {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "P", "").unwrap();
        restart_over(&storage).set_resource_limits(project_id, 12_345, 23_456, 7).unwrap();

        let explicit = Coordinator::with_resource_budget(storage.clone(), StdArc::new(EventBus::new()), ProviderRegistry::new(), budget_of(1_000, 2_000, 2));
        assert_eq!(explicit.resource_budget().vram_mb, 1_000, "a caller that passes a budget is never overridden by a saved one");
    }

    #[tokio::test]
    async fn a_corrupt_or_invalid_saved_budget_falls_back_to_the_default_instead_of_failing_startup() {
        let default = solexa_resources::ResourceBudget::default();
        for bad in [
            "not json at all",
            "{}",
            r#"{"vram_mb":1000,"ram_mb":1000}"#,
            r#"{"vram_mb":"lots","ram_mb":1000,"max_concurrent":2}"#,
            r#"{"vram_mb":1000,"ram_mb":1000,"max_concurrent":0}"#,
            r#"{"vram_mb":1000,"ram_mb":1000,"max_concurrent":9999}"#,
            r#"{"vram_mb":-5,"ram_mb":1000,"max_concurrent":2}"#,
            r#"{"vram_mb":99999999999,"ram_mb":1000,"max_concurrent":2}"#,
        ] {
            let storage = StdArc::new(Storage::in_memory().unwrap());
            storage.set_setting("resource_budget", bad).unwrap();
            let coordinator = restart_over(&storage);
            let b = coordinator.resource_budget();
            assert_eq!((b.vram_mb, b.ram_mb, b.max_concurrent), (default.vram_mb, default.ram_mb, default.max_concurrent), "saved value {bad:?} must be ignored");
        }
        // positive control: a valid saved value (including CPU-only zero memory) IS applied
        let storage = StdArc::new(Storage::in_memory().unwrap());
        storage.set_setting("resource_budget", r#"{"vram_mb":0,"ram_mb":0,"max_concurrent":1}"#).unwrap();
        let b = restart_over(&storage).resource_budget();
        assert_eq!((b.vram_mb, b.ram_mb, b.max_concurrent), (0, 0, 1));
    }

    #[tokio::test]
    async fn a_refused_resize_is_not_saved_and_an_accepted_one_says_it_was() {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "P", "").unwrap();
        let coordinator = restart_over(&storage);
        assert!(coordinator.set_resource_limits(project_id, 1_000, 1_000, 0).is_err());
        assert_eq!(storage.get_setting("resource_budget").unwrap(), None, "a refused resize must not be saved");

        coordinator.set_resource_limits(project_id, 4_000, 5_000, 3).unwrap();
        let saved = storage.get_setting("resource_budget").unwrap().expect("an accepted resize is saved");
        let saved: serde_json::Value = serde_json::from_str(&saved).unwrap();
        assert_eq!((saved["vram_mb"].as_u64(), saved["ram_mb"].as_u64(), saved["max_concurrent"].as_u64()), (Some(4_000), Some(5_000), Some(3)));
    }

    #[tokio::test]
    async fn the_budget_changed_event_records_that_it_was_saved() {
        let rig = ResizeRig::new(8_000, 2);
        rig.coordinator.set_resource_limits(rig.project_id, 9_000, 40_000, 2).unwrap();
        let events = rig.budget_events();
        assert_eq!(events[0].payload["persisted"], json!(true), "{:?}", events[0].payload);
    }

    #[tokio::test]
    async fn if_saving_fails_the_change_still_applies_but_the_caller_and_the_event_say_so() {
        let rig = ResizeRig::new(8_000, 2);
        rig.storage.sabotage_settings_table_for_tests();
        let err = rig.coordinator.set_resource_limits(rig.project_id, 9_000, 40_000, 3).unwrap_err().to_string();
        assert!(err.contains("could not be saved") && err.contains("revert on restart"), "{err}");
        assert_eq!(rig.coordinator.resource_budget().vram_mb, 9_000, "the live change applied for this session");
        let events = rig.budget_events();
        assert_eq!(events.len(), 1, "the change is still audited");
        assert_eq!(events[0].payload["persisted"], json!(false), "{:?}", events[0].payload);
    }

    // ---- round 62: reset the resource budget to its default -------------------

    #[tokio::test]
    async fn resetting_restores_the_default_live_and_forgets_the_saved_override() {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "P", "").unwrap();
        let coordinator = restart_over(&storage);
        coordinator.set_resource_limits(project_id, 1_111, 2_222, 3).unwrap();
        assert!(storage.get_setting("resource_budget").unwrap().is_some(), "premise: an override is saved");

        let previous = coordinator.reset_resource_budget(project_id).unwrap();
        assert_eq!((previous.vram_mb, previous.max_concurrent), (1_111, 3), "returns what it replaced");
        let default = solexa_resources::ResourceBudget::default();
        let now = coordinator.resource_budget();
        assert_eq!((now.vram_mb, now.ram_mb, now.max_concurrent), (default.vram_mb, default.ram_mb, default.max_concurrent));
        assert_eq!(storage.get_setting("resource_budget").unwrap(), None, "the saved override must be deleted, not overwritten with the default");

        let after_restart = restart_over(&storage).resource_budget();
        assert_eq!(after_restart.vram_mb, default.vram_mb, "a restart must stay at the default");
    }

    #[tokio::test]
    async fn a_reset_is_audited_as_a_reset() {
        let rig = ResizeRig::new(8_000, 2);
        rig.coordinator.set_resource_limits(rig.project_id, 9_000, 40_000, 2).unwrap();
        rig.coordinator.reset_resource_budget(rig.project_id).unwrap();
        let events = rig.budget_events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].payload["reset"], json!(false), "an ordinary resize is not a reset: {:?}", events[0].payload);
        assert_eq!(events[1].payload["reset"], json!(true), "{:?}", events[1].payload);
        assert_eq!(events[1].payload["persisted"], json!(true));
        assert_eq!(events[1].payload["previous"]["vram_mb"], json!(9_000));
    }

    #[tokio::test]
    async fn resetting_when_nothing_is_saved_is_harmless() {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "P", "").unwrap();
        let coordinator = restart_over(&storage);
        coordinator.reset_resource_budget(project_id).unwrap();
        assert_eq!(coordinator.resource_budget().max_concurrent, solexa_resources::ResourceBudget::default().max_concurrent);
    }

    #[tokio::test]
    async fn a_reset_that_would_strand_a_queued_task_is_refused_and_keeps_the_saved_override() {
        // A model bigger than the default budget, running under an enlarged one.
        let default = solexa_resources::ResourceBudget::default();
        let mut big = mock_model();
        big.estimated_vram_mb = default.vram_mb + 6_000;
        let rig = ResizeRig::with_model(big.clone(), big.estimated_vram_mb, 4);
        rig.coordinator.set_resource_limits(rig.project_id, big.estimated_vram_mb, big.estimated_ram_mb * 8, 4).unwrap();
        let submit = rig.submit();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(rig.coordinator.resource_snapshot().queued, 1, "premise: a subtask is queued, needing more than the default allows");

        let err = rig.coordinator.reset_resource_budget(rig.project_id).unwrap_err();
        assert!(err.to_string().contains("queued task"), "{err}");
        assert_eq!(rig.coordinator.resource_budget().vram_mb, big.estimated_vram_mb, "a refused reset changes nothing");
        assert!(rig.storage.get_setting("resource_budget").unwrap().is_some(), "a refused reset must not delete the saved override");
        assert_eq!(rig.budget_events().len(), 1, "only the earlier resize was recorded, not the refused reset");

        let subtasks: Vec<Task> = rig.storage.list_tasks(rig.project_id).unwrap().into_iter().filter(|t| t.parent_task_id.is_some()).collect();
        rig.coordinator.cancel_task(subtasks[1].id).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), submit).await.unwrap().unwrap().unwrap();
    }

    #[tokio::test]
    async fn if_clearing_the_saved_value_fails_the_reset_still_applies_but_says_the_old_budget_returns() {
        let rig = ResizeRig::new(8_000, 2);
        rig.storage.sabotage_settings_table_for_tests();
        let err = rig.coordinator.reset_resource_budget(rig.project_id).unwrap_err().to_string();
        assert!(err.contains("could not be cleared") && err.contains("restart"), "{err}");
        assert_eq!(rig.coordinator.resource_budget().vram_mb, solexa_resources::ResourceBudget::default().vram_mb, "the live reset applied");
        assert_eq!(rig.budget_events()[0].payload["persisted"], json!(false));
    }

    // ---- round 63: capability / tag matching ignores case and whitespace -----

    #[tokio::test]
    async fn a_specialist_whose_capability_was_typed_with_capitals_is_still_routed_to() {
        let (coordinator, project_id) = build_coordinator().await;
        for mut specialist in coordinator.storage.list_specialists(project_id).unwrap() {
            specialist.capabilities = specialist.capabilities.iter().map(|c| format!(" {} ", c.to_uppercase())).collect();
            coordinator.storage.update_specialist(&specialist).unwrap();
        }
        let submission = coordinator
            .submit_request(UserRequest { project_id, text: "Refactor the Rust backend auth module.".into(), requested_by: "human".into() })
            .await
            .expect("the planner's `backend` must still find a specialist typed ` BACKEND `");
        let trace = coordinator.events_for_correlation(submission.correlation_id).unwrap();
        assert!(trace.iter().any(|e| e.event_type == EventType::ModelInvoked), "the task must actually have run");
    }

    #[tokio::test]
    async fn knowledge_tagged_in_a_different_case_reaches_the_model() {
        let mut item = knowledge(Uuid::nil(), solexa_domain::KnowledgeKind::Constraint, "CASE-RULE-7714", solexa_domain::KnowledgeStatus::Accepted);
        item.tags = vec!["Backend".into()];
        let sent = prompts_sent_for(vec![item], READ_EXEC(), 1, false).await;
        assert!(sent[0].0.contains("CASE-RULE-7714"), "a `Backend` tag must match the task's `backend`: {:?}", sent[0].0);
    }

    #[tokio::test]
    async fn a_knowledge_scope_typed_in_a_different_case_still_narrows_correctly() {
        let mut auth = knowledge(Uuid::nil(), solexa_domain::KnowledgeKind::Constraint, "AUTH-CASE-1", solexa_domain::KnowledgeStatus::Accepted);
        auth.tags = vec!["backend".into(), "auth".into()];
        let mut billing = knowledge(Uuid::nil(), solexa_domain::KnowledgeKind::Constraint, "BILLING-CASE-2", solexa_domain::KnowledgeStatus::Accepted);
        billing.tags = vec!["backend".into(), "billing".into()];
        let sent = prompts_sent_scoped(vec![auth, billing], READ_EXEC(), 1, false, vec!["AUTH".into()]).await;
        assert!(sent[0].0.contains("AUTH-CASE-1") && !sent[0].0.contains("BILLING-CASE-2"), "{:?}", sent[0].0);
    }

    // ---- round 64: a task's owner is a MEMBER, not a specialist ----------------

    /// The one task created for a single-capability request, after it ran.
    fn only_task(coordinator: &Coordinator, project_id: Uuid) -> Task {
        let tasks = coordinator.storage.list_tasks(project_id).unwrap();
        assert_eq!(tasks.len(), 1, "premise: a single-capability request makes one task");
        tasks.into_iter().next().unwrap()
    }

    fn backend_specialist_id(coordinator: &Coordinator, project_id: Uuid) -> Uuid {
        coordinator.storage.list_specialists(project_id).unwrap().into_iter().find(|s| s.has_capability("backend")).unwrap().id
    }

    async fn run_backend_request(coordinator: &Coordinator, project_id: Uuid) -> Submission {
        coordinator
            .submit_request(UserRequest { project_id, text: "Refactor the Rust backend auth module.".into(), requested_by: "human".into() })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn the_owner_of_a_task_is_the_member_who_runs_its_specialist_not_the_specialist() {
        let (coordinator, project_id) = build_coordinator().await;
        let specialist_id = backend_specialist_id(&coordinator, project_id);
        let mut member = solexa_domain::Member::new_ai(project_id, "Ava", "backend AI");
        member.specialist_ids = vec![specialist_id];
        coordinator.storage.insert_member(&member).unwrap();

        run_backend_request(&coordinator, project_id).await;
        let task = only_task(&coordinator, project_id);
        assert_eq!(task.assigned_specialist_id, Some(specialist_id), "premise: the task ran on that specialist");
        assert_eq!(task.owner_id, Some(member.id), "owner_id is documented as a MEMBER id");
        assert_ne!(task.owner_id, Some(specialist_id), "a specialist id in the member slot matches nothing");
    }

    #[tokio::test]
    async fn a_specialist_no_member_runs_leaves_the_task_unowned_rather_than_wrongly_owned() {
        let (coordinator, project_id) = build_coordinator().await;
        // a member exists, but does not run the backend specialist
        let other = solexa_domain::Member::new_ai(project_id, "Unrelated", "");
        coordinator.storage.insert_member(&other).unwrap();
        run_backend_request(&coordinator, project_id).await;
        assert_eq!(only_task(&coordinator, project_id).owner_id, None);
    }

    #[tokio::test]
    async fn a_soft_deleted_member_never_owns_new_work() {
        let (coordinator, project_id) = build_coordinator().await;
        let specialist_id = backend_specialist_id(&coordinator, project_id);
        let mut gone = solexa_domain::Member::new_ai(project_id, "Gone", "");
        gone.specialist_ids = vec![specialist_id];
        gone.deleted_at = Some(chrono::Utc::now());
        coordinator.storage.insert_member(&gone).unwrap();
        run_backend_request(&coordinator, project_id).await;
        assert_eq!(only_task(&coordinator, project_id).owner_id, None, "a deleted member must not be made accountable");

        // positive control: the same setup with a live member does assign
        let (coordinator, project_id) = build_coordinator().await;
        let specialist_id = backend_specialist_id(&coordinator, project_id);
        let mut live = solexa_domain::Member::new_ai(project_id, "Live", "");
        live.specialist_ids = vec![specialist_id];
        coordinator.storage.insert_member(&live).unwrap();
        run_backend_request(&coordinator, project_id).await;
        assert_eq!(only_task(&coordinator, project_id).owner_id, Some(live.id));
    }

    #[tokio::test]
    async fn when_several_members_run_the_same_specialist_the_pick_is_deterministic() {
        for _ in 0..3 {
            let (coordinator, project_id) = build_coordinator().await;
            let specialist_id = backend_specialist_id(&coordinator, project_id);
            let mut ids = Vec::new();
            for name in ["A", "B", "C"] {
                let mut m = solexa_domain::Member::new_ai(project_id, name, "");
                m.specialist_ids = vec![specialist_id];
                ids.push(m.id);
                coordinator.storage.insert_member(&m).unwrap();
            }
            run_backend_request(&coordinator, project_id).await;
            assert_eq!(only_task(&coordinator, project_id).owner_id, ids.iter().min().copied(), "smallest member id wins ties");
        }
    }

    #[tokio::test]
    async fn each_subtask_is_owned_by_the_member_running_its_own_specialist() {
        let (coordinator, project_id) = build_coordinator().await;
        let specialists = coordinator.storage.list_specialists(project_id).unwrap();
        let mut by_capability = std::collections::HashMap::new();
        for (cap, name) in [("backend", "Backend Owner"), ("sql", "SQL Owner")] {
            let specialist = specialists.iter().find(|s| s.has_capability(cap)).unwrap();
            let mut m = solexa_domain::Member::new_ai(project_id, name, "");
            m.specialist_ids = vec![specialist.id];
            coordinator.storage.insert_member(&m).unwrap();
            by_capability.insert(specialist.id, m.id);
        }
        coordinator
            .submit_request(UserRequest { project_id, text: "Refactor the Rust backend and write the SQL migration for the new auth table.".into(), requested_by: "human".into() })
            .await
            .unwrap();
        let subtasks: Vec<Task> = coordinator.storage.list_tasks(project_id).unwrap().into_iter().filter(|t| t.parent_task_id.is_some()).collect();
        assert_eq!(subtasks.len(), 2);
        for task in subtasks {
            let expected = by_capability[&task.assigned_specialist_id.unwrap()];
            assert_eq!(task.owner_id, Some(expected), "subtask owner must follow ITS specialist");
        }
    }

    #[tokio::test]
    async fn the_task_assigned_event_names_the_owning_member() {
        let (coordinator, project_id) = build_coordinator().await;
        let specialist_id = backend_specialist_id(&coordinator, project_id);
        let mut member = solexa_domain::Member::new_ai(project_id, "Ava", "");
        member.specialist_ids = vec![specialist_id];
        coordinator.storage.insert_member(&member).unwrap();
        let submission = run_backend_request(&coordinator, project_id).await;
        let trace = coordinator.events_for_correlation(submission.correlation_id).unwrap();
        let assigned = trace.iter().find(|e| e.event_type == EventType::TaskAssigned).unwrap();
        assert_eq!(assigned.payload["owner_member_id"], json!(member.id), "{:?}", assigned.payload);
        // and with no owning member the field is an explicit null, not absent or a specialist id
        let (coordinator, project_id) = build_coordinator().await;
        let submission = run_backend_request(&coordinator, project_id).await;
        let trace = coordinator.events_for_correlation(submission.correlation_id).unwrap();
        let assigned = trace.iter().find(|e| e.event_type == EventType::TaskAssigned).unwrap();
        assert_eq!(assigned.payload["owner_member_id"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn with_equally_capable_specialists_the_oldest_is_primary_and_its_member_owns_the_task() {
        let (coordinator, project_id) = build_coordinator().await;
        let oldest = coordinator.storage.list_specialists(project_id).unwrap().into_iter().find(|s| s.has_capability("backend")).unwrap();
        // A NEWER, equally capable specialist whose id sorts BEFORE every random uuid,
        // so "smallest id" and "oldest" disagree.
        let mut newer = oldest.clone();
        newer.id = Uuid::from_u128(1);
        newer.name = "Newer backend specialist".into();
        coordinator.storage.insert_specialist(project_id, &newer).unwrap();
        let mut owner_of_oldest = solexa_domain::Member::new_ai(project_id, "Owner of oldest", "");
        owner_of_oldest.specialist_ids = vec![oldest.id];
        let mut owner_of_newer = solexa_domain::Member::new_ai(project_id, "Owner of newer", "");
        owner_of_newer.id = Uuid::from_u128(2); // also sorts first
        owner_of_newer.specialist_ids = vec![newer.id];
        coordinator.storage.insert_member(&owner_of_oldest).unwrap();
        coordinator.storage.insert_member(&owner_of_newer).unwrap();

        let submission = run_backend_request(&coordinator, project_id).await;
        let task = only_task(&coordinator, project_id);
        let trace = coordinator.events_for_correlation(submission.correlation_id).unwrap();
        let assigned = trace.iter().find(|e| e.event_type == EventType::TaskAssigned).unwrap();
        assert_eq!(assigned.payload["mode"], json!("consensus"), "premise: both specialists were consulted");
        assert_eq!(task.assigned_specialist_id, Some(oldest.id), "ties on track record go to the oldest specialist");
        assert_eq!(task.owner_id, Some(owner_of_oldest.id), "and the owner follows the primary");
    }

    // ---- round 65: concurrent UI actions must not lose each other's updates ----
    //
    // Each mutator below reads a whole row, changes one field and writes the
    // whole row back. Two of them running at once on the same row can each
    // read the same starting copy, so the second write silently reverts the
    // first. These tests run real OS threads released together by a barrier.

    const STRESS_ROUNDS: usize = 40;

    /// Runs every closure on its own blocking-pool thread, all released at once.
    async fn run_concurrently(jobs: Vec<Box<dyn FnOnce() + Send + 'static>>) {
        let barrier = StdArc::new(std::sync::Barrier::new(jobs.len()));
        let handles: Vec<_> = jobs
            .into_iter()
            .map(|job| {
                let barrier = barrier.clone();
                tokio::task::spawn_blocking(move || {
                    barrier.wait();
                    job();
                })
            })
            .collect();
        for h in handles {
            h.await.unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrently_assigning_roles_to_one_member_loses_none() {
        let (coordinator, project_id) = build_coordinator().await;
        let coordinator = StdArc::new(coordinator);
        for round in 0..STRESS_ROUNDS {
            let member = solexa_domain::Member::new_ai(project_id, format!("m{round}"), "");
            coordinator.storage.insert_member(&member).unwrap();
            let roles: Vec<Uuid> = (0..8).map(|i| coordinator.create_role(project_id, format!("r{round}-{i}"), vec![Permission::Read]).unwrap().id).collect();
            let jobs = roles
                .iter()
                .map(|role_id| {
                    let (c, m, r) = (coordinator.clone(), member.id, *role_id);
                    Box::new(move || c.assign_role(m, r).unwrap()) as Box<dyn FnOnce() + Send>
                })
                .collect();
            run_concurrently(jobs).await;
            let stored = coordinator.storage.get_member(member.id).unwrap().unwrap();
            let mut have = stored.role_ids.clone();
            have.sort();
            let mut want = roles.clone();
            want.sort();
            assert_eq!(have, want, "round {round}: a concurrent assign_role overwrote another's role");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrently_adding_members_to_one_team_loses_none() {
        let (coordinator, project_id) = build_coordinator().await;
        let coordinator = StdArc::new(coordinator);
        for round in 0..STRESS_ROUNDS {
            let team = solexa_domain::Team::new(project_id, format!("t{round}"), "");
            coordinator.storage.insert_team(&team).unwrap();
            let members: Vec<Uuid> = (0..8)
                .map(|i| {
                    let m = solexa_domain::Member::new_ai(project_id, format!("m{round}-{i}"), "");
                    coordinator.storage.insert_member(&m).unwrap();
                    m.id
                })
                .collect();
            let jobs = members
                .iter()
                .map(|member_id| {
                    let (c, t, m) = (coordinator.clone(), team.id, *member_id);
                    Box::new(move || c.add_member_to_team(t, m).unwrap()) as Box<dyn FnOnce() + Send>
                })
                .collect();
            run_concurrently(jobs).await;
            let mut have = coordinator.storage.get_team(team.id).unwrap().unwrap().member_ids;
            have.sort();
            let mut want = members.clone();
            want.sort();
            assert_eq!(have, want, "round {round}: a concurrent add_member_to_team overwrote another's membership");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn deactivating_a_specialist_while_a_role_is_assigned_keeps_both_changes() {
        let (coordinator, project_id) = build_coordinator().await;
        let coordinator = StdArc::new(coordinator);
        let specialist_id = backend_specialist_id(&coordinator, project_id);
        for round in 0..STRESS_ROUNDS {
            let role = coordinator.create_role(project_id, format!("r{round}"), vec![Permission::Read]).unwrap();
            let (c1, c2) = (coordinator.clone(), coordinator.clone());
            let (role_id, active) = (role.id, round % 2 == 0);
            run_concurrently(vec![
                Box::new(move || c1.set_specialist_active(project_id, specialist_id, active).unwrap()),
                Box::new(move || c2.assign_role_to_specialist(specialist_id, role_id).unwrap()),
            ])
            .await;
            let stored = coordinator.storage.get_specialist(specialist_id).unwrap().unwrap();
            assert!(stored.role_ids.contains(&role.id), "round {round}: the role assignment was overwritten by the active toggle");
            assert_eq!(stored.active, active, "round {round}: the active toggle was overwritten by the role assignment");
        }
    }

    // -- the entity gate itself (deterministic, unlike the stress tests above) --

    async fn member_and_role(coordinator: &Coordinator, project_id: Uuid) -> (Uuid, Uuid) {
        let member = solexa_domain::Member::new_ai(project_id, "gate-member", "");
        coordinator.storage.insert_member(&member).unwrap();
        let role = coordinator.create_role(project_id, "gate-role", vec![Permission::Read]).unwrap();
        (member.id, role.id)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_row_mutator_waits_while_another_thread_holds_the_entity_gate() {
        let (coordinator, project_id) = build_coordinator().await;
        let coordinator = StdArc::new(coordinator);
        let (member_id, role_id) = member_and_role(&coordinator, project_id).await;

        let guard = coordinator.entity_gate();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = {
            let c = coordinator.clone();
            std::thread::spawn(move || {
                c.assign_role(member_id, role_id).unwrap();
                tx.send(()).unwrap();
            })
        };
        assert!(rx.recv_timeout(std::time::Duration::from_millis(150)).is_err(), "assign_role must wait for the gate, not run through it");
        assert!(coordinator.storage.get_member(member_id).unwrap().unwrap().role_ids.is_empty(), "and must not have written anything yet");
        drop(guard);
        rx.recv_timeout(std::time::Duration::from_secs(5)).expect("once the gate is released the mutator must proceed");
        worker.join().unwrap();
        assert_eq!(coordinator.storage.get_member(member_id).unwrap().unwrap().role_ids, vec![role_id]);

        // Phase 2: the SAME thread takes the gate a second time. If releasing
        // the first guard had left this thread's re-entrancy depth above zero,
        // this acquisition would silently skip the lock and the worker below
        // would sail through.
        let guard = coordinator.entity_gate();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = {
            let c = coordinator.clone();
            std::thread::spawn(move || {
                c.unassign_role(member_id, role_id).unwrap();
                tx.send(()).unwrap();
            })
        };
        assert!(rx.recv_timeout(std::time::Duration::from_millis(150)).is_err(), "the gate must be exclusive again after a full release");
        drop(guard);
        rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        worker.join().unwrap();
    }

    #[tokio::test]
    async fn the_gate_is_reentrant_so_a_mutator_may_call_another_while_holding_it() {
        let (coordinator, project_id) = build_coordinator().await;
        let (member_id, role_id) = member_and_role(&coordinator, project_id).await;
        let _outer = coordinator.entity_gate();
        let _inner = coordinator.entity_gate();
        // would deadlock forever if the gate were an ordinary mutex
        coordinator.assign_role(member_id, role_id).unwrap();
        coordinator.unassign_role(member_id, role_id).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_gate_is_released_when_a_holder_panics_and_does_not_poison_later_use() {
        let (coordinator, project_id) = build_coordinator().await;
        let coordinator = StdArc::new(coordinator);
        let (member_id, role_id) = member_and_role(&coordinator, project_id).await;
        let c = coordinator.clone();
        let result = std::thread::spawn(move || {
            let _gate = c.entity_gate();
            panic!("a mutator panicked while holding the gate");
        })
        .join();
        assert!(result.is_err(), "premise: the thread panicked");
        let c = coordinator.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            c.assign_role(member_id, role_id).unwrap();
            tx.send(()).unwrap();
        });
        rx.recv_timeout(std::time::Duration::from_secs(5)).expect("a panic must not wedge every later UI action");
    }

    #[test]
    fn every_function_that_rewrites_a_whole_row_takes_the_entity_gate() {
        // A lint over this file's own source. Whole-row `update_*` calls are
        // read-modify-write, so each caller must hold the gate, and a mutator
        // added next year will be held to the same rule.
        let src = include_str!("lib.rs");
        let code = &src[..src.find("#[cfg(test)]\nmod tests").expect("tests module marker")];
        let starts: Vec<(usize, &str)> = code
            .match_indices("\n    ")
            .filter_map(|(i, _)| {
                let rest = &code[i + 5..];
                let rest = rest.strip_prefix("pub ").unwrap_or(rest);
                let rest = rest.strip_prefix("async ").unwrap_or(rest);
                let rest = rest.strip_prefix("fn ")?;
                let name = rest.split('(').next()?;
                (!name.contains(' ')).then_some((i, name))
            })
            .collect();
        let gated_writes = [
            "update_member(", "update_team(", "update_specialist(", "update_role(", "update_decision(",
            "update_conflict(", "update_knowledge_item_status(", "update_pending_sync_change(",
        ];
        let mut missing = Vec::new();
        let mut checked = 0;
        for (n, (start, name)) in starts.iter().enumerate() {
            let end = starts.get(n + 1).map(|(s, _)| *s).unwrap_or(code.len());
            let body = &code[*start..end];
            if gated_writes.iter().any(|w| body.contains(&format!("self.storage.{w}"))) {
                checked += 1;
                if !body.contains("self.entity_gate()") {
                    missing.push(*name);
                }
            }
        }
        assert!(checked >= 20, "the lint found only {checked} mutators, its scan is broken, not the code");
        assert!(missing.is_empty(), "whole-row writers that do not take the entity gate: {missing:?}");
    }

    // ---- round 55: opt-in role enforcement on approver actions ---------

    #[tokio::test]
    async fn enabling_role_enforcement_is_refused_when_nobody_could_approve_and_disabling_is_always_allowed() {
        let (coordinator, project_id) = build_coordinator().await;
        assert!(!coordinator.enforce_approver_roles(project_id).unwrap(), "default-off for every project");

        // Nobody holds Approve: turning it on would dead-end every approval.
        let err = coordinator.set_enforce_approver_roles(project_id, true).unwrap_err();
        assert!(err.to_string().contains("no member of this project holds Permission::Approve"), "{err}");
        assert!(!coordinator.enforce_approver_roles(project_id).unwrap(), "a refused enable must change nothing");

        // A role that grants only Review isn't enough...
        let alex = solexa_domain::Member::new_human(project_id, "Alex");
        coordinator.register_member(alex.clone()).unwrap();
        let reviewer_role = coordinator.create_role(project_id, "Reviewer", vec![Permission::Review]).unwrap();
        coordinator.assign_role(alex.id, reviewer_role.id).unwrap();
        assert!(coordinator.set_enforce_approver_roles(project_id, true).is_err());

        // ...one that grants Approve is (positive control).
        let approver_role = coordinator.create_role(project_id, "Approver", vec![Permission::Approve]).unwrap();
        coordinator.assign_role(alex.id, approver_role.id).unwrap();
        coordinator.set_enforce_approver_roles(project_id, true).unwrap();
        assert!(coordinator.enforce_approver_roles(project_id).unwrap());

        // Even if the approver later loses the role, turning it OFF always works.
        coordinator.unassign_role(alex.id, approver_role.id).unwrap();
        coordinator.set_enforce_approver_roles(project_id, false).unwrap();
        assert!(!coordinator.enforce_approver_roles(project_id).unwrap());
    }

    #[tokio::test]
    async fn with_enforcement_on_a_member_without_approve_cannot_resolve_a_decision_but_an_approver_can() {
        let (coordinator, project_id) = build_coordinator().await;
        let decision = solexa_domain::Decision::new(
            project_id,
            "Which database should be used?",
            vec!["PostgreSQL".into(), "SQLite".into()],
            vec!["backend".into()],
        );
        coordinator.record_decision(decision.clone()).unwrap();

        let plain = solexa_domain::Member::new_human(project_id, "Plain");
        let approver = solexa_domain::Member::new_human(project_id, "Approver");
        coordinator.register_member(plain.clone()).unwrap();
        coordinator.register_member(approver.clone()).unwrap();
        let role = coordinator.create_role(project_id, "Approver", vec![Permission::Approve]).unwrap();
        coordinator.assign_role(approver.id, role.id).unwrap();

        // Enforcement OFF (the default): the role-less member can resolve,
        // proven on a second decision so this one stays available below.
        let other = solexa_domain::Decision::new(project_id, "Which cache?", vec!["Redis".into(), "None".into()], vec![]);
        coordinator.record_decision(other.clone()).unwrap();
        coordinator.resolve_decision(other.id, "Redis", plain.id).expect("behavior is unchanged until a project opts in");

        coordinator.set_enforce_approver_roles(project_id, true).unwrap();

        let err = coordinator.resolve_decision(decision.id, "PostgreSQL", plain.id).unwrap_err();
        assert!(err.to_string().contains("isn't authorized to resolve a decision"), "{err}");
        assert!(err.to_string().contains("Permission::Approve"), "{err}");
        let still_open = coordinator.list_decisions(project_id).unwrap().into_iter().find(|d| d.id == decision.id).unwrap();
        assert_eq!(still_open.status, solexa_domain::DecisionStatus::Open, "a refused attempt must not write anything");
        assert!(still_open.resolution.is_none() && still_open.approved_by.is_none());

        coordinator.resolve_decision(decision.id, "PostgreSQL", approver.id).expect("a member holding Approve may resolve");
        let resolved = coordinator.list_decisions(project_id).unwrap().into_iter().find(|d| d.id == decision.id).unwrap();
        assert_eq!(resolved.approved_by.as_deref(), Some("Approver"));
    }

    #[tokio::test]
    async fn with_enforcement_on_a_member_without_approve_cannot_resolve_a_task_conflict_or_a_sync_conflict() {
        let (coordinator, project_id, specialist_a_id, _b) = build_coordinator_with_disagreeing_backend_specialists().await;
        let submission = coordinator
            .submit_request(UserRequest { project_id, text: "Refactor the Rust backend auth module.".into(), requested_by: "human".into() })
            .await
            .unwrap();
        let conflict = coordinator.list_conflicts(project_id).unwrap().into_iter().next().unwrap();

        let plain = solexa_domain::Member::new_human(project_id, "Plain");
        let approver = solexa_domain::Member::new_human(project_id, "Approver");
        coordinator.register_member(plain.clone()).unwrap();
        coordinator.register_member(approver.clone()).unwrap();
        let role = coordinator.create_role(project_id, "Approver", vec![Permission::Approve]).unwrap();
        coordinator.assign_role(approver.id, role.id).unwrap();
        coordinator.set_enforce_approver_roles(project_id, true).unwrap();

        // Task conflict: refused, and NOTHING changed, conflict still open,
        // task not completed.
        let err = coordinator.resolve_conflict(conflict.id, specialist_a_id, plain.id).unwrap_err();
        assert!(err.to_string().contains("isn't authorized to resolve a conflict"), "{err}");
        assert_eq!(coordinator.get_conflict(conflict.id).unwrap().unwrap().status, solexa_domain::ConflictStatus::Open);
        assert_ne!(coordinator.get_task(submission.task_id).unwrap().unwrap().state, TaskState::Completed);

        // Sync conflict: the gate runs BEFORE anything else, so a task
        // conflict handed to `resolve_sync_conflict` shows which check fired.
        // Role-less -> the permission error; approver -> gets PAST the gate
        // and hits the later "no sync_object" error instead (positive control).
        let denied = coordinator.resolve_sync_conflict(conflict.id, specialist_a_id, plain.id).unwrap_err();
        assert!(denied.to_string().contains("isn't authorized to resolve a sync conflict"), "{denied}");
        let past_gate = coordinator.resolve_sync_conflict(conflict.id, specialist_a_id, approver.id).unwrap_err();
        assert!(past_gate.to_string().contains("no sync_object"), "the approver must get past the permission gate: {past_gate}");

        // And the approver can resolve the task conflict (positive control).
        coordinator.resolve_conflict(conflict.id, specialist_a_id, approver.id).unwrap();
        assert_eq!(coordinator.get_task(submission.task_id).unwrap().unwrap().state, TaskState::Completed);
    }

    #[tokio::test]
    async fn role_enforcement_is_per_project_and_leaves_other_projects_unchanged() {
        let (coordinator, project_a) = build_coordinator().await;
        let project_b = Uuid::new_v4();
        coordinator.storage.insert_project(project_b, "Other Project", "").unwrap();

        // Enforce in A (needs an approver there first).
        let approver = solexa_domain::Member::new_human(project_a, "Approver A");
        coordinator.register_member(approver.clone()).unwrap();
        let role = coordinator.create_role(project_a, "Approver", vec![Permission::Approve]).unwrap();
        coordinator.assign_role(approver.id, role.id).unwrap();
        coordinator.set_enforce_approver_roles(project_a, true).unwrap();

        // B did not opt in: a role-less member there still resolves decisions.
        let decision_b = solexa_domain::Decision::new(project_b, "Which?", vec!["X".into(), "Y".into()], vec![]);
        coordinator.record_decision(decision_b.clone()).unwrap();
        let plain_b = solexa_domain::Member::new_human(project_b, "Plain B");
        coordinator.register_member(plain_b.clone()).unwrap();
        assert!(!coordinator.enforce_approver_roles(project_b).unwrap());
        coordinator.resolve_decision(decision_b.id, "X", plain_b.id).expect("project B never opted in");
    }

    // ---- round 57: no half-applied conflict resolution ------------------

    #[tokio::test]
    async fn resolving_a_conflict_whose_task_was_cancelled_fails_without_marking_the_conflict_resolved() {
        // The task is awaiting a human choice (Review, open conflict) and is
        // then cancelled. Resolving the conflict used to mark the conflict
        // Resolved FIRST, then fail on the task's state transition, an
        // error to the user, but a half-applied write on disk.
        let (coordinator, project_id, specialist_a_id, _b) = build_coordinator_with_disagreeing_backend_specialists().await;
        let submission = coordinator
            .submit_request(UserRequest { project_id, text: "Refactor the Rust backend auth module.".into(), requested_by: "human".into() })
            .await
            .unwrap();
        let conflict = coordinator.list_conflicts(project_id).unwrap().into_iter().next().unwrap();
        let approver = solexa_domain::Member::new_human(project_id, "Approver");
        coordinator.register_member(approver.clone()).unwrap();
        coordinator.cancel_task(submission.task_id).unwrap();
        assert_eq!(coordinator.get_task(submission.task_id).unwrap().unwrap().state, TaskState::Cancelled, "precondition");

        let err = coordinator.resolve_conflict(conflict.id, specialist_a_id, approver.id).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("cancel"), "the error should say why: {err}");

        let after = coordinator.get_conflict(conflict.id).unwrap().unwrap();
        assert_eq!(after.status, solexa_domain::ConflictStatus::Open, "a refused resolution must leave the conflict Open, nothing may be written");
        assert!(after.resolution.is_none() && after.approved_by.is_none());
        assert_eq!(coordinator.get_task(submission.task_id).unwrap().unwrap().state, TaskState::Cancelled, "and the task is untouched");
    }

    #[tokio::test]
    async fn resolve_conflict_rejects_an_unregistered_approver() {
        let (coordinator, project_id, specialist_a_id, _specialist_b_id) =
            build_coordinator_with_disagreeing_backend_specialists().await;

        coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Refactor the Rust backend auth module.".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();
        let conflict = coordinator.list_conflicts(project_id).unwrap().into_iter().next().unwrap();

        let err = coordinator
            .resolve_conflict(conflict.id, specialist_a_id, Uuid::new_v4())
            .unwrap_err();
        assert!(err.to_string().contains("is not registered"));

        // The conflict must remain untouched by the rejected attempt.
        let still_open = coordinator.get_conflict(conflict.id).unwrap().unwrap();
        assert_eq!(still_open.status, solexa_domain::ConflictStatus::Open);
    }

    #[tokio::test]
    async fn nested_team_membership_resolves_recursively() {
        let (coordinator, project_id) = build_coordinator().await;

        let human = solexa_domain::Member::new_human(project_id, "Alex");
        let ai_backend = solexa_domain::Member::new_ai(project_id, "Backend Bot", "runs the general specialist");
        let ai_sql = solexa_domain::Member::new_ai(project_id, "SQL Bot", "runs the SQL specialist");
        coordinator.register_member(human.clone()).unwrap();
        coordinator.register_member(ai_backend.clone()).unwrap();
        coordinator.register_member(ai_sql.clone()).unwrap();

        // Engineering (parent, just the human)
        //   └── Backend (child, the two AI members)
        let parent_team = solexa_domain::Team::new(project_id, "Engineering", "ship the product")
            .with_members(vec![human.id]);
        coordinator.register_team(parent_team.clone()).unwrap();

        let child_team = solexa_domain::Team::new(project_id, "Backend", "own backend services")
            .nested_under(parent_team.id)
            .with_members(vec![ai_backend.id, ai_sql.id]);
        coordinator.register_team(child_team).unwrap();

        // The parent team's OWN member_ids only lists the human, its
        // full membership, including the nested Backend team, must
        // still resolve to all three.
        let all_members = coordinator.team_members_recursive(parent_team.id).unwrap();
        let ids: std::collections::HashSet<_> = all_members.iter().map(|m| m.id).collect();
        assert_eq!(ids, [human.id, ai_backend.id, ai_sql.id].into_iter().collect());
    }

    #[tokio::test]
    async fn adding_a_member_to_a_team_is_persisted_and_idempotent() {
        let (coordinator, project_id) = build_coordinator().await;

        let human = solexa_domain::Member::new_human(project_id, "Alex");
        coordinator.register_member(human.clone()).unwrap();
        let team = solexa_domain::Team::new(project_id, "Sandbox Team", "handle whatever comes in");
        coordinator.register_team(team.clone()).unwrap();

        let new_member = solexa_domain::Member::new_ai(project_id, "Typography Bot", "runs a new specialist");
        coordinator.register_member(new_member.clone()).unwrap();
        coordinator.add_member_to_team(team.id, new_member.id).unwrap();

        let reloaded = coordinator.list_teams(project_id).unwrap();
        let reloaded_team = reloaded.iter().find(|t| t.id == team.id).unwrap();
        assert!(reloaded_team.member_ids.contains(&new_member.id));

        // Adding the same member again must not duplicate it or error.
        coordinator.add_member_to_team(team.id, new_member.id).unwrap();
        let reloaded_again = coordinator.list_teams(project_id).unwrap();
        let reloaded_team_again = reloaded_again.iter().find(|t| t.id == team.id).unwrap();
        assert_eq!(
            reloaded_team_again.member_ids.iter().filter(|id| **id == new_member.id).count(),
            1
        );

        // Adding to a team that doesn't exist is an explicit error, not
        // a silent no-op.
        assert!(coordinator.add_member_to_team(Uuid::new_v4(), new_member.id).is_err());
    }

    #[tokio::test]
    async fn removing_a_member_from_a_team_is_persisted_idempotent_and_never_deletes_the_member() {
        let (coordinator, project_id) = build_coordinator().await;

        let human = solexa_domain::Member::new_human(project_id, "Alex");
        coordinator.register_member(human.clone()).unwrap();
        let team = solexa_domain::Team::new(project_id, "Sandbox Team", "handle whatever comes in");
        coordinator.register_team(team.clone()).unwrap();

        let member = solexa_domain::Member::new_ai(project_id, "Typography Bot", "runs a specialist");
        coordinator.register_member(member.clone()).unwrap();
        coordinator.add_member_to_team(team.id, member.id).unwrap();
        let with_member = coordinator.list_teams(project_id).unwrap();
        assert!(with_member.iter().find(|t| t.id == team.id).unwrap().member_ids.contains(&member.id));

        coordinator.remove_member_from_team(team.id, member.id).unwrap();
        let without_member = coordinator.list_teams(project_id).unwrap();
        let reloaded_team = without_member.iter().find(|t| t.id == team.id).unwrap();
        assert!(!reloaded_team.member_ids.contains(&member.id));
        // The other member (Alex, never added to this team) is
        // untouched, removal only ever affects the one team/member
        // pair asked for.
        assert!(!reloaded_team.member_ids.contains(&human.id));

        // Removing the member is never the same as deleting the
        // member, they still exist in the project, just off this
        // team's roster.
        let still_registered = coordinator.get_member(member.id).unwrap();
        assert!(still_registered.is_some());

        // Removing an already-absent member is a no-op, not an error,
        // the mirrored idempotency of add_member_to_team's "adding an
        // already-present member is a no-op."
        coordinator.remove_member_from_team(team.id, member.id).unwrap();
        let still_without_member = coordinator.list_teams(project_id).unwrap();
        assert!(!still_without_member.iter().find(|t| t.id == team.id).unwrap().member_ids.contains(&member.id));

        // Removing from a team that doesn't exist is an explicit
        // error, not a silent no-op, same as add_member_to_team.
        assert!(coordinator.remove_member_from_team(Uuid::new_v4(), member.id).is_err());
    }

    #[tokio::test]
    async fn deleting_and_restoring_a_member_is_idempotent_and_never_touches_the_row_otherwise() {
        let (coordinator, project_id) = build_coordinator().await;
        let member = solexa_domain::Member::new_human(project_id, "Alex");
        coordinator.register_member(member.clone()).unwrap();

        let before = coordinator.get_member(member.id).unwrap().unwrap();
        assert!(!before.is_deleted());

        coordinator.delete_member(member.id).unwrap();
        let deleted = coordinator.get_member(member.id).unwrap().unwrap();
        assert!(deleted.is_deleted());
        // Nothing else about the row changed.
        assert_eq!(deleted.display_name, "Alex");

        // Deleting an already-deleted member is a no-op, not an error
        // or a refreshed timestamp.
        let first_deleted_at = deleted.deleted_at;
        coordinator.delete_member(member.id).unwrap();
        assert_eq!(coordinator.get_member(member.id).unwrap().unwrap().deleted_at, first_deleted_at);

        coordinator.restore_member(member.id).unwrap();
        let restored = coordinator.get_member(member.id).unwrap().unwrap();
        assert!(!restored.is_deleted());
        assert_eq!(restored.display_name, "Alex");

        // Restoring an already-active member is a no-op, not an error.
        coordinator.restore_member(member.id).unwrap();

        assert!(coordinator.delete_member(Uuid::new_v4()).is_err());
    }

    #[tokio::test]
    async fn deleting_and_restoring_a_team_is_idempotent_and_never_touches_its_roster() {
        let (coordinator, project_id) = build_coordinator().await;
        let member = solexa_domain::Member::new_human(project_id, "Alex");
        coordinator.register_member(member.clone()).unwrap();
        let team = solexa_domain::Team::new(project_id, "Sandbox Team", "handle whatever comes in");
        coordinator.register_team(team.clone()).unwrap();
        coordinator.add_member_to_team(team.id, member.id).unwrap();

        coordinator.delete_team(team.id).unwrap();
        let deleted = coordinator.list_teams(project_id).unwrap().into_iter().find(|t| t.id == team.id).unwrap();
        assert!(deleted.is_deleted());
        // The roster survives a delete untouched, restoring gives the
        // team back exactly as it was.
        assert_eq!(deleted.member_ids, vec![member.id]);

        coordinator.restore_team(team.id).unwrap();
        let restored = coordinator.list_teams(project_id).unwrap().into_iter().find(|t| t.id == team.id).unwrap();
        assert!(!restored.is_deleted());
        assert_eq!(restored.member_ids, vec![member.id]);
    }

    #[tokio::test]
    async fn editing_a_deleted_member_or_team_does_not_accidentally_restore_it() {
        // Regression test for a real bug found while building this:
        // edit_member/edit_team defensively override id/project_id/etc.
        // from the existing row, but hadn't been overriding deleted_at
        // an ordinary rename would have silently un-deleted anything
        // it touched.
        let (coordinator, project_id) = build_coordinator().await;
        let member = solexa_domain::Member::new_human(project_id, "Alex");
        coordinator.register_member(member.clone()).unwrap();
        coordinator.delete_member(member.id).unwrap();

        let mut edited = coordinator.get_member(member.id).unwrap().unwrap();
        edited.display_name = "Alexandra".into();
        edited.deleted_at = None; // what a naive edit form would submit
        coordinator.edit_member(project_id, member.id, edited).unwrap();

        let after_edit = coordinator.get_member(member.id).unwrap().unwrap();
        assert!(after_edit.is_deleted(), "editing must not silently restore a deleted member");
        assert_eq!(after_edit.display_name, "Alexandra", "the edit itself must still apply");

        let team = solexa_domain::Team::new(project_id, "Sandbox Team", "handle whatever comes in");
        coordinator.register_team(team.clone()).unwrap();
        coordinator.delete_team(team.id).unwrap();

        let mut edited_team = coordinator.list_teams(project_id).unwrap().into_iter().find(|t| t.id == team.id).unwrap();
        edited_team.mission = "a new mission".into();
        edited_team.deleted_at = None;
        coordinator.edit_team(project_id, team.id, edited_team).unwrap();

        let after_team_edit = coordinator.list_teams(project_id).unwrap().into_iter().find(|t| t.id == team.id).unwrap();
        assert!(after_team_edit.is_deleted(), "editing must not silently restore a deleted team");
        assert_eq!(after_team_edit.mission, "a new mission");
    }

    /// The full symmetric lifecycle both functions are meant to
    /// support together, not just individually: a member can be added,
    /// removed, and re-added to the same team, ending up in exactly
    /// the state a human would expect at each step, this is the case
    /// a standalone "Add to team" UI control (as opposed to only ever
    /// joining a team as a side effect of specialist creation) now
    /// exercises for the first time.
    #[tokio::test]
    async fn a_member_can_be_added_removed_and_re_added_to_the_same_team() {
        let (coordinator, project_id) = build_coordinator().await;

        let team = solexa_domain::Team::new(project_id, "Sandbox Team", "handle whatever comes in");
        coordinator.register_team(team.clone()).unwrap();
        let member = solexa_domain::Member::new_human(project_id, "Alex");
        coordinator.register_member(member.clone()).unwrap();

        coordinator.add_member_to_team(team.id, member.id).unwrap();
        assert!(coordinator.list_teams(project_id).unwrap()[0].member_ids.contains(&member.id));

        coordinator.remove_member_from_team(team.id, member.id).unwrap();
        assert!(!coordinator.list_teams(project_id).unwrap()[0].member_ids.contains(&member.id));

        coordinator.add_member_to_team(team.id, member.id).unwrap();
        let final_team = coordinator.list_teams(project_id).unwrap();
        let final_team = final_team.iter().find(|t| t.id == team.id).unwrap();
        assert!(final_team.member_ids.contains(&member.id));
        // Re-adding doesn't duplicate the id in the roster.
        assert_eq!(final_team.member_ids.iter().filter(|id| **id == member.id).count(), 1);
    }

    #[tokio::test]
    async fn edit_member_renames_a_member_without_disturbing_identity_or_type() {
        let (coordinator, project_id) = build_coordinator().await;

        let human = solexa_domain::Member::new_human(project_id, "Alex");
        let human_id = human.id;
        coordinator.register_member(human).unwrap();

        let mut renamed = coordinator.get_member(human_id).unwrap().unwrap();
        renamed.display_name = "Alexandra".into();
        renamed.description = "Product owner".into();
        coordinator.edit_member(project_id, human_id, renamed).unwrap();

        let reloaded = coordinator.get_member(human_id).unwrap().unwrap();
        assert_eq!(reloaded.display_name, "Alexandra");
        assert_eq!(reloaded.description, "Product owner");
        assert_eq!(reloaded.id, human_id, "editing must preserve identity");

        // An edit can't smuggle a member-type change through, a human
        // stays a human even if the caller's `updated` value claims
        // otherwise.
        let mut pretend_ai = coordinator.get_member(human_id).unwrap().unwrap();
        pretend_ai.member_type = solexa_domain::MemberType::Ai;
        coordinator.edit_member(project_id, human_id, pretend_ai).unwrap();
        let still_human = coordinator.get_member(human_id).unwrap().unwrap();
        assert_eq!(still_human.member_type, solexa_domain::MemberType::Human);

        // An empty display name is rejected.
        let mut blank = coordinator.get_member(human_id).unwrap().unwrap();
        blank.display_name = "  ".into();
        assert!(coordinator.edit_member(project_id, human_id, blank).is_err());

        // Editing a member that doesn't exist is an explicit error.
        assert!(coordinator
            .edit_member(project_id, Uuid::new_v4(), solexa_domain::Member::new_human(project_id, "Ghost"))
            .is_err());
    }

    /// Round 46: `role_ids` is exclusively managed by
    /// `assign_role`/`unassign_role`, a generic rename through
    /// `edit_member` must never silently wipe it, the same protection
    /// `member_type` and `deleted_at` already get above. This is a real
    /// bug this round caught and fixed, not a speculative test: the
    /// Tauri `update_member` command constructs its placeholder
    /// `Member` with `role_ids: vec![]`, exactly like it already does
    /// for `member_type`/`deleted_at`, relying on `edit_member` to
    /// override it.
    #[tokio::test]
    async fn edit_member_never_disturbs_role_ids() {
        let (coordinator, project_id) = build_coordinator().await;
        let human = solexa_domain::Member::new_human(project_id, "Alex");
        let human_id = human.id;
        coordinator.register_member(human).unwrap();
        let role = coordinator.create_role(project_id, "Manager", vec![Permission::Approve]).unwrap();
        coordinator.assign_role(human_id, role.id).unwrap();
        assert_eq!(coordinator.get_member(human_id).unwrap().unwrap().role_ids, vec![role.id]);

        // A caller that constructs its `updated` value with an empty
        // (or entirely different) role_ids, exactly what a Tauri
        // command building a fresh struct literal would naturally do,
        // must not be able to smuggle that through.
        let mut renamed = coordinator.get_member(human_id).unwrap().unwrap();
        renamed.display_name = "Alexandra".into();
        renamed.role_ids = vec![];
        coordinator.edit_member(project_id, human_id, renamed).unwrap();

        let reloaded = coordinator.get_member(human_id).unwrap().unwrap();
        assert_eq!(reloaded.display_name, "Alexandra", "the legitimate part of the edit must still go through");
        assert_eq!(reloaded.role_ids, vec![role.id], "role_ids must survive an unrelated edit untouched");
    }

    /// Spec section 11, "Model Swapping": "The user should be able to
    /// replace an AI member's model without destroying: member
    /// identity, history, responsibilities, permissions, team
    /// relationships." Proven here with the member's team membership
    /// (established via `add_member_to_team`) as the stand-in for
    /// "relationships that must survive the swap."
    #[tokio::test]
    async fn edit_member_can_swap_which_specialist_an_ai_member_runs_without_losing_team_membership() {
        let (coordinator, project_id) = build_coordinator().await;
        let model_id = coordinator.list_models()[0].id;

        let old_specialist = Specialist {
            id: Uuid::new_v4(),
            name: "Old Specialist".into(),
            role: "reviewer".into(),
            domain: "general".into(),
            capabilities: vec!["general".into()],
            preferred_model_ids: vec![model_id],
            tools: vec![],
            knowledge_scope: vec![],
            permissions: vec![Permission::Read, Permission::Execute],
            output_schema: None,
            escalation_policy: EscalationPolicy::default(),
            active: true,
                    role_ids: Vec::new(), max_tokens: None, temperature: None,
        };
        let old_specialist_id = old_specialist.id;
        coordinator.register_specialist(project_id, old_specialist).unwrap();

        let new_specialist = Specialist {
            id: Uuid::new_v4(),
            name: "New Specialist".into(),
            role: "reviewer".into(),
            domain: "general".into(),
            capabilities: vec!["general".into()],
            preferred_model_ids: vec![model_id],
            tools: vec![],
            knowledge_scope: vec![],
            permissions: vec![Permission::Read, Permission::Execute],
            output_schema: None,
            escalation_policy: EscalationPolicy::default(),
            active: true,
                    role_ids: Vec::new(), max_tokens: None, temperature: None,
        };
        let new_specialist_id = new_specialist.id;
        coordinator.register_specialist(project_id, new_specialist).unwrap();

        let mut member = solexa_domain::Member::new_ai(project_id, "Utility Bot", "runs a specialist");
        member.specialist_ids = vec![old_specialist_id];
        let member_id = member.id;
        coordinator.register_member(member).unwrap();

        let team = solexa_domain::Team::new(project_id, "Sandbox Team", "handle whatever comes in");
        coordinator.register_team(team.clone()).unwrap();
        coordinator.add_member_to_team(team.id, member_id).unwrap();

        let mut swapped = coordinator.get_member(member_id).unwrap().unwrap();
        swapped.specialist_ids = vec![new_specialist_id];
        coordinator.edit_member(project_id, member_id, swapped).unwrap();

        let reloaded = coordinator.get_member(member_id).unwrap().unwrap();
        assert_eq!(reloaded.id, member_id, "identity survives the swap");
        assert_eq!(reloaded.specialist_ids, vec![new_specialist_id]);

        let reloaded_team = coordinator.list_teams(project_id).unwrap();
        let reloaded_team = reloaded_team.iter().find(|t| t.id == team.id).unwrap();
        assert!(reloaded_team.member_ids.contains(&member_id), "team relationship survives the swap");
    }

    #[tokio::test]
    async fn edit_team_renames_a_team_without_disturbing_membership_or_hierarchy() {
        let (coordinator, project_id) = build_coordinator().await;

        let parent_team = solexa_domain::Team::new(project_id, "Parent Team", "coordinate the sub-teams");
        coordinator.register_team(parent_team.clone()).unwrap();

        let child_team = solexa_domain::Team::new(project_id, "Sandbox Team", "handle whatever comes in")
            .nested_under(parent_team.id);
        coordinator.register_team(child_team.clone()).unwrap();

        let member = solexa_domain::Member::new_human(project_id, "Alex");
        let member_id = member.id;
        coordinator.register_member(member).unwrap();
        coordinator.add_member_to_team(child_team.id, member_id).unwrap();

        let mut renamed = coordinator.list_teams(project_id).unwrap().into_iter().find(|t| t.id == child_team.id).unwrap();
        renamed.name = "Renamed Team".into();
        renamed.mission = "a sharper mission statement".into();
        coordinator.edit_team(project_id, child_team.id, renamed).unwrap();

        let reloaded = coordinator.list_teams(project_id).unwrap();
        let reloaded_team = reloaded.iter().find(|t| t.id == child_team.id).unwrap();
        assert_eq!(reloaded_team.name, "Renamed Team");
        assert_eq!(reloaded_team.mission, "a sharper mission statement");
        assert_eq!(reloaded_team.id, child_team.id, "editing must preserve identity");

        // An edit can't smuggle a membership or hierarchy change
        // through, those stay whatever they already were, even if the
        // caller's `updated` value claims otherwise.
        let mut sneaky = coordinator.list_teams(project_id).unwrap().into_iter().find(|t| t.id == child_team.id).unwrap();
        sneaky.member_ids = vec![];
        sneaky.parent_team_id = None;
        coordinator.edit_team(project_id, child_team.id, sneaky).unwrap();
        let still_intact = coordinator.list_teams(project_id).unwrap();
        let still_intact = still_intact.iter().find(|t| t.id == child_team.id).unwrap();
        assert!(still_intact.member_ids.contains(&member_id), "membership must survive an unrelated edit");
        assert_eq!(still_intact.parent_team_id, Some(parent_team.id), "hierarchy must survive an unrelated edit");

        // An empty name is rejected.
        let mut blank = coordinator.list_teams(project_id).unwrap().into_iter().find(|t| t.id == child_team.id).unwrap();
        blank.name = "  ".into();
        assert!(coordinator.edit_team(project_id, child_team.id, blank).is_err());

        // Editing a team that doesn't exist is an explicit error.
        assert!(coordinator
            .edit_team(project_id, Uuid::new_v4(), solexa_domain::Team::new(project_id, "Ghost", "n/a"))
            .is_err());
    }

    #[tokio::test]
    async fn register_team_rejects_an_empty_name() {
        let (coordinator, project_id) = build_coordinator().await;
        let blank = solexa_domain::Team::new(project_id, "  ", "n/a");
        assert!(coordinator.register_team(blank).is_err());
    }

    #[tokio::test]
    async fn set_team_parent_moves_a_team_and_can_detach_it_to_the_top_level() {
        let (coordinator, project_id) = build_coordinator().await;

        let team_a = solexa_domain::Team::new(project_id, "Team A", "n/a");
        coordinator.register_team(team_a.clone()).unwrap();
        let team_b = solexa_domain::Team::new(project_id, "Team B", "n/a");
        coordinator.register_team(team_b.clone()).unwrap();

        // Starts with no parent.
        assert_eq!(coordinator.list_teams(project_id).unwrap().iter().find(|t| t.id == team_b.id).unwrap().parent_team_id, None);

        coordinator.set_team_parent(project_id, team_b.id, Some(team_a.id)).unwrap();
        let after_attach = coordinator.list_teams(project_id).unwrap();
        assert_eq!(after_attach.iter().find(|t| t.id == team_b.id).unwrap().parent_team_id, Some(team_a.id));

        // Detaching back to the top level.
        coordinator.set_team_parent(project_id, team_b.id, None).unwrap();
        let after_detach = coordinator.list_teams(project_id).unwrap();
        assert_eq!(after_detach.iter().find(|t| t.id == team_b.id).unwrap().parent_team_id, None);

        // Moving to a parent that doesn't exist, or moving a team that
        // doesn't exist, are both explicit errors.
        assert!(coordinator.set_team_parent(project_id, team_b.id, Some(Uuid::new_v4())).is_err());
        assert!(coordinator.set_team_parent(project_id, Uuid::new_v4(), Some(team_a.id)).is_err());
    }

    #[tokio::test]
    async fn set_team_parent_refuses_to_create_a_cycle() {
        let (coordinator, project_id) = build_coordinator().await;

        let grandparent = solexa_domain::Team::new(project_id, "Grandparent", "n/a");
        coordinator.register_team(grandparent.clone()).unwrap();
        let parent = solexa_domain::Team::new(project_id, "Parent", "n/a").nested_under(grandparent.id);
        coordinator.register_team(parent.clone()).unwrap();
        let child = solexa_domain::Team::new(project_id, "Child", "n/a").nested_under(parent.id);
        coordinator.register_team(child.clone()).unwrap();

        // A team can't become its own parent.
        assert!(coordinator.set_team_parent(project_id, parent.id, Some(parent.id)).is_err());

        // A team can't be moved under its own direct child...
        assert!(coordinator.set_team_parent(project_id, parent.id, Some(child.id)).is_err());
        // ...or under a more distant descendant (grandparent → child
        // would put the whole chain in a loop).
        assert!(coordinator.set_team_parent(project_id, grandparent.id, Some(child.id)).is_err());

        // None of the rejected attempts actually changed anything,
        // a rejected re-parent leaves the hierarchy exactly as it was.
        let teams = coordinator.list_teams(project_id).unwrap();
        assert_eq!(teams.iter().find(|t| t.id == parent.id).unwrap().parent_team_id, Some(grandparent.id));
        assert_eq!(teams.iter().find(|t| t.id == grandparent.id).unwrap().parent_team_id, None);

        // Moving a team to a genuinely unrelated team is still fine,
        // the cycle check doesn't over-trigger on every non-trivial move.
        let unrelated = solexa_domain::Team::new(project_id, "Unrelated", "n/a");
        coordinator.register_team(unrelated.clone()).unwrap();
        coordinator.set_team_parent(project_id, child.id, Some(unrelated.id)).unwrap();
        let reloaded = coordinator.list_teams(project_id).unwrap();
        assert_eq!(reloaded.iter().find(|t| t.id == child.id).unwrap().parent_team_id, Some(unrelated.id));
    }

    #[tokio::test]
    async fn register_specialist_rejects_a_name_or_capability_that_would_make_it_unroutable() {
        let (coordinator, project_id) = build_coordinator().await;
        let model_id = coordinator.list_models()[0].id;

        let no_name = Specialist {
            id: Uuid::new_v4(),
            name: "".into(),
            role: "reviewer".into(),
            domain: "typography".into(),
            capabilities: vec!["typography".into()],
            preferred_model_ids: vec![model_id],
            tools: vec![],
            knowledge_scope: vec![],
            permissions: vec![Permission::Read, Permission::Execute],
            output_schema: None,
            escalation_policy: EscalationPolicy::default(),
            active: true,
                    role_ids: Vec::new(), max_tokens: None, temperature: None,
        };
        assert!(coordinator.register_specialist(project_id, no_name).is_err());

        let no_capabilities = Specialist {
            id: Uuid::new_v4(),
            name: "Typography Specialist".into(),
            role: "reviewer".into(),
            domain: "typography".into(),
            capabilities: vec![],
            preferred_model_ids: vec![model_id],
            tools: vec![],
            knowledge_scope: vec![],
            permissions: vec![Permission::Read, Permission::Execute],
            output_schema: None,
            escalation_policy: EscalationPolicy::default(),
            active: true,
                    role_ids: Vec::new(), max_tokens: None, temperature: None,
        };
        assert!(coordinator.register_specialist(project_id, no_capabilities).is_err());
    }

    /// The end-to-end proof that the whole point of this round actually
    /// works: a specialist created through `register_specialist` (i.e.
    /// the same path a "create specialist" UI command would use), not
    /// seed data, is immediately capable of being routed a real task,
    /// with a real `SpecialistRegistered` event on record.
    ///
    /// Deliberately builds its own coordinator with no other
    /// specialists registered (rather than reusing `build_coordinator`,
    /// whose seeded General Specialist already covers every keyword-
    /// inferable capability except "sql") so the new specialist is
    /// unambiguously the only match, this test is about registration
    /// making a specialist routable at all, not about routing among
    /// several matches (that's Phase 2/5's coverage already).
    #[tokio::test]
    async fn specialist_registered_outside_seed_data_is_immediately_routable() {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let events = StdArc::new(bus);
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let model = mock_model();
        let model_id = model.id;
        storage.insert_model(&model).unwrap();
        let mut providers = ProviderRegistry::new();
        providers.register(StdArc::new(MockLocalProvider::new(vec![model])));
        let coordinator = Coordinator::new(storage, events, providers);

        let typography_specialist = Specialist {
            id: Uuid::new_v4(),
            name: "Typography Specialist".into(),
            role: "typography reviewer".into(),
            domain: "typography".into(),
            capabilities: vec!["design".into()],
            preferred_model_ids: vec![model_id],
            tools: vec![],
            knowledge_scope: vec![],
            permissions: vec![Permission::Read, Permission::Propose, Permission::Execute],
            output_schema: None,
            escalation_policy: EscalationPolicy::default(),
            active: true,
                    role_ids: Vec::new(), max_tokens: None, temperature: None,
        };
        let specialist_id = typography_specialist.id;
        coordinator.register_specialist(project_id, typography_specialist).unwrap();

        // The seed-equivalent "wrap it in an AI member" step a
        // create-specialist command would also perform.
        let mut member = solexa_domain::Member::new_ai(project_id, "Typography Bot", "runs the Typography Specialist");
        member.specialist_ids = vec![specialist_id];
        coordinator.register_member(member).unwrap();

        let submission = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Review the typography hierarchy on the landing page.".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();

        let task = coordinator.get_task(submission.task_id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Completed);
        assert_eq!(task.assigned_specialist_id, Some(specialist_id));

        // Note: `SpecialistRegistered` fires with its own fresh
        // correlation id (registration isn't part of any task's trace)
        // consistent with `MemberRegistered`/`TeamRegistered`, which
        // are likewise fire-and-forget audit entries no existing test
        // asserts on via a task trace. The real proof this round cares
        // about is above: the task actually completed, assigned to the
        // specialist that only exists because of `register_specialist`.
        let listed = coordinator.list_specialists(project_id).unwrap();
        assert!(listed.iter().any(|s| s.id == specialist_id));
    }

    #[tokio::test]
    async fn editing_a_specialist_persists_new_fields_and_rejects_making_it_unroutable() {
        let (coordinator, project_id) = build_coordinator().await;
        let model_id = coordinator.list_models()[0].id;

        let original = Specialist {
            id: Uuid::new_v4(),
            name: "Typography Specialist".into(),
            role: "typography reviewer".into(),
            domain: "typography".into(),
            capabilities: vec!["design".into()],
            preferred_model_ids: vec![model_id],
            tools: vec![],
            knowledge_scope: vec![],
            permissions: vec![Permission::Read, Permission::Execute],
            output_schema: None,
            escalation_policy: EscalationPolicy::default(),
            active: true,
                    role_ids: Vec::new(), max_tokens: None, temperature: None,
        };
        let specialist_id = original.id;
        coordinator.register_specialist(project_id, original).unwrap();

        let mut edited = coordinator.list_specialists(project_id).unwrap();
        let mut edited = edited.remove(edited.iter().position(|s| s.id == specialist_id).unwrap());
        edited.role = "senior typography reviewer".into();
        edited.tools = vec!["font metadata parser".into()];
        coordinator.edit_specialist(project_id, specialist_id, edited).unwrap();

        let reloaded = coordinator.list_specialists(project_id).unwrap();
        let reloaded = reloaded.iter().find(|s| s.id == specialist_id).unwrap();
        assert_eq!(reloaded.role, "senior typography reviewer");
        assert_eq!(reloaded.tools, vec!["font metadata parser".to_string()]);
        // Editing preserves identity, the id doesn't drift even though
        // the caller's `updated` value could in principle carry a
        // different one.
        assert_eq!(reloaded.id, specialist_id);

        // An edit that would make the specialist unroutable is rejected,
        // same validation as registration.
        let mut unroutable = reloaded.clone();
        unroutable.capabilities = vec![];
        assert!(coordinator.edit_specialist(project_id, specialist_id, unroutable).is_err());

        // Archiving, then editing an unrelated field, must not silently
        // reactivate the specialist, `active` only changes via
        // `set_specialist_active`, never as a side effect of a field
        // edit (this is the bug this test would have caught: an early
        // version of `edit_specialist` just wrote whatever `active`
        // value happened to be on the `updated` struct the caller
        // passed in, and the Tauri `update_specialist` command always
        // sends `active: true` since editing doesn't expose that field).
        coordinator.set_specialist_active(project_id, specialist_id, false).unwrap();
        let mut still_archived = coordinator.list_specialists(project_id).unwrap();
        let still_archived =
            still_archived.remove(still_archived.iter().position(|s| s.id == specialist_id).unwrap());
        assert!(!still_archived.active);
        let mut unrelated_edit = still_archived.clone();
        unrelated_edit.role = "lead typography reviewer".into();
        coordinator.edit_specialist(project_id, specialist_id, unrelated_edit).unwrap();
        let after_edit = coordinator.list_specialists(project_id).unwrap();
        let after_edit = after_edit.iter().find(|s| s.id == specialist_id).unwrap();
        assert_eq!(after_edit.role, "lead typography reviewer");
        assert!(!after_edit.active, "editing must not silently reactivate an archived specialist");

        // Editing a specialist that doesn't exist is an explicit error.
        let ghost = Specialist {
            id: Uuid::new_v4(),
            name: "Ghost".into(),
            role: "".into(),
            domain: "".into(),
            capabilities: vec!["design".into()],
            preferred_model_ids: vec![],
            tools: vec![],
            knowledge_scope: vec![],
            permissions: vec![],
            output_schema: None,
            escalation_policy: EscalationPolicy::default(),
            active: true,
                    role_ids: Vec::new(), max_tokens: None, temperature: None,
        };
        assert!(coordinator.edit_specialist(project_id, Uuid::new_v4(), ghost).is_err());
    }

    /// The point of archiving: an archived specialist stops being
    /// routable and stops appearing in discovery rankings, but is never
    /// deleted, it keeps its identity and history, and reactivating it
    /// restores routability immediately. Built on an isolated
    /// coordinator (same reasoning as the routability test above) so
    /// there's exactly one capable specialist to archive/reactivate,
    /// not several competing ones.
    #[tokio::test]
    async fn archiving_a_specialist_excludes_it_from_routing_and_ranking_until_reactivated() {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let events = StdArc::new(bus);
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let model = mock_model();
        let model_id = model.id;
        storage.insert_model(&model).unwrap();
        let mut providers = ProviderRegistry::new();
        providers.register(StdArc::new(MockLocalProvider::new(vec![model])));
        let coordinator = Coordinator::new(storage, events, providers);

        let specialist = Specialist {
            id: Uuid::new_v4(),
            name: "Typography Specialist".into(),
            role: "typography reviewer".into(),
            domain: "typography".into(),
            capabilities: vec!["design".into()],
            preferred_model_ids: vec![model_id],
            tools: vec![],
            knowledge_scope: vec![],
            permissions: vec![Permission::Read, Permission::Propose, Permission::Execute],
            output_schema: None,
            escalation_policy: EscalationPolicy::default(),
            active: true,
                    role_ids: Vec::new(), max_tokens: None, temperature: None,
        };
        let specialist_id = specialist.id;
        coordinator.register_specialist(project_id, specialist).unwrap();
        let mut member = solexa_domain::Member::new_ai(project_id, "Typography Bot", "runs it");
        member.specialist_ids = vec![specialist_id];
        coordinator.register_member(member).unwrap();

        // Archiving is idempotent.
        coordinator.set_specialist_active(project_id, specialist_id, false).unwrap();
        coordinator.set_specialist_active(project_id, specialist_id, false).unwrap();

        assert!(coordinator
            .rank_specialists_for_capability(project_id, "design")
            .unwrap()
            .is_empty());

        let submission = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Review the typography hierarchy on the landing page.".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();
        let task = coordinator.get_task(submission.task_id).unwrap().unwrap();
        // Nobody eligible, same visible-block path as "no matching
        // specialist at all" (Phase 5's existing behavior), not an error.
        assert_eq!(task.state, TaskState::Blocked);
        assert_eq!(task.assigned_specialist_id, None);

        // The specialist still exists, still visible, just inactive.
        let listed = coordinator.list_specialists(project_id).unwrap();
        assert!(!listed.iter().find(|s| s.id == specialist_id).unwrap().active);

        // Reactivating restores routability immediately.
        coordinator.set_specialist_active(project_id, specialist_id, true).unwrap();
        assert_eq!(coordinator.rank_specialists_for_capability(project_id, "design").unwrap().len(), 1);

        let submission2 = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Review the typography hierarchy on the checkout page.".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();
        let task2 = coordinator.get_task(submission2.task_id).unwrap().unwrap();
        assert_eq!(task2.state, TaskState::Completed);
        assert_eq!(task2.assigned_specialist_id, Some(specialist_id));

        // Archiving/reactivating a specialist that doesn't exist is an
        // explicit error.
        assert!(coordinator.set_specialist_active(project_id, Uuid::new_v4(), false).is_err());
    }

    #[tokio::test]
    async fn exporting_and_importing_a_specialist_package_round_trips_configuration_and_benchmark() {
        let (coordinator, project_id) = build_coordinator().await;
        let model_id = coordinator.list_models()[0].id;

        let original = Specialist {
            id: Uuid::new_v4(),
            name: "Typography Specialist".into(),
            role: "typography reviewer".into(),
            domain: "typography".into(),
            capabilities: vec!["design".into(), "typography".into()],
            preferred_model_ids: vec![model_id],
            tools: vec!["font metadata parser".into()],
            knowledge_scope: vec!["brand system".into()],
            permissions: vec![Permission::Read, Permission::Propose, Permission::Execute],
            output_schema: None,
            escalation_policy: EscalationPolicy::default(),
            active: true,
                    role_ids: Vec::new(), max_tokens: None, temperature: None,
        };
        let original_id = original.id;
        coordinator.register_specialist(project_id, original).unwrap();
        // Give it a real, measured track record, not a neutral 0/0,
        // so the export/import round trip has something meaningful to
        // carry over.
        coordinator.storage().record_benchmark_outcome(original_id, solexa_domain::BenchmarkOutcome::VerificationPassed).unwrap();
        coordinator.storage().record_benchmark_outcome(original_id, solexa_domain::BenchmarkOutcome::VerificationPassed).unwrap();
        coordinator.storage().record_benchmark_outcome(original_id, solexa_domain::BenchmarkOutcome::VerificationFailed).unwrap();

        let package = coordinator.export_specialist_package(original_id).unwrap();
        assert_eq!(package.specialist.name, "Typography Specialist");
        assert_eq!(package.benchmark.verification_passed_count, 2);
        assert_eq!(package.benchmark.verification_failed_count, 1);

        // A package is a plain, serializable document, round-trip it
        // through JSON the way a real export/import would actually
        // move it (a file, a paste buffer), not just pass the Rust
        // struct directly from one function to the next.
        let json = serde_json::to_string(&package).unwrap();
        let package: solexa_domain::SpecialistPackage = serde_json::from_str(&json).unwrap();

        let imported_id = coordinator.import_specialist_package(project_id, package).unwrap();
        assert_ne!(imported_id, original_id, "an imported specialist is a new instance, not a claim to be the same object");

        let imported = coordinator.list_specialists(project_id).unwrap();
        let imported = imported.iter().find(|s| s.id == imported_id).unwrap();
        assert_eq!(imported.name, "Typography Specialist");
        assert_eq!(imported.capabilities, vec!["design".to_string(), "typography".to_string()]);
        assert_eq!(imported.tools, vec!["font metadata parser".to_string()]);
        assert!(imported.active, "an imported specialist is always active by default");

        let imported_benchmark = coordinator.storage().get_benchmark(imported_id).unwrap();
        assert_eq!(imported_benchmark.verification_passed_count, 2);
        assert_eq!(imported_benchmark.verification_failed_count, 1);

        // The original is completely untouched by any of this.
        let original_reloaded = coordinator.list_specialists(project_id).unwrap();
        let original_reloaded = original_reloaded.iter().find(|s| s.id == original_id).unwrap();
        assert_eq!(original_reloaded.name, "Typography Specialist");
    }

    #[tokio::test]
    async fn importing_a_specialist_package_always_activates_it_regardless_of_the_exported_state() {
        let (coordinator, project_id) = build_coordinator().await;
        let model_id = coordinator.list_models()[0].id;

        let specialist = Specialist {
            id: Uuid::new_v4(),
            name: "Archived Specialist".into(),
            role: "reviewer".into(),
            domain: "general".into(),
            capabilities: vec!["general".into()],
            preferred_model_ids: vec![model_id],
            tools: vec![],
            knowledge_scope: vec![],
            permissions: vec![Permission::Read, Permission::Execute],
            output_schema: None,
            escalation_policy: EscalationPolicy::default(),
            active: true,
                    role_ids: Vec::new(), max_tokens: None, temperature: None,
        };
        let specialist_id = specialist.id;
        coordinator.register_specialist(project_id, specialist).unwrap();
        coordinator.set_specialist_active(project_id, specialist_id, false).unwrap();

        let mut package = coordinator.export_specialist_package(specialist_id).unwrap();
        assert!(!package.specialist.active, "the exported package honestly reflects the archived source");

        let imported_id = coordinator.import_specialist_package(project_id, package.clone()).unwrap();
        let imported = coordinator.list_specialists(project_id).unwrap();
        let imported = imported.iter().find(|s| s.id == imported_id).unwrap();
        assert!(imported.active, "installing a specialist should not default to it being disabled");

        // A package claiming a format version this build doesn't
        // recognize is rejected outright, not guessed at.
        package.package_format_version = 999;
        assert!(coordinator.import_specialist_package(project_id, package).is_err());
    }

    #[tokio::test]
    async fn importing_a_specialist_package_into_a_different_project_works() {
        let (coordinator, project_a) = build_coordinator().await;
        let model_id = coordinator.list_models()[0].id;
        let project_b = Uuid::new_v4();
        coordinator.storage().insert_project(project_b, "Second Project", "").unwrap();

        let specialist = Specialist {
            id: Uuid::new_v4(),
            name: "Portable Specialist".into(),
            role: "reviewer".into(),
            domain: "general".into(),
            capabilities: vec!["general".into()],
            preferred_model_ids: vec![model_id],
            tools: vec![],
            knowledge_scope: vec![],
            permissions: vec![Permission::Read, Permission::Execute],
            output_schema: None,
            escalation_policy: EscalationPolicy::default(),
            active: true,
                    role_ids: Vec::new(), max_tokens: None, temperature: None,
        };
        let specialist_id = specialist.id;
        coordinator.register_specialist(project_a, specialist).unwrap();

        let package = coordinator.export_specialist_package(specialist_id).unwrap();
        let imported_id = coordinator.import_specialist_package(project_b, package).unwrap();

        assert!(coordinator.list_specialists(project_b).unwrap().iter().any(|s| s.id == imported_id));
        assert!(!coordinator.list_specialists(project_a).unwrap().iter().any(|s| s.id == imported_id));

        // Exporting or importing against a specialist/package that
        // doesn't resolve is an explicit error.
        assert!(coordinator.export_specialist_package(Uuid::new_v4()).is_err());
    }

    #[tokio::test]
    async fn exporting_and_importing_a_team_package_recreates_ai_members_and_specialists() {
        let (coordinator, project_id) = build_coordinator().await;
        let model_id = coordinator.list_models()[0].id;

        let team = solexa_domain::Team::new(project_id, "Backend Team", "own the backend");
        coordinator.register_team(team.clone()).unwrap();

        let specialist = Specialist {
            id: Uuid::new_v4(),
            name: "Backend Specialist".into(),
            role: "backend engineer".into(),
            domain: "backend".into(),
            capabilities: vec!["backend".into()],
            preferred_model_ids: vec![model_id],
            tools: vec!["compiler".into()],
            knowledge_scope: vec![],
            permissions: vec![Permission::Read, Permission::Execute],
            output_schema: None,
            escalation_policy: EscalationPolicy::default(),
            active: true,
                    role_ids: Vec::new(), max_tokens: None, temperature: None,
        };
        let specialist_id = specialist.id;
        coordinator.register_specialist(project_id, specialist).unwrap();
        coordinator.storage().record_benchmark_outcome(specialist_id, solexa_domain::BenchmarkOutcome::VerificationPassed).unwrap();
        coordinator.storage().record_benchmark_outcome(specialist_id, solexa_domain::BenchmarkOutcome::VerificationPassed).unwrap();

        let mut ai_member = solexa_domain::Member::new_ai(project_id, "Backend Bot", "runs the backend specialist");
        ai_member.specialist_ids = vec![specialist_id];
        let ai_member_id = ai_member.id;
        coordinator.register_member(ai_member).unwrap();
        coordinator.add_member_to_team(team.id, ai_member_id).unwrap();

        let human = solexa_domain::Member::new_human(project_id, "Alex");
        coordinator.register_member(human.clone()).unwrap();
        coordinator.add_member_to_team(team.id, human.id).unwrap();

        let package = coordinator.export_team_package(team.id).unwrap();
        assert_eq!(package.name, "Backend Team");
        assert_eq!(package.members.len(), 1, "human members must be excluded from the package");
        assert_eq!(package.members[0].display_name, "Backend Bot");
        assert_eq!(package.members[0].specialist.specialist.name, "Backend Specialist");
        assert_eq!(package.members[0].specialist.benchmark.verification_passed_count, 2);

        // Round-trip through JSON the way a real export/import would.
        let json = serde_json::to_string(&package).unwrap();
        let package: solexa_domain::TeamPackage = serde_json::from_str(&json).unwrap();

        let new_project_id = Uuid::new_v4();
        coordinator.storage().insert_project(new_project_id, "Second Project", "").unwrap();
        let new_team_id = coordinator.import_team_package(new_project_id, package).unwrap();
        assert_ne!(new_team_id, team.id);

        let new_teams = coordinator.list_teams(new_project_id).unwrap();
        let new_team = new_teams.iter().find(|t| t.id == new_team_id).unwrap();
        assert_eq!(new_team.name, "Backend Team");
        assert_eq!(new_team.member_ids.len(), 1, "only the AI member should have been recreated");

        let new_members = coordinator.list_members(new_project_id).unwrap();
        assert_eq!(new_members.len(), 1);
        let new_member = &new_members[0];
        assert_eq!(new_member.display_name, "Backend Bot");
        assert_ne!(new_member.id, ai_member_id, "an imported member is a new instance");

        let new_specialists = coordinator.list_specialists(new_project_id).unwrap();
        assert_eq!(new_specialists.len(), 1);
        let new_specialist = &new_specialists[0];
        assert_eq!(new_specialist.name, "Backend Specialist");
        assert_ne!(new_specialist.id, specialist_id);
        assert!(new_specialist.active);

        let new_benchmark = coordinator.storage().get_benchmark(new_specialist.id).unwrap();
        assert_eq!(new_benchmark.verification_passed_count, 2, "benchmark history travels with the team package");

        // The original project's roster is completely untouched.
        assert_eq!(coordinator.list_members(project_id).unwrap().len(), 2);
        assert_eq!(coordinator.list_teams(project_id).unwrap()[0].member_ids.len(), 2);
    }

    #[tokio::test]
    async fn importing_a_team_package_rejects_an_unrecognized_format_version() {
        let (coordinator, project_id) = build_coordinator().await;
        let mut package = solexa_domain::TeamPackage {
            package_format_version: 999,
            name: "Ghost Team".into(),
            mission: "n/a".into(),
            members: vec![],
        };
        assert!(coordinator.import_team_package(project_id, package.clone()).is_err());

        // A recognized version with zero members is still a valid,
        // if minimal, team package, an all-human or empty team is a
        // real state, not an error.
        package.package_format_version = solexa_domain::CURRENT_PACKAGE_FORMAT_VERSION;
        let team_id = coordinator.import_team_package(project_id, package).unwrap();
        let teams = coordinator.list_teams(project_id).unwrap();
        assert!(teams.iter().any(|t| t.id == team_id && t.name == "Ghost Team" && t.member_ids.is_empty()));

        // Exporting a team that doesn't exist is an explicit error.
        assert!(coordinator.export_team_package(Uuid::new_v4()).is_err());
    }

    #[tokio::test]
    async fn saving_a_specialist_to_the_registry_and_installing_it_round_trips_config_and_benchmark() {
        let (coordinator, project_id) = build_coordinator().await;
        let model_id = coordinator.list_models()[0].id;

        let specialist = Specialist {
            id: Uuid::new_v4(),
            name: "Typography Specialist".into(),
            role: "typography reviewer".into(),
            domain: "typography".into(),
            capabilities: vec!["design".into(), "typography".into()],
            preferred_model_ids: vec![model_id],
            tools: vec![],
            knowledge_scope: vec![],
            permissions: vec![Permission::Read, Permission::Execute],
            output_schema: None,
            escalation_policy: EscalationPolicy::default(),
            active: true,
                    role_ids: Vec::new(), max_tokens: None, temperature: None,
        };
        let specialist_id = specialist.id;
        coordinator.register_specialist(project_id, specialist).unwrap();
        coordinator.storage().record_benchmark_outcome(specialist_id, solexa_domain::BenchmarkOutcome::VerificationPassed).unwrap();

        let entry_id = coordinator.save_specialist_to_registry(project_id, specialist_id).unwrap();

        let entries = coordinator.list_registry_entries().unwrap();
        let entry = entries.iter().find(|e| e.id == entry_id).unwrap();
        assert_eq!(entry.kind, solexa_domain::RegistryEntryKind::Specialist);
        assert_eq!(entry.name, "Typography Specialist");
        assert_eq!(entry.domain, "typography");
        assert_eq!(entry.capabilities, vec!["design".to_string(), "typography".to_string()]);

        // Install into a different project entirely, the registry is
        // global, not scoped to where the entry came from.
        let other_project = Uuid::new_v4();
        coordinator.storage().insert_project(other_project, "Other Project", "").unwrap();
        let installed_id = coordinator.install_from_registry(other_project, entry_id).unwrap();
        assert_ne!(installed_id, specialist_id);

        let installed = coordinator.list_specialists(other_project).unwrap();
        let installed = installed.iter().find(|s| s.id == installed_id).unwrap();
        assert_eq!(installed.name, "Typography Specialist");
        assert!(installed.active);

        let installed_benchmark = coordinator.storage().get_benchmark(installed_id).unwrap();
        assert_eq!(installed_benchmark.verification_passed_count, 1);

        // Installing the same entry again produces yet another,
        // independent specialist, the registry entry is a template,
        // not consumed by installing it.
        let installed_again = coordinator.install_from_registry(project_id, entry_id).unwrap();
        assert_ne!(installed_again, installed_id);
    }

    #[tokio::test]
    async fn saving_a_team_to_the_registry_computes_the_union_of_its_members_capabilities() {
        let (coordinator, project_id) = build_coordinator().await;
        let model_id = coordinator.list_models()[0].id;

        let team = solexa_domain::Team::new(project_id, "Review Team", "review everything");
        coordinator.register_team(team.clone()).unwrap();

        let backend_specialist = Specialist {
            id: Uuid::new_v4(),
            name: "Backend Specialist".into(),
            role: "reviewer".into(),
            domain: "backend".into(),
            capabilities: vec!["backend".into()],
            preferred_model_ids: vec![model_id],
            tools: vec![],
            knowledge_scope: vec![],
            permissions: vec![Permission::Read, Permission::Execute],
            output_schema: None,
            escalation_policy: EscalationPolicy::default(),
            active: true,
                    role_ids: Vec::new(), max_tokens: None, temperature: None,
        };
        let backend_id = backend_specialist.id;
        coordinator.register_specialist(project_id, backend_specialist).unwrap();
        let mut backend_member = solexa_domain::Member::new_ai(project_id, "Backend Bot", "runs backend");
        backend_member.specialist_ids = vec![backend_id];
        let backend_member_id = backend_member.id;
        coordinator.register_member(backend_member).unwrap();
        coordinator.add_member_to_team(team.id, backend_member_id).unwrap();

        let sql_specialist = Specialist {
            id: Uuid::new_v4(),
            name: "SQL Specialist".into(),
            role: "reviewer".into(),
            domain: "sql".into(),
            capabilities: vec!["sql".into()],
            preferred_model_ids: vec![model_id],
            tools: vec![],
            knowledge_scope: vec![],
            permissions: vec![Permission::Read, Permission::Execute],
            output_schema: None,
            escalation_policy: EscalationPolicy::default(),
            active: true,
                    role_ids: Vec::new(), max_tokens: None, temperature: None,
        };
        let sql_id = sql_specialist.id;
        coordinator.register_specialist(project_id, sql_specialist).unwrap();
        let mut sql_member = solexa_domain::Member::new_ai(project_id, "SQL Bot", "runs sql");
        sql_member.specialist_ids = vec![sql_id];
        let sql_member_id = sql_member.id;
        coordinator.register_member(sql_member).unwrap();
        coordinator.add_member_to_team(team.id, sql_member_id).unwrap();

        let entry_id = coordinator.save_team_to_registry(project_id, team.id).unwrap();
        let entries = coordinator.list_registry_entries().unwrap();
        let entry = entries.iter().find(|e| e.id == entry_id).unwrap();
        assert_eq!(entry.kind, solexa_domain::RegistryEntryKind::Team);
        assert_eq!(entry.name, "Review Team");
        assert_eq!(entry.domain, "");
        let mut capabilities = entry.capabilities.clone();
        capabilities.sort();
        assert_eq!(capabilities, vec!["backend".to_string(), "sql".to_string()]);

        let installed_team_id = coordinator.install_from_registry(project_id, entry_id).unwrap();
        assert_ne!(installed_team_id, team.id);
        let installed_teams = coordinator.list_teams(project_id).unwrap();
        let installed_team = installed_teams.iter().find(|t| t.id == installed_team_id).unwrap();
        assert_eq!(installed_team.member_ids.len(), 2);
    }

    #[tokio::test]
    async fn removing_a_registry_entry_is_permanent_and_installing_it_afterward_fails() {
        let (coordinator, project_id) = build_coordinator().await;
        let model_id = coordinator.list_models()[0].id;

        let specialist = Specialist {
            id: Uuid::new_v4(),
            name: "Disposable Specialist".into(),
            role: "reviewer".into(),
            domain: "general".into(),
            capabilities: vec!["general".into()],
            preferred_model_ids: vec![model_id],
            tools: vec![],
            knowledge_scope: vec![],
            permissions: vec![Permission::Read, Permission::Execute],
            output_schema: None,
            escalation_policy: EscalationPolicy::default(),
            active: true,
                    role_ids: Vec::new(), max_tokens: None, temperature: None,
        };
        let specialist_id = specialist.id;
        coordinator.register_specialist(project_id, specialist).unwrap();
        let entry_id = coordinator.save_specialist_to_registry(project_id, specialist_id).unwrap();
        assert!(coordinator.list_registry_entries().unwrap().iter().any(|e| e.id == entry_id));

        coordinator.remove_from_registry(entry_id).unwrap();
        assert!(!coordinator.list_registry_entries().unwrap().iter().any(|e| e.id == entry_id));
        assert!(coordinator.install_from_registry(project_id, entry_id).is_err());

        // Removing something that was never there, or wasn't a
        // specialist/team/registry entry that ever existed, is an
        // explicit error, not a silent no-op, the registry is small
        // and personal enough that a caller removing the wrong id is
        // worth surfacing, unlike team membership's forgiving idempotency.
        assert!(coordinator.remove_from_registry(Uuid::new_v4()).is_err());

        // Saving or installing against a specialist/entry that doesn't
        // resolve is likewise an explicit error.
        assert!(coordinator.save_specialist_to_registry(project_id, Uuid::new_v4()).is_err());
        assert!(coordinator.install_from_registry(project_id, Uuid::new_v4()).is_err());
    }

    #[tokio::test]
    async fn a_capable_specialist_without_execute_permission_is_never_assigned() {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let events = StdArc::new(bus);
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let model = mock_model();
        let model_id = model.id;
        storage.insert_model(&model).unwrap();

        // Capable of "backend" work, but only ever allowed to propose,
        // never actually execute it. A specialist like this should be
        // just as invisible to the scheduler as one with no matching
        // capability at all.
        let proposer_only = Specialist {
            id: Uuid::new_v4(),
            name: "Backend Reviewer".into(),
            role: "reviewer".into(),
            domain: "backend".into(),
            capabilities: vec!["backend".into()],
            preferred_model_ids: vec![model_id],
            tools: vec![],
            knowledge_scope: vec![],
            permissions: vec![Permission::Read, Permission::Propose], // no Execute
            output_schema: None,
            escalation_policy: EscalationPolicy::default(),
            active: true,
                    role_ids: Vec::new(), max_tokens: None, temperature: None,
        };
        storage.insert_specialist(project_id, &proposer_only).unwrap();

        let mut providers = ProviderRegistry::new();
        providers.register(StdArc::new(MockLocalProvider::new(vec![model])));
        let coordinator = Coordinator::new(storage, events, providers);

        let submission = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Refactor the Rust backend auth module.".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();

        let task = coordinator.get_task(submission.task_id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Blocked);
        assert!(task.assigned_specialist_id.is_none(), "an unauthorized specialist must never be assigned");

        let trace = coordinator.events_for_correlation(submission.correlation_id).unwrap();
        let blocked_event = trace.iter().find(|e| e.event_type == EventType::TaskBlocked).unwrap();
        let reason = blocked_event.payload["reason"].as_str().unwrap();
        assert!(
            reason.contains("lack Permission::Execute"),
            "the block reason should distinguish 'unauthorized' from 'no matching capability': {reason}"
        );
    }

    /// Round 47: the companion case to the test just above, a
    /// specialist with NO direct `Permission::Execute` becomes
    /// assignable once it holds a `Role` that grants it. Proves
    /// `specialist_grants` (and the two enforcement sites that now call
    /// it) actually changes real scheduling behavior, not just that the
    /// data round-trips through storage.
    #[tokio::test]
    async fn a_specialist_gains_execute_eligibility_through_an_assigned_role() {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let events = StdArc::new(bus);
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let model = mock_model();
        let model_id = model.id;
        storage.insert_model(&model).unwrap();

        let proposer_only = Specialist {
            id: Uuid::new_v4(),
            name: "Backend Reviewer".into(),
            role: "reviewer".into(),
            domain: "backend".into(),
            capabilities: vec!["backend".into()],
            preferred_model_ids: vec![model_id],
            tools: vec![],
            knowledge_scope: vec![],
            permissions: vec![Permission::Read, Permission::Propose], // no direct Execute
            output_schema: None,
            escalation_policy: EscalationPolicy::default(),
            active: true,
            role_ids: Vec::new(), max_tokens: None, temperature: None,
        };
        let specialist_id = proposer_only.id;
        storage.insert_specialist(project_id, &proposer_only).unwrap();

        let mut providers = ProviderRegistry::new();
        providers.register(StdArc::new(MockLocalProvider::new(vec![model])));
        let coordinator = Coordinator::new(storage, events, providers);

        // Cross-project rejection, checked before the real assignment,
        // same shape as `assign_role_rejects_a_role_from_a_different_project`.
        let other_project = coordinator.create_project("Other Project", "").unwrap();
        let foreign_role = coordinator.create_role(other_project, "Executor", vec![Permission::Execute]).unwrap();
        let err = coordinator.assign_role_to_specialist(specialist_id, foreign_role.id).unwrap_err();
        assert!(err.to_string().contains("different project"));

        let role = coordinator.create_role(project_id, "Executor", vec![Permission::Execute]).unwrap();
        assert!(!coordinator.specialist_has_permission(specialist_id, Permission::Execute).unwrap());

        coordinator.assign_role_to_specialist(specialist_id, role.id).unwrap();
        assert!(coordinator.specialist_has_permission(specialist_id, Permission::Execute).unwrap());
        // Idempotent re-assignment, same convention as assign_role.
        coordinator.assign_role_to_specialist(specialist_id, role.id).unwrap();
        assert_eq!(
            coordinator.list_specialists(project_id).unwrap().iter().find(|s| s.id == specialist_id).unwrap().role_ids.len(),
            1
        );

        let submission = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Refactor the Rust backend auth module.".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();

        let task = coordinator.get_task(submission.task_id).unwrap().unwrap();
        assert_eq!(task.assigned_specialist_id, Some(specialist_id), "the role-granted Execute permission should make this specialist assignable now");
        assert_ne!(task.state, TaskState::Blocked);

        // Unassigning the role must revoke eligibility again, proven
        // directly (not by resubmitting, since Phase-8 track record
        // would now favor this specialist for unrelated reasons).
        coordinator.unassign_role_from_specialist(specialist_id, role.id).unwrap();
        assert!(!coordinator.specialist_has_permission(specialist_id, Permission::Execute).unwrap());
    }

    #[tokio::test]
    async fn an_authorized_specialist_is_still_picked_over_an_unauthorized_one() {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let events = StdArc::new(bus);
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let model = mock_model();
        let model_id = model.id;
        storage.insert_model(&model).unwrap();

        let proposer_only = Specialist {
            id: Uuid::new_v4(),
            name: "Backend Reviewer".into(),
            role: "reviewer".into(),
            domain: "backend".into(),
            capabilities: vec!["backend".into()],
            preferred_model_ids: vec![model_id],
            tools: vec![],
            knowledge_scope: vec![],
            permissions: vec![Permission::Read, Permission::Propose],
            output_schema: None,
            escalation_policy: EscalationPolicy::default(),
            active: true,
                    role_ids: Vec::new(), max_tokens: None, temperature: None,
        };
        let executor = general_specialist(model_id);
        let executor_id = executor.id;
        storage.insert_specialist(project_id, &proposer_only).unwrap();
        storage.insert_specialist(project_id, &executor).unwrap();

        let mut providers = ProviderRegistry::new();
        providers.register(StdArc::new(MockLocalProvider::new(vec![model])));
        let coordinator = Coordinator::new(storage, events, providers);

        let submission = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Refactor the Rust backend auth module.".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();

        let task = coordinator.get_task(submission.task_id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Completed);
        assert_eq!(task.assigned_specialist_id, Some(executor_id));
    }

    #[tokio::test]
    async fn a_completed_run_records_a_verification_passed_benchmark_outcome() {
        let (coordinator, project_id) = build_coordinator().await;

        coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Refactor the Rust backend auth module.".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();

        let ranked = coordinator.rank_specialists_for_capability(project_id, "backend").unwrap();
        assert_eq!(ranked.len(), 1);
        let (_, benchmark) = &ranked[0];
        assert_eq!(benchmark.verification_passed_count, 1);
        assert_eq!(benchmark.verification_failed_count, 0);
    }

    #[tokio::test]
    async fn the_better_benchmarked_specialist_is_picked_as_primary_owner() {
        let (coordinator, project_id, specialist_a_id, specialist_b_id) =
            build_coordinator_with_disagreeing_backend_specialists().await;

        // Give specialist B a poor track record and specialist A a
        // clean one, entirely through storage, no task execution
        // needed to establish prior history for this test.
        for _ in 0..3 {
            coordinator
                .storage()
                .record_benchmark_outcome(specialist_b_id, solexa_domain::BenchmarkOutcome::VerificationFailed)
                .unwrap();
        }
        coordinator
            .storage()
            .record_benchmark_outcome(specialist_a_id, solexa_domain::BenchmarkOutcome::VerificationPassed)
            .unwrap();

        let ranked = coordinator.rank_specialists_for_capability(project_id, "backend").unwrap();
        assert_eq!(ranked[0].0.id, specialist_a_id, "the better-benchmarked specialist should rank first");
        assert_eq!(ranked[1].0.id, specialist_b_id);

        // Both are still consulted (Phase 5 consensus is unaffected by
        // ranking), but the task's owner-of-record should be the
        // better-proven one, not whichever was inserted first.
        let submission = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Refactor the Rust backend auth module.".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();
        let task = coordinator.get_task(submission.task_id).unwrap().unwrap();
        assert_eq!(task.assigned_specialist_id, Some(specialist_a_id));
    }

    #[tokio::test]
    async fn set_task_priority_updates_the_stored_task_and_emits_an_event() {
        let (coordinator, project_id) = build_coordinator().await;
        let submission = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Refactor the Rust backend auth module.".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();
        assert_eq!(coordinator.get_task(submission.task_id).unwrap().unwrap().priority, 5);

        coordinator.set_task_priority(submission.task_id, 9).unwrap();

        let task = coordinator.get_task(submission.task_id).unwrap().unwrap();
        assert_eq!(task.priority, 9);

        let trace = coordinator.events_for_correlation(submission.correlation_id).unwrap();
        // set_task_priority uses its own correlation id (it's not part
        // of the task's execution run), so it should NOT show up here,
        // confirms it doesn't pollute the original run's trace.
        assert!(!trace.iter().any(|e| e.event_type == EventType::TaskPriorityChanged));
    }

    #[tokio::test]
    async fn set_task_priority_rejects_an_out_of_range_value() {
        let (coordinator, project_id) = build_coordinator().await;
        let submission = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Refactor the Rust backend auth module.".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();

        let err = coordinator.set_task_priority(submission.task_id, 0).unwrap_err();
        assert!(err.to_string().contains("between 1 and 10"));

        // Rejected, the stored priority must be untouched.
        assert_eq!(coordinator.get_task(submission.task_id).unwrap().unwrap().priority, 5);
    }

    #[tokio::test]
    async fn set_task_deadline_updates_the_stored_task_and_emits_an_event() {
        let (coordinator, project_id) = build_coordinator().await;
        let submission = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Refactor the Rust backend auth module.".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();
        assert_eq!(coordinator.get_task(submission.task_id).unwrap().unwrap().deadline, None);

        let deadline = chrono::Utc::now() + chrono::Duration::hours(2);
        coordinator.set_task_deadline(submission.task_id, Some(deadline)).unwrap();

        let task = coordinator.get_task(submission.task_id).unwrap().unwrap();
        // RFC 3339 round-trips to the microsecond.
        assert_eq!(task.deadline.unwrap().timestamp_micros(), deadline.timestamp_micros());

        let trace = coordinator.events_for_correlation(submission.correlation_id).unwrap();
        // Same reasoning as set_task_priority's own test: its own
        // correlation id, so it must not pollute the run's trace.
        assert!(!trace.iter().any(|e| e.event_type == EventType::TaskDeadlineChanged));
    }

    #[tokio::test]
    async fn set_task_deadline_with_none_clears_an_existing_deadline() {
        let (coordinator, project_id) = build_coordinator().await;
        let submission = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Refactor the Rust backend auth module.".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();
        coordinator.set_task_deadline(submission.task_id, Some(chrono::Utc::now())).unwrap();
        assert!(coordinator.get_task(submission.task_id).unwrap().unwrap().deadline.is_some());

        coordinator.set_task_deadline(submission.task_id, None).unwrap();
        assert_eq!(coordinator.get_task(submission.task_id).unwrap().unwrap().deadline, None);
    }

    #[tokio::test]
    async fn resolve_decision_requires_a_registered_member_and_persists_the_resolution() {
        let (coordinator, project_id) = build_coordinator().await;

        let decision = solexa_domain::Decision::new(
            project_id,
            "Which database should be used?",
            vec!["PostgreSQL".into(), "SQLite".into()],
            vec!["backend".into()],
        );
        coordinator.record_decision(decision.clone()).unwrap();

        // An unregistered approver is rejected, same rule as conflicts.
        let err = coordinator.resolve_decision(decision.id, "PostgreSQL", Uuid::new_v4()).unwrap_err();
        assert!(err.to_string().contains("is not registered"));

        let reviewer = solexa_domain::Member::new_human(project_id, "Alex");
        coordinator.register_member(reviewer.clone()).unwrap();
        coordinator.resolve_decision(decision.id, "PostgreSQL", reviewer.id).unwrap();

        let resolved = coordinator
            .list_decisions(project_id)
            .unwrap()
            .into_iter()
            .find(|d| d.id == decision.id)
            .unwrap();
        assert_eq!(resolved.status, solexa_domain::DecisionStatus::Resolved);
        assert_eq!(resolved.resolution.as_deref(), Some("PostgreSQL"));
        assert_eq!(resolved.approved_by.as_deref(), Some("Alex"));
    }

    #[tokio::test]
    async fn resolve_decision_rejects_a_resolution_that_is_not_a_declared_option() {
        let (coordinator, project_id) = build_coordinator().await;

        let decision = solexa_domain::Decision::new(
            project_id,
            "Which database should be used?",
            vec!["PostgreSQL".into(), "SQLite".into()],
            vec!["backend".into()],
        );
        coordinator.record_decision(decision.clone()).unwrap();

        let reviewer = solexa_domain::Member::new_human(project_id, "Alex");
        coordinator.register_member(reviewer.clone()).unwrap();

        let err = coordinator.resolve_decision(decision.id, "MongoDB", reviewer.id).unwrap_err();
        assert!(err.to_string().contains("not one of this decision's declared options"));

        // Rejected, the decision must remain open.
        let still_open = coordinator
            .list_decisions(project_id)
            .unwrap()
            .into_iter()
            .find(|d| d.id == decision.id)
            .unwrap();
        assert_eq!(still_open.status, solexa_domain::DecisionStatus::Open);
    }

    #[tokio::test]
    async fn archiving_a_knowledge_item_changes_status_without_deleting_it() {
        let (coordinator, project_id) = build_coordinator().await;

        let constraint = solexa_domain::KnowledgeItem::new(
            project_id,
            solexa_domain::KnowledgeKind::Constraint,
            "DB choice",
            "Must use PostgreSQL, not SQLite, in production.",
            solexa_domain::Provenance::human("alice"),
            vec!["backend".into()],
        );
        coordinator.record_knowledge(constraint.clone()).unwrap();

        coordinator.archive_knowledge_item(constraint.id).unwrap();

        let items = coordinator.list_knowledge(project_id).unwrap();
        assert_eq!(items.len(), 1, "archiving must not delete the item");
        assert_eq!(items[0].status, solexa_domain::KnowledgeStatus::Archived);
        assert_eq!(items[0].content, constraint.content);
    }

    #[tokio::test]
    async fn archiving_an_unknown_knowledge_item_fails_clearly() {
        let (coordinator, _project_id) = build_coordinator().await;
        let err = coordinator.archive_knowledge_item(Uuid::new_v4()).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn model_review_approves_normal_output_and_completes_the_task() {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let events = StdArc::new(bus);
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let generation_model = mock_model();
        let generation_model_id = generation_model.id;
        storage.insert_model(&generation_model).unwrap();

        let mut specialist = general_specialist(generation_model_id);
        specialist.escalation_policy.requires_model_review = true;
        storage.insert_specialist(project_id, &specialist).unwrap();

        let mut providers = ProviderRegistry::new();
        providers.register(StdArc::new(MockLocalProvider::new(vec![generation_model])));
        let coordinator = Coordinator::new(storage, events, providers);

        let submission = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Refactor the Rust backend auth module.".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();

        let task = coordinator.get_task(submission.task_id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Completed, "a review with no rejection marker should approve and complete");

        let trace = coordinator.events_for_correlation(submission.correlation_id).unwrap();
        assert!(trace.iter().any(|e| e.event_type == EventType::ModelReviewStarted));
        let completed = trace.iter().find(|e| e.event_type == EventType::ModelReviewCompleted).unwrap();
        assert_eq!(completed.payload["approved"], true);
        // Only one model was registered, so review honestly reused it,
        // and says so rather than hiding it.
        let started = trace.iter().find(|e| e.event_type == EventType::ModelReviewStarted).unwrap();
        assert_eq!(started.payload["same_model_as_generation"], true);
    }

    /// Round 50: the wiring proof for `REVIEW_MAX_TOKENS`, a provider
    /// that records every `GenerationRequest` it receives, with BOTH
    /// review kinds enabled on one task. The two review calls must carry
    /// the bound; the main task call must not (it stays unbounded, so
    /// nothing about ordinary generation changes).
    #[tokio::test]
    async fn review_calls_are_output_bounded_but_the_main_task_call_is_not() {
        struct RecordingProvider {
            inner: MockLocalProvider,
            seen: StdArc<std::sync::Mutex<Vec<(bool, Option<u32>)>>>,
        }
        #[async_trait::async_trait]
        impl solexa_models::ModelProvider for RecordingProvider {
            fn list_models(&self) -> Vec<ModelMetadata> {
                self.inner.list_models()
            }
            async fn generate(&self, request: solexa_models::GenerationRequest) -> anyhow::Result<solexa_models::GenerationResult> {
                let is_review = request.prompt.contains("Respond with VERDICT:");
                self.seen.lock().unwrap().push((is_review, request.max_tokens));
                self.inner.generate(request).await
            }
        }

        let storage = StdArc::new(Storage::in_memory().unwrap());
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let events = StdArc::new(bus);
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let model = mock_model();
        let model_id = model.id;
        storage.insert_model(&model).unwrap();
        let mut producer = general_specialist(model_id);
        producer.escalation_policy.requires_model_review = true;
        producer.escalation_policy.requires_specialist_review = true;
        storage.insert_specialist(project_id, &producer).unwrap();
        storage.insert_specialist(project_id, &reviewer_specialist(model_id)).unwrap();

        let seen = StdArc::new(std::sync::Mutex::new(Vec::new()));
        let mut providers = ProviderRegistry::new();
        providers.register(StdArc::new(RecordingProvider { inner: MockLocalProvider::new(vec![model]), seen: seen.clone() }));
        let coordinator = Coordinator::new(storage, events, providers);

        let submission = coordinator
            .submit_request(UserRequest { project_id, text: "Refactor the Rust backend auth module.".into(), requested_by: "human".into() })
            .await
            .unwrap();
        let trace = coordinator.events_for_correlation(submission.correlation_id).unwrap();
        assert!(trace.iter().any(|e| e.event_type == EventType::ModelReviewStarted));
        assert!(trace.iter().any(|e| e.event_type == EventType::SpecialistReviewStarted));

        let seen = seen.lock().unwrap();
        let reviews: Vec<_> = seen.iter().filter(|(is_review, _)| *is_review).collect();
        let tasks: Vec<_> = seen.iter().filter(|(is_review, _)| !*is_review).collect();
        assert_eq!(reviews.len(), 2, "one model review and one specialist review should each have called the provider");
        assert!(reviews.iter().all(|(_, m)| *m == Some(REVIEW_MAX_TOKENS)), "every review call must carry the bound: {reviews:?}");
        assert!(!tasks.is_empty() && tasks.iter().all(|(_, m)| m.is_none()), "the main task call must stay unbounded: {tasks:?}");
    }

    // ---- round 51: a truncated review is inconclusive, not approved -----

    /// Answers review calls (the prompt containing "Respond with
    /// VERDICT:") with a scripted text and finish reason, and everything
    /// else through `MockLocalProvider`, so a test can stage exactly
    /// "the reviewer was cut off by its token limit" without a real model.
    struct ScriptedReviewProvider {
        inner: MockLocalProvider,
        review_text: String,
        review_finish: FinishReason,
    }

    #[async_trait::async_trait]
    impl solexa_models::ModelProvider for ScriptedReviewProvider {
        fn list_models(&self) -> Vec<ModelMetadata> {
            self.inner.list_models()
        }
        async fn generate(&self, request: solexa_models::GenerationRequest) -> anyhow::Result<solexa_models::GenerationResult> {
            if request.prompt.contains("Respond with VERDICT:") {
                return Ok(solexa_models::GenerationResult {
                    text: self.review_text.clone(),
                    estimated_tokens: 5,
                    finish_reason: self.review_finish,
                });
            }
            self.inner.generate(request).await
        }
    }

    #[derive(Clone, Copy)]
    enum ReviewKind {
        Model,
        Specialist,
    }

    /// Runs one task through the chosen review kind with a scripted
    /// review. Returns the task's final state, the `*ReviewCompleted`
    /// payload, and the `RevisionRequested` reason if there was one.
    async fn run_task_with_scripted_review(
        kind: ReviewKind,
        review_text: &str,
        review_finish: FinishReason,
    ) -> (TaskState, serde_json::Value, Option<String>) {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let events = StdArc::new(bus);
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let model = mock_model();
        let model_id = model.id;
        storage.insert_model(&model).unwrap();
        let mut producer = general_specialist(model_id);
        match kind {
            ReviewKind::Model => producer.escalation_policy.requires_model_review = true,
            ReviewKind::Specialist => {
                producer.escalation_policy.requires_specialist_review = true;
                storage.insert_specialist(project_id, &reviewer_specialist(model_id)).unwrap();
            }
        }
        storage.insert_specialist(project_id, &producer).unwrap();

        let mut providers = ProviderRegistry::new();
        providers.register(StdArc::new(ScriptedReviewProvider {
            inner: MockLocalProvider::new(vec![model]),
            review_text: review_text.into(),
            review_finish,
        }));
        let coordinator = Coordinator::new(storage, events, providers);

        let submission = coordinator
            .submit_request(UserRequest { project_id, text: "Refactor the Rust backend auth module.".into(), requested_by: "human".into() })
            .await
            .unwrap();
        let task = coordinator.get_task(submission.task_id).unwrap().unwrap();
        let trace = coordinator.events_for_correlation(submission.correlation_id).unwrap();
        let completed_type = match kind {
            ReviewKind::Model => EventType::ModelReviewCompleted,
            ReviewKind::Specialist => EventType::SpecialistReviewCompleted,
        };
        let completed = trace.iter().find(|e| e.event_type == completed_type).expect("the review must have completed").payload.clone();
        let reason = trace
            .iter()
            .find(|e| e.event_type == EventType::RevisionRequested)
            .map(|e| e.payload["reason"].as_str().unwrap().to_string());
        (task.state, completed, reason)
    }

    // ---- round 52: per-specialist generation limits --------------------

    /// Records the limits on every non-review (i.e. main task) call and
    /// reports a scripted finish reason for it.
    struct LimitRecordingProvider {
        inner: MockLocalProvider,
        main_finish: FinishReason,
        seen: StdArc<std::sync::Mutex<Vec<(Option<u32>, Option<f32>)>>>,
    }

    #[async_trait::async_trait]
    impl solexa_models::ModelProvider for LimitRecordingProvider {
        fn list_models(&self) -> Vec<ModelMetadata> {
            self.inner.list_models()
        }
        async fn generate(&self, request: solexa_models::GenerationRequest) -> anyhow::Result<solexa_models::GenerationResult> {
            self.seen.lock().unwrap().push((request.max_tokens, request.temperature));
            let mut result = self.inner.generate(request).await?;
            result.finish_reason = self.main_finish;
            Ok(result)
        }
    }

    /// Runs one task through a specialist with the given limits. Returns
    /// the final state, the limits the provider saw, and the `Verified` and
    /// `ModelCompleted` payloads.
    async fn run_task_with_limits(
        max_tokens: Option<u32>,
        temperature: Option<f32>,
        main_finish: FinishReason,
    ) -> (TaskState, Vec<(Option<u32>, Option<f32>)>, serde_json::Value, serde_json::Value) {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let events = StdArc::new(bus);
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let model = mock_model();
        let model_id = model.id;
        storage.insert_model(&model).unwrap();
        let mut specialist = general_specialist(model_id);
        specialist.max_tokens = max_tokens;
        specialist.temperature = temperature;
        storage.insert_specialist(project_id, &specialist).unwrap();

        let seen = StdArc::new(std::sync::Mutex::new(Vec::new()));
        let mut providers = ProviderRegistry::new();
        providers.register(StdArc::new(LimitRecordingProvider { inner: MockLocalProvider::new(vec![model]), main_finish, seen: seen.clone() }));
        let coordinator = Coordinator::new(storage, events, providers);

        let submission = coordinator
            .submit_request(UserRequest { project_id, text: "Refactor the Rust backend auth module.".into(), requested_by: "human".into() })
            .await
            .unwrap();
        let task = coordinator.get_task(submission.task_id).unwrap().unwrap();
        let trace = coordinator.events_for_correlation(submission.correlation_id).unwrap();
        let payload = |t: EventType| trace.iter().find(|e| e.event_type == t).unwrap_or_else(|| panic!("missing {t:?}")).payload.clone();
        let seen_now = seen.lock().unwrap().clone();
        (task.state, seen_now, payload(EventType::Verified), payload(EventType::ModelCompleted))
    }

    #[tokio::test]
    async fn a_specialists_limits_reach_its_main_task_call_and_unset_means_unbounded() {
        let (_, seen, _, _) = run_task_with_limits(Some(7), Some(0.3), FinishReason::Stop).await;
        assert_eq!(seen, vec![(Some(7), Some(0.3))], "the specialist's own limits must be what the provider is asked to honor");

        let (_, seen, _, _) = run_task_with_limits(None, None, FinishReason::Stop).await;
        assert_eq!(seen, vec![(None, None)], "an unconfigured specialist must send the exact pre-round-52 request");
    }

    #[tokio::test]
    async fn output_cut_off_by_the_specialists_own_limit_is_not_verified_as_complete() {
        let (state, _, verified, completed) = run_task_with_limits(Some(7), None, FinishReason::Length).await;
        assert_eq!(state, TaskState::Revision, "a task whose output hit the specialist's limit must not complete");
        assert_eq!(verified["passed"], false);
        let details = verified["details"].as_str().unwrap();
        assert!(details.contains("7 token limit"), "the reason must name the limit: {details}");
        assert_eq!(completed["truncated_by_limit"], true);
        assert_eq!(completed["finish_reason"], "length");
    }

    #[tokio::test]
    async fn hitting_the_limit_is_only_a_failure_when_the_output_was_actually_cut_off() {
        // A limit that was set but not reached: the model stopped on its own.
        let (state, _, verified, completed) = run_task_with_limits(Some(500), None, FinishReason::Stop).await;
        assert_eq!(state, TaskState::Completed);
        assert_eq!(verified["passed"], true);
        assert_eq!(completed["truncated_by_limit"], false);
    }

    #[tokio::test]
    async fn a_runtime_reported_length_with_no_limit_configured_is_left_alone() {
        // Opt-in: with no limit set, a context-ceiling `Length` must not
        // start failing tasks for specialists that never asked for bounds.
        let (state, _, verified, completed) = run_task_with_limits(None, None, FinishReason::Length).await;
        assert_eq!(state, TaskState::Completed);
        assert_eq!(verified["passed"], true);
        assert_eq!(completed["truncated_by_limit"], false);
        assert_eq!(completed["finish_reason"], "length", "the finish reason is still reported for the record");
    }

    // ---- round 53: interrupting an in-flight model call ----------------

    /// Observable flags for `ObservedSlowProvider`. `finished` flips only
    /// if a slow call is allowed to run to the end of its delay, so after a
    /// cancel it staying `false` is the proof the call was aborted rather
    /// than merely abandoned; `dropped` proves its future was dropped.
    #[derive(Clone, Default)]
    struct CallFlags {
        started: StdArc<std::sync::atomic::AtomicBool>,
        finished: StdArc<std::sync::atomic::AtomicBool>,
        dropped: StdArc<std::sync::atomic::AtomicBool>,
    }

    impl CallFlags {
        fn get(flag: &StdArc<std::sync::atomic::AtomicBool>) -> bool {
            flag.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    struct ObservedSlowProvider {
        model: ModelMetadata,
        delay: std::time::Duration,
        slow_main: bool,
        slow_review: bool,
        flags: CallFlags,
    }

    #[async_trait::async_trait]
    impl solexa_models::ModelProvider for ObservedSlowProvider {
        fn list_models(&self) -> Vec<ModelMetadata> {
            vec![self.model.clone()]
        }

        async fn generate(
            &self,
            request: solexa_models::GenerationRequest,
        ) -> anyhow::Result<solexa_models::GenerationResult> {
            use std::sync::atomic::Ordering;
            let is_review = request.prompt.contains("Respond with VERDICT:");
            let text = if is_review { "VERDICT: APPROVED\nLooks fine.".to_string() } else { format!("handled: {}", request.prompt) };
            if (is_review && self.slow_review) || (!is_review && self.slow_main) {
                struct DropFlag(StdArc<std::sync::atomic::AtomicBool>);
                impl Drop for DropFlag {
                    fn drop(&mut self) {
                        self.0.store(true, Ordering::SeqCst);
                    }
                }
                let _on_drop = DropFlag(self.flags.dropped.clone());
                self.flags.started.store(true, Ordering::SeqCst);
                tokio::time::sleep(self.delay).await;
                self.flags.finished.store(true, Ordering::SeqCst);
            }
            Ok(solexa_models::GenerationResult { text, estimated_tokens: 1, finish_reason: solexa_models::FinishReason::Unknown })
        }
    }

    /// Records every event published, regardless of correlation id,
    /// needed because `cancel_task` emits `TaskCancelled` under its own
    /// fresh correlation id (a human action), so it is not part of the
    /// request's trace.
    #[derive(Clone, Default)]
    struct CaptureSink(StdArc<std::sync::Mutex<Vec<(EventType, serde_json::Value)>>>);

    impl solexa_events::EventSink for CaptureSink {
        fn record(&self, event: &solexa_events::Event) -> anyhow::Result<()> {
            self.0.lock().unwrap().push((event.event_type, event.payload.clone()));
            Ok(())
        }
    }

    struct InFlightHarness {
        coordinator: StdArc<Coordinator>,
        storage: StdArc<Storage>,
        project_id: Uuid,
        flags: CallFlags,
        captured: CaptureSink,
    }

    /// `specialist_count` > 1 gives several specialists the SAME
    /// capability, which is what makes the coordinator run a consensus
    /// round (concurrent model calls for one task).
    fn in_flight_harness(slow_main: bool, slow_review: bool, requires_model_review: bool, specialist_count: usize, delay_ms: u64) -> InFlightHarness {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let captured = CaptureSink::default();
        bus.add_sink(Box::new(captured.clone()));
        let events = StdArc::new(bus);
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let model = mock_model();
        storage.insert_model(&model).unwrap();
        for n in 0..specialist_count {
            let mut specialist = general_specialist(model.id);
            specialist.id = Uuid::new_v4();
            specialist.name = format!("General {n}");
            specialist.escalation_policy.requires_model_review = requires_model_review;
            storage.insert_specialist(project_id, &specialist).unwrap();
        }

        let flags = CallFlags::default();
        let mut providers = ProviderRegistry::new();
        providers.register(StdArc::new(ObservedSlowProvider {
            model,
            delay: std::time::Duration::from_millis(delay_ms),
            slow_main,
            slow_review,
            flags: flags.clone(),
        }));
        let coordinator = StdArc::new(Coordinator::new(storage.clone(), events, providers));
        InFlightHarness { coordinator, storage, project_id, flags, captured }
    }

    impl InFlightHarness {
        fn submit(&self) -> tokio::task::JoinHandle<Result<Submission>> {
            let coordinator = self.coordinator.clone();
            let project_id = self.project_id;
            tokio::spawn(async move {
                coordinator
                    .submit_request(UserRequest { project_id, text: "Refactor the Rust backend auth module.".into(), requested_by: "human".into() })
                    .await
            })
        }

        async fn wait_until_started(&self) {
            for _ in 0..200 {
                if CallFlags::get(&self.flags.started) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            panic!("the slow model call never started");
        }

        fn only_task(&self) -> Task {
            let tasks = self.storage.list_tasks(self.project_id).unwrap();
            assert_eq!(tasks.len(), 1, "this harness submits one undecomposed task");
            tasks.into_iter().next().unwrap()
        }
    }

    #[tokio::test]
    async fn cancelling_a_task_whose_model_call_is_in_flight_aborts_that_call_and_ends_the_request() {
        let h = in_flight_harness(true, false, false, 1, 1200);
        let submit = h.submit();
        h.wait_until_started().await;
        let task = h.only_task();
        assert_eq!(task.state, TaskState::Running, "precondition: genuinely mid-generation");

        let t0 = std::time::Instant::now();
        h.coordinator.cancel_task(task.id).expect("an in-flight task is cancellable now, not refused");
        let submission = tokio::time::timeout(std::time::Duration::from_millis(900), submit)
            .await
            .expect("the request must end promptly, not wait out the 1200ms model call")
            .unwrap()
            .expect("a cancelled request is not an error");
        assert!(t0.elapsed() < std::time::Duration::from_millis(900), "took {:?}", t0.elapsed());

        assert_eq!(h.coordinator.get_task(task.id).unwrap().unwrap().state, TaskState::Cancelled, "nothing may resurrect it");

        // The real proof: wait well past when the call WOULD have finished.
        tokio::time::sleep(std::time::Duration::from_millis(1400)).await;
        assert!(CallFlags::get(&h.flags.dropped), "the provider call's future must have been dropped");
        assert!(!CallFlags::get(&h.flags.finished), "the provider call must have been ABORTED, not left running detached in the background");

        let snapshot = h.coordinator.resource_snapshot();
        assert_eq!((snapshot.running, snapshot.queued), (0, 0), "the interrupted task must release its hardware");
        assert_eq!(h.coordinator.resources.pending_cancellations(), 0, "a task cancelled while generating never consumed its cancel marker, it must be pruned when the task ends, not kept forever");

        let all = h.captured.0.lock().unwrap().clone();
        let cancelled = all.iter().find(|(t, _)| *t == EventType::TaskCancelled).expect("a TaskCancelled event");
        assert_eq!(cancelled.1["interrupted"], true, "the task was inside the pipeline, so it was interrupted");
        let trace = h.coordinator.events_for_correlation(submission.correlation_id).unwrap();
        for finished_type in [EventType::ModelCompleted, EventType::Verified, EventType::TaskCompleted] {
            assert!(!trace.iter().any(|e| e.event_type == finished_type), "a cancelled task must not report {finished_type:?}");
        }
    }

    #[tokio::test]
    async fn the_same_slow_task_left_alone_runs_to_completion_so_the_cancel_test_is_not_vacuous() {
        // Positive control for the test above: with no cancel, the identical
        // setup completes, `finished` DOES flip, and the task ends Completed.
        let h = in_flight_harness(true, false, false, 1, 300);
        let submission = h.submit().await.unwrap().unwrap();
        assert!(CallFlags::get(&h.flags.finished), "an uncancelled slow call runs to the end of its delay");
        assert_eq!(h.coordinator.get_task(submission.task_id).unwrap().unwrap().state, TaskState::Completed);
    }

    #[tokio::test]
    async fn cancelling_during_a_model_review_interrupts_the_review_and_the_task_stays_cancelled() {
        // The main call is instant; the REVIEW call is the slow one in
        // flight, and its result would otherwise complete the task.
        let h = in_flight_harness(false, true, true, 1, 1200);
        let submit = h.submit();
        h.wait_until_started().await;
        let task = h.only_task();
        assert_eq!(task.state, TaskState::Review, "precondition: the task is mid-review");

        h.coordinator.cancel_task(task.id).unwrap();
        tokio::time::timeout(std::time::Duration::from_millis(900), submit)
            .await
            .expect("must not wait out the review call")
            .unwrap()
            .unwrap();

        assert_eq!(h.coordinator.get_task(task.id).unwrap().unwrap().state, TaskState::Cancelled, "neither Completed nor Revision may overwrite it");
        tokio::time::sleep(std::time::Duration::from_millis(1400)).await;
        assert!(!CallFlags::get(&h.flags.finished), "the review call must have been aborted too");
        assert_eq!(h.coordinator.get_task(task.id).unwrap().unwrap().state, TaskState::Cancelled, "still cancelled long after the review would have returned");
    }

    #[tokio::test]
    async fn cancelling_a_consensus_task_interrupts_every_concurrent_model_call() {
        // Two specialists share a capability, so two model calls run at once.
        let h = in_flight_harness(true, false, false, 2, 1200);
        let submit = h.submit();
        h.wait_until_started().await;
        let task = h.only_task();

        h.coordinator.cancel_task(task.id).unwrap();
        tokio::time::timeout(std::time::Duration::from_millis(900), submit)
            .await
            .expect("both calls must be interrupted, not just one")
            .unwrap()
            .unwrap();
        assert_eq!(h.coordinator.get_task(task.id).unwrap().unwrap().state, TaskState::Cancelled);
        tokio::time::sleep(std::time::Duration::from_millis(1400)).await;
        assert!(!CallFlags::get(&h.flags.finished), "neither concurrent call may have run to the end");
        let snapshot = h.coordinator.resource_snapshot();
        assert_eq!((snapshot.running, snapshot.queued), (0, 0));
    }

    #[tokio::test]
    async fn cancelling_a_decomposed_parent_interrupts_all_of_its_generating_subtasks() {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let events = StdArc::new(bus);
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();
        let model = mock_model();
        storage.insert_model(&model).unwrap();
        storage.insert_specialist(project_id, &general_specialist(model.id)).unwrap();
        storage.insert_specialist(project_id, &sql_specialist(model.id)).unwrap();
        let flags = CallFlags::default();
        let mut providers = ProviderRegistry::new();
        providers.register(StdArc::new(ObservedSlowProvider { model, delay: std::time::Duration::from_millis(1200), slow_main: true, slow_review: false, flags: flags.clone() }));
        let coordinator = StdArc::new(Coordinator::new(storage.clone(), events, providers));

        let submit = tokio::spawn({
            let coordinator = coordinator.clone();
            async move {
                coordinator
                    .submit_request(UserRequest { project_id, text: "Refactor the Rust backend and write the SQL migration for the new auth table.".into(), requested_by: "human".into() })
                    .await
            }
        });
        for _ in 0..200 {
            if CallFlags::get(&flags.started) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let tasks = storage.list_tasks(project_id).unwrap();
        assert_eq!(tasks.len(), 3, "a parent and two subtasks");
        let parent = tasks.iter().find(|t| t.parent_task_id.is_none()).unwrap();

        coordinator.cancel_task(parent.id).unwrap();
        tokio::time::timeout(std::time::Duration::from_millis(900), submit)
            .await
            .expect("cancelling the parent must end the whole request promptly")
            .unwrap()
            .expect("a cancelled request is not an error");

        for task in storage.list_tasks(project_id).unwrap() {
            assert_eq!(task.state, TaskState::Cancelled, "task {} ({}) must be Cancelled, and the parent must not have been escalated to Review afterwards", task.id, task.title);
        }
        tokio::time::sleep(std::time::Duration::from_millis(1400)).await;
        assert!(!CallFlags::get(&flags.finished), "no subtask's model call may have run to completion");
        assert_eq!(coordinator.resources.pending_cancellations(), 0, "neither the parent (which never waits) nor the generating subtasks may leave a cancel marker behind");
    }

    #[tokio::test]
    async fn a_pipeline_write_can_never_overwrite_a_cancelled_task() {
        // The invariant that makes interrupting safe, tested directly: a
        // late write from the pipeline (whose in-memory copy still says
        // Running) is refused, and the stored Cancelled survives.
        let (coordinator, project_id) = build_coordinator().await;
        coordinator
            .submit_request(UserRequest { project_id, text: "Refactor the Rust backend auth module.".into(), requested_by: "human".into() })
            .await
            .unwrap();
        let mut cancelled = coordinator.storage.list_tasks(project_id).unwrap().remove(0);
        cancelled.state = TaskState::Cancelled;
        coordinator.storage.update_task(&cancelled).unwrap();

        let mut stale = cancelled.clone();
        stale.state = TaskState::Running;
        let err = coordinator.persist_task(&stale).unwrap_err();
        assert!(err.downcast_ref::<TaskCancelled>().is_some(), "{err}");
        assert_eq!(coordinator.get_task(cancelled.id).unwrap().unwrap().state, TaskState::Cancelled);

        // Positive controls: reconciling TO Cancelled is allowed, and an
        // ordinary task is written normally.
        coordinator.persist_task(&cancelled).unwrap();
        let mut ordinary = cancelled.clone();
        ordinary.id = Uuid::new_v4();
        ordinary.state = TaskState::Running;
        coordinator.storage.insert_task(&ordinary).unwrap();
        ordinary.title = "renamed".into();
        coordinator.persist_task(&ordinary).unwrap();
        assert_eq!(coordinator.get_task(ordinary.id).unwrap().unwrap().title, "renamed");
    }

    // ---- round 54: backend validation of generation limits ------------

    #[tokio::test]
    async fn registering_or_editing_a_specialist_rejects_invalid_limits_in_the_backend_not_just_the_ui() {
        let (coordinator, project_id) = build_coordinator().await;
        let model_id = coordinator.list_models()[0].id;

        for (max_tokens, temperature) in [(Some(0), None), (None, Some(2.5)), (None, Some(-1.0)), (None, Some(f32::NAN))] {
            let mut candidate = general_specialist(model_id);
            candidate.name = "Limits Candidate".into();
            candidate.max_tokens = max_tokens;
            candidate.temperature = temperature;
            let err = coordinator.register_specialist(project_id, candidate).unwrap_err();
            assert!(err.to_string().contains("token") || err.to_string().contains("temperature"), "{max_tokens:?}/{temperature:?}: {err}");
        }
        assert!(
            !coordinator.list_specialists(project_id).unwrap().iter().any(|s| s.name == "Limits Candidate"),
            "a rejected registration must not have stored anything"
        );

        // Positive control: the boundary values are accepted.
        let mut ok = general_specialist(model_id);
        ok.name = "Limits Candidate".into();
        ok.max_tokens = Some(1);
        ok.temperature = Some(2.0);
        coordinator.register_specialist(project_id, ok.clone()).unwrap();

        // Editing: a bad edit is refused and the stored limits are untouched.
        let mut bad_edit = ok.clone();
        bad_edit.max_tokens = Some(0);
        assert!(coordinator.edit_specialist(project_id, ok.id, bad_edit).is_err());
        let mut bad_temp = ok.clone();
        bad_temp.temperature = Some(7.0);
        assert!(coordinator.edit_specialist(project_id, ok.id, bad_temp).is_err());
        let stored = coordinator.list_specialists(project_id).unwrap().into_iter().find(|s| s.id == ok.id).unwrap();
        assert_eq!((stored.max_tokens, stored.temperature), (Some(1), Some(2.0)), "a refused edit must leave the stored limits alone");
    }

    #[tokio::test]
    async fn a_specialist_that_reached_storage_with_invalid_limits_never_sends_them_to_the_provider() {
        // `run_task_with_limits` writes straight to storage, the way a
        // specialist applied from another device's git snapshot does, it
        // never passed `register_specialist`'s validation. Zero tokens and
        // an out-of-range temperature would fail EVERY task it runs.
        let (state, seen, verified, completed) = run_task_with_limits(Some(0), Some(9.0), FinishReason::Length).await;
        assert_eq!(seen, vec![(None, None)], "invalid limits must be dropped, not sent to the daemon");
        assert_eq!(state, TaskState::Completed, "and a zero limit must not make every result look truncated");
        assert_eq!(verified["passed"], true);
        assert_eq!(completed["truncated_by_limit"], false);
    }

    #[tokio::test]
    async fn editing_a_specialist_never_strips_its_roles_but_does_apply_new_limits() {
        // Latent since round 47: `edit_specialist` carried `active` over
        // from the stored row but took `role_ids` straight from the caller.
        // The desktop edit form has no role control (roles are managed by
        // assign/unassign) so it sends an empty list, and every ordinary
        // edit silently removed the specialist's roles, and the
        // permissions they granted.
        let (coordinator, project_id) = build_coordinator().await;
        let mut specialist = coordinator.list_specialists(project_id).unwrap().remove(0);
        let specialist_id = specialist.id;
        let role = coordinator.create_role(project_id, "Executor", vec![Permission::Execute]).unwrap();
        coordinator.assign_role_to_specialist(specialist_id, role.id).unwrap();

        // What an edit form submits: no role_ids, new limits.
        specialist.role_ids = Vec::new();
        specialist.max_tokens = Some(300);
        specialist.temperature = Some(0.2);
        coordinator.edit_specialist(project_id, specialist_id, specialist).unwrap();

        let reloaded = coordinator.list_specialists(project_id).unwrap().into_iter().find(|s| s.id == specialist_id).unwrap();
        assert_eq!(reloaded.role_ids, vec![role.id], "an unrelated field edit must not remove an assigned role");
        assert_eq!((reloaded.max_tokens, reloaded.temperature), (Some(300), Some(0.2)), "the limits ARE part of the edit");

        // And clearing a limit through an edit really clears it.
        let mut again = reloaded;
        again.max_tokens = None;
        again.temperature = None;
        coordinator.edit_specialist(project_id, specialist_id, again).unwrap();
        let cleared = coordinator.list_specialists(project_id).unwrap().into_iter().find(|s| s.id == specialist_id).unwrap();
        assert_eq!((cleared.max_tokens, cleared.temperature), (None, None));
        assert_eq!(cleared.role_ids, vec![role.id]);
    }

    #[test]
    fn review_verdict_only_calls_a_missing_verdict_inconclusive_when_the_output_was_cut_off() {
        use FinishReason::*;
        // No verdict line: truncation is the only thing that changes the answer.
        assert_eq!(review_verdict("thinking about it...", Length), ReviewVerdict::Inconclusive);
        assert_eq!(review_verdict("thinking about it...", Stop), ReviewVerdict::Approved);
        assert_eq!(review_verdict("thinking about it...", Unknown), ReviewVerdict::Approved);
        // A verdict that made it out before the cutoff still stands, either way.
        assert_eq!(review_verdict("VERDICT: APPROVED\nReason: fine, and also", Length), ReviewVerdict::Approved);
        assert_eq!(review_verdict("VERDICT: REJECTED\nReason: bad, and also", Length), ReviewVerdict::Rejected);
        assert_eq!(review_verdict("  verdict: rejected", Stop), ReviewVerdict::Rejected);
    }

    #[tokio::test]
    async fn a_model_review_cut_off_before_its_verdict_sends_the_task_to_revision_not_completion() {
        let (state, completed, reason) =
            run_task_with_scripted_review(ReviewKind::Model, "Let me think through this carefully. First, the function", FinishReason::Length).await;
        assert_eq!(state, TaskState::Revision, "a truncated review is not an approval");
        assert_eq!(completed["approved"], false);
        assert_eq!(completed["inconclusive"], true);
        assert_eq!(completed["finish_reason"], "length");
        let reason = reason.expect("the task should have been sent back with a reason");
        assert!(reason.contains("inconclusive") && reason.contains("output limit"), "got: {reason}");
        assert!(!reason.contains("rejected the result"), "an inconclusive review must not be reported as a rejection: {reason}");
    }

    #[tokio::test]
    async fn a_specialist_review_cut_off_before_its_verdict_sends_the_task_to_revision_not_completion() {
        let (state, completed, reason) =
            run_task_with_scripted_review(ReviewKind::Specialist, "Considering the edge cases first", FinishReason::Length).await;
        assert_eq!(state, TaskState::Revision);
        assert_eq!(completed["approved"], false);
        assert_eq!(completed["inconclusive"], true);
        let reason = reason.unwrap();
        assert!(reason.starts_with("specialist review was inconclusive"), "got: {reason}");
    }

    #[tokio::test]
    async fn a_review_with_no_verdict_that_finished_normally_still_approves_as_before() {
        for finish in [FinishReason::Stop, FinishReason::Unknown] {
            for kind in [ReviewKind::Model, ReviewKind::Specialist] {
                let (state, completed, reason) = run_task_with_scripted_review(kind, "Looks fine to me.", finish).await;
                assert_eq!(state, TaskState::Completed, "unchanged default for a review that wasn't cut off ({finish:?})");
                assert_eq!(completed["approved"], true);
                assert_eq!(completed["inconclusive"], false);
                assert!(reason.is_none());
            }
        }
    }

    #[tokio::test]
    async fn a_review_that_hit_the_limit_after_giving_its_verdict_keeps_that_verdict() {
        let (state, completed, _) =
            run_task_with_scripted_review(ReviewKind::Model, "VERDICT: APPROVED\nReason: correct, and furthermore the", FinishReason::Length).await;
        assert_eq!(state, TaskState::Completed);
        assert_eq!(completed["inconclusive"], false);

        let (state, completed, reason) =
            run_task_with_scripted_review(ReviewKind::Specialist, "VERDICT: REJECTED\nReason: wrong, and furthermore the", FinishReason::Length).await;
        assert_eq!(state, TaskState::Revision);
        assert_eq!(completed["inconclusive"], false);
        assert!(reason.unwrap().contains("specialist review rejected the result"));
    }

    #[tokio::test]
    async fn model_review_rejection_sends_the_task_to_revision_instead_of_completing() {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let events = StdArc::new(bus);
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let generation_model = mock_model();
        let generation_model_id = generation_model.id;
        storage.insert_model(&generation_model).unwrap();
        // A second, distinct model so review-model-selection ("pick any
        // model different from the one that generated the result")
        // actually reaches MockReviewerProvider's rejection logic,
        // MockLocalProvider itself never emits "VERDICT: REJECTED" no
        // matter what it's asked to review, so reviewing via the same
        // model as generation could never exercise this path.
        let mut reviewer_model = mock_model();
        reviewer_model.id = Uuid::new_v4();
        reviewer_model.name = "mock-reviewer".into();
        storage.insert_model(&reviewer_model).unwrap();

        let mut specialist = general_specialist(generation_model_id);
        specialist.escalation_policy.requires_model_review = true;
        storage.insert_specialist(project_id, &specialist).unwrap();

        let mut providers = ProviderRegistry::new();
        providers.register(StdArc::new(MockLocalProvider::new(vec![generation_model])));
        providers.register(StdArc::new(MockReviewerProvider::new(vec![reviewer_model])));
        let coordinator = Coordinator::new(storage, events, providers);

        // MockLocalProvider's output always echoes the task prompt
        // verbatim, so embedding the marker in the request text is
        // enough for it to appear inside the generated content that
        // then gets reviewed, MockReviewerProvider flags any reviewed
        // content containing "REVIEW_REJECT".
        let submission = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Refactor the Rust backend auth module. REVIEW_REJECT".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();

        let task = coordinator.get_task(submission.task_id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Revision);

        let trace = coordinator.events_for_correlation(submission.correlation_id).unwrap();
        assert!(!trace.iter().any(|e| e.event_type == EventType::TaskCompleted));
        let revision = trace.iter().find(|e| e.event_type == EventType::RevisionRequested).unwrap();
        assert!(revision.payload["reason"].as_str().unwrap().contains("model review rejected"));

        let review_started = trace.iter().find(|e| e.event_type == EventType::ModelReviewStarted).unwrap();
        assert_eq!(
            review_started.payload["same_model_as_generation"], false,
            "the distinct reviewer model should have been selected, not the generation model"
        );
    }

    #[tokio::test]
    async fn specialist_review_approves_and_completes_the_task() {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let events = StdArc::new(bus);
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let generation_model = mock_model();
        let generation_model_id = generation_model.id;
        storage.insert_model(&generation_model).unwrap();

        let mut producer = general_specialist(generation_model_id);
        producer.escalation_policy.requires_specialist_review = true;
        storage.insert_specialist(project_id, &producer).unwrap();

        let reviewer = reviewer_specialist(generation_model_id);
        let reviewer_id = reviewer.id;
        storage.insert_specialist(project_id, &reviewer).unwrap();

        let mut providers = ProviderRegistry::new();
        providers.register(StdArc::new(MockLocalProvider::new(vec![generation_model])));
        let coordinator = Coordinator::new(storage, events, providers);

        let submission = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Refactor the Rust backend auth module.".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();

        let task = coordinator.get_task(submission.task_id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Completed, "a review with no rejection marker should approve and complete");
        // The producing specialist, not the reviewer, is still the one
        // of record for this task, review is a gate on acceptance, not
        // a reassignment.
        assert_eq!(task.assigned_specialist_id, Some(producer.id));

        let trace = coordinator.events_for_correlation(submission.correlation_id).unwrap();
        assert!(trace.iter().any(|e| e.event_type == EventType::SpecialistReviewStarted));
        let started = trace.iter().find(|e| e.event_type == EventType::SpecialistReviewStarted).unwrap();
        assert_eq!(started.payload["reviewer_specialist_id"], reviewer_id.to_string());
        let completed = trace.iter().find(|e| e.event_type == EventType::SpecialistReviewCompleted).unwrap();
        assert_eq!(completed.payload["approved"], true);
        // Model review was never opted into, so it must never run,
        // specialist review and model review are independent toggles.
        assert!(!trace.iter().any(|e| e.event_type == EventType::ModelReviewStarted));
    }

    #[tokio::test]
    async fn specialist_review_rejection_sends_the_task_to_revision_instead_of_completing() {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let events = StdArc::new(bus);
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let generation_model = mock_model();
        let generation_model_id = generation_model.id;
        storage.insert_model(&generation_model).unwrap();
        // A second, distinct model for the reviewer specialist to run on
        // same reasoning as the model-review rejection test:
        // MockLocalProvider never emits "VERDICT: REJECTED" on its own,
        // so exercising the rejection path needs MockReviewerProvider.
        let mut reviewer_model = mock_model();
        reviewer_model.id = Uuid::new_v4();
        reviewer_model.name = "mock-reviewer".into();
        storage.insert_model(&reviewer_model).unwrap();

        let mut producer = general_specialist(generation_model_id);
        producer.escalation_policy.requires_specialist_review = true;
        storage.insert_specialist(project_id, &producer).unwrap();

        let reviewer = reviewer_specialist(reviewer_model.id);
        storage.insert_specialist(project_id, &reviewer).unwrap();

        let mut providers = ProviderRegistry::new();
        providers.register(StdArc::new(MockLocalProvider::new(vec![generation_model])));
        providers.register(StdArc::new(MockReviewerProvider::new(vec![reviewer_model])));
        let coordinator = Coordinator::new(storage, events, providers);

        let submission = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Refactor the Rust backend auth module. REVIEW_REJECT".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();

        let task = coordinator.get_task(submission.task_id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Revision);

        let trace = coordinator.events_for_correlation(submission.correlation_id).unwrap();
        assert!(!trace.iter().any(|e| e.event_type == EventType::TaskCompleted));
        let revision = trace.iter().find(|e| e.event_type == EventType::RevisionRequested).unwrap();
        assert!(revision.payload["reason"].as_str().unwrap().contains("specialist review rejected"));
        let completed = trace.iter().find(|e| e.event_type == EventType::SpecialistReviewCompleted).unwrap();
        assert_eq!(completed.payload["approved"], false);
    }

    /// When literally no other specialist is registered at all, there is
    /// nothing meaningful to fall back to (unlike `model_review`, which
    /// can always honestly fall back to the same model), the check
    /// should skip itself and let deterministic verification, which
    /// already passed, stand on its own.
    #[tokio::test]
    async fn specialist_review_skips_gracefully_when_no_other_specialist_exists() {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let events = StdArc::new(bus);
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let model = mock_model();
        let model_id = model.id;
        storage.insert_model(&model).unwrap();

        let mut producer = general_specialist(model_id);
        producer.escalation_policy.requires_specialist_review = true;
        storage.insert_specialist(project_id, &producer).unwrap();
        // No second specialist registered at all.

        let mut providers = ProviderRegistry::new();
        providers.register(StdArc::new(MockLocalProvider::new(vec![model])));
        let coordinator = Coordinator::new(storage, events, providers);

        let submission = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Refactor the Rust backend auth module.".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();

        let task = coordinator.get_task(submission.task_id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Completed, "no eligible reviewer should skip the check, not block the task");

        let trace = coordinator.events_for_correlation(submission.correlation_id).unwrap();
        assert!(trace.iter().any(|e| e.event_type == EventType::SpecialistReviewSkipped));
        assert!(!trace.iter().any(|e| e.event_type == EventType::SpecialistReviewStarted));
    }

    /// A candidate that overlaps on capability but lacks
    /// `Permission::Review` must be excluded from consideration, not
    /// just never happen to be picked, this proves the permission gate
    /// is actually enforced, the same standard Phase 7 held
    /// `Permission::Execute` to for task assignment.
    #[tokio::test]
    async fn specialist_review_excludes_a_candidate_without_review_permission_and_skips() {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let events = StdArc::new(bus);
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let model = mock_model();
        let model_id = model.id;
        storage.insert_model(&model).unwrap();

        let mut producer = general_specialist(model_id);
        producer.escalation_policy.requires_specialist_review = true;
        storage.insert_specialist(project_id, &producer).unwrap();

        // Same capability as the producer, so it would be an eligible
        // reviewer on capability grounds alone, but it never opted
        // into Permission::Review. Deliberately NOT given Execute
        // either: with Execute it would also become eligible for the
        // task's *initial* assignment alongside the producer, and two
        // Execute-eligible specialists both attempting the same task
        // triggers Phase 5's conflict path instead of ever reaching
        // specialist review at all, a different mechanism than the one
        // this test means to exercise.
        let mut no_review_permission = reviewer_specialist(model_id);
        no_review_permission.permissions = vec![Permission::Read, Permission::Propose];
        storage.insert_specialist(project_id, &no_review_permission).unwrap();

        let mut providers = ProviderRegistry::new();
        providers.register(StdArc::new(MockLocalProvider::new(vec![model])));
        let coordinator = Coordinator::new(storage, events, providers);

        let submission = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Refactor the Rust backend auth module.".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();

        let task = coordinator.get_task(submission.task_id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Completed);

        let trace = coordinator.events_for_correlation(submission.correlation_id).unwrap();
        assert!(trace.iter().any(|e| e.event_type == EventType::SpecialistReviewSkipped));
        assert!(!trace.iter().any(|e| e.event_type == EventType::SpecialistReviewStarted));
    }

    // ---- per-project model configuration (round 41) --------------------

    #[tokio::test]
    async fn models_available_to_excludes_only_what_this_project_has_disabled() {
        let (coordinator, project_id) = build_coordinator().await;
        let all = coordinator.list_models();
        assert_eq!(coordinator.models_available_to(project_id).unwrap().len(), all.len());

        let model_id = all[0].id;
        coordinator.set_model_enabled(project_id, model_id, false).unwrap();
        let available = coordinator.models_available_to(project_id).unwrap();
        assert_eq!(available.len(), all.len() - 1);
        assert!(!available.iter().any(|m| m.id == model_id));
        assert_eq!(coordinator.disabled_model_ids(project_id).unwrap(), vec![model_id]);

        coordinator.set_model_enabled(project_id, model_id, true).unwrap();
        assert_eq!(coordinator.models_available_to(project_id).unwrap().len(), all.len());
    }

    #[tokio::test]
    async fn a_specialist_whose_preferred_model_is_disabled_falls_back_to_another_available_model() {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let events = StdArc::new(bus);

        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();
        let preferred = mock_model();
        let preferred_id = preferred.id;
        let fallback = mock_model();
        let fallback_id = fallback.id;
        storage.insert_model(&preferred).unwrap();
        storage.insert_model(&fallback).unwrap();
        storage.insert_specialist(project_id, &general_specialist(preferred_id)).unwrap();
        let mut providers = ProviderRegistry::new();
        providers.register(StdArc::new(MockLocalProvider::new(vec![preferred, fallback])));
        let coordinator = Coordinator::new(storage, events, providers);

        // Disabling the specialist's own preferred model *for this
        // project* round 41's actual claim is that this is enough to
        // keep it from being used, even though the specialist itself
        // was never touched and still lists it first.
        coordinator.set_model_enabled(project_id, preferred_id, false).unwrap();

        let submission = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Refactor the Rust backend auth module.".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();
        let task = coordinator.get_task(submission.task_id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Completed);
        assert_eq!(task.result.unwrap().produced_by_model_id, fallback_id);

        let trace = coordinator.events_for_correlation(submission.correlation_id).unwrap();
        let invoked = trace.iter().find(|e| e.event_type == EventType::ModelInvoked).unwrap();
        assert_eq!(invoked.payload["model_id"], fallback_id.to_string());
    }

    #[tokio::test]
    async fn model_review_never_picks_a_project_disabled_model_even_as_the_same_model_fallback() {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let events = StdArc::new(bus);

        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();
        let generation_model = mock_model();
        let generation_model_id = generation_model.id;
        let other_model = mock_model();
        let other_model_id = other_model.id;
        storage.insert_model(&generation_model).unwrap();
        storage.insert_model(&other_model).unwrap();
        let mut specialist = general_specialist(generation_model_id);
        specialist.escalation_policy.requires_model_review = true;
        storage.insert_specialist(project_id, &specialist).unwrap();
        let mut providers = ProviderRegistry::new();
        providers.register(StdArc::new(MockLocalProvider::new(vec![generation_model, other_model])));
        let coordinator = Coordinator::new(storage, events, providers);

        // Without this, model_review would pick `other_model` as
        // reviewer (any model different from the generation model).
        // Disabling it for this project should force the "reuse the
        // generation model" fallback instead, exactly as if
        // `other_model` had never been registered at all.
        coordinator.set_model_enabled(project_id, other_model_id, false).unwrap();

        let submission = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Refactor the Rust backend auth module.".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();
        let task = coordinator.get_task(submission.task_id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Completed);

        let trace = coordinator.events_for_correlation(submission.correlation_id).unwrap();
        let started = trace.iter().find(|e| e.event_type == EventType::ModelReviewStarted).unwrap();
        assert_eq!(started.payload["reviewer_model_id"], generation_model_id.to_string());
        assert_eq!(started.payload["same_model_as_generation"], true);
    }

    #[tokio::test]
    async fn model_review_streams_live_token_events_from_its_own_generation_too() {
        // Round 40: model_review used to call `provider.generate()`
        // directly, no live token events at all from a review pass,
        // only from the specialist's own generation via
        // run_specialist. Built by hand (see
        // model_token_generated_events_are_live_only_and_never_reach_durable_history,
        // round 39) so a subscriber can be grabbed before `events`
        // moves into `Coordinator::new`.
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let events = StdArc::new(bus);
        let mut live = events.subscribe();

        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();
        let generation_model = mock_model();
        let generation_model_id = generation_model.id;
        storage.insert_model(&generation_model).unwrap();
        let mut specialist = general_specialist(generation_model_id);
        specialist.escalation_policy.requires_model_review = true;
        storage.insert_specialist(project_id, &specialist).unwrap();
        let mut providers = ProviderRegistry::new();
        providers.register(StdArc::new(MockLocalProvider::new(vec![generation_model])));
        let coordinator = Coordinator::new(storage, events, providers);

        let submission = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Refactor the Rust backend auth module.".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();
        let task = coordinator.get_task(submission.task_id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Completed);

        let mut token_events = Vec::new();
        while let Ok(event) = live.try_recv() {
            if event.event_type == EventType::ModelTokenGenerated {
                token_events.push(event);
            }
        }

        // MockLocalProvider's honest default streaming is one chunk per
        // `generate()`-equivalent call, and this run makes exactly two
        // such calls: the specialist's own generation (run_specialist)
        // and the review pass (model_review). Two token events, not
        // one, is the actual proof this round's feature exists.
        assert_eq!(token_events.len(), 2, "{token_events:?}");
        let review_token = token_events
            .iter()
            .find(|e| e.payload.get("reviewer_model_id").is_some())
            .expect("model_review's own generation must stream a token event too");
        assert_eq!(review_token.payload["task_id"], task.id.to_string());
        assert_eq!(review_token.payload["chunk_index"], 1);
        assert_eq!(review_token.correlation_id, submission.correlation_id);

        // Still ephemeral: neither reached durable storage.
        let trace = coordinator.events_for_correlation(submission.correlation_id).unwrap();
        assert!(trace.iter().all(|e| e.event_type != EventType::ModelTokenGenerated));
    }

    #[tokio::test]
    async fn specialist_review_streams_live_token_events_from_its_own_generation_too() {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let events = StdArc::new(bus);
        let mut live = events.subscribe();

        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();
        let model = mock_model();
        let model_id = model.id;
        storage.insert_model(&model).unwrap();
        let mut producer = general_specialist(model_id);
        producer.escalation_policy.requires_specialist_review = true;
        storage.insert_specialist(project_id, &producer).unwrap();
        let reviewer = reviewer_specialist(model_id);
        let reviewer_id = reviewer.id;
        storage.insert_specialist(project_id, &reviewer).unwrap();
        let mut providers = ProviderRegistry::new();
        providers.register(StdArc::new(MockLocalProvider::new(vec![model])));
        let coordinator = Coordinator::new(storage, events, providers);

        let submission = coordinator
            .submit_request(UserRequest {
                project_id,
                text: "Refactor the Rust backend auth module.".into(),
                requested_by: "human".into(),
            })
            .await
            .unwrap();
        let task = coordinator.get_task(submission.task_id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Completed);

        let mut token_events = Vec::new();
        while let Ok(event) = live.try_recv() {
            if event.event_type == EventType::ModelTokenGenerated {
                token_events.push(event);
            }
        }

        assert_eq!(token_events.len(), 2, "{token_events:?}");
        let review_token = token_events
            .iter()
            .find(|e| e.payload.get("reviewer_specialist_id").is_some())
            .expect("specialist_review's own generation must stream a token event too");
        assert_eq!(review_token.payload["reviewer_specialist_id"], reviewer_id.to_string());
        assert_eq!(review_token.payload["task_id"], task.id.to_string());
        assert_eq!(review_token.correlation_id, submission.correlation_id);
    }

    // ---- git sync (round 22) -----------------------------------------

    /// A fresh coordinator + storage for `project_id`, with no
    /// seeded specialists, the sync tests build their own Members/
    /// Teams/Specialists so each test's fixture matches exactly what
    /// it's asserting about.
    fn empty_coordinator_for(project_id: Uuid) -> Coordinator {
        let storage = StdArc::new(Storage::in_memory().unwrap());
        let mut bus = EventBus::new();
        bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
        let events = StdArc::new(bus);
        storage.insert_project(project_id, "Team Roster Test", "").unwrap();
        let providers = ProviderRegistry::new();
        Coordinator::new(storage, events, providers)
    }

    fn git(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git").arg("-C").arg(dir).args(args).status().unwrap();
        assert!(status.success(), "git {:?} failed in {}", args, dir.display());
    }

    /// A bare repo (standing in for a GitHub repo) plus one real local
    /// clone of it, with an initial commit already pushed, matching a
    /// freshly created remote repo a teammate has already cloned.
    fn init_bare_remote_and_clone(tmp: &std::path::Path, clone_name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let remote = tmp.join("remote.git");
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, &["init", "--bare", "-b", "main"]);
        let clone_dir = tmp.join(clone_name);
        git(tmp, &["clone", remote.to_str().unwrap(), clone_dir.to_str().unwrap()]);
        git(&clone_dir, &["config", "user.email", "test@example.com"]);
        git(&clone_dir, &["config", "user.name", "Test"]);
        git(&clone_dir, &["commit", "--allow-empty", "-m", "initial commit"]);
        git(&clone_dir, &["push", "-u", "origin", "main"]);
        (remote, clone_dir)
    }

    fn clone_of(tmp: &std::path::Path, remote: &std::path::Path, clone_name: &str) -> std::path::PathBuf {
        let clone_dir = tmp.join(clone_name);
        git(tmp, &["clone", remote.to_str().unwrap(), clone_dir.to_str().unwrap()]);
        git(&clone_dir, &["config", "user.email", "test@example.com"]);
        git(&clone_dir, &["config", "user.name", "Test"]);
        clone_dir
    }

    #[test]
    fn link_git_repo_rejects_a_directory_that_isnt_a_git_repo() {
        let project_id = Uuid::new_v4();
        let coordinator = empty_coordinator_for(project_id);
        let tmp = tempfile::tempdir().unwrap();
        let not_a_repo = tmp.path().join("just-a-folder");
        std::fs::create_dir_all(&not_a_repo).unwrap();

        let err = coordinator.link_git_repo(project_id, not_a_repo.to_str().unwrap().into()).unwrap_err();
        assert!(err.to_string().contains("doesn't look like a git working directory"));
        assert!(coordinator.git_sync_status(project_id).unwrap().is_none());
    }

    #[test]
    fn export_project_snapshot_is_canonicalized_regardless_of_insertion_order() {
        let project_id = Uuid::new_v4();
        let coordinator = empty_coordinator_for(project_id);
        let team_a = Team { id: Uuid::new_v4(), project_id, name: "Zeta".into(), mission: "".into(), member_ids: vec![], parent_team_id: None, deleted_at: None };
        let team_b = Team { id: Uuid::new_v4(), project_id, name: "Alpha".into(), mission: "".into(), member_ids: vec![], parent_team_id: None, deleted_at: None };
        // Inserted in Zeta-then-Alpha order, canonicalize() must still
        // sort by id, not by name or insertion order, so two exports of
        // the same underlying set are byte-identical.
        coordinator.storage.insert_team(&team_a).unwrap();
        coordinator.storage.insert_team(&team_b).unwrap();

        let snapshot1 = coordinator.export_project_snapshot(project_id).unwrap();
        let snapshot2 = coordinator.export_project_snapshot(project_id).unwrap();
        let mut ids1: Vec<Uuid> = snapshot1.teams.iter().map(|t| t.id).collect();
        let mut ids2: Vec<Uuid> = snapshot2.teams.iter().map(|t| t.id).collect();
        assert!(ids1.windows(2).all(|w| w[0] <= w[1]), "canonicalize() must sort teams by id");
        ids1.sort();
        ids2.sort();
        assert_eq!(ids1, ids2);
        // format_version/timestamp aside, the same underlying state
        // must serialize identically both times.
        assert_eq!(
            serde_json::to_value(&snapshot1.teams).unwrap(),
            serde_json::to_value(&snapshot2.teams).unwrap()
        );
    }

    #[test]
    fn pushes_and_pulls_a_team_roster_through_a_real_git_remote() {
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_a) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let clone_b = clone_of(tmp.path(), &remote, "device_b");

        // "Device A": has the team roster, pushes it.
        let coordinator_a = empty_coordinator_for(project_id);
        let human = Member::new_human(project_id, "Alex");
        coordinator_a.storage.insert_member(&human).unwrap();
        let team = Team { id: Uuid::new_v4(), project_id, name: "Engineering".into(), mission: "ship SOLEXA".into(), member_ids: vec![human.id], parent_team_id: None, deleted_at: None };
        coordinator_a.storage.insert_team(&team).unwrap();

        coordinator_a.link_git_repo(project_id, clone_a.to_str().unwrap().into()).unwrap();
        let status = coordinator_a.git_sync_status(project_id).unwrap().unwrap();
        assert!(status.local_dirty, "never-synced project must show as dirty");
        let push = coordinator_a.push_to_git(project_id).unwrap();
        assert!(push.pushed);
        let status = coordinator_a.git_sync_status(project_id).unwrap().unwrap();
        assert!(!status.local_dirty, "right after a push, local must match the synced baseline");

        // A second push with nothing new is a genuine no-op.
        let second_push = coordinator_a.push_to_git(project_id).unwrap();
        assert!(!second_push.pushed);

        // "Device B": a completely separate Storage/Coordinator for the
        // *same* project_id, pulls A's roster in from the real remote.
        let coordinator_b = empty_coordinator_for(project_id);
        coordinator_b.link_git_repo(project_id, clone_b.to_str().unwrap().into()).unwrap();
        let pull = coordinator_b.pull_from_git(project_id).unwrap();
        assert_eq!(pull.teams_applied, 1);
        assert_eq!(pull.members_applied, 1);
        assert!(pull.conflicts_created.is_empty());

        let pulled_team = coordinator_b.storage.get_team(team.id).unwrap().unwrap();
        assert_eq!(pulled_team.name, "Engineering");
        assert_eq!(pulled_team.mission, "ship SOLEXA");
        assert_eq!(coordinator_b.storage.get_member(human.id).unwrap().unwrap().display_name, "Alex");
    }

    #[test]
    fn pull_fast_forwards_a_change_only_one_side_made() {
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_a) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let clone_b = clone_of(tmp.path(), &remote, "device_b");

        let team_id = Uuid::new_v4();
        let coordinator_a = empty_coordinator_for(project_id);
        coordinator_a.storage.insert_team(&Team { id: team_id, project_id, name: "Engineering".into(), mission: "v1".into(), member_ids: vec![], parent_team_id: None, deleted_at: None }).unwrap();
        coordinator_a.link_git_repo(project_id, clone_a.to_str().unwrap().into()).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        // Device B starts from the same synced baseline, untouched.
        let coordinator_b = empty_coordinator_for(project_id);
        coordinator_b.link_git_repo(project_id, clone_b.to_str().unwrap().into()).unwrap();
        coordinator_b.pull_from_git(project_id).unwrap();

        // Only A edits the mission and pushes again.
        let mut edited = coordinator_a.storage.get_team(team_id).unwrap().unwrap();
        edited.mission = "v2, edited only on A".into();
        coordinator_a.storage.update_team(&edited).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        // B never touched the team locally, so this must be a silent,
        // automatic fast-forward, no conflict.
        let pull = coordinator_b.pull_from_git(project_id).unwrap();
        assert_eq!(pull.teams_applied, 1);
        assert!(pull.conflicts_created.is_empty());
        assert_eq!(coordinator_b.storage.get_team(team_id).unwrap().unwrap().mission, "v2, edited only on A");
    }

    #[test]
    fn concurrent_edits_become_a_conflict_and_resolving_it_applies_the_chosen_version() {
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_a) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let clone_b = clone_of(tmp.path(), &remote, "device_b");

        let team_id = Uuid::new_v4();
        let coordinator_a = empty_coordinator_for(project_id);
        coordinator_a.storage.insert_team(&Team { id: team_id, project_id, name: "Engineering".into(), mission: "shared baseline".into(), member_ids: vec![], parent_team_id: None, deleted_at: None }).unwrap();
        coordinator_a.link_git_repo(project_id, clone_a.to_str().unwrap().into()).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let coordinator_b = empty_coordinator_for(project_id);
        coordinator_b.link_git_repo(project_id, clone_b.to_str().unwrap().into()).unwrap();
        coordinator_b.pull_from_git(project_id).unwrap();

        // Both devices edit the SAME field differently while "offline"
        // from each other, then both sync.
        let mut on_a = coordinator_a.storage.get_team(team_id).unwrap().unwrap();
        on_a.mission = "A's version".into();
        coordinator_a.storage.update_team(&on_a).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let mut on_b = coordinator_b.storage.get_team(team_id).unwrap().unwrap();
        on_b.mission = "B's version".into();
        coordinator_b.storage.update_team(&on_b).unwrap();

        let pull = coordinator_b.pull_from_git(project_id).unwrap();
        assert_eq!(pull.teams_applied, 0, "a genuine conflict must not be silently fast-forwarded");
        assert_eq!(pull.conflicts_created.len(), 1);

        // B's own edit must survive untouched until the conflict is
        // resolved, never silently overwritten by A's push.
        assert_eq!(coordinator_b.storage.get_team(team_id).unwrap().unwrap().mission, "B's version");

        let conflict = coordinator_b.storage.get_conflict(pull.conflicts_created[0]).unwrap().unwrap();
        assert!(conflict.task_id.is_none());
        let sync_object = conflict.sync_object.unwrap();
        assert_eq!(sync_object.kind, SyncObjectKind::Team);
        assert_eq!(sync_object.object_id, team_id);
        assert_eq!(sync_object.field.as_deref(), Some("mission"), "round 25: the conflict scopes to the specific field, not the whole Team");
        assert_eq!(conflict.positions.len(), 2);
        assert!(conflict.positions.iter().all(|p| p.model_id.is_none() && p.verification_passed.is_none()));

        // Resolve in favor of A's (the "Remote") position.
        let approver = Member::new_human(project_id, "Sam");
        coordinator_b.storage.insert_member(&approver).unwrap();
        let remote_position = conflict.positions.iter().find(|p| p.specialist_name.starts_with("Remote")).unwrap();
        coordinator_b.resolve_sync_conflict(conflict.id, remote_position.specialist_id, approver.id).unwrap();

        let resolved_team = coordinator_b.storage.get_team(team_id).unwrap().unwrap();
        assert_eq!(resolved_team.mission, "A's version");
        let resolved_conflict = coordinator_b.storage.get_conflict(conflict.id).unwrap().unwrap();
        assert_eq!(resolved_conflict.status, solexa_domain::ConflictStatus::Resolved);
    }

    #[test]
    fn devices_editing_different_fields_of_the_same_team_merge_automatically() {
        // Round 25's actual point: A renames the team, B independently
        // rewrites its mission, genuinely non-overlapping edits, which
        // must merge cleanly instead of producing an all-or-nothing
        // whole-object conflict the way rounds 22-24 would have.
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_a) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let clone_b = clone_of(tmp.path(), &remote, "device_b");

        let team_id = Uuid::new_v4();
        let coordinator_a = empty_coordinator_for(project_id);
        coordinator_a
            .storage
            .insert_team(&Team { id: team_id, project_id, name: "Engineering".into(), mission: "shared baseline".into(), member_ids: vec![], parent_team_id: None, deleted_at: None })
            .unwrap();
        coordinator_a.link_git_repo(project_id, clone_a.to_str().unwrap().into()).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let coordinator_b = empty_coordinator_for(project_id);
        coordinator_b.link_git_repo(project_id, clone_b.to_str().unwrap().into()).unwrap();
        coordinator_b.pull_from_git(project_id).unwrap();

        // A renames the team (leaves mission alone) and pushes.
        let mut on_a = coordinator_a.storage.get_team(team_id).unwrap().unwrap();
        on_a.name = "Platform Engineering".into();
        coordinator_a.storage.update_team(&on_a).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        // B rewrites the mission (leaves name alone), never touching A's change.
        let mut on_b = coordinator_b.storage.get_team(team_id).unwrap().unwrap();
        on_b.mission = "ship faster".into();
        coordinator_b.storage.update_team(&on_b).unwrap();

        let pull = coordinator_b.pull_from_git(project_id).unwrap();
        assert!(pull.conflicts_created.is_empty(), "non-overlapping field edits must merge automatically, not conflict");
        assert_eq!(pull.teams_applied, 1, "the team's remote-only field (name) was fast-forwarded in");

        let merged = coordinator_b.storage.get_team(team_id).unwrap().unwrap();
        assert_eq!(merged.name, "Platform Engineering", "A's field should have been merged in");
        assert_eq!(merged.mission, "ship faster", "B's own field must survive the merge untouched");
    }

    #[test]
    fn resolve_sync_conflict_still_applies_a_pre_round_25_whole_object_conflict() {
        // Backward compatibility: a sync conflict created before round
        // 25 has `sync_object.field: None` and a position's content is
        // the *entire* object, not one field's value. `resolve_sync_conflict`
        // must still handle one of these correctly if it's still sitting
        // unresolved when a project upgrades.
        let project_id = Uuid::new_v4();
        let coordinator = empty_coordinator_for(project_id);
        let team_id = Uuid::new_v4();
        coordinator
            .storage
            .insert_team(&Team { id: team_id, project_id, name: "Engineering".into(), mission: "old value".into(), member_ids: vec![], parent_team_id: None, deleted_at: None })
            .unwrap();
        let approver = Member::new_human(project_id, "Sam");
        coordinator.storage.insert_member(&approver).unwrap();

        let whole_object_conflict = Conflict::new_sync(
            project_id,
            SyncObjectRef { kind: SyncObjectKind::Team, object_id: team_id, field: None },
            "Team \"Engineering\" was edited differently locally and on the linked repo",
            vec![
                ConflictPosition {
                    specialist_id: Uuid::new_v4(),
                    specialist_name: "Local".into(),
                    model_id: None,
                    content: serde_json::to_value(&Team { id: team_id, project_id, name: "Engineering".into(), mission: "old value".into(), member_ids: vec![], parent_team_id: None, deleted_at: None }).unwrap(),
                    verification_passed: None,
                },
                ConflictPosition {
                    specialist_id: Uuid::new_v4(),
                    specialist_name: "Remote (abc1234)".into(),
                    model_id: None,
                    content: serde_json::to_value(&Team { id: team_id, project_id, name: "Engineering (renamed)".into(), mission: "new mission entirely".into(), member_ids: vec![], parent_team_id: None, deleted_at: None }).unwrap(),
                    verification_passed: None,
                },
            ],
        );
        coordinator.storage.insert_conflict(&whole_object_conflict).unwrap();

        let remote_position = whole_object_conflict.positions.iter().find(|p| p.specialist_name.starts_with("Remote")).unwrap();
        coordinator.resolve_sync_conflict(whole_object_conflict.id, remote_position.specialist_id, approver.id).unwrap();

        let resolved = coordinator.storage.get_team(team_id).unwrap().unwrap();
        assert_eq!(resolved.name, "Engineering (renamed)");
        assert_eq!(resolved.mission, "new mission entirely");
    }

    #[test]
    fn devices_adding_different_members_to_the_same_team_merge_via_union() {
        // Round 26's actual point: two people adding *different* new
        // members to the same team's roster while both offline is the
        // common case a whole-list conflict used to punish unnecessarily.
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_a) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let clone_b = clone_of(tmp.path(), &remote, "device_b");

        let team_id = Uuid::new_v4();
        let existing_member = Member::new_human(project_id, "Alex");
        let coordinator_a = empty_coordinator_for(project_id);
        coordinator_a.storage.insert_member(&existing_member).unwrap();
        coordinator_a
            .storage
            .insert_team(&Team { id: team_id, project_id, name: "Engineering".into(), mission: "".into(), member_ids: vec![existing_member.id], parent_team_id: None, deleted_at: None })
            .unwrap();
        coordinator_a.link_git_repo(project_id, clone_a.to_str().unwrap().into()).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let coordinator_b = empty_coordinator_for(project_id);
        coordinator_b.link_git_repo(project_id, clone_b.to_str().unwrap().into()).unwrap();
        coordinator_b.pull_from_git(project_id).unwrap();

        // A adds a brand new member to the team and pushes.
        let member_added_by_a = Member::new_human(project_id, "Sam");
        coordinator_a.storage.insert_member(&member_added_by_a).unwrap();
        let mut team_on_a = coordinator_a.storage.get_team(team_id).unwrap().unwrap();
        team_on_a.member_ids.push(member_added_by_a.id);
        coordinator_a.storage.update_team(&team_on_a).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        // B, independently and without seeing A's change, adds a
        // *different* new member to the same team.
        let member_added_by_b = Member::new_human(project_id, "Priya");
        coordinator_b.storage.insert_member(&member_added_by_b).unwrap();
        let mut team_on_b = coordinator_b.storage.get_team(team_id).unwrap().unwrap();
        team_on_b.member_ids.push(member_added_by_b.id);
        coordinator_b.storage.update_team(&team_on_b).unwrap();

        let pull = coordinator_b.pull_from_git(project_id).unwrap();
        assert!(pull.conflicts_created.is_empty(), "two different additions to the same list must never conflict");
        assert_eq!(pull.teams_applied, 1);

        let merged = coordinator_b.storage.get_team(team_id).unwrap().unwrap();
        assert_eq!(merged.member_ids.len(), 3, "the original member plus both independently-added members");
        assert!(merged.member_ids.contains(&existing_member.id));
        assert!(merged.member_ids.contains(&member_added_by_a.id), "A's addition must survive the merge");
        assert!(merged.member_ids.contains(&member_added_by_b.id), "B's own addition must survive the merge");
    }

    #[test]
    fn an_addition_on_one_side_and_a_removal_on_the_other_merge_correctly() {
        // The other common non-conflicting case: A adds a member while
        // B independently removes a *different* existing member.
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_a) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let clone_b = clone_of(tmp.path(), &remote, "device_b");

        let team_id = Uuid::new_v4();
        let member_to_keep = Member::new_human(project_id, "Alex");
        let member_to_remove = Member::new_human(project_id, "Jordan");
        let coordinator_a = empty_coordinator_for(project_id);
        coordinator_a.storage.insert_member(&member_to_keep).unwrap();
        coordinator_a.storage.insert_member(&member_to_remove).unwrap();
        coordinator_a
            .storage
            .insert_team(&Team { id: team_id, project_id, name: "Engineering".into(), mission: "".into(), member_ids: vec![member_to_keep.id, member_to_remove.id], parent_team_id: None, deleted_at: None })
            .unwrap();
        coordinator_a.link_git_repo(project_id, clone_a.to_str().unwrap().into()).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let coordinator_b = empty_coordinator_for(project_id);
        coordinator_b.link_git_repo(project_id, clone_b.to_str().unwrap().into()).unwrap();
        coordinator_b.pull_from_git(project_id).unwrap();

        // A adds a new member.
        let member_added_by_a = Member::new_human(project_id, "Sam");
        coordinator_a.storage.insert_member(&member_added_by_a).unwrap();
        let mut team_on_a = coordinator_a.storage.get_team(team_id).unwrap().unwrap();
        team_on_a.member_ids.push(member_added_by_a.id);
        coordinator_a.storage.update_team(&team_on_a).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        // B, independently, removes Jordan.
        let mut team_on_b = coordinator_b.storage.get_team(team_id).unwrap().unwrap();
        team_on_b.member_ids.retain(|id| *id != member_to_remove.id);
        coordinator_b.storage.update_team(&team_on_b).unwrap();

        let pull = coordinator_b.pull_from_git(project_id).unwrap();
        assert!(pull.conflicts_created.is_empty());
        let merged = coordinator_b.storage.get_team(team_id).unwrap().unwrap();
        assert!(merged.member_ids.contains(&member_to_keep.id));
        assert!(!merged.member_ids.contains(&member_to_remove.id), "B's removal must survive");
        assert!(merged.member_ids.contains(&member_added_by_a.id), "A's addition must survive");
        assert_eq!(merged.member_ids.len(), 2);
    }

    #[test]
    fn deleting_a_team_on_one_device_syncs_as_deleted_on_another_with_no_conflict() {
        // Round 27's actual point: `deleted_at` is just another field,
        // so it fast-forwards through the exact same per-object merge
        // every other untouched field already gets, no new sync logic
        // needed for a delete to propagate correctly.
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_a) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let clone_b = clone_of(tmp.path(), &remote, "device_b");

        let team_id = Uuid::new_v4();
        let coordinator_a = empty_coordinator_for(project_id);
        coordinator_a
            .storage
            .insert_team(&Team { id: team_id, project_id, name: "Engineering".into(), mission: "".into(), member_ids: vec![], parent_team_id: None, deleted_at: None })
            .unwrap();
        coordinator_a.link_git_repo(project_id, clone_a.to_str().unwrap().into()).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let coordinator_b = empty_coordinator_for(project_id);
        coordinator_b.link_git_repo(project_id, clone_b.to_str().unwrap().into()).unwrap();
        coordinator_b.pull_from_git(project_id).unwrap();
        assert!(!coordinator_b.storage.get_team(team_id).unwrap().unwrap().is_deleted());

        // A deletes the team and pushes; B never touched this team locally.
        coordinator_a.delete_team(team_id).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let pull = coordinator_b.pull_from_git(project_id).unwrap();
        assert!(pull.conflicts_created.is_empty(), "a one-sided delete must fast-forward, not conflict");
        assert_eq!(pull.teams_applied, 1);
        assert!(coordinator_b.storage.get_team(team_id).unwrap().unwrap().is_deleted());

        // And it's genuinely reversible from either side, B can pull a
        // restore just as cleanly as it pulled the delete.
        coordinator_a.restore_team(team_id).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();
        let restore_pull = coordinator_b.pull_from_git(project_id).unwrap();
        assert!(restore_pull.conflicts_created.is_empty());
        assert!(!coordinator_b.storage.get_team(team_id).unwrap().unwrap().is_deleted());
    }

    #[test]
    fn both_devices_deleting_the_same_team_independently_becomes_a_resolvable_conflict() {
        // The one honest rough edge: since each device stamps its own
        // deletion instant, two independent deletes still collide on
        // the exact `deleted_at` value, resolving it (either side)
        // still leaves the team correctly deleted either way.
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_a) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let clone_b = clone_of(tmp.path(), &remote, "device_b");

        let team_id = Uuid::new_v4();
        let coordinator_a = empty_coordinator_for(project_id);
        coordinator_a
            .storage
            .insert_team(&Team { id: team_id, project_id, name: "Engineering".into(), mission: "".into(), member_ids: vec![], parent_team_id: None, deleted_at: None })
            .unwrap();
        coordinator_a.link_git_repo(project_id, clone_a.to_str().unwrap().into()).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let coordinator_b = empty_coordinator_for(project_id);
        coordinator_b.link_git_repo(project_id, clone_b.to_str().unwrap().into()).unwrap();
        coordinator_b.pull_from_git(project_id).unwrap();

        coordinator_a.delete_team(team_id).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();
        coordinator_b.delete_team(team_id).unwrap();

        let pull = coordinator_b.pull_from_git(project_id).unwrap();
        assert_eq!(pull.conflicts_created.len(), 1);
        let conflict = coordinator_b.storage.get_conflict(pull.conflicts_created[0]).unwrap().unwrap();
        assert_eq!(conflict.sync_object.unwrap().field.as_deref(), Some("deleted_at"));

        // Resolved either way, the team ends up deleted, both sides
        // agreed on *that*, only the precise instant differed.
        let approver = Member::new_human(project_id, "Sam");
        coordinator_b.storage.insert_member(&approver).unwrap();
        let local_position = conflict.positions.iter().find(|p| p.specialist_name == "Local").unwrap();
        coordinator_b.resolve_sync_conflict(conflict.id, local_position.specialist_id, approver.id).unwrap();
        assert!(coordinator_b.storage.get_team(team_id).unwrap().unwrap().is_deleted());
    }

    // ---- list-merge + soft-delete interaction (round 28) --------------
    //
    // Round 27's own README named this gap explicitly: round 26's list
    // union-merge (`member_ids`) and round 27's soft delete
    // (`deleted_at`) had never been exercised *together* in one pull,
    // even though both are "just fields" on the same `Team` object and
    // were expected to compose for free. These three tests are that
    // missing coverage, no production code changed to make them pass,
    // which is itself the finding: `merge_object_fields` already
    // handles every field independently (round 25), so a scalar field
    // (`deleted_at`) and a list field (`member_ids`) diverging on the
    // same object at the same time were already safe.

    #[test]
    fn deleting_a_team_and_independently_removing_one_of_its_members_from_its_roster_merge_without_conflict() {
        // The exact scenario round 27's README named: A deletes the
        // team; B, never having seen A's delete, removes a member from
        // that same team's roster. Two different fields on the same
        // object, changed independently on each side, must merge
        // cleanly, not collide into a whole-object conflict.
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_a) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let clone_b = clone_of(tmp.path(), &remote, "device_b");

        let team_id = Uuid::new_v4();
        let member_to_keep = Member::new_human(project_id, "Alex");
        let member_to_remove = Member::new_human(project_id, "Jordan");
        let coordinator_a = empty_coordinator_for(project_id);
        coordinator_a.storage.insert_member(&member_to_keep).unwrap();
        coordinator_a.storage.insert_member(&member_to_remove).unwrap();
        coordinator_a
            .storage
            .insert_team(&Team { id: team_id, project_id, name: "Engineering".into(), mission: "".into(), member_ids: vec![member_to_keep.id, member_to_remove.id], parent_team_id: None, deleted_at: None })
            .unwrap();
        coordinator_a.link_git_repo(project_id, clone_a.to_str().unwrap().into()).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let coordinator_b = empty_coordinator_for(project_id);
        coordinator_b.link_git_repo(project_id, clone_b.to_str().unwrap().into()).unwrap();
        coordinator_b.pull_from_git(project_id).unwrap();

        // A deletes the whole team (touches only `deleted_at`) and pushes.
        coordinator_a.delete_team(team_id).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        // B, independently and before pulling A's delete, removes one
        // member from the roster (touches only `member_ids`).
        coordinator_b.remove_member_from_team(team_id, member_to_remove.id).unwrap();

        let pull = coordinator_b.pull_from_git(project_id).unwrap();
        assert!(pull.conflicts_created.is_empty(), "a delete and an unrelated roster edit on the same team must merge automatically, not conflict");
        assert_eq!(pull.teams_applied, 1, "A's deleted_at was the remote-only change fast-forwarded in");

        let merged = coordinator_b.storage.get_team(team_id).unwrap().unwrap();
        assert!(merged.is_deleted(), "A's delete must land");
        assert!(merged.member_ids.contains(&member_to_keep.id));
        assert!(!merged.member_ids.contains(&member_to_remove.id), "B's own removal must survive the merge");
    }

    #[test]
    fn a_teams_delete_flag_and_a_genuine_two_sided_roster_union_merge_in_the_same_pull() {
        // A harder version of the above: this time *both* sides also
        // add a different new member to the roster (round 26's actual
        // union-merge codepath, which only activates when a field
        // changed on both sides) at the same time as the team being
        // deleted on one side, proving the scalar fast-forward
        // (`deleted_at`) and the list union-merge (`member_ids`) run
        // side by side within a single object without interfering.
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_a) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let clone_b = clone_of(tmp.path(), &remote, "device_b");

        let team_id = Uuid::new_v4();
        let original_member = Member::new_human(project_id, "Alex");
        let coordinator_a = empty_coordinator_for(project_id);
        coordinator_a.storage.insert_member(&original_member).unwrap();
        coordinator_a
            .storage
            .insert_team(&Team { id: team_id, project_id, name: "Engineering".into(), mission: "".into(), member_ids: vec![original_member.id], parent_team_id: None, deleted_at: None })
            .unwrap();
        coordinator_a.link_git_repo(project_id, clone_a.to_str().unwrap().into()).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let coordinator_b = empty_coordinator_for(project_id);
        coordinator_b.link_git_repo(project_id, clone_b.to_str().unwrap().into()).unwrap();
        coordinator_b.pull_from_git(project_id).unwrap();

        // A adds a member AND deletes the team, then pushes both changes together.
        let member_added_by_a = Member::new_human(project_id, "Sam");
        coordinator_a.storage.insert_member(&member_added_by_a).unwrap();
        coordinator_a.add_member_to_team(team_id, member_added_by_a.id).unwrap();
        coordinator_a.delete_team(team_id).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        // B, independently, adds a *different* new member, never touching deleted_at.
        let member_added_by_b = Member::new_human(project_id, "Priya");
        coordinator_b.storage.insert_member(&member_added_by_b).unwrap();
        coordinator_b.add_member_to_team(team_id, member_added_by_b.id).unwrap();

        let pull = coordinator_b.pull_from_git(project_id).unwrap();
        assert!(pull.conflicts_created.is_empty(), "a scalar fast-forward and a list union-merge on the same object must both resolve automatically");
        assert_eq!(pull.teams_applied, 1);

        let merged = coordinator_b.storage.get_team(team_id).unwrap().unwrap();
        assert!(merged.is_deleted(), "A's delete (a plain fast-forwarded field) must still land");
        assert_eq!(merged.member_ids.len(), 3, "original member plus both independently-added members, via round 26's union merge");
        assert!(merged.member_ids.contains(&original_member.id));
        assert!(merged.member_ids.contains(&member_added_by_a.id));
        assert!(merged.member_ids.contains(&member_added_by_b.id));
    }

    #[test]
    fn deleting_a_member_and_removing_that_same_member_from_a_team_roster_on_another_device_both_land_cleanly() {
        // The cross-object version: `Member.deleted_at` and
        // `Team.member_ids` are two different sync objects entirely, so
        // this was always expected to be safe, but it's the one
        // scenario that actually produces the "obviously correct" end
        // state a user would want after deleting someone: they end up
        // marked deleted *and* off the roster, even though neither
        // device performed both actions itself.
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_a) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let clone_b = clone_of(tmp.path(), &remote, "device_b");

        let team_id = Uuid::new_v4();
        let member = Member::new_human(project_id, "Jordan");
        let coordinator_a = empty_coordinator_for(project_id);
        coordinator_a.storage.insert_member(&member).unwrap();
        coordinator_a
            .storage
            .insert_team(&Team { id: team_id, project_id, name: "Engineering".into(), mission: "".into(), member_ids: vec![member.id], parent_team_id: None, deleted_at: None })
            .unwrap();
        coordinator_a.link_git_repo(project_id, clone_a.to_str().unwrap().into()).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let coordinator_b = empty_coordinator_for(project_id);
        coordinator_b.link_git_repo(project_id, clone_b.to_str().unwrap().into()).unwrap();
        coordinator_b.pull_from_git(project_id).unwrap();

        // A soft-deletes the Member itself and pushes.
        coordinator_a.delete_member(member.id).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        // B, independently, removes that same member from the team's
        // roster, a different object (the Team), never having seen
        // A's delete.
        coordinator_b.remove_member_from_team(team_id, member.id).unwrap();

        let pull = coordinator_b.pull_from_git(project_id).unwrap();
        assert!(pull.conflicts_created.is_empty(), "two different sync objects diverging independently must never conflict with each other");
        assert_eq!(pull.members_applied, 1, "A's member delete was the remote-only change fast-forwarded in");

        assert!(coordinator_b.storage.get_member(member.id).unwrap().unwrap().is_deleted());
        assert!(!coordinator_b.storage.get_team(team_id).unwrap().unwrap().member_ids.contains(&member.id));
    }

    #[test]
    fn resolve_sync_conflict_and_resolve_conflict_each_reject_the_others_conflict_kind() {
        let project_id = Uuid::new_v4();
        let coordinator = empty_coordinator_for(project_id);
        let approver = Member::new_human(project_id, "Sam");
        coordinator.storage.insert_member(&approver).unwrap();

        let task_conflict = Conflict::new(
            project_id,
            Uuid::new_v4(),
            "which result?",
            vec![ConflictPosition { specialist_id: Uuid::new_v4(), specialist_name: "A".into(), model_id: Some(Uuid::new_v4()), content: serde_json::json!("a"), verification_passed: Some(true) }],
        );
        coordinator.storage.insert_conflict(&task_conflict).unwrap();
        let err = coordinator
            .resolve_sync_conflict(task_conflict.id, task_conflict.positions[0].specialist_id, approver.id)
            .unwrap_err();
        assert!(err.to_string().contains("resolve_conflict instead"));

        let sync_conflict = Conflict::new_sync(
            project_id,
            SyncObjectRef { kind: SyncObjectKind::Team, object_id: Uuid::new_v4(), field: Some("mission".into()) },
            "which team version?",
            vec![ConflictPosition { specialist_id: Uuid::new_v4(), specialist_name: "Local".into(), model_id: None, content: serde_json::json!({}), verification_passed: None }],
        );
        coordinator.storage.insert_conflict(&sync_conflict).unwrap();
        let err = coordinator
            .resolve_conflict(sync_conflict.id, sync_conflict.positions[0].specialist_id, approver.id)
            .unwrap_err();
        assert!(err.to_string().contains("resolve_sync_conflict instead"));
    }

    #[test]
    fn unlink_git_repo_removes_the_link_but_never_touches_synced_data() {
        let project_id = Uuid::new_v4();
        let coordinator = empty_coordinator_for(project_id);
        let tmp = tempfile::tempdir().unwrap();
        let (_remote, clone_dir) = init_bare_remote_and_clone(tmp.path(), "device_a");

        assert!(coordinator.unlink_git_repo(project_id).is_err(), "unlinking a never-linked project must error");

        coordinator.link_git_repo(project_id, clone_dir.to_str().unwrap().into()).unwrap();
        let team = Team { id: Uuid::new_v4(), project_id, name: "Engineering".into(), mission: "".into(), member_ids: vec![], parent_team_id: None, deleted_at: None };
        coordinator.storage.insert_team(&team).unwrap();
        coordinator.push_to_git(project_id).unwrap();

        coordinator.unlink_git_repo(project_id).unwrap();
        assert!(coordinator.git_sync_status(project_id).unwrap().is_none());
        // The synced Team itself is untouched by unlinking.
        assert!(coordinator.storage.get_team(team.id).unwrap().is_some());
    }

    // ---- Project Brain sync (round 23) --------------------------------

    fn sample_knowledge_item(project_id: Uuid) -> KnowledgeItem {
        KnowledgeItem::new(
            project_id,
            solexa_domain::KnowledgeKind::Constraint,
            "Offline mode must remain available",
            "The app must work with no internet connection.",
            solexa_domain::Provenance::human("Alex"),
            vec!["architecture".into()],
        )
    }

    #[test]
    fn export_project_snapshot_includes_the_project_brain() {
        let project_id = Uuid::new_v4();
        let coordinator = empty_coordinator_for(project_id);
        let item = sample_knowledge_item(project_id);
        coordinator.storage.insert_knowledge_item(&item).unwrap();
        let decision = Decision::new(project_id, "Which database?", vec!["Postgres".into(), "SQLite".into()], vec![]);
        coordinator.storage.insert_decision(&decision).unwrap();

        let snapshot = coordinator.export_project_snapshot(project_id).unwrap();
        assert_eq!(snapshot.knowledge_items.len(), 1);
        assert_eq!(snapshot.knowledge_items[0].id, item.id);
        assert_eq!(snapshot.decisions.len(), 1);
        assert_eq!(snapshot.decisions[0].id, decision.id);
    }

    #[test]
    fn pull_fast_forwards_a_knowledge_item_status_change_and_a_decision_resolution() {
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_a) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let clone_b = clone_of(tmp.path(), &remote, "device_b");

        let item = sample_knowledge_item(project_id);
        let decision = Decision::new(project_id, "Which database?", vec!["Postgres".into(), "SQLite".into()], vec![]);
        let coordinator_a = empty_coordinator_for(project_id);
        coordinator_a.storage.insert_knowledge_item(&item).unwrap();
        coordinator_a.storage.insert_decision(&decision).unwrap();
        coordinator_a.link_git_repo(project_id, clone_a.to_str().unwrap().into()).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let coordinator_b = empty_coordinator_for(project_id);
        coordinator_b.link_git_repo(project_id, clone_b.to_str().unwrap().into()).unwrap();
        coordinator_b.pull_from_git(project_id).unwrap();

        // Only A archives the item and resolves the decision, then pushes.
        coordinator_a.archive_knowledge_item(item.id).unwrap();
        let approver_a = Member::new_human(project_id, "Alex");
        coordinator_a.storage.insert_member(&approver_a).unwrap();
        coordinator_a.resolve_decision(decision.id, "Postgres", approver_a.id).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        // B never touched either locally, both must fast-forward silently.
        let pull = coordinator_b.pull_from_git(project_id).unwrap();
        assert_eq!(pull.knowledge_items_applied, 1);
        assert_eq!(pull.decisions_applied, 1);
        assert!(pull.conflicts_created.is_empty());

        assert_eq!(coordinator_b.storage.get_knowledge_item(item.id).unwrap().unwrap().status, solexa_domain::KnowledgeStatus::Archived);
        let pulled_decision = coordinator_b.storage.get_decision(decision.id).unwrap().unwrap();
        assert_eq!(pulled_decision.status, solexa_domain::DecisionStatus::Resolved);
        assert_eq!(pulled_decision.resolution.as_deref(), Some("Postgres"));
    }

    #[test]
    fn two_devices_resolving_the_same_decision_differently_becomes_a_conflict() {
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_a) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let clone_b = clone_of(tmp.path(), &remote, "device_b");

        let decision = Decision::new(project_id, "Which database?", vec!["Postgres".into(), "SQLite".into()], vec![]);
        let coordinator_a = empty_coordinator_for(project_id);
        coordinator_a.storage.insert_decision(&decision).unwrap();
        coordinator_a.link_git_repo(project_id, clone_a.to_str().unwrap().into()).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let coordinator_b = empty_coordinator_for(project_id);
        coordinator_b.link_git_repo(project_id, clone_b.to_str().unwrap().into()).unwrap();
        coordinator_b.pull_from_git(project_id).unwrap();

        // Both resolve the SAME open decision differently while "offline"
        // from each other.
        let approver_a = Member::new_human(project_id, "Alex");
        coordinator_a.storage.insert_member(&approver_a).unwrap();
        coordinator_a.resolve_decision(decision.id, "Postgres", approver_a.id).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let approver_b = Member::new_human(project_id, "Sam");
        coordinator_b.storage.insert_member(&approver_b).unwrap();
        coordinator_b.resolve_decision(decision.id, "SQLite", approver_b.id).unwrap();

        let pull = coordinator_b.pull_from_git(project_id).unwrap();
        assert_eq!(pull.decisions_applied, 0, "a genuine conflict must not be silently fast-forwarded");
        // resolve_decision sets `resolution` and `approved_by` together,
        // and both differ between A's and B's resolutions, round 25's
        // field-level diffing surfaces each as its own conflict rather
        // than one bundled "how was this resolved" conflict (a named
        // rough edge in the round's README: these two fields are always
        // set atomically by the app, but diffed independently here).
        assert_eq!(pull.conflicts_created.len(), 2);

        // B's own resolution survives untouched until the conflicts are resolved.
        assert_eq!(coordinator_b.storage.get_decision(decision.id).unwrap().unwrap().resolution.as_deref(), Some("SQLite"));

        let fields: std::collections::HashSet<String> = pull
            .conflicts_created
            .iter()
            .map(|id| coordinator_b.storage.get_conflict(*id).unwrap().unwrap().sync_object.unwrap().field.unwrap())
            .collect();
        assert_eq!(fields, std::collections::HashSet::from(["resolution".to_string(), "approved_by".to_string()]));

        // Resolve both field conflicts in favor of A's ("Remote") side.
        for conflict_id in &pull.conflicts_created {
            let conflict = coordinator_b.storage.get_conflict(*conflict_id).unwrap().unwrap();
            let sync_object = conflict.sync_object.clone().unwrap();
            assert_eq!(sync_object.kind, SyncObjectKind::Decision);
            assert_eq!(sync_object.object_id, decision.id);
            let remote_position = conflict.positions.iter().find(|p| p.specialist_name.starts_with("Remote")).unwrap();
            coordinator_b.resolve_sync_conflict(*conflict_id, remote_position.specialist_id, approver_b.id).unwrap();
        }

        let resolved = coordinator_b.storage.get_decision(decision.id).unwrap().unwrap();
        assert_eq!(resolved.resolution.as_deref(), Some("Postgres"));
        assert_eq!(resolved.approved_by.as_deref(), Some("Alex"));
    }

    // ---- round 56: role assignments are device-local, never synced -----
    //
    // Role *definitions* have never travelled in a snapshot (only members'
    // and specialists' `role_ids` did), so a role id meant nothing on the
    // other device, and, worse, `upsert_synced_member` overwrote the whole
    // row. These tests pin the policy: a pull can never change who holds
    // which role, a snapshot can never produce a role conflict, and nothing
    // role-shaped is ever written to the repo.

    /// Two linked devices sharing one project and one member, "Alex".
    fn two_devices_sharing_alex() -> (tempfile::TempDir, Uuid, Coordinator, Coordinator, std::path::PathBuf, Member) {
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_a) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let clone_b = clone_of(tmp.path(), &remote, "device_b");
        let alex = Member::new_human(project_id, "Alex");

        let coordinator_a = empty_coordinator_for(project_id);
        coordinator_a.storage.insert_member(&alex).unwrap();
        coordinator_a.link_git_repo(project_id, clone_a.to_str().unwrap().into()).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let coordinator_b = empty_coordinator_for(project_id);
        coordinator_b.link_git_repo(project_id, clone_b.to_str().unwrap().into()).unwrap();
        coordinator_b.pull_from_git(project_id).unwrap();
        assert!(coordinator_b.storage.get_member(alex.id).unwrap().is_some(), "precondition: B has Alex");
        (tmp, project_id, coordinator_a, coordinator_b, clone_a, alex)
    }

    /// Pushes `snapshot` to A's repo WITHOUT going through the coordinator,
    /// i.e. what a hostile collaborator, or a repo written by a pre-round-56
    /// version, can put there.
    fn push_raw_snapshot(clone: &std::path::Path, snapshot: &ProjectSnapshot) {
        solexa_sync::write_snapshot(clone, snapshot).unwrap();
        solexa_sync::commit_and_push(clone, "raw snapshot").unwrap();
    }

    #[test]
    fn a_pulled_snapshot_can_never_grant_a_member_a_local_role_even_if_it_names_one() {
        let (_tmp, project_id, coordinator_a, coordinator_b, clone_a, alex) = two_devices_sharing_alex();
        // B defines a real Approve role. Nobody holds it.
        let approver = coordinator_b.create_role(project_id, "Approver", vec![Permission::Approve]).unwrap();
        assert!(!coordinator_b.member_has_permission(alex.id, Permission::Approve).unwrap(), "precondition");

        // The remote snapshot names that exact role id for Alex.
        let mut hostile = coordinator_a.export_project_snapshot(project_id).unwrap();
        hostile.members.iter_mut().find(|m| m.id == alex.id).unwrap().role_ids = vec![approver.id];
        push_raw_snapshot(&clone_a, &hostile);

        coordinator_b.pull_from_git(project_id).unwrap();

        assert!(
            !coordinator_b.member_has_permission(alex.id, Permission::Approve).unwrap(),
            "a pull must never be able to grant Approve, that would let anyone with push access to the repo approve things"
        );
        assert!(coordinator_b.storage.get_member(alex.id).unwrap().unwrap().role_ids.is_empty());
    }

    #[test]
    fn a_pulled_snapshot_can_never_grant_a_specialist_a_local_role_even_if_it_names_one() {
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_a) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let clone_b = clone_of(tmp.path(), &remote, "device_b");
        let specialist = sql_specialist(Uuid::new_v4());

        let coordinator_a = empty_coordinator_for(project_id);
        coordinator_a.storage.insert_specialist(project_id, &specialist).unwrap();
        coordinator_a.link_git_repo(project_id, clone_a.to_str().unwrap().into()).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();
        let coordinator_b = empty_coordinator_for(project_id);
        coordinator_b.link_git_repo(project_id, clone_b.to_str().unwrap().into()).unwrap();
        coordinator_b.pull_from_git(project_id).unwrap();

        let executor = coordinator_b.create_role(project_id, "Executor", vec![Permission::Execute]).unwrap();
        let mut hostile = coordinator_a.export_project_snapshot(project_id).unwrap();
        hostile.specialists.iter_mut().find(|s| s.id == specialist.id).unwrap().role_ids = vec![executor.id];
        push_raw_snapshot(&clone_a, &hostile);

        coordinator_b.pull_from_git(project_id).unwrap();
        let after = coordinator_b.storage.get_specialist(specialist.id).unwrap().unwrap();
        assert!(after.role_ids.is_empty(), "a pull must not assign a role to a specialist: {:?}", after.role_ids);
    }

    #[test]
    fn two_devices_assigning_different_roles_to_one_member_raise_no_conflict_and_each_keeps_its_own() {
        let (_tmp, project_id, coordinator_a, coordinator_b, _clone_a, alex) = two_devices_sharing_alex();
        let approver_a = coordinator_a.create_role(project_id, "Approver", vec![Permission::Approve]).unwrap();
        coordinator_a.assign_role(alex.id, approver_a.id).unwrap();
        let reviewer_b = coordinator_b.create_role(project_id, "Reviewer", vec![Permission::Review]).unwrap();
        coordinator_b.assign_role(alex.id, reviewer_b.id).unwrap();

        coordinator_a.push_to_git(project_id).unwrap();
        let pull = coordinator_b.pull_from_git(project_id).unwrap();

        assert!(pull.conflicts_created.is_empty(), "role ids are meaningless across devices, they must not create a sync conflict: {:?}", pull.conflicts_created);
        // B's Alex is exactly what B made him: a Reviewer, not an Approver.
        assert!(coordinator_b.member_has_permission(alex.id, Permission::Review).unwrap(), "B's own assignment must survive the pull");
        assert!(!coordinator_b.member_has_permission(alex.id, Permission::Approve).unwrap(), "A's assignment must not leak into B");
        // And symmetric: pulling B's push doesn't disturb A.
        coordinator_b.push_to_git(project_id).unwrap();
        coordinator_a.pull_from_git(project_id).unwrap();
        assert!(coordinator_a.member_has_permission(alex.id, Permission::Approve).unwrap());
        assert!(!coordinator_a.member_has_permission(alex.id, Permission::Review).unwrap());
    }

    #[test]
    fn pulling_a_remote_edit_to_a_member_keeps_the_local_role_assignments() {
        // The destructive direction: a remote edit to some OTHER field is
        // applied, and the whole row used to be overwritten with the
        // remote's `role_ids`.
        let (_tmp, project_id, coordinator_a, coordinator_b, _clone_a, alex) = two_devices_sharing_alex();
        let approver = coordinator_b.create_role(project_id, "Approver", vec![Permission::Approve]).unwrap();
        coordinator_b.assign_role(alex.id, approver.id).unwrap();

        let mut renamed = coordinator_a.storage.get_member(alex.id).unwrap().unwrap();
        renamed.display_name = "Alexandra".into();
        coordinator_a.storage.update_member(&renamed).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let pull = coordinator_b.pull_from_git(project_id).unwrap();
        assert_eq!(pull.members_applied, 1, "positive control: the remote rename really was applied");
        assert_eq!(coordinator_b.storage.get_member(alex.id).unwrap().unwrap().display_name, "Alexandra");
        assert!(coordinator_b.member_has_permission(alex.id, Permission::Approve).unwrap(), "applying a remote edit must not strip B's local role");
    }

    #[test]
    fn a_member_who_arrives_from_the_remote_holds_no_roles_here() {
        let (_tmp, project_id, coordinator_a, coordinator_b, clone_a, _alex) = two_devices_sharing_alex();
        let newcomer = Member::new_human(project_id, "Newcomer");
        let mut snapshot = coordinator_a.export_project_snapshot(project_id).unwrap();
        let mut with_role = newcomer.clone();
        with_role.role_ids = vec![Uuid::new_v4()];
        snapshot.members.push(with_role);
        let snapshot = snapshot.canonicalize();
        push_raw_snapshot(&clone_a, &snapshot);

        coordinator_b.pull_from_git(project_id).unwrap();
        let arrived = coordinator_b.storage.get_member(newcomer.id).unwrap().expect("positive control: the new member did arrive");
        assert!(arrived.role_ids.is_empty(), "a brand-new member starts with no roles on this device: {:?}", arrived.role_ids);
    }

    #[test]
    fn a_remote_role_list_cannot_create_a_sync_conflict_with_a_local_assignment() {
        // Only the read-side sanitizing prevents THIS (the upserts can keep
        // the local list, but by then the field-level diff has already
        // flagged two different lists as a conflict): B assigns a local role,
        // and a legacy/hostile remote carries some other role list for the
        // same member.
        let (_tmp, project_id, coordinator_a, coordinator_b, clone_a, alex) = two_devices_sharing_alex();
        let local_role = coordinator_b.create_role(project_id, "Reviewer", vec![Permission::Review]).unwrap();
        coordinator_b.assign_role(alex.id, local_role.id).unwrap();

        let mut legacy = coordinator_a.export_project_snapshot(project_id).unwrap();
        legacy.members.iter_mut().find(|m| m.id == alex.id).unwrap().role_ids = vec![Uuid::new_v4()];
        push_raw_snapshot(&clone_a, &legacy);

        let pull = coordinator_b.pull_from_git(project_id).unwrap();
        assert!(pull.conflicts_created.is_empty(), "a role list in the repo must never become a sync conflict: {:?}", pull.conflicts_created);
        assert_eq!(pull.members_applied, 0, "a remote that differs ONLY in a role list has changed nothing that syncs, it must not count as an applied edit");
        assert_eq!(coordinator_b.storage.get_member(alex.id).unwrap().unwrap().role_ids, vec![local_role.id], "B's own assignment is untouched");
    }

    #[test]
    fn a_remote_that_differs_only_in_role_lists_queues_no_pending_review_item() {
        // With the review gate on, a role-only remote difference used to be
        // queued as an incoming "change" for a human to approve or deny, a
        // review item about something that can never apply.
        let (_tmp, project_id, coordinator_a, coordinator_b, clone_a, alex) = two_devices_sharing_alex();
        coordinator_b.set_require_review_for_incoming_changes(project_id, true).unwrap();
        let mut legacy = coordinator_a.export_project_snapshot(project_id).unwrap();
        legacy.members.iter_mut().find(|m| m.id == alex.id).unwrap().role_ids = vec![Uuid::new_v4()];
        push_raw_snapshot(&clone_a, &legacy);

        let pull = coordinator_b.pull_from_git(project_id).unwrap();
        assert!(pull.pending_changes_created.is_empty(), "nothing syncable changed, so nothing may be queued for review: {:?}", pull.pending_changes_created);
        assert!(coordinator_b.list_pending_sync_changes(project_id).unwrap().is_empty());
    }

    #[test]
    fn the_synced_upserts_keep_an_existing_rows_local_roles_and_never_import_incoming_ones() {
        // Called directly so this pins the upserts themselves, independent of
        // the read-side sanitizing that normally runs first.
        let project_id = Uuid::new_v4();
        let coordinator = empty_coordinator_for(project_id);
        let local_role = coordinator.create_role(project_id, "Approver", vec![Permission::Approve]).unwrap();
        let foreign_role = Uuid::new_v4();

        // Member: existing keeps ITS roles; a new one starts with none.
        let mut existing = Member::new_human(project_id, "Alex");
        existing.role_ids = vec![local_role.id];
        coordinator.storage.insert_member(&existing).unwrap();
        let mut incoming = existing.clone();
        incoming.display_name = "Alexandra".into();
        incoming.role_ids = vec![foreign_role];
        coordinator.upsert_synced_member(&incoming).unwrap();
        let after = coordinator.storage.get_member(existing.id).unwrap().unwrap();
        assert_eq!(after.display_name, "Alexandra", "positive control: the rest of the incoming row was applied");
        assert_eq!(after.role_ids, vec![local_role.id], "the existing member keeps its own roles");

        let mut newcomer = Member::new_human(project_id, "Newcomer");
        newcomer.role_ids = vec![local_role.id];
        coordinator.upsert_synced_member(&newcomer).unwrap();
        assert!(coordinator.storage.get_member(newcomer.id).unwrap().unwrap().role_ids.is_empty(), "a member arriving via sync holds no roles here, even one that happens to exist locally");

        // Specialist: same two rules.
        let mut spec = sql_specialist(Uuid::new_v4());
        spec.role_ids = vec![local_role.id];
        coordinator.storage.insert_specialist(project_id, &spec).unwrap();
        let mut spec_in = spec.clone();
        spec_in.name = "Renamed".into();
        spec_in.role_ids = vec![foreign_role];
        coordinator.upsert_synced_specialist(project_id, &spec_in).unwrap();
        let spec_after = coordinator.storage.get_specialist(spec.id).unwrap().unwrap();
        assert_eq!(spec_after.name, "Renamed", "positive control");
        assert_eq!(spec_after.role_ids, vec![local_role.id]);

        let mut fresh = sql_specialist(Uuid::new_v4());
        fresh.role_ids = vec![local_role.id];
        coordinator.upsert_synced_specialist(project_id, &fresh).unwrap();
        assert!(coordinator.storage.get_specialist(fresh.id).unwrap().unwrap().role_ids.is_empty());
    }

    #[test]
    fn nothing_role_shaped_is_ever_written_to_the_repo_or_the_stored_baseline() {
        let (_tmp, project_id, coordinator_a, _coordinator_b, clone_a, alex) = two_devices_sharing_alex();
        let role = coordinator_a.create_role(project_id, "Approver", vec![Permission::Approve]).unwrap();
        coordinator_a.assign_role(alex.id, role.id).unwrap();
        assert_eq!(coordinator_a.storage.get_member(alex.id).unwrap().unwrap().role_ids, vec![role.id], "precondition: A really holds the role locally");

        coordinator_a.push_to_git(project_id).unwrap();

        let in_repo = solexa_sync::read_snapshot(&clone_a).unwrap();
        assert!(in_repo.members.iter().all(|m| m.role_ids.is_empty()), "the pushed snapshot must carry no role assignments");
        assert!(in_repo.specialists.iter().all(|s| s.role_ids.is_empty()));
        let baseline = coordinator_a.storage.get_git_sync_config(project_id).unwrap().unwrap().last_synced_snapshot_json.unwrap();
        assert!(!baseline.contains(&role.id.to_string()), "the stored baseline must not carry them either");
        // ...while A's own role assignment is, of course, untouched locally.
        assert_eq!(coordinator_a.storage.get_member(alex.id).unwrap().unwrap().role_ids, vec![role.id]);
        // And a role change alone is not "local changes to push".
        let status = coordinator_a.git_sync_status(project_id).unwrap().unwrap();
        assert!(!status.local_dirty, "assigning a role is device-local policy, not a sync change");
    }

    // ---- round 30: mirror a sync Conflict as a GitHub PR --------------
    //
    // `solexa_github::fake::FakeGithubClient` stands in for a real,
    // authenticated `gh` CLI session, round 30's README explains why
    // that's the one part of this feature that can't be exercised for
    // real in this sandbox (no live GitHub account to push branches
    // to). Everything *else* here is real: a genuine field-level sync
    // Conflict, a genuine git branch pushed to a genuine bare remote,
    // and genuine calls into the same `resolve_sync_conflict` every
    // other conflict in this file goes through.

    fn coordinator_with_fake_github(project_id: Uuid) -> (Coordinator, StdArc<solexa_github::fake::FakeGithubClient>) {
        let fake = StdArc::new(solexa_github::fake::FakeGithubClient::new());
        let coordinator = empty_coordinator_for(project_id).with_github_client(fake.clone());
        (coordinator, fake)
    }

    #[test]
    fn pull_from_git_automatically_opens_a_real_pr_with_locals_value_and_restores_the_original_branch() {
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_a) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let clone_b = clone_of(tmp.path(), &remote, "device_b");

        let team_id = Uuid::new_v4();
        let coordinator_a = empty_coordinator_for(project_id);
        coordinator_a
            .storage
            .insert_team(&Team { id: team_id, project_id, name: "Engineering".into(), mission: "shared baseline".into(), member_ids: vec![], parent_team_id: None, deleted_at: None })
            .unwrap();
        coordinator_a.link_git_repo(project_id, clone_a.to_str().unwrap().into()).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let (coordinator_b, fake) = coordinator_with_fake_github(project_id);
        coordinator_b.link_git_repo(project_id, clone_b.to_str().unwrap().into()).unwrap();
        coordinator_b.pull_from_git(project_id).unwrap();

        let mut on_a = coordinator_a.storage.get_team(team_id).unwrap().unwrap();
        on_a.mission = "A's version".into();
        coordinator_a.storage.update_team(&on_a).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let mut on_b = coordinator_b.storage.get_team(team_id).unwrap().unwrap();
        on_b.mission = "B's version".into();
        coordinator_b.storage.update_team(&on_b).unwrap();

        let pull = coordinator_b.pull_from_git(project_id).unwrap();
        assert_eq!(pull.conflicts_created.len(), 1);
        let conflict_id = pull.conflicts_created[0];

        // Round 31: pull_from_git already sent this automatically, no
        // manual `send_conflict_to_github` call needed here at all.
        let branch_before = solexa_sync::current_branch(&clone_b).unwrap();
        assert_eq!(pull.github_prs_created.len(), 1);
        assert!(pull.github_pr_errors.is_empty(), "got: {:?}", pull.github_pr_errors);
        let (sent_conflict_id, url) = &pull.github_prs_created[0];
        assert_eq!(*sent_conflict_id, conflict_id);
        assert!(url.starts_with("https://github.com/fake-org/fake-repo/pull/"));

        // The conflict remembers the PR.
        let conflict = coordinator_b.storage.get_conflict(conflict_id).unwrap().unwrap();
        assert_eq!(conflict.github_pr_url.as_deref(), Some(url.as_str()));
        assert_eq!(conflict.status, solexa_domain::ConflictStatus::Open, "sending to GitHub doesn't resolve anything by itself");

        // The fake recorded a PR proposing B's ("Local") value, based
        // on B's currently checked-out branch.
        assert_eq!(fake.created_pr_count(), 1);
        let created = &fake.created_prs.lock().unwrap()[0];
        assert_eq!(created.base_branch, branch_before);
        assert!(created.head_branch.starts_with("solexa-conflict/team-"), "got: {}", created.head_branch);
        assert!(created.body.contains("B's version"), "PR body should include local's proposed value: {}", created.body);

        // The branch is a *real* branch a real remote really has, not
        // just something the fake claims happened.
        let remote_branches = String::from_utf8(std::process::Command::new("git").arg("-C").arg(&remote).args(["branch", "--list"]).output().unwrap().stdout).unwrap();
        assert!(remote_branches.contains(&created.head_branch), "remote branches: {remote_branches}");

        // And the working tree is back where the user left it.
        assert_eq!(solexa_sync::current_branch(&clone_b).unwrap(), branch_before);
    }

    #[test]
    fn send_conflict_to_github_rejects_a_conflict_that_already_has_a_pr() {
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_a) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let clone_b = clone_of(tmp.path(), &remote, "device_b");

        let team_id = Uuid::new_v4();
        let coordinator_a = empty_coordinator_for(project_id);
        coordinator_a.storage.insert_team(&Team { id: team_id, project_id, name: "Engineering".into(), mission: "shared baseline".into(), member_ids: vec![], parent_team_id: None, deleted_at: None }).unwrap();
        coordinator_a.link_git_repo(project_id, clone_a.to_str().unwrap().into()).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let (coordinator_b, _fake) = coordinator_with_fake_github(project_id);
        coordinator_b.link_git_repo(project_id, clone_b.to_str().unwrap().into()).unwrap();
        coordinator_b.pull_from_git(project_id).unwrap();

        let mut on_a = coordinator_a.storage.get_team(team_id).unwrap().unwrap();
        on_a.mission = "A's version".into();
        coordinator_a.storage.update_team(&on_a).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();
        let mut on_b = coordinator_b.storage.get_team(team_id).unwrap().unwrap();
        on_b.mission = "B's version".into();
        coordinator_b.storage.update_team(&on_b).unwrap();
        let pull = coordinator_b.pull_from_git(project_id).unwrap();
        let conflict_id = pull.conflicts_created[0];
        // Round 31: the pull above already sent this one automatically,
        // calling it again by hand is the "duplicate" this test checks.
        assert_eq!(pull.github_prs_created.len(), 1);

        let err = coordinator_b.send_conflict_to_github(conflict_id).unwrap_err();
        assert!(err.to_string().contains("already has a GitHub PR"), "got: {err}");
    }

    #[test]
    fn check_conflict_github_pr_status_merged_resolves_accepting_local() {
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_a) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let clone_b = clone_of(tmp.path(), &remote, "device_b");

        let team_id = Uuid::new_v4();
        let coordinator_a = empty_coordinator_for(project_id);
        coordinator_a.storage.insert_team(&Team { id: team_id, project_id, name: "Engineering".into(), mission: "shared baseline".into(), member_ids: vec![], parent_team_id: None, deleted_at: None }).unwrap();
        coordinator_a.link_git_repo(project_id, clone_a.to_str().unwrap().into()).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let (coordinator_b, fake) = coordinator_with_fake_github(project_id);
        coordinator_b.link_git_repo(project_id, clone_b.to_str().unwrap().into()).unwrap();
        coordinator_b.pull_from_git(project_id).unwrap();

        let mut on_a = coordinator_a.storage.get_team(team_id).unwrap().unwrap();
        on_a.mission = "A's version".into();
        coordinator_a.storage.update_team(&on_a).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();
        let mut on_b = coordinator_b.storage.get_team(team_id).unwrap().unwrap();
        on_b.mission = "B's version".into();
        coordinator_b.storage.update_team(&on_b).unwrap();
        let pull = coordinator_b.pull_from_git(project_id).unwrap();
        let conflict_id = pull.conflicts_created[0];

        // Round 31: pull_from_git already sent it automatically.
        let url = &pull.github_prs_created[0].1;
        fake.set_status(url, solexa_github::PrState::Merged);

        let approver = Member::new_human(project_id, "Sam");
        coordinator_b.storage.insert_member(&approver).unwrap();
        let outcome = coordinator_b.check_conflict_github_pr_status(conflict_id, approver.id).unwrap();
        assert_eq!(outcome, GithubPrCheckOutcome::ResolvedAcceptingLocal);

        // Merging the PR = accepting the LOCAL (B's) value.
        assert_eq!(coordinator_b.storage.get_team(team_id).unwrap().unwrap().mission, "B's version");
        let resolved = coordinator_b.storage.get_conflict(conflict_id).unwrap().unwrap();
        assert_eq!(resolved.status, solexa_domain::ConflictStatus::Resolved);
    }

    #[test]
    fn check_conflict_github_pr_status_closed_resolves_accepting_remote() {
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_a) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let clone_b = clone_of(tmp.path(), &remote, "device_b");

        let team_id = Uuid::new_v4();
        let coordinator_a = empty_coordinator_for(project_id);
        coordinator_a.storage.insert_team(&Team { id: team_id, project_id, name: "Engineering".into(), mission: "shared baseline".into(), member_ids: vec![], parent_team_id: None, deleted_at: None }).unwrap();
        coordinator_a.link_git_repo(project_id, clone_a.to_str().unwrap().into()).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let (coordinator_b, fake) = coordinator_with_fake_github(project_id);
        coordinator_b.link_git_repo(project_id, clone_b.to_str().unwrap().into()).unwrap();
        coordinator_b.pull_from_git(project_id).unwrap();

        let mut on_a = coordinator_a.storage.get_team(team_id).unwrap().unwrap();
        on_a.mission = "A's version".into();
        coordinator_a.storage.update_team(&on_a).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();
        let mut on_b = coordinator_b.storage.get_team(team_id).unwrap().unwrap();
        on_b.mission = "B's version".into();
        coordinator_b.storage.update_team(&on_b).unwrap();
        let pull = coordinator_b.pull_from_git(project_id).unwrap();
        let conflict_id = pull.conflicts_created[0];

        // Round 31: pull_from_git already sent it automatically.
        let url = &pull.github_prs_created[0].1;
        fake.set_status(url, solexa_github::PrState::Closed);

        let approver = Member::new_human(project_id, "Sam");
        coordinator_b.storage.insert_member(&approver).unwrap();
        let outcome = coordinator_b.check_conflict_github_pr_status(conflict_id, approver.id).unwrap();
        assert_eq!(outcome, GithubPrCheckOutcome::ResolvedAcceptingRemote);

        // Closing without merging = keeping what was already there (A's/"Remote"'s value).
        assert_eq!(coordinator_b.storage.get_team(team_id).unwrap().unwrap().mission, "A's version");
        let resolved = coordinator_b.storage.get_conflict(conflict_id).unwrap().unwrap();
        assert_eq!(resolved.status, solexa_domain::ConflictStatus::Resolved);
    }

    #[test]
    fn check_conflict_github_pr_status_still_open_makes_no_local_change() {
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_a) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let clone_b = clone_of(tmp.path(), &remote, "device_b");

        let team_id = Uuid::new_v4();
        let coordinator_a = empty_coordinator_for(project_id);
        coordinator_a.storage.insert_team(&Team { id: team_id, project_id, name: "Engineering".into(), mission: "shared baseline".into(), member_ids: vec![], parent_team_id: None, deleted_at: None }).unwrap();
        coordinator_a.link_git_repo(project_id, clone_a.to_str().unwrap().into()).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let (coordinator_b, _fake) = coordinator_with_fake_github(project_id);
        coordinator_b.link_git_repo(project_id, clone_b.to_str().unwrap().into()).unwrap();
        coordinator_b.pull_from_git(project_id).unwrap();

        let mut on_a = coordinator_a.storage.get_team(team_id).unwrap().unwrap();
        on_a.mission = "A's version".into();
        coordinator_a.storage.update_team(&on_a).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();
        let mut on_b = coordinator_b.storage.get_team(team_id).unwrap().unwrap();
        on_b.mission = "B's version".into();
        coordinator_b.storage.update_team(&on_b).unwrap();
        let pull = coordinator_b.pull_from_git(project_id).unwrap();
        let conflict_id = pull.conflicts_created[0];

        // Round 31: pull_from_git already sent it automatically, left
        // at the fake's default (freshly-created PRs start Open).
        assert_eq!(pull.github_prs_created.len(), 1);

        let approver = Member::new_human(project_id, "Sam");
        coordinator_b.storage.insert_member(&approver).unwrap();
        let outcome = coordinator_b.check_conflict_github_pr_status(conflict_id, approver.id).unwrap();
        assert_eq!(outcome, GithubPrCheckOutcome::StillOpen);

        // B's own value is untouched, and the conflict is still open,
        // exactly as if the PR had never been sent at all.
        assert_eq!(coordinator_b.storage.get_team(team_id).unwrap().unwrap().mission, "B's version");
        assert_eq!(coordinator_b.storage.get_conflict(conflict_id).unwrap().unwrap().status, solexa_domain::ConflictStatus::Open);
    }

    // ---- round 31: automatic PR creation for every new conflict ------
    //
    // Round 30 shipped this as an explicit, opt-in, per-conflict
    // action. Asked directly what to build next, the user wanted this
    // to just happen, "no more manual 'Send' click", so `pull_from_git`
    // itself now attempts it for every conflict it creates. The one new
    // thing worth a dedicated test is what happens when that attempt
    // can't succeed: it must never take the pull down with it.

    #[test]
    fn pull_from_git_records_a_github_error_without_failing_the_pull_when_gh_is_unavailable() {
        // Deliberately NOT using `coordinator_with_fake_github` here,
        // this uses the real, default `GhCliClient`, and this sandbox's
        // `gh` is genuinely installed but genuinely never authenticated
        // (round 30), so this is a real "the precondition fails" case,
        // not a simulated one.
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_a) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let clone_b = clone_of(tmp.path(), &remote, "device_b");

        let team_id = Uuid::new_v4();
        let coordinator_a = empty_coordinator_for(project_id);
        coordinator_a.storage.insert_team(&Team { id: team_id, project_id, name: "Engineering".into(), mission: "shared baseline".into(), member_ids: vec![], parent_team_id: None, deleted_at: None }).unwrap();
        coordinator_a.link_git_repo(project_id, clone_a.to_str().unwrap().into()).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let coordinator_b = empty_coordinator_for(project_id);
        coordinator_b.link_git_repo(project_id, clone_b.to_str().unwrap().into()).unwrap();
        coordinator_b.pull_from_git(project_id).unwrap();

        let mut on_a = coordinator_a.storage.get_team(team_id).unwrap().unwrap();
        on_a.mission = "A's version".into();
        coordinator_a.storage.update_team(&on_a).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();
        let mut on_b = coordinator_b.storage.get_team(team_id).unwrap().unwrap();
        on_b.mission = "B's version".into();
        coordinator_b.storage.update_team(&on_b).unwrap();

        // The pull itself must still succeed...
        let pull = coordinator_b.pull_from_git(project_id).unwrap();
        assert_eq!(pull.conflicts_created.len(), 1, "the conflict is still created regardless of GitHub");

        // ...even though the automatic GitHub attempt for its new
        // conflict couldn't possibly have worked.
        assert!(pull.github_prs_created.is_empty());
        assert_eq!(pull.github_pr_errors.len(), 1);
        assert_eq!(pull.github_pr_errors[0].0, pull.conflicts_created[0]);
        assert!(pull.github_pr_errors[0].1.contains("gh"), "got: {}", pull.github_pr_errors[0].1);

        // And the conflict is exactly as resolvable locally as it
        // always was, GitHub mirroring is additive, never a
        // precondition for SOLEXA's own Conflict Center.
        let conflict = coordinator_b.storage.get_conflict(pull.conflicts_created[0]).unwrap().unwrap();
        assert!(conflict.github_pr_url.is_none());
        assert_eq!(conflict.status, solexa_domain::ConflictStatus::Open);
    }

    // ---- round 32: automatic PR *status checking* (item 2 of the ------
    // user's "1 and 2 and 3 then 4" order) ------------------------------
    //
    // Round 31 made *sending* a conflict to GitHub automatic. This
    // round does the same for *checking* it, a sweep over every open,
    // already-mirrored conflict in a project, reusing
    // `check_conflict_github_pr_status` entirely rather than
    // duplicating its resolution logic.

    #[test]
    fn check_all_conflict_github_pr_statuses_resolves_every_eligible_conflict_in_one_call() {
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_a) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let clone_b = clone_of(tmp.path(), &remote, "device_b");

        let team_1 = Uuid::new_v4();
        let team_2 = Uuid::new_v4();
        let coordinator_a = empty_coordinator_for(project_id);
        coordinator_a.storage.insert_team(&Team { id: team_1, project_id, name: "Engineering".into(), mission: "shared baseline".into(), member_ids: vec![], parent_team_id: None, deleted_at: None }).unwrap();
        coordinator_a.storage.insert_team(&Team { id: team_2, project_id, name: "Design".into(), mission: "shared baseline".into(), member_ids: vec![], parent_team_id: None, deleted_at: None }).unwrap();
        coordinator_a.link_git_repo(project_id, clone_a.to_str().unwrap().into()).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let (coordinator_b, fake) = coordinator_with_fake_github(project_id);
        coordinator_b.link_git_repo(project_id, clone_b.to_str().unwrap().into()).unwrap();
        coordinator_b.pull_from_git(project_id).unwrap();

        // Two independent conflicts in one pull, both auto-sent by round 31.
        let mut team_1_on_a = coordinator_a.storage.get_team(team_1).unwrap().unwrap();
        team_1_on_a.mission = "A's version".into();
        coordinator_a.storage.update_team(&team_1_on_a).unwrap();
        let mut team_2_on_a = coordinator_a.storage.get_team(team_2).unwrap().unwrap();
        team_2_on_a.mission = "A's version".into();
        coordinator_a.storage.update_team(&team_2_on_a).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let mut team_1_on_b = coordinator_b.storage.get_team(team_1).unwrap().unwrap();
        team_1_on_b.mission = "B's version".into();
        coordinator_b.storage.update_team(&team_1_on_b).unwrap();
        let mut team_2_on_b = coordinator_b.storage.get_team(team_2).unwrap().unwrap();
        team_2_on_b.mission = "B's version".into();
        coordinator_b.storage.update_team(&team_2_on_b).unwrap();

        let pull = coordinator_b.pull_from_git(project_id).unwrap();
        assert_eq!(pull.conflicts_created.len(), 2);
        assert_eq!(pull.github_prs_created.len(), 2, "both conflicts should have been auto-sent by round 31");

        let url_for = |conflict_id: Uuid| pull.github_prs_created.iter().find(|(id, _)| *id == conflict_id).unwrap().1.clone();
        let team_1_conflict = coordinator_b.storage.list_conflicts(project_id).unwrap().into_iter().find(|c| c.sync_object.as_ref().unwrap().object_id == team_1).unwrap();
        let team_2_conflict = coordinator_b.storage.list_conflicts(project_id).unwrap().into_iter().find(|c| c.sync_object.as_ref().unwrap().object_id == team_2).unwrap();
        fake.set_status(&url_for(team_1_conflict.id), solexa_github::PrState::Merged);
        fake.set_status(&url_for(team_2_conflict.id), solexa_github::PrState::Closed);

        let approver = Member::new_human(project_id, "Sam");
        coordinator_b.storage.insert_member(&approver).unwrap();
        let sweep = coordinator_b.check_all_conflict_github_pr_statuses(project_id, approver.id).unwrap();

        assert_eq!(sweep.checked.len(), 2);
        assert!(sweep.errors.is_empty(), "got: {:?}", sweep.errors);
        let outcome_for = |conflict_id: Uuid| sweep.checked.iter().find(|(id, _)| *id == conflict_id).unwrap().1;
        assert_eq!(outcome_for(team_1_conflict.id), GithubPrCheckOutcome::ResolvedAcceptingLocal);
        assert_eq!(outcome_for(team_2_conflict.id), GithubPrCheckOutcome::ResolvedAcceptingRemote);

        // One sweep call resolved both, each the right way.
        assert_eq!(coordinator_b.storage.get_team(team_1).unwrap().unwrap().mission, "B's version");
        assert_eq!(coordinator_b.storage.get_team(team_2).unwrap().unwrap().mission, "A's version");
    }

    #[test]
    fn check_all_conflict_github_pr_statuses_skips_a_conflict_thats_already_resolved() {
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_a) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let clone_b = clone_of(tmp.path(), &remote, "device_b");

        let team_1 = Uuid::new_v4();
        let team_2 = Uuid::new_v4();
        let coordinator_a = empty_coordinator_for(project_id);
        coordinator_a.storage.insert_team(&Team { id: team_1, project_id, name: "Engineering".into(), mission: "shared baseline".into(), member_ids: vec![], parent_team_id: None, deleted_at: None }).unwrap();
        coordinator_a.storage.insert_team(&Team { id: team_2, project_id, name: "Design".into(), mission: "shared baseline".into(), member_ids: vec![], parent_team_id: None, deleted_at: None }).unwrap();
        coordinator_a.link_git_repo(project_id, clone_a.to_str().unwrap().into()).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let (coordinator_b, _fake) = coordinator_with_fake_github(project_id);
        coordinator_b.link_git_repo(project_id, clone_b.to_str().unwrap().into()).unwrap();
        coordinator_b.pull_from_git(project_id).unwrap();

        let mut team_1_on_a = coordinator_a.storage.get_team(team_1).unwrap().unwrap();
        team_1_on_a.mission = "A's version".into();
        coordinator_a.storage.update_team(&team_1_on_a).unwrap();
        let mut team_2_on_a = coordinator_a.storage.get_team(team_2).unwrap().unwrap();
        team_2_on_a.mission = "A's version".into();
        coordinator_a.storage.update_team(&team_2_on_a).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let mut team_1_on_b = coordinator_b.storage.get_team(team_1).unwrap().unwrap();
        team_1_on_b.mission = "B's version".into();
        coordinator_b.storage.update_team(&team_1_on_b).unwrap();
        let mut team_2_on_b = coordinator_b.storage.get_team(team_2).unwrap().unwrap();
        team_2_on_b.mission = "B's version".into();
        coordinator_b.storage.update_team(&team_2_on_b).unwrap();

        let pull = coordinator_b.pull_from_git(project_id).unwrap();
        assert_eq!(pull.conflicts_created.len(), 2);

        let approver = Member::new_human(project_id, "Sam");
        coordinator_b.storage.insert_member(&approver).unwrap();

        // Resolve team_1's conflict manually, the ordinary way, before
        // the sweep ever runs, simulating a human who used the
        // Conflict Center directly instead of waiting for GitHub.
        let team_1_conflict = coordinator_b.storage.list_conflicts(project_id).unwrap().into_iter().find(|c| c.sync_object.as_ref().unwrap().object_id == team_1).unwrap();
        let local_position = team_1_conflict.positions.iter().find(|p| p.specialist_name == "Local").unwrap();
        coordinator_b.resolve_sync_conflict(team_1_conflict.id, local_position.specialist_id, approver.id).unwrap();

        let sweep = coordinator_b.check_all_conflict_github_pr_statuses(project_id, approver.id).unwrap();

        // Only team_2's conflict was actually eligible, team_1's is
        // skipped by the sweep's own filter, never even reaching
        // `check_conflict_github_pr_status`.
        assert_eq!(sweep.checked.len(), 1);
        let team_2_conflict = coordinator_b.storage.list_conflicts(project_id).unwrap().into_iter().find(|c| c.sync_object.as_ref().unwrap().object_id == team_2).unwrap();
        assert_eq!(sweep.checked[0].0, team_2_conflict.id);
        assert_eq!(sweep.checked[0].1, GithubPrCheckOutcome::StillOpen);
    }

    #[test]
    fn check_all_conflict_github_pr_statuses_records_a_per_conflict_error_without_failing_the_sweep() {
        // The real, default `GhCliClient` again, this sandbox's `gh`
        // is genuinely installed but never authenticated, so `gh pr
        // view` genuinely fails, the same way it would for a user who
        // logged out of `gh` sometime between sending a conflict to
        // GitHub and this sweep running.
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_a) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let clone_b = clone_of(tmp.path(), &remote, "device_b");

        let team_id = Uuid::new_v4();
        let coordinator_a = empty_coordinator_for(project_id);
        coordinator_a.storage.insert_team(&Team { id: team_id, project_id, name: "Engineering".into(), mission: "shared baseline".into(), member_ids: vec![], parent_team_id: None, deleted_at: None }).unwrap();
        coordinator_a.link_git_repo(project_id, clone_a.to_str().unwrap().into()).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let coordinator_b = empty_coordinator_for(project_id);
        coordinator_b.link_git_repo(project_id, clone_b.to_str().unwrap().into()).unwrap();
        coordinator_b.pull_from_git(project_id).unwrap();

        let mut on_a = coordinator_a.storage.get_team(team_id).unwrap().unwrap();
        on_a.mission = "A's version".into();
        coordinator_a.storage.update_team(&on_a).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();
        let mut on_b = coordinator_b.storage.get_team(team_id).unwrap().unwrap();
        on_b.mission = "B's version".into();
        coordinator_b.storage.update_team(&on_b).unwrap();
        let pull = coordinator_b.pull_from_git(project_id).unwrap();
        let conflict_id = pull.conflicts_created[0];
        // Round 31's automatic send failed too (same unavailable `gh`).
        // Set the URL directly, standing in for it having succeeded at
        // some point in the past when `gh` *was* authenticated.
        coordinator_b.storage.set_conflict_github_pr_url(conflict_id, "https://github.com/fake-org/fake-repo/pull/1").unwrap();

        let approver = Member::new_human(project_id, "Sam");
        coordinator_b.storage.insert_member(&approver).unwrap();
        let sweep = coordinator_b.check_all_conflict_github_pr_statuses(project_id, approver.id).unwrap();

        assert!(sweep.checked.is_empty());
        assert_eq!(sweep.errors.len(), 1);
        assert_eq!(sweep.errors[0].0, conflict_id);

        // Still fully open and resolvable locally regardless.
        assert_eq!(coordinator_b.storage.get_conflict(conflict_id).unwrap().unwrap().status, solexa_domain::ConflictStatus::Open);
    }

    #[test]
    fn check_conflict_github_pr_status_on_an_already_resolved_conflict_is_a_noop() {
        // If this incorrectly tried to look up a PR's status instead of
        // short-circuiting, it would fail with "was never sent to
        // GitHub" (this conflict has no `github_pr_url` at all) rather
        // than quietly returning `AlreadyResolved`, so a passing test
        // here really does confirm the short-circuit runs first.
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_a) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let clone_b = clone_of(tmp.path(), &remote, "device_b");

        let team_id = Uuid::new_v4();
        let coordinator_a = empty_coordinator_for(project_id);
        coordinator_a.storage.insert_team(&Team { id: team_id, project_id, name: "Engineering".into(), mission: "shared baseline".into(), member_ids: vec![], parent_team_id: None, deleted_at: None }).unwrap();
        coordinator_a.link_git_repo(project_id, clone_a.to_str().unwrap().into()).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let coordinator_b = empty_coordinator_for(project_id);
        coordinator_b.link_git_repo(project_id, clone_b.to_str().unwrap().into()).unwrap();
        coordinator_b.pull_from_git(project_id).unwrap();

        let mut on_a = coordinator_a.storage.get_team(team_id).unwrap().unwrap();
        on_a.mission = "A's version".into();
        coordinator_a.storage.update_team(&on_a).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();
        let mut on_b = coordinator_b.storage.get_team(team_id).unwrap().unwrap();
        on_b.mission = "B's version".into();
        coordinator_b.storage.update_team(&on_b).unwrap();
        let pull = coordinator_b.pull_from_git(project_id).unwrap();
        let conflict_id = pull.conflicts_created[0];

        let approver = Member::new_human(project_id, "Sam");
        coordinator_b.storage.insert_member(&approver).unwrap();
        let conflict = coordinator_b.storage.get_conflict(conflict_id).unwrap().unwrap();
        let local_position = conflict.positions.iter().find(|p| p.specialist_name == "Local").unwrap();
        coordinator_b.resolve_sync_conflict(conflict_id, local_position.specialist_id, approver.id).unwrap();

        let outcome = coordinator_b.check_conflict_github_pr_status(conflict_id, approver.id).unwrap();
        assert_eq!(outcome, GithubPrCheckOutcome::AlreadyResolved);
    }

    // ---- round 33: mirror a recorded Decision as a GitHub Issue -------
    //
    // Item 3 of the user's "1 and 2 and 3 then 4" order. Unlike
    // Conflicts (round 30), a Decision can have any number of options,
    // so an Issue's open/closed state can't cleanly represent which one
    // was picked the way a PR's merge/close represented "accept local
    // vs. keep remote". So this direction is one-way: SOLEXA creates
    // the issue for visibility, and reflects a resolution back onto it
    // as a closing comment once one is actually made in SOLEXA, SOLEXA
    // never reads a decision's outcome back *from* GitHub the way
    // `check_conflict_github_pr_status` does for Conflicts.

    #[test]
    fn send_decision_to_github_opens_a_real_issue_naming_the_options() {
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (_remote, clone_dir) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let (coordinator, fake) = coordinator_with_fake_github(project_id);
        coordinator.link_git_repo(project_id, clone_dir.to_str().unwrap().into()).unwrap();

        let decision = Decision::new(project_id, "Which database?", vec!["Postgres".into(), "SQLite".into()], vec![]);
        let decision_id = decision.id;
        coordinator.record_decision(decision).unwrap();

        let url = coordinator.send_decision_to_github(decision_id).unwrap();
        assert!(url.contains("/issues/"));

        let stored = coordinator.storage.get_decision(decision_id).unwrap().unwrap();
        assert_eq!(stored.github_issue_url.as_deref(), Some(url.as_str()));

        assert_eq!(fake.created_issue_count(), 1);
        let created = &fake.created_issues.lock().unwrap()[0];
        assert_eq!(created.title, "Which database?");
        assert!(created.body.contains("Postgres"), "got: {}", created.body);
        assert!(created.body.contains("SQLite"), "got: {}", created.body);
        assert!(created.body.contains("visibility only"), "should say this doesn't resolve it, got: {}", created.body);
    }

    #[test]
    fn send_decision_to_github_rejects_a_decision_that_already_has_an_issue() {
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (_remote, clone_dir) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let (coordinator, _fake) = coordinator_with_fake_github(project_id);
        coordinator.link_git_repo(project_id, clone_dir.to_str().unwrap().into()).unwrap();

        let decision = Decision::new(project_id, "Which database?", vec!["Postgres".into(), "SQLite".into()], vec![]);
        let decision_id = decision.id;
        coordinator.record_decision(decision).unwrap();
        coordinator.send_decision_to_github(decision_id).unwrap();

        let err = coordinator.send_decision_to_github(decision_id).unwrap_err();
        assert!(err.to_string().contains("already has a GitHub issue"), "got: {err}");
    }

    #[test]
    fn resolving_a_mirrored_decision_closes_its_issue_with_a_comment_naming_the_outcome() {
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (_remote, clone_dir) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let (coordinator, fake) = coordinator_with_fake_github(project_id);
        coordinator.link_git_repo(project_id, clone_dir.to_str().unwrap().into()).unwrap();

        let decision = Decision::new(project_id, "Which database?", vec!["Postgres".into(), "SQLite".into()], vec![]);
        let decision_id = decision.id;
        coordinator.record_decision(decision).unwrap();
        let url = coordinator.send_decision_to_github(decision_id).unwrap();

        let approver = Member::new_human(project_id, "Alex");
        coordinator.storage.insert_member(&approver).unwrap();
        coordinator.resolve_decision(decision_id, "Postgres", approver.id).unwrap();

        // The decision itself is resolved regardless of GitHub.
        let resolved = coordinator.storage.get_decision(decision_id).unwrap().unwrap();
        assert_eq!(resolved.resolution.as_deref(), Some("Postgres"));

        // And the mirrored issue was closed with a comment naming the
        // actual outcome, not just "resolved", the specific choice.
        let closed = fake.closed_issues.lock().unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].issue_url, url);
        assert!(closed[0].comment.contains("Postgres"), "got: {}", closed[0].comment);
        assert!(closed[0].comment.contains("Alex"), "should name the approver, got: {}", closed[0].comment);
    }

    #[test]
    fn resolving_a_decision_that_was_never_sent_to_github_needs_no_github_at_all() {
        // The overwhelmingly common case: most decisions are never
        // mirrored. Resolving one must not so much as check whether
        // `gh` exists.
        let project_id = Uuid::new_v4();
        let coordinator = empty_coordinator_for(project_id);
        // Deliberately NOT linking a git repo at all, if resolving
        // tried to reach GitHub regardless of `github_issue_url`, this
        // would panic on a missing git sync config instead of quietly
        // doing nothing.
        let decision = Decision::new(project_id, "Which database?", vec!["Postgres".into(), "SQLite".into()], vec![]);
        let decision_id = decision.id;
        coordinator.record_decision(decision).unwrap();

        let approver = Member::new_human(project_id, "Alex");
        coordinator.storage.insert_member(&approver).unwrap();
        coordinator.resolve_decision(decision_id, "Postgres", approver.id).unwrap();

        assert_eq!(coordinator.storage.get_decision(decision_id).unwrap().unwrap().resolution.as_deref(), Some("Postgres"));
    }

    #[test]
    fn resolving_a_mirrored_decision_still_succeeds_when_gh_is_unavailable() {
        // The real, default `GhCliClient`, this sandbox's `gh` is
        // genuinely installed but never authenticated, so closing the
        // issue genuinely fails. `resolve_decision` must still succeed:
        // SOLEXA's own record of the decision is already the source of
        // truth, made unconditionally, before this round-33 addition
        // ever touches GitHub at all.
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (_remote, clone_dir) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let coordinator = empty_coordinator_for(project_id);
        coordinator.link_git_repo(project_id, clone_dir.to_str().unwrap().into()).unwrap();

        let decision = Decision::new(project_id, "Which database?", vec!["Postgres".into(), "SQLite".into()], vec![]);
        let decision_id = decision.id;
        coordinator.record_decision(decision).unwrap();
        // Simulate having sent it successfully in the past, when `gh`
        // *was* authenticated, set the URL directly rather than via
        // `send_decision_to_github`, which would itself refuse right
        // now given the same unavailable `gh`.
        coordinator.storage.set_decision_github_issue_url(decision_id, "https://github.com/fake-org/fake-repo/issues/1").unwrap();

        let approver = Member::new_human(project_id, "Alex");
        coordinator.storage.insert_member(&approver).unwrap();
        let result = coordinator.resolve_decision(decision_id, "Postgres", approver.id);
        assert!(result.is_ok(), "got: {result:?}");
        assert_eq!(coordinator.storage.get_decision(decision_id).unwrap().unwrap().resolution.as_deref(), Some("Postgres"));
    }

    // ---- round 35: multiple projects in one install ------------------
    //
    // The engine was always multi-project; these confirm the small
    // coordinator surface added to let the desktop shell use that, and
    // more importantly, that two projects in one database really are
    // isolated from each other at the coordinator level, not just in
    // principle.

    #[test]
    fn create_project_makes_a_listable_empty_project_and_rejects_a_blank_name() {
        let project_id = Uuid::new_v4();
        let coordinator = empty_coordinator_for(project_id);

        let blank = coordinator.create_project("   ", "");
        assert!(blank.is_err(), "a whitespace-only project name should be rejected");

        let new_id = coordinator.create_project("Compiler Work", "rust frontend").unwrap();
        let found = coordinator.get_project(new_id).unwrap().unwrap();
        assert_eq!(found.name, "Compiler Work");

        // Deliberately empty, seed.rs only bootstraps the very first
        // project, never every subsequent one.
        assert!(coordinator.list_members(new_id).unwrap().is_empty());
        assert!(coordinator.list_teams(new_id).unwrap().is_empty());
        assert!(coordinator.list_specialists(new_id).unwrap().is_empty());

        let all = coordinator.list_projects().unwrap();
        assert_eq!(all.len(), 2, "the fixture's own project plus the new one");
        assert!(all.iter().any(|p| p.id == new_id));
        assert!(all.iter().any(|p| p.id == project_id));
    }

    #[test]
    fn two_projects_in_one_install_keep_their_teams_and_brains_fully_separate() {
        let project_a = Uuid::new_v4();
        let coordinator = empty_coordinator_for(project_a);
        let project_b = coordinator.create_project("Compiler Work", "").unwrap();

        let alex = Member::new_human(project_a, "Alex");
        coordinator.storage.insert_member(&alex).unwrap();
        coordinator
            .storage
            .insert_team(&Team { id: Uuid::new_v4(), project_id: project_a, name: "Engineering".into(), mission: "".into(), member_ids: vec![alex.id], parent_team_id: None, deleted_at: None })
            .unwrap();
        coordinator.record_decision(Decision::new(project_a, "Which database?", vec!["Postgres".into()], vec![])).unwrap();

        let sam = Member::new_human(project_b, "Sam");
        coordinator.storage.insert_member(&sam).unwrap();

        // Each project sees only its own rows, the guarantee the whole
        // multi-project story rests on.
        assert_eq!(coordinator.list_members(project_a).unwrap().len(), 1);
        assert_eq!(coordinator.list_members(project_b).unwrap().len(), 1);
        assert_eq!(coordinator.list_members(project_a).unwrap()[0].id, alex.id);
        assert_eq!(coordinator.list_members(project_b).unwrap()[0].id, sam.id);

        assert_eq!(coordinator.list_teams(project_a).unwrap().len(), 1);
        assert!(coordinator.list_teams(project_b).unwrap().is_empty());

        assert_eq!(coordinator.storage.list_decisions(project_a).unwrap().len(), 1);
        assert!(coordinator.storage.list_decisions(project_b).unwrap().is_empty());
    }

    #[test]
    fn each_project_has_its_own_independent_git_repo_link() {
        // Worth its own test because git sync is the one subsystem
        // where "which project am I" is a real runtime path, not just a
        // WHERE clause: two projects should be able to sync to two
        // different repos without either noticing the other.
        let project_a = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (_remote_a, clone_a) = init_bare_remote_and_clone(tmp.path(), "repo_a");
        let (_remote_b, clone_b) = init_bare_remote_and_clone(tmp.path(), "repo_b");

        let coordinator = empty_coordinator_for(project_a);
        let project_b = coordinator.create_project("Compiler Work", "").unwrap();

        coordinator.link_git_repo(project_a, clone_a.to_str().unwrap().into()).unwrap();
        coordinator.link_git_repo(project_b, clone_b.to_str().unwrap().into()).unwrap();

        let status_a = coordinator.git_sync_status(project_a).unwrap().unwrap();
        let status_b = coordinator.git_sync_status(project_b).unwrap().unwrap();
        assert_eq!(status_a.config.repo_path, clone_a.to_str().unwrap());
        assert_eq!(status_b.config.repo_path, clone_b.to_str().unwrap());

        // Unlinking one leaves the other entirely alone.
        coordinator.unlink_git_repo(project_a).unwrap();
        assert!(coordinator.git_sync_status(project_a).unwrap().is_none());
        assert!(coordinator.git_sync_status(project_b).unwrap().is_some());
    }

    // ---- roles and the pending-sync-change review gate (round 46) --------

    #[test]
    fn member_has_permission_is_the_union_across_every_role_and_false_with_none() {
        let project_id = Uuid::new_v4();
        let coordinator = empty_coordinator_for(project_id);
        let member = Member::new_human(project_id, "Alex");
        coordinator.storage.insert_member(&member).unwrap();

        assert!(!coordinator.member_has_permission(member.id, Permission::Approve).unwrap(), "no role assigned yet, nothing is granted just by existing");

        let reader = coordinator.create_role(project_id, "Reader", vec![Permission::Read]).unwrap();
        let approver = coordinator.create_role(project_id, "Manager", vec![Permission::Approve]).unwrap();
        coordinator.assign_role(member.id, reader.id).unwrap();
        assert!(coordinator.member_has_permission(member.id, Permission::Read).unwrap());
        assert!(!coordinator.member_has_permission(member.id, Permission::Approve).unwrap(), "holding Reader alone must not grant Approve");

        // Holding a SECOND role adds to the union rather than replacing
        // the first, the flat, no-hierarchy union Discord itself uses.
        coordinator.assign_role(member.id, approver.id).unwrap();
        assert!(coordinator.member_has_permission(member.id, Permission::Read).unwrap());
        assert!(coordinator.member_has_permission(member.id, Permission::Approve).unwrap());

        // Idempotent re-assignment.
        coordinator.assign_role(member.id, approver.id).unwrap();
        assert_eq!(coordinator.storage.get_member(member.id).unwrap().unwrap().role_ids.len(), 2);

        // Unassigning drops it straight back out of the union.
        coordinator.unassign_role(member.id, approver.id).unwrap();
        assert!(!coordinator.member_has_permission(member.id, Permission::Approve).unwrap());
        assert!(coordinator.member_has_permission(member.id, Permission::Read).unwrap(), "unassigning one role must not touch the other");

        // Renaming/re-permissioning a role is reflected immediately for
        // every current holder, with no per-member update needed.
        coordinator.update_role(reader.id, "Read-Only", vec![]).unwrap();
        assert!(!coordinator.member_has_permission(member.id, Permission::Read).unwrap());
        assert_eq!(coordinator.list_roles(project_id).unwrap().len(), 2);
    }

    #[test]
    fn assign_role_rejects_a_role_from_a_different_project() {
        let project_a = Uuid::new_v4();
        let coordinator = empty_coordinator_for(project_a);
        let project_b = coordinator.create_project("Other Project", "").unwrap();

        let member = Member::new_human(project_a, "Alex");
        coordinator.storage.insert_member(&member).unwrap();
        let foreign_role = coordinator.create_role(project_b, "Manager", vec![Permission::Approve]).unwrap();

        let err = coordinator.assign_role(member.id, foreign_role.id).unwrap_err();
        assert!(err.to_string().contains("different project"));
        assert!(coordinator.storage.get_member(member.id).unwrap().unwrap().role_ids.is_empty());
    }

    #[test]
    fn pull_from_git_queues_pending_changes_instead_of_applying_when_review_is_required() {
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_a) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let clone_b = clone_of(tmp.path(), &remote, "device_b");

        // Device A pushes a new team.
        let coordinator_a = empty_coordinator_for(project_id);
        let team = Team { id: Uuid::new_v4(), project_id, name: "Engineering".into(), mission: "ship SOLEXA".into(), member_ids: vec![], parent_team_id: None, deleted_at: None };
        coordinator_a.storage.insert_team(&team).unwrap();
        coordinator_a.link_git_repo(project_id, clone_a.to_str().unwrap().into()).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        // Device B opts into the review gate BEFORE pulling.
        let coordinator_b = empty_coordinator_for(project_id);
        coordinator_b.link_git_repo(project_id, clone_b.to_str().unwrap().into()).unwrap();
        coordinator_b.set_require_review_for_incoming_changes(project_id, true).unwrap();

        let pull = coordinator_b.pull_from_git(project_id).unwrap();
        assert_eq!(pull.teams_applied, 0, "must NOT fast-forward while review is required");
        assert_eq!(pull.pending_changes_created.len(), 1);
        assert!(pull.conflicts_created.is_empty(), "a non-conflicting change is pending review, not a conflict");
        assert!(coordinator_b.storage.get_team(team.id).unwrap().is_none(), "must not exist locally until explicitly applied");

        let pending = coordinator_b.list_pending_sync_changes(project_id).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].kind, SyncObjectKind::Team);
        assert_eq!(pending[0].label, "Engineering");
        assert!(pending[0].is_new);
        assert_eq!(pending[0].status, PendingSyncChangeStatus::Pending);

        // Pulling AGAIN with nothing new on the remote must refresh in
        // place, not create a second row.
        coordinator_b.pull_from_git(project_id).unwrap();
        assert_eq!(coordinator_b.list_pending_sync_changes(project_id).unwrap().len(), 1, "must dedup against the still-pending row");

        // A member with no Approve permission is rejected outright.
        let unprivileged = Member::new_human(project_id, "Casey");
        coordinator_b.storage.insert_member(&unprivileged).unwrap();
        let err = coordinator_b.apply_pending_sync_change(pending[0].id, unprivileged.id).unwrap_err();
        assert!(err.to_string().contains("Permission::Approve"));
        assert!(coordinator_b.storage.get_team(team.id).unwrap().is_none(), "a rejected apply must not have touched local state");

        // A member holding a role with Approve can apply it for real.
        let manager = Member::new_human(project_id, "Alex");
        coordinator_b.storage.insert_member(&manager).unwrap();
        let manager_role = coordinator_b.create_role(project_id, "Manager", vec![Permission::Approve]).unwrap();
        coordinator_b.assign_role(manager.id, manager_role.id).unwrap();

        coordinator_b.apply_pending_sync_change(pending[0].id, manager.id).unwrap();
        let applied_team = coordinator_b.storage.get_team(team.id).unwrap().unwrap();
        assert_eq!(applied_team.name, "Engineering");
        let decided = coordinator_b.list_pending_sync_changes(project_id).unwrap();
        assert_eq!(decided[0].status, PendingSyncChangeStatus::Applied);
        assert_eq!(decided[0].decided_by.as_deref(), Some("Alex"));

        // Deciding it a second time is a clear error, not a silent no-op.
        let err = coordinator_b.apply_pending_sync_change(pending[0].id, manager.id).unwrap_err();
        assert!(err.to_string().contains("already decided"));
    }

    #[test]
    fn deny_pending_sync_change_records_a_reason_and_never_applies_it() {
        let project_id = Uuid::new_v4();
        let tmp = tempfile::tempdir().unwrap();
        let (remote, clone_a) = init_bare_remote_and_clone(tmp.path(), "device_a");
        let clone_b = clone_of(tmp.path(), &remote, "device_b");

        let coordinator_a = empty_coordinator_for(project_id);
        let member = Member::new_human(project_id, "Jordan");
        coordinator_a.storage.insert_member(&member).unwrap();
        coordinator_a.link_git_repo(project_id, clone_a.to_str().unwrap().into()).unwrap();
        coordinator_a.push_to_git(project_id).unwrap();

        let coordinator_b = empty_coordinator_for(project_id);
        coordinator_b.link_git_repo(project_id, clone_b.to_str().unwrap().into()).unwrap();
        coordinator_b.set_require_review_for_incoming_changes(project_id, true).unwrap();
        coordinator_b.pull_from_git(project_id).unwrap();

        let pending = coordinator_b.list_pending_sync_changes(project_id).unwrap();
        assert_eq!(pending.len(), 1);

        let manager = Member::new_human(project_id, "Alex");
        coordinator_b.storage.insert_member(&manager).unwrap();
        let role = coordinator_b.create_role(project_id, "Manager", vec![Permission::Approve]).unwrap();
        coordinator_b.assign_role(manager.id, role.id).unwrap();

        coordinator_b.deny_pending_sync_change(pending[0].id, manager.id, Some("not on our roster yet".into())).unwrap();

        assert!(coordinator_b.storage.get_member(member.id).unwrap().is_none(), "a denied change must never be applied");
        let decided = coordinator_b.list_pending_sync_changes(project_id).unwrap();
        assert_eq!(decided[0].status, PendingSyncChangeStatus::Denied);
        assert_eq!(decided[0].denial_reason.as_deref(), Some("not on our roster yet"));
        assert_eq!(decided[0].decided_by.as_deref(), Some("Alex"));
    }
}
