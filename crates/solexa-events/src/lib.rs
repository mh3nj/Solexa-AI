//! solexa-events, the immutable event log primitives for SOLEXA.
//!
//! Every meaningful thing that happens in the engine (a task changing
//! state, a specialist being assigned, a model being invoked, a
//! verification running) is represented as an `Event`. Events are never
//! mutated after creation, they are the append-only audit trail that
//! makes a run replayable (see rule: "every meaningful workflow must be
//! replayable").
//!
//! This crate defines the event shape and a small in-process `EventBus`
//! that fans a published event out to:
//!   1. any number of durable `EventSink`s (e.g. the SQLite storage layer)
//!   2. live subscribers (e.g. the Tauri UI, watching a task's execution
//!      trace update in real time)
//!
//! It deliberately knows nothing about tasks, specialists, or models,
//! those are domain concepts (see `solexa-domain`). This keeps the event
//! log a generic, reusable substrate rather than something coupled to
//! any one workflow.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Who (or what) caused an event to be emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorType {
    Human,
    Ai,
    System,
}

/// The catalogue of event types the Phase 1 core loop can emit.
///
/// This is intentionally a closed enum rather than a free-form string:
/// a fixed vocabulary is what makes the trace UI and any future
/// analytics/replay tooling reliable. Extend it as new subsystems
/// (scheduler, consensus, conflicts, ...) come online in later phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    RequestReceived,
    PlannerStarted,
    PlannerCompleted,
    TaskCreated,
    TaskAssigned,
    TaskAccepted,
    TaskBlocked,
    ContextPackaged,
    ResourceAcquired,
    ResourceReleased,
    ModelInvoked,
    ModelCompleted,
    ResultSubmitted,
    VerificationStarted,
    Verified,
    ModelReviewStarted,
    ModelReviewCompleted,
    RevisionRequested,
    Escalated,
    Approved,
    Rejected,
    TaskCompleted,
    TaskFailed,
    TaskPriorityChanged,
    /// Round 44: a task's deadline was set, changed, or cleared.
    TaskDeadlineChanged,
    KnowledgeItemAdded,
    KnowledgeItemArchived,
    DecisionRecorded,
    DecisionResolved,
    ConflictDetected,
    ConflictResolved,
    MemberRegistered,
    TeamRegistered,
    SpecialistRegistered,
    TeamMemberAdded,
    TeamMemberRemoved,
    TeamReparented,
    SpecialistImported,
    TeamImported,
    SavedToRegistry,
    InstalledFromRegistry,
    SpecialistUpdated,
    SpecialistActiveChanged,
    SpecialistReviewStarted,
    SpecialistReviewCompleted,
    SpecialistReviewSkipped,
    MemberUpdated,
    TeamUpdated,
    GitRepoLinked,
    GitRepoUnlinked,
    PushedToGit,
    PulledFromGit,
    MemberDeleted,
    MemberRestored,
    TeamDeleted,
    TeamRestored,
    ConflictSentToGithub,
    /// Round 39: one partial chunk of a model's streamed output. Unlike
    /// every other variant here, this one is never durably persisted,
    /// see `EventBus::publish_ephemeral`. It exists purely as a live
    /// signal for a trace view to render growing text as a model
    /// generates it, not as project history worth keeping forever.
    ModelTokenGenerated,
    /// Round 46: a project's `require_review_for_incoming_changes` gate
    /// was turned on or off.
    RequireReviewForIncomingChangesChanged,
    /// Round 55: a project's `enforce_approver_roles` setting was turned
    /// on or off.
    EnforceApproverRolesChanged,
    /// Round 60: the engine's resource budget (VRAM / RAM / concurrency) was
    /// resized while running. Payload: `previous` and `current`.
    ResourceBudgetChanged,
    /// Round 46: `pull_from_git` queued a non-conflicting incoming
    /// change for review instead of applying it immediately.
    PendingSyncChangeQueued,
    /// Round 46: a registered Member with `Permission::Approve`
    /// explicitly applied a queued incoming change.
    PendingSyncChangeApplied,
    /// Round 46: a registered Member with `Permission::Approve`
    /// explicitly denied a queued incoming change.
    PendingSyncChangeDenied,
    /// Round 46: a new named permission bundle (Role) was created.
    RoleCreated,
    /// Round 46: a Role's name or permission set was changed.
    RoleUpdated,
    /// Round 46: a Role was added to or removed from a Member.
    MemberRoleAssigned,
    MemberRoleUnassigned,
    /// Round 49: a task was cancelled, either while still waiting on
    /// hardware capacity (genuinely interrupted) or before it ever
    /// reached the resource wait.
    TaskCancelled,
}

/// An immutable record of something that happened in a project.
///
/// Fields mirror the shape called for in the SOLEXA event-system spec:
/// every event carries enough identity and correlation information to
/// reconstruct a full execution trace after the fact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub event_id: Uuid,
    pub project_id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub actor_id: String,
    pub actor_type: ActorType,
    pub event_type: EventType,
    pub payload: serde_json::Value,
    /// Groups every event belonging to the same top-level run (e.g. one
    /// user request and everything it caused) so the UI can pull a full
    /// trace with a single query.
    pub correlation_id: Uuid,
    /// The event that directly caused this one, if any. `None` marks the
    /// root of a trace (typically a `RequestReceived` event).
    pub causation_id: Option<Uuid>,
}

impl Event {
    pub fn new(
        project_id: Uuid,
        actor_id: impl Into<String>,
        actor_type: ActorType,
        event_type: EventType,
        payload: serde_json::Value,
        correlation_id: Uuid,
        causation_id: Option<Uuid>,
    ) -> Self {
        Self {
            event_id: Uuid::new_v4(),
            project_id,
            timestamp: Utc::now(),
            actor_id: actor_id.into(),
            actor_type,
            event_type,
            payload,
            correlation_id,
            causation_id,
        }
    }
}

/// Something that can durably persist events (e.g. the SQLite `events`
/// table). Kept as a trait so the event bus never depends on storage
/// directly, storage depends on events, not the other way around.
pub trait EventSink: Send + Sync {
    fn record(&self, event: &Event) -> anyhow::Result<()>;
}

/// Lets a shared `Arc<T>` be registered as an `EventSink` directly
/// (`bus.add_sink(Box::new(ArcSink(storage.clone())))`) instead of every
/// caller writing its own one-line wrapper. Lives here rather than in
/// e.g. solexa-storage because Rust's orphan rules only allow
/// `impl EventSink for Arc<T>` where `EventSink` is a local trait, and
/// this is the one crate where that's true.
pub struct ArcSink<T: EventSink>(pub std::sync::Arc<T>);

impl<T: EventSink> EventSink for ArcSink<T> {
    fn record(&self, event: &Event) -> anyhow::Result<()> {
        self.0.record(event)
    }
}

/// Fan-out publisher: every published event is (a) handed to every
/// registered durable sink, in order, and (b) broadcast to any live
/// subscribers. A sink failing to persist an event is treated as fatal
/// to the publish call, silently dropping part of the audit trail is
/// worse than surfacing the error to the caller.
pub struct EventBus {
    sinks: Vec<Box<dyn EventSink>>,
    live: tokio::sync::broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new() -> Self {
        let (live, _rx) = tokio::sync::broadcast::channel(1024);
        Self {
            sinks: Vec::new(),
            live,
        }
    }

    pub fn add_sink(&mut self, sink: Box<dyn EventSink>) {
        self.sinks.push(sink);
    }

    /// Subscribe to a live stream of every event published from this
    /// point forward (used by the Tauri app to push execution-trace
    /// updates to the UI as they happen).
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Event> {
        self.live.subscribe()
    }

    pub fn publish(&self, event: Event) -> anyhow::Result<()> {
        for sink in &self.sinks {
            sink.record(&event)?;
        }
        // A live-stream send failing just means nobody is currently
        // listening (e.g. no UI window open yet), that's fine.
        let _ = self.live.send(event);
        Ok(())
    }

    /// Round 39: broadcasts to live subscribers only, every durable
    /// `EventSink` (SQLite included) is deliberately skipped.
    ///
    /// The doc comment at the top of this file already draws the
    /// distinction this method acts on: a published event fans out to
    /// (1) durable sinks and (2) live subscribers. Until now every
    /// event took both paths. Streamed model tokens are the first
    /// event kind that genuinely shouldn't: a single generation can
    /// produce hundreds of chunks, each carrying a fragment of text
    /// with essentially no standalone audit value, the *complete*
    /// text is already recorded durably via the existing
    /// `ModelCompleted`/`ResultSubmitted` events once generation
    /// finishes. Writing every chunk to SQLite as its own permanent,
    /// replayable Event would bloat the audit trail with noise the
    /// spec's own "chat should not become the database" principle
    /// warns against, this is that same principle applied to raw
    /// token output instead of chat.
    ///
    /// Because this never reaches a sink, an ephemeral event is
    /// invisible to anything that reads durable history
    /// (`events_for_correlation`, a reopened task's trace, replay),
    /// deliberately. It only ever exists for whoever is watching live.
    pub fn publish_ephemeral(&self, event: Event) -> anyhow::Result<()> {
        let _ = self.live.send(event);
        Ok(())
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct RecordingSink(Arc<Mutex<Vec<Event>>>);
    impl EventSink for RecordingSink {
        fn record(&self, event: &Event) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(event.clone());
            Ok(())
        }
    }

    #[test]
    fn publish_reaches_every_sink() {
        let mut bus = EventBus::new();
        let captured = Arc::new(Mutex::new(Vec::new()));
        bus.add_sink(Box::new(RecordingSink(captured.clone())));

        let project_id = Uuid::new_v4();
        let correlation_id = Uuid::new_v4();
        bus.publish(Event::new(
            project_id,
            "planner",
            ActorType::System,
            EventType::PlannerStarted,
            serde_json::json!({"note": "test"}),
            correlation_id,
            None,
        ))
        .unwrap();

        assert_eq!(captured.lock().unwrap().len(), 1);
        assert_eq!(captured.lock().unwrap()[0].event_type, EventType::PlannerStarted);
    }

    #[tokio::test]
    async fn ephemeral_publish_reaches_live_subscribers_but_never_a_durable_sink() {
        let mut bus = EventBus::new();
        let captured = Arc::new(Mutex::new(Vec::new()));
        bus.add_sink(Box::new(RecordingSink(captured.clone())));
        let mut live = bus.subscribe();

        let project_id = Uuid::new_v4();
        let correlation_id = Uuid::new_v4();
        bus.publish_ephemeral(Event::new(
            project_id,
            "coordinator",
            ActorType::System,
            EventType::ModelTokenGenerated,
            serde_json::json!({"chunk": "hel"}),
            correlation_id,
            None,
        ))
        .unwrap();

        // Reached the live subscriber...
        let received = live.recv().await.unwrap();
        assert_eq!(received.event_type, EventType::ModelTokenGenerated);
        // ...but never the durable sink, unlike an ordinary `publish`.
        assert!(captured.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn live_subscribers_receive_published_events() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();

        let project_id = Uuid::new_v4();
        let correlation_id = Uuid::new_v4();
        bus.publish(Event::new(
            project_id,
            "planner",
            ActorType::System,
            EventType::PlannerStarted,
            serde_json::json!({}),
            correlation_id,
            None,
        ))
        .unwrap();

        let received = rx.recv().await.unwrap();
        assert_eq!(received.event_type, EventType::PlannerStarted);
    }
}
