//! solexa-resources, the resource-aware scheduler's foundation (Phase
//! 3 of the roadmap: "the scheduler must be hardware-aware... the
//! system must never assume that local hardware is unlimited").
//!
//! Originally written as deliberately *not* a full task scheduler,
//! priority queues, deadlines, and cancellation were all named as "the
//! rest of Phase 3 and beyond." Since then: priority (round 36),
//! deadlines (round 44), and, for the wait-on-budget phase only,
//! cancellation (round 49, `acquire_cancellable`/`cancel`) have landed.
//! It remains the one piece every scheduling feature sits on top of: a
//! `ResourceManager` that knows the local machine's
//! VRAM/RAM budget, tracks what's currently reserved against it, and
//! makes callers wait (rather than oversubscribing the hardware) when a
//! model's footprint doesn't currently fit.
//!
//! Concurrency is capped two ways at once, matching the spec's "the
//! goal is maximum useful computation, not maximum concurrent models":
//! a hard cap on simultaneous executions (a semaphore permit), and a
//! memory budget that can refuse a reservation even when a permit is
//! free (e.g. one large model already using most of the VRAM budget).

use solexa_domain::{ModelMetadata, Uuid};
use std::collections::HashSet;
use std::sync::Mutex;
use tokio::sync::{Notify, Semaphore, SemaphorePermit};

#[derive(Debug, Clone, Copy)]
pub struct ResourceBudget {
    pub vram_mb: u32,
    pub ram_mb: u32,
    /// Hard cap on simultaneous model executions, independent of
    /// memory, this is "maximum useful computation" acting as a
    /// backstop even when the memory numbers alone would allow more.
    pub max_concurrent: usize,
}

impl ResourceBudget {
    /// Round 61: the one definition of an acceptable budget, used by
    /// `set_budget` and by whoever loads a saved budget at startup, so a
    /// hand-edited or corrupt saved value can never be applied. `max_concurrent`
    /// must be 1..=`MAX_CONCURRENT_LIMIT`; zero memory is allowed (CPU-only).
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.max_concurrent == 0 {
            return Err("max_concurrent must be at least 1");
        }
        if self.max_concurrent > MAX_CONCURRENT_LIMIT {
            return Err("max_concurrent is above the supported limit");
        }
        Ok(())
    }
}

impl Default for ResourceBudget {
    /// A generous default (nothing in this repo's mock models comes
    /// close to it) so existing single/few-task flows never contend by
    /// accident, contention is something a caller opts into with a
    /// tighter budget, e.g. in a test that wants to prove queuing
    /// happens.
    fn default() -> Self {
        Self {
            vram_mb: 24_000,
            ram_mb: 64_000,
            max_concurrent: 4,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ResourceSnapshot {
    pub used_vram_mb: u32,
    pub used_ram_mb: u32,
    pub running: usize,
    /// Adopted from a parallel round: how many callers are currently
    /// blocked in `acquire`, waiting on capacity `running` doesn't have
    /// room for yet. Exposed so a live "Resource Center" view can show
    /// contention forming (e.g. "4 running, 2 queued") rather than only
    /// ever showing a snapshot of what's already fit, `running` alone
    /// can't distinguish "nothing else wants to run" from "the budget
    /// is the bottleneck," which is exactly the distinction the spec's
    /// hardware-aware scheduler is supposed to make visible.
    pub queued: usize,
}

struct ResourceState {
    /// Round 60: the budget lives in the state (not as an immutable field) so
    /// it can be resized while work is running, see `set_budget`.
    budget: ResourceBudget,
    /// Round 60: concurrency permits that currently exist in the semaphore
    /// (held + available). The semaphore can add permits but never take back
    /// ones that are held, so shrinking the cap below what is running is paid
    /// off lazily: `permit_debt` permits are *forgotten* instead of returned as
    /// their holders finish. The effective cap is `permits_total - permit_debt`.
    permits_total: usize,
    permit_debt: usize,
    used_vram_mb: u32,
    used_ram_mb: u32,
    /// Round 36: everyone currently blocked in `acquire`, so capacity
    /// can be handed to the highest-priority waiter instead of whoever
    /// the runtime happens to wake first. Small by construction, it
    /// holds only tasks actively waiting on hardware, bounded by
    /// `max_concurrent` plus however many are queued behind it.
    waiters: Vec<Waiter>,
    next_ticket: u64,
    /// Round 49: owners (a caller-supplied correlation id, in
    /// practice, a `Task` id) whose `acquire_cancellable` wait should
    /// end in `Err(AcquireError::Cancelled)` instead of ever getting a guard.
    /// Set by `cancel()`, consumed (removed) the moment the matching
    /// wait loop next wakes and checks it. An id that was never
    /// actually waiting (or already finished) sitting here harmlessly
    /// does nothing, this module's own doc comment named
    /// "cancellation" as future work since Phase 3; this is that,
    /// scoped to the wait-on-budget phase specifically (see
    /// `acquire_cancellable`'s own doc comment for the full scope
    /// boundary, a task already past `acquire` and generating isn't
    /// covered by this).
    cancelled: HashSet<Uuid>,
}

#[derive(Clone, Copy)]
struct Waiter {
    ticket: u64,
    priority: u8,
    /// Round 60: what this waiter needs, so a budget shrink can refuse to
    /// strand it (see `set_budget`).
    need_vram_mb: u32,
    need_ram_mb: u32,
    /// Round 44: when this waiter would like to have started by.
    /// `Instant`, not `DateTime`, the caller (coordinator) converts a
    /// task's own `DateTime<Utc>` deadline into a duration-from-now
    /// before calling `acquire`, since a monotonic clock is what
    /// ordering comparisons need and wall-clock time is what a human
    /// sets, converting once at the boundary keeps that distinction
    /// from leaking into this module's own comparisons.
    deadline: Option<std::time::Instant>,
}

impl ResourceState {
    /// The waiter entitled to the next free capacity. Three-level sort,
    /// each level only breaking ties left by the one before it:
    /// 1. **Priority** higher wins, strictly (round 36's own
    ///    deliberate choice: a lower-priority waiter that would fit
    ///    never jumps ahead of a higher-priority one that doesn't yet,
    ///    even when the higher one has no deadline pressure at all).
    ///    Round 44 doesn't touch this tier, a deadline is not a
    ///    priority override, it only matters *within* one.
    /// 2. **Deadline urgency** among equal priority, the waiter with
    ///    the *sooner* deadline goes first; a waiter with no deadline
    ///    at all is treated as *less* urgent than any waiter that has
    ///    one, never as "due immediately." `Option<Reverse<Instant>>`
    ///    gets this for free from `Option`'s own derived `Ord` (`None <
    ///    Some(_)` always) composed with `Reverse` (so the *earlier*
    ///    `Instant` inside a `Some` compares as the *greater* value,
    ///    i.e. wins under `max_by_key`).
    /// 3. **Arrival order** FIFO, same as before round 44: equal
    ///    priority and (both missing, or numerically equal) deadlines
    ///    keep the exact tie-break every prior round already relied on.
    fn front(&self) -> Option<Waiter> {
        self.waiters
            .iter()
            .copied()
            .max_by_key(|w| (w.priority, w.deadline.map(std::cmp::Reverse), std::cmp::Reverse(w.ticket)))
    }

    fn remove(&mut self, ticket: u64) {
        self.waiters.retain(|w| w.ticket != ticket);
    }
}

/// Why `ResourceManager::acquire_cancellable` ended without a guard.
/// Hand-written `Error` impls, this crate has never depended on
/// `anyhow`/`thiserror` and two small error types don't justify starting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquireError {
    /// Round 49: `cancel(owner)` ended the wait instead of capacity freeing up.
    Cancelled,
    /// Round 60: the budget was resized so the model can never fit it any more
    /// (VRAM or RAM now smaller than the model's own footprint). Without this a
    /// caller the resize couldn't see, one still waiting on a concurrency
    /// permit, would wait forever.
    ExceedsBudget,
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcquireError::Cancelled => write!(f, "the wait for hardware capacity was cancelled"),
            AcquireError::ExceedsBudget => write!(f, "the model no longer fits the resource budget"),
        }
    }
}

impl std::error::Error for AcquireError {}

/// Round 60: why `ResourceManager::set_budget` refused a resize. A refused
/// resize changes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeError {
    Invalid(&'static str),
    /// `waiting` callers are queued for a model that would no longer fit the
    /// proposed VRAM/RAM budget, they would wait forever.
    WouldStrand { waiting: usize },
}

impl std::fmt::Display for ResizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResizeError::Invalid(why) => write!(f, "invalid resource budget: {why}"),
            ResizeError::WouldStrand { waiting } => write!(
                f,
                "{waiting} queued task(s) need more memory than the proposed budget, cancel them or choose a larger budget"
            ),
        }
    }
}

impl std::error::Error for ResizeError {}

/// Upper bound on `max_concurrent` accepted by `set_budget`, a sanity cap (a
/// typo such as 40000 should not become a real semaphore size), far above any
/// useful number of simultaneous local model runs.
pub const MAX_CONCURRENT_LIMIT: usize = 256;

/// A concurrency permit that honors `permit_debt`: when the cap was shrunk
/// below what is running, returning a permit *forgets* it instead.
struct ManagedPermit<'a> {
    manager: &'a ResourceManager,
    permit: Option<SemaphorePermit<'a>>,
}

impl Drop for ManagedPermit<'_> {
    fn drop(&mut self) {
        let Some(permit) = self.permit.take() else { return };
        let mut state = self.manager.state.lock().unwrap();
        if state.permit_debt > 0 {
            state.permit_debt -= 1;
            state.permits_total -= 1;
            permit.forget();
        }
        // else: `permit` drops here and returns to the semaphore
    }
}

pub struct ResourceManager {
    state: Mutex<ResourceState>,
    /// Notified every time a reservation is released, so a waiting
    /// `acquire` can re-check whether it now fits instead of polling.
    notify: Notify,
    concurrency: Semaphore,
}

/// Held for the duration of one model execution. Releases its VRAM/RAM
/// reservation and concurrency permit when dropped (typically by
/// letting it go out of scope right after the model call returns),
/// but callers that want the release to be a visible, timestamped
/// `Event` should drop it explicitly and emit that event themselves
/// immediately after, rather than relying on `Drop` timing (see
/// `solexa-coordinator::execute_task`).
pub struct ResourceGuard<'a> {
    manager: &'a ResourceManager,
    vram_mb: u32,
    ram_mb: u32,
    _permit: ManagedPermit<'a>,
}

impl Drop for ResourceGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.manager.state.lock().unwrap();
        state.used_vram_mb = state.used_vram_mb.saturating_sub(self.vram_mb);
        state.used_ram_mb = state.used_ram_mb.saturating_sub(self.ram_mb);
        drop(state);
        self.manager.notify.notify_waiters();
    }
}

/// A caller's place in the priority queue, removed on drop so a
/// cancelled `acquire` can never wedge the queue (see `acquire`).
struct WaitTicket<'a> {
    manager: &'a ResourceManager,
    id: u64,
    armed: bool,
}

impl<'a> WaitTicket<'a> {
    fn register(manager: &'a ResourceManager, priority: u8, deadline: Option<std::time::Instant>, model: &ModelMetadata) -> Self {
        let mut state = manager.state.lock().unwrap();
        let id = state.next_ticket;
        state.next_ticket += 1;
        state.waiters.push(Waiter { ticket: id, priority, need_vram_mb: model.estimated_vram_mb, need_ram_mb: model.estimated_ram_mb, deadline });
        drop(state);
        // A new waiter can outrank whoever is currently at the front,
        // so everyone still blocked needs to re-evaluate whose turn it
        // is, otherwise a lower-priority waiter that already lost a
        // race would never be reconsidered.
        manager.notify.notify_waiters();
        Self { manager, id, armed: true }
    }

    /// Called once the ticket has already been removed under the same
    /// lock that consumed the capacity, so `Drop` doesn't remove it
    /// twice or notify pointlessly.
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for WaitTicket<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut state = self.manager.state.lock().unwrap();
        state.remove(self.id);
        drop(state);
        // Our departure may promote someone else to the front.
        self.manager.notify.notify_waiters();
    }
}

impl ResourceManager {
    pub fn new(budget: ResourceBudget) -> Self {
        Self {
            state: Mutex::new(ResourceState {
                budget,
                permits_total: budget.max_concurrent,
                permit_debt: 0,
                used_vram_mb: 0,
                used_ram_mb: 0,
                waiters: Vec::new(),
                next_ticket: 0,
                cancelled: HashSet::new(),
            }),
            notify: Notify::new(),
            concurrency: Semaphore::new(budget.max_concurrent),
        }
    }

    pub fn budget(&self) -> ResourceBudget {
        self.state.lock().unwrap().budget
    }

    /// Round 60: resizes the budget while work is running, returning the
    /// budget it replaced. Atomic: a refused resize changes nothing.
    ///
    /// * **VRAM/RAM** take effect at once. Shrinking below what is currently
    ///   reserved never evicts running work, it just gates new work until
    ///   enough finishes. Growing wakes waiters immediately.
    /// * **`max_concurrent`** grows immediately; shrinking removes free permits
    ///   at once and the rest as running guards finish (`permit_debt`).
    /// * A shrink is **refused** (`WouldStrand`) if any caller already queued
    ///   needs more memory than the proposed budget, it would wait forever.
    ///   Callers not yet in the queue (still waiting for a concurrency permit)
    ///   can't be seen here; they get `AcquireError::ExceedsBudget` instead.
    /// * `max_concurrent` must be 1..=`MAX_CONCURRENT_LIMIT`. Zero memory is
    ///   allowed (a CPU-only setup), the strand check protects waiters.
    pub fn set_budget(&self, new: ResourceBudget) -> Result<ResourceBudget, ResizeError> {
        new.validate().map_err(ResizeError::Invalid)?;
        let mut state = self.state.lock().unwrap();
        let stranded = state
            .waiters
            .iter()
            .filter(|w| w.need_vram_mb > new.vram_mb || w.need_ram_mb > new.ram_mb)
            .count();
        if stranded > 0 {
            return Err(ResizeError::WouldStrand { waiting: stranded });
        }

        let previous = state.budget;
        state.budget = new;

        let effective = state.permits_total - state.permit_debt;
        if new.max_concurrent > effective {
            let need = new.max_concurrent - effective;
            let cancelled_debt = need.min(state.permit_debt);
            state.permit_debt -= cancelled_debt;
            let add = need - cancelled_debt;
            if add > 0 {
                self.concurrency.add_permits(add);
                state.permits_total += add;
            }
        } else if new.max_concurrent < effective {
            let mut shrink = effective - new.max_concurrent;
            while shrink > 0 {
                match self.concurrency.try_acquire() {
                    Ok(free) => {
                        free.forget();
                        state.permits_total -= 1;
                        shrink -= 1;
                    }
                    Err(_) => break,
                }
            }
            state.permit_debt += shrink;
        }
        drop(state);
        // A bigger budget may now fit a waiter; a smaller one lets
        // `ExceedsBudget` callers notice. Either way, everyone re-checks.
        self.notify.notify_waiters();
        Ok(previous)
    }

    pub fn snapshot(&self) -> ResourceSnapshot {
        let state = self.state.lock().unwrap();
        ResourceSnapshot {
            used_vram_mb: state.used_vram_mb,
            used_ram_mb: state.used_ram_mb,
            running: state.permits_total - self.concurrency.available_permits(),
            // Adopted from a parallel round: `waiters` only ever holds
            // callers genuinely blocked in `acquire` (registered by
            // `WaitTicket::register`, removed on drop, a cancelled
            // wait can't leak an entry), so its length is exactly the
            // current queue depth, no separate bookkeeping needed.
            queued: state.waiters.len(),
        }
    }

    /// Waits until both a concurrency permit and enough VRAM/RAM budget
    /// are available for `model`, reserves them, and returns a guard
    /// that releases the reservation when dropped. A model whose
    /// footprint alone exceeds the *entire* budget will wait forever,
    /// that is treated as a configuration error the caller should catch
    /// up front (`fits_in_budget`), not something this method can
    /// recover from silently.
    ///
    /// Round 36: `priority` (1–10, matching `Task.priority`) decides who
    /// gets freed capacity when several callers are waiting at once.
    /// Before this, whoever the runtime happened to wake first won, so
    /// a priority-10 task could sit behind a priority-1 one, the
    /// stored priority genuinely changed nothing.
    ///
    /// Strict priority, deliberately: if the highest-priority waiter
    /// doesn't fit yet, lower-priority waiters that *would* fit still
    /// wait. That head-of-line blocking is the point, without it, a
    /// steady stream of small low-priority models could starve one
    /// large high-priority model indefinitely. It's bounded in
    /// practice, since running work always finishes and releases its
    /// reservation.
    ///
    /// Round 44: `deadline`, if given, breaks ties *within* a priority
    /// tier, see `ResourceState::front`'s own doc comment for the
    /// exact three-level rule. `None` is the common case (most work has
    /// no explicit deadline) and behaves exactly as every round-36 test
    /// already expects: pure FIFO among equal-priority waiters.
    pub async fn acquire<'a>(
        &'a self,
        model: &ModelMetadata,
        priority: u8,
        deadline: Option<std::time::Instant>,
    ) -> ResourceGuard<'a> {
        self.acquire_cancellable(model, priority, deadline, None)
            .await
            .expect("acquire() never passes an owner, so acquire_cancellable can never return Err for it")
    }

    /// Round 49: the cancellable form of `acquire`, closing the gap
    /// this module's own doc comment has named as future work since
    /// Phase 3. `owner` is a caller-supplied correlation id (in
    /// practice, a `Task` id), pass `None` for a wait nothing should
    /// ever be able to cancel (that's exactly what `acquire` itself
    /// does above). While `owner` is `Some`, a concurrent call to
    /// `cancel(owner)` ends this wait with `Err(AcquireError::Cancelled)`
    /// instead of a guard, cleanly releasing this wait's place in the
    /// queue (and its concurrency permit) the same way a dropped
    /// future already does.
    ///
    /// Scope, named plainly: this only ever interrupts the
    /// *wait-on-budget* phase, a caller still queued behind
    /// insufficient VRAM/RAM, exactly what `ResourceSnapshot::queued`
    /// counts. A caller that has already been handed a `ResourceGuard`
    /// and is generating is past this method entirely; interrupting
    /// genuinely in-flight generation is a different, larger problem
    /// (no dispatcher loop exists to check a flag mid-HTTP-call) and
    /// is deliberately not attempted here, see `Coordinator::cancel_task`'s
    /// own doc comment for the full boundary this draws.
    pub async fn acquire_cancellable<'a>(
        &'a self,
        model: &ModelMetadata,
        priority: u8,
        deadline: Option<std::time::Instant>,
        owner: Option<Uuid>,
    ) -> Result<ResourceGuard<'a>, AcquireError> {
        let permit = ManagedPermit {
            manager: self,
            permit: Some(
                self.concurrency
                    .acquire()
                    .await
                    .expect("ResourceManager's semaphore is never closed"),
            ),
        };

        // Registered as its own guard so that a caller whose future is
        // dropped mid-wait (cancellation, a timeout, a failing sibling
        // in a `join_all`) can't leave a ticket behind, a stale ticket
        // at the front of the queue would block every other waiter
        // forever.
        let ticket = WaitTicket::register(self, priority, deadline, model);

        loop {
            // Register interest *before* checking the condition: if we
            // checked first and only then subscribed, a release that
            // happens in between would notify no one and we'd wait
            // forever. This ordering is required by `Notify`'s
            // documented semantics, not just a style preference.
            let notified = self.notify.notified();

            {
                let mut state = self.state.lock().unwrap();
                if let Some(owner_id) = owner {
                    if state.cancelled.remove(&owner_id) {
                        state.remove(ticket.id);
                        drop(state);
                        ticket.disarm();
                        return Err(AcquireError::Cancelled);
                    }
                }
                // Round 60: the budget can shrink while we wait. A model larger
                // than the *whole* budget can never run, fail loudly rather
                // than waiting forever (the up-front `fits_in_budget` check
                // only saw the budget as it was then).
                if model.estimated_vram_mb > state.budget.vram_mb || model.estimated_ram_mb > state.budget.ram_mb {
                    state.remove(ticket.id);
                    drop(state);
                    ticket.disarm();
                    return Err(AcquireError::ExceedsBudget);
                }
                let fits = state.used_vram_mb + model.estimated_vram_mb <= state.budget.vram_mb
                    && state.used_ram_mb + model.estimated_ram_mb <= state.budget.ram_mb;
                // `front()` can only be None if our own ticket vanished,
                // which `WaitTicket` makes impossible while we hold it.
                let our_turn = state.front().map(|w| w.ticket == ticket.id).unwrap_or(false);
                if fits && our_turn {
                    state.used_vram_mb += model.estimated_vram_mb;
                    state.used_ram_mb += model.estimated_ram_mb;
                    state.remove(ticket.id);
                    drop(state);
                    ticket.disarm();
                    return Ok(ResourceGuard {
                        manager: self,
                        vram_mb: model.estimated_vram_mb,
                        ram_mb: model.estimated_ram_mb,
                        _permit: permit,
                    });
                }
            }

            notified.await;
        }
    }

    /// Ends a matching in-progress `acquire_cancellable(..., Some(owner))`
    /// wait, if one is currently blocked, a no-op, not an error, if
    /// `owner` isn't (or is no longer) waiting on anything, the same
    /// "idempotent, not an error" convention `assign_role` and friends
    /// already use elsewhere in this project for this shape of action.
    pub fn cancel(&self, owner: Uuid) {
        let mut state = self.state.lock().unwrap();
        state.cancelled.insert(owner);
        drop(state);
        self.notify.notify_waiters();
    }

    /// Round 54: drops a pending cancel marker for `owner`. `cancel`
    /// records the marker so a wait that is *about* to start sees it, and
    /// `acquire_cancellable` consumes it when it does, but a marker for an
    /// owner that was never waiting (already generating, or finished) is
    /// consumed by nothing, so the caller that knows the owner's work has
    /// ended calls this. Idempotent; a no-op if there is no marker.
    pub fn forget_cancel(&self, owner: Uuid) {
        self.state.lock().unwrap().cancelled.remove(&owner);
    }

    /// Round 54: how many cancel markers are currently outstanding, for
    /// tests and diagnostics (a steadily growing number is the leak
    /// `forget_cancel` exists to prevent).
    pub fn pending_cancellations(&self) -> usize {
        self.state.lock().unwrap().cancelled.len()
    }

    /// Whether `model` could ever be satisfied by this budget, even
    /// with nothing else running, lets a caller fail fast with a clear
    /// "this model doesn't fit on this machine" instead of hanging in
    /// `acquire` forever.
    pub fn fits_in_budget(&self, model: &ModelMetadata) -> bool {
        let state = self.state.lock().unwrap();
        model.estimated_vram_mb <= state.budget.vram_mb && model.estimated_ram_mb <= state.budget.ram_mb
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solexa_domain::{Modality, Uuid};
    use std::sync::Arc;
    use std::time::Duration;

    fn model(vram_mb: u32) -> ModelMetadata {
        ModelMetadata {
            id: Uuid::new_v4(),
            name: "test-model".into(),
            provider: "mock".into(),
            modalities: vec![Modality::Text],
            context_capacity_tokens: 4096,
            parameter_count_billions: None,
            quantization: None,
            estimated_vram_mb: vram_mb,
            estimated_ram_mb: 1_000,
            tool_support: false,
            structured_output_support: false,
            streaming_support: false,
            local: true,
            license: None,
        }
    }

    #[tokio::test]
    async fn acquire_reserves_and_release_frees_the_budget() {
        let manager = ResourceManager::new(ResourceBudget { vram_mb: 8_000, ram_mb: 16_000, max_concurrent: 4 });
        let m = model(6_000);

        let guard = manager.acquire(&m, 5, None).await;
        assert_eq!(manager.snapshot().used_vram_mb, 6_000);

        drop(guard);
        assert_eq!(manager.snapshot().used_vram_mb, 0);
    }

    #[tokio::test]
    async fn a_model_that_does_not_fit_makes_the_caller_wait_for_a_release() {
        let manager = Arc::new(ResourceManager::new(ResourceBudget {
            vram_mb: 6_000,
            ram_mb: 16_000,
            max_concurrent: 4,
        }));
        let big = model(6_000);
        let holder = manager.acquire(&big, 5, None).await; // consumes the entire VRAM budget

        let waiter_manager = manager.clone();
        let waiter = tokio::spawn(async move {
            let m = model(6_000);
            let _guard = waiter_manager.acquire(&m, 5, None).await;
            "acquired"
        });

        // The waiter must NOT complete while `holder` is still held,
        // give it a moment to (wrongly) succeed if the budget check is
        // broken, then confirm it's still pending.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished(), "waiter acquired before the budget was released");

        drop(holder);
        let result = tokio::time::timeout(Duration::from_millis(500), waiter)
            .await
            .expect("waiter should complete shortly after release")
            .unwrap();
        assert_eq!(result, "acquired");
    }

    #[tokio::test]
    async fn max_concurrent_caps_simultaneous_guards_even_with_ample_memory() {
        let manager = ResourceManager::new(ResourceBudget { vram_mb: 100_000, ram_mb: 100_000, max_concurrent: 1 });
        let m = model(10);

        let first = manager.acquire(&m, 5, None).await;
        assert_eq!(manager.snapshot().running, 1);

        let second_fut = manager.acquire(&m, 5, None);
        tokio::pin!(second_fut);
        // With max_concurrent = 1 and `first` still held, polling once
        // must not resolve the second acquire immediately.
        assert!(futures_poll_pending(&mut second_fut));

        drop(first);
        let _second = second_fut.await;
        assert_eq!(manager.snapshot().running, 1);
    }

    /// Polls a future exactly once and reports whether it was still
    /// Pending, without pulling in a dev-dependency on `futures` just
    /// for this one check.
    fn futures_poll_pending<F: std::future::Future + Unpin>(fut: &mut F) -> bool {
        use std::task::{Context, Poll};
        struct NoopWaker;
        impl std::task::Wake for NoopWaker {
            fn wake(self: Arc<Self>) {}
        }
        let waker = std::task::Waker::from(Arc::new(NoopWaker));
        let mut cx = Context::from_waker(&waker);
        matches!(std::pin::Pin::new(fut).poll(&mut cx), Poll::Pending)
    }

    // ---- round 36: priority actually decides who goes next ----------
    //
    // Before this round `acquire` was first-come-first-served: whoever
    // the runtime happened to wake when capacity freed won the race, so
    // `Task.priority` (stored and settable since an early round) changed
    // nothing at all. These tests are what make that stop being true.

    #[tokio::test]
    async fn freed_capacity_goes_to_the_highest_priority_waiter_not_the_first_one_to_ask() {
        let manager = Arc::new(ResourceManager::new(ResourceBudget { vram_mb: 8_000, ram_mb: 16_000, max_concurrent: 8 }));
        let m = model(6_000);

        // Fill the budget so everything below genuinely has to wait.
        let holder = manager.acquire(&m, 5, None).await;

        let order = Arc::new(Mutex::new(Vec::new()));

        // Three low-priority waiters register BEFORE the high-priority
        // one. Under first-come-first-served the high waiter would have
        // to win a race against three earlier arrivals to come out
        // first; under priority ordering it is guaranteed to. An
        // earlier version of this test used a single low waiter and
        // passed even with the priority check disabled, it was proving
        // nothing. Three makes the two behaviours genuinely different.
        let mut handles = Vec::new();
        for (label, priority) in [("low-1", 1u8), ("low-2", 1), ("low-3", 1), ("high", 9)] {
            let manager = manager.clone();
            let order = order.clone();
            let m = m.clone();
            handles.push(tokio::spawn(async move {
                let guard = manager.acquire(&m, priority, None).await;
                order.lock().unwrap().push(label);
                // Hold briefly so the next waiter can't overlap and make
                // the recorded order ambiguous.
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                drop(guard);
            }));
            // Let this waiter actually reach `acquire` and register
            // before the next one is spawned, so arrival order is
            // genuinely lows-then-high rather than a race.
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }

        drop(holder);
        for h in handles {
            h.await.unwrap();
        }

        let order = order.lock().unwrap();
        assert_eq!(order[0], "high", "the priority-9 waiter must go first despite arriving last; got {order:?}");
        // And the lows still keep their own arrival order behind it.
        assert_eq!(&order[1..], &["low-1", "low-2", "low-3"], "got {order:?}");
    }

    #[tokio::test]
    async fn equal_priority_waiters_keep_their_arrival_order() {
        // Priority ordering must not reshuffle tasks that share a
        // priority, otherwise the common case (everything at the
        // default) would become non-deterministic, which is the
        // opposite of what this round is for.
        let manager = Arc::new(ResourceManager::new(ResourceBudget { vram_mb: 8_000, ram_mb: 16_000, max_concurrent: 8 }));
        let m = model(6_000);
        let holder = manager.acquire(&m, 5, None).await;

        let order = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for label in ["first", "second", "third"] {
            let manager = manager.clone();
            let order = order.clone();
            let m = m.clone();
            handles.push(tokio::spawn(async move {
                let guard = manager.acquire(&m, 5, None).await;
                order.lock().unwrap().push(label);
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                drop(guard);
            }));
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }

        drop(holder);
        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(*order.lock().unwrap(), vec!["first", "second", "third"]);
    }

    // ---- round 44: deadline breaks ties within a priority tier ------

    #[tokio::test]
    async fn an_earlier_deadline_wins_the_tie_within_the_same_priority_tier() {
        let manager = Arc::new(ResourceManager::new(ResourceBudget { vram_mb: 8_000, ram_mb: 16_000, max_concurrent: 8 }));
        let m = model(6_000);
        let holder = manager.acquire(&m, 5, None).await;

        let now = std::time::Instant::now();
        let order = Arc::new(Mutex::new(Vec::new()));
        // Registered out of deadline order (soonest last) so a pass
        // here can only be explained by real deadline comparison, not
        // by coincidentally matching arrival order too.
        let waiters: Vec<(&str, std::time::Instant)> = vec![
            ("due-in-30s", now + Duration::from_secs(30)),
            ("due-in-10s", now + Duration::from_secs(10)),
            ("due-in-20s", now + Duration::from_secs(20)),
        ];
        let mut handles = Vec::new();
        for (label, deadline) in waiters {
            let manager = manager.clone();
            let order = order.clone();
            let m = m.clone();
            handles.push(tokio::spawn(async move {
                let guard = manager.acquire(&m, 5, Some(deadline)).await;
                order.lock().unwrap().push(label);
                tokio::time::sleep(Duration::from_millis(20)).await;
                drop(guard);
            }));
            tokio::time::sleep(Duration::from_millis(30)).await;
        }

        drop(holder);
        for h in handles {
            h.await.unwrap();
        }

        let order = order.lock().unwrap();
        assert_eq!(*order, vec!["due-in-10s", "due-in-20s", "due-in-30s"], "got {order:?}");
    }

    #[tokio::test]
    async fn a_waiter_with_any_deadline_outranks_one_with_none_at_equal_priority() {
        // A missing deadline must read as "less urgent than anything
        // with a real one", never as "due right now," which would
        // perversely let unscheduled work jump every scheduled task.
        let manager = Arc::new(ResourceManager::new(ResourceBudget { vram_mb: 8_000, ram_mb: 16_000, max_concurrent: 8 }));
        let m = model(6_000);
        let holder = manager.acquire(&m, 5, None).await;

        let order = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        // "no-deadline" registers first, "has-deadline" second, plain
        // FIFO would put "no-deadline" first; deadline-awareness must
        // override that.
        for (label, deadline) in [("no-deadline", None), ("has-deadline", Some(std::time::Instant::now() + Duration::from_secs(5)))] {
            let manager = manager.clone();
            let order = order.clone();
            let m = m.clone();
            handles.push(tokio::spawn(async move {
                let guard = manager.acquire(&m, 5, deadline).await;
                order.lock().unwrap().push(label);
                tokio::time::sleep(Duration::from_millis(20)).await;
                drop(guard);
            }));
            tokio::time::sleep(Duration::from_millis(30)).await;
        }

        drop(holder);
        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(*order.lock().unwrap(), vec!["has-deadline", "no-deadline"]);
    }

    #[tokio::test]
    async fn a_deadline_never_overrides_priority_even_when_imminent() {
        // Locks in round 36's deliberate choice (see `acquire`'s own
        // doc comment): priority is the outer sort key, deadline only
        // breaks ties inside it. An urgent deadline on low-priority
        // work must not let it cut ahead of higher-priority work that
        // has none at all.
        let manager = Arc::new(ResourceManager::new(ResourceBudget { vram_mb: 8_000, ram_mb: 16_000, max_concurrent: 8 }));
        let m = model(6_000);
        let holder = manager.acquire(&m, 5, None).await;

        let order = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        let urgent_low_priority = (
            "low-priority-imminent-deadline",
            1u8,
            Some(std::time::Instant::now() + Duration::from_millis(1)),
        );
        let relaxed_high_priority = ("high-priority-no-deadline", 9u8, None);
        for (label, priority, deadline) in [urgent_low_priority, relaxed_high_priority] {
            let manager = manager.clone();
            let order = order.clone();
            let m = m.clone();
            handles.push(tokio::spawn(async move {
                let guard = manager.acquire(&m, priority, deadline).await;
                order.lock().unwrap().push(label);
                tokio::time::sleep(Duration::from_millis(20)).await;
                drop(guard);
            }));
            tokio::time::sleep(Duration::from_millis(30)).await;
        }

        drop(holder);
        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(order.lock().unwrap()[0], "high-priority-no-deadline");
    }

    #[tokio::test]
    async fn a_cancelled_waiter_does_not_wedge_the_queue() {
        // The risk the WaitTicket guard exists for: a future dropped
        // mid-wait used to be impossible to get wrong (there was no
        // queue), but now a leaked ticket sitting at the front would
        // block every other waiter forever. This proves it doesn't.
        let manager = Arc::new(ResourceManager::new(ResourceBudget { vram_mb: 8_000, ram_mb: 16_000, max_concurrent: 8 }));
        let m = model(6_000);
        let holder = manager.acquire(&m, 5, None).await;

        // A very high priority waiter that gives up before capacity frees.
        let doomed = tokio::spawn({
            let manager = manager.clone();
            let m = m.clone();
            async move {
                let _ = manager.acquire(&m, 10, None).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        doomed.abort();
        let _ = doomed.await;

        drop(holder);

        // Someone lower-priority must still be able to proceed.
        let survivor = tokio::time::timeout(std::time::Duration::from_secs(2), manager.acquire(&m, 1, None)).await;
        assert!(survivor.is_ok(), "an aborted waiter left its ticket in the queue and blocked everyone");
    }

    // ---- adopted from a parallel round: ResourceSnapshot::queued -------

    #[tokio::test]
    async fn snapshot_reports_the_current_queue_depth_as_waiters_come_and_go() {
        // Modeled on `equal_priority_waiters_keep_their_arrival_order`:
        // one holder exhausts the VRAM budget, three more callers pile
        // up behind it, and `snapshot().queued` should track exactly
        // how many of them are genuinely blocked at each point, not
        // just before-and-after, but *while* they're actually waiting,
        // which is the whole reason a live Resource Center view would
        // read this field.
        let manager = Arc::new(ResourceManager::new(ResourceBudget { vram_mb: 8_000, ram_mb: 16_000, max_concurrent: 8 }));
        let m = model(6_000);
        let holder = manager.acquire(&m, 5, None).await;
        assert_eq!(manager.snapshot().queued, 0, "nothing is waiting yet, the holder took the only slot the budget allows");

        let mut handles = Vec::new();
        for _ in 0..3 {
            let manager = manager.clone();
            let m = m.clone();
            handles.push(tokio::spawn(async move {
                let guard = manager.acquire(&m, 5, None).await;
                tokio::time::sleep(Duration::from_millis(20)).await;
                drop(guard);
            }));
            // Give each spawned task's `acquire` call time to actually
            // register its ticket before the next one starts, and
            // before we read the snapshot below.
            tokio::time::sleep(Duration::from_millis(30)).await;
        }

        assert_eq!(
            manager.snapshot().queued,
            3,
            "three callers are blocked behind the holder, the budget only fits one 6000MB model at a time"
        );

        drop(holder);
        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(
            manager.snapshot().queued,
            0,
            "the queue should drain back to zero once everyone has had a turn and released"
        );
    }

    // ---- round 49: cancellation ----------------------------------------

    #[tokio::test]
    async fn cancel_ends_a_waiting_acquire_with_an_error_and_frees_its_queue_slot() {
        let manager = Arc::new(ResourceManager::new(ResourceBudget { vram_mb: 8_000, ram_mb: 16_000, max_concurrent: 8 }));
        let m = model(6_000);
        let holder = manager.acquire(&m, 5, None).await;

        let owner = Uuid::new_v4();
        let waiter = tokio::spawn({
            let manager = manager.clone();
            let m = m.clone();
            async move { manager.acquire_cancellable(&m, 5, None, Some(owner)).await.map(|_guard| ()) }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(manager.snapshot().queued, 1, "the waiter must really be blocked behind the holder before we cancel it");

        manager.cancel(owner);
        let outcome = tokio::time::timeout(Duration::from_secs(2), waiter).await.expect("a cancelled wait must end promptly, not hang").unwrap();
        assert_eq!(outcome, Err(AcquireError::Cancelled));
        assert_eq!(manager.snapshot().queued, 0, "the cancelled waiter must leave the queue");

        // The cancelled waiter's concurrency permit must be released
        // too, and it must not have wedged anyone behind it: with the
        // holder gone, a brand-new ordinary acquire still succeeds.
        drop(holder);
        let next = tokio::time::timeout(Duration::from_secs(2), manager.acquire(&m, 5, None)).await;
        assert!(next.is_ok(), "a cancelled wait left the queue or the budget in a stuck state");
    }

    #[tokio::test]
    async fn cancelling_an_owner_that_is_not_waiting_is_a_harmless_noop() {
        let manager = ResourceManager::new(ResourceBudget { vram_mb: 8_000, ram_mb: 16_000, max_concurrent: 8 });
        manager.cancel(Uuid::new_v4());
        // A different, never-cancelled owner proceeds normally.
        let guard = manager.acquire_cancellable(&model(6_000), 5, None, Some(Uuid::new_v4())).await;
        assert!(guard.is_ok());
    }

    #[tokio::test]
    async fn cancelling_one_waiter_leaves_the_others_to_get_their_turn() {
        let manager = Arc::new(ResourceManager::new(ResourceBudget { vram_mb: 8_000, ram_mb: 16_000, max_concurrent: 8 }));
        let m = model(6_000);
        let holder = manager.acquire(&m, 5, None).await;

        let (doomed, survivor) = (Uuid::new_v4(), Uuid::new_v4());
        let spawn_waiter = |owner: Uuid| {
            let manager = manager.clone();
            let m = m.clone();
            tokio::spawn(async move { manager.acquire_cancellable(&m, 5, None, Some(owner)).await.map(|_guard| ()) })
        };
        let doomed_handle = spawn_waiter(doomed);
        tokio::time::sleep(Duration::from_millis(30)).await;
        let survivor_handle = spawn_waiter(survivor);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(manager.snapshot().queued, 2);

        manager.cancel(doomed);
        let doomed_outcome = tokio::time::timeout(Duration::from_secs(2), doomed_handle).await.expect("a cancelled wait must end promptly, not hang").unwrap();
        assert_eq!(doomed_outcome, Err(AcquireError::Cancelled));
        assert_eq!(manager.snapshot().queued, 1, "only the cancelled waiter should have left");

        drop(holder);
        let survivor_outcome = tokio::time::timeout(Duration::from_secs(2), survivor_handle).await.expect("the surviving waiter never got its turn").unwrap();
        assert_eq!(survivor_outcome, Ok(()));
    }

    // ---- round 54: cancel markers are prunable ----------------------

    #[tokio::test]
    async fn forget_cancel_removes_an_unconsumed_marker_so_the_owners_next_acquire_proceeds() {
        let manager = ResourceManager::new(ResourceBudget { vram_mb: 8_000, ram_mb: 16_000, max_concurrent: 8 });
        let m = model(1_000);
        let owner = Uuid::new_v4();

        // A marker for an owner that is NOT waiting is consumed by nothing...
        manager.cancel(owner);
        assert_eq!(manager.pending_cancellations(), 1);
        // ...and, left in place, it cancels that owner's next acquire even
        // though capacity is free (this is the documented pre-cancel
        // behavior, and why a stale one is a hazard, not just a leak).
        assert!(manager.acquire_cancellable(&m, 5, None, Some(owner)).await.is_err());
        assert_eq!(manager.pending_cancellations(), 0, "acquiring consumes the marker it observed");

        // forget_cancel is what removes one nobody will ever consume.
        manager.cancel(owner);
        manager.forget_cancel(owner);
        assert_eq!(manager.pending_cancellations(), 0);
        assert!(manager.acquire_cancellable(&m, 5, None, Some(owner)).await.is_ok(), "a forgotten marker must not cancel the owner's next acquire");
    }

    #[test]
    fn forget_cancel_is_idempotent_and_only_touches_its_own_owner() {
        let manager = ResourceManager::new(ResourceBudget { vram_mb: 8_000, ram_mb: 16_000, max_concurrent: 8 });
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        manager.forget_cancel(a); // no marker: a no-op, not an error
        manager.cancel(a);
        manager.cancel(b);
        manager.forget_cancel(a);
        manager.forget_cancel(a);
        assert_eq!(manager.pending_cancellations(), 1, "b's marker must survive a's removal");
    }

    // ---- round 60: the budget can be resized while running ----------------

    fn budget(vram_mb: u32, max_concurrent: usize) -> ResourceBudget {
        ResourceBudget { vram_mb, ram_mb: 100_000, max_concurrent }
    }

    /// True if `fut` is still pending after a short grace period.
    async fn stays_pending<F: std::future::Future>(fut: F) -> bool {
        tokio::time::timeout(Duration::from_millis(60), fut).await.is_err()
    }

    #[tokio::test]
    async fn growing_the_vram_budget_lets_a_queued_waiter_through() {
        let manager = Arc::new(ResourceManager::new(budget(1_000, 8)));
        let m = model(800);
        let holder = manager.acquire(&m, 5, None).await;
        let waiter = tokio::spawn({
            let (manager, m) = (manager.clone(), m.clone());
            async move {
                let _guard = manager.acquire(&m, 5, None).await;
            }
        });
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(manager.snapshot().queued, 1, "it must really be queued first");

        let previous = manager.set_budget(budget(2_000, 8)).unwrap();
        assert_eq!(previous.vram_mb, 1_000, "set_budget returns what it replaced");
        assert_eq!(manager.budget().vram_mb, 2_000);
        tokio::time::timeout(Duration::from_millis(500), waiter).await.expect("a grown budget must wake the waiter").unwrap();
        drop(holder);
    }

    #[tokio::test]
    async fn shrinking_below_current_usage_never_evicts_running_work_but_gates_new_work() {
        let manager = ResourceManager::new(budget(2_000, 8));
        let (a, b) = (manager.acquire(&model(800), 5, None).await, manager.acquire(&model(800), 5, None).await);
        manager.set_budget(budget(1_000, 8)).unwrap();
        assert_eq!(manager.snapshot().used_vram_mb, 1_600, "running work keeps its reservation");
        assert_eq!(manager.budget().vram_mb, 1_000);

        assert!(stays_pending(manager.acquire(&model(300), 5, None)).await, "over-budget: new work must wait");
        drop(a);
        assert!(stays_pending(manager.acquire(&model(300), 5, None)).await, "800 used + 300 > 1000: still waits");
        drop(b);
        let ok = tokio::time::timeout(Duration::from_millis(500), manager.acquire(&model(300), 5, None)).await;
        assert!(ok.is_ok(), "once usage drops under the new budget, work proceeds");
    }

    #[tokio::test]
    async fn a_shrink_that_would_strand_a_queued_waiter_is_rejected_and_changes_nothing() {
        let manager = Arc::new(ResourceManager::new(budget(1_000, 8)));
        let m = model(800);
        let holder = manager.acquire(&m, 5, None).await;
        let waiter = tokio::spawn({
            let (manager, m) = (manager.clone(), m.clone());
            async move { manager.acquire(&m, 5, None).await.vram_mb }
        });
        tokio::time::sleep(Duration::from_millis(40)).await;

        let err = manager.set_budget(budget(500, 8)).unwrap_err();
        assert_eq!(err, ResizeError::WouldStrand { waiting: 1 });
        assert_eq!(manager.budget().vram_mb, 1_000, "a rejected resize must leave the budget untouched");
        // positive control: a shrink the waiter still fits in is fine
        manager.set_budget(budget(900, 8)).unwrap();
        drop(holder);
        let served = tokio::time::timeout(Duration::from_millis(500), waiter).await.expect("the waiter must still be served").unwrap();
        assert_eq!(served, 800);
    }

    #[tokio::test]
    async fn a_waiter_the_resize_could_not_see_fails_loudly_instead_of_hanging() {
        // max_concurrent = 1: the second caller is blocked on the concurrency
        // permit, so it is NOT yet in the waiter queue and the strand check
        // cannot see it. When it finally runs it must get ExceedsBudget.
        let manager = Arc::new(ResourceManager::new(budget(1_000, 1)));
        let holder = manager.acquire(&model(10), 5, None).await;
        let waiter = tokio::spawn({
            let manager = manager.clone();
            async move { manager.acquire_cancellable(&model(800), 5, None, None).await.map(|_g| ()) }
        });
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(manager.snapshot().queued, 0, "premise: invisible to the queue");

        manager.set_budget(budget(500, 1)).unwrap();
        drop(holder);
        let outcome = tokio::time::timeout(Duration::from_millis(500), waiter).await.expect("must not hang").unwrap();
        assert_eq!(outcome, Err(AcquireError::ExceedsBudget));
        // and it released its permit / queue slot: ordinary work still runs
        let next = tokio::time::timeout(Duration::from_millis(500), manager.acquire(&model(10), 5, None)).await;
        assert!(next.is_ok(), "the failed waiter left the manager wedged");
    }

    #[tokio::test]
    async fn growing_the_concurrency_cap_admits_a_blocked_caller() {
        let manager = Arc::new(ResourceManager::new(budget(100_000, 1)));
        let holder = manager.acquire(&model(10), 5, None).await;
        let waiter = tokio::spawn({
            let manager = manager.clone();
            async move {
                let _g = manager.acquire(&model(10), 5, None).await;
            }
        });
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(!waiter.is_finished(), "premise: cap of 1 blocks the second caller");
        manager.set_budget(budget(100_000, 2)).unwrap();
        tokio::time::timeout(Duration::from_millis(500), waiter).await.expect("a larger cap must admit it").unwrap();
        drop(holder);
    }

    #[tokio::test]
    async fn shrinking_the_concurrency_cap_below_whats_running_is_honored_as_guards_finish() {
        let manager = ResourceManager::new(budget(100_000, 3));
        let m = model(10);
        let (g1, g2, g3) = (manager.acquire(&m, 5, None).await, manager.acquire(&m, 5, None).await, manager.acquire(&m, 5, None).await);
        manager.set_budget(budget(100_000, 1)).unwrap();
        assert_eq!(manager.snapshot().running, 3, "running work is never evicted");
        assert!(stays_pending(manager.acquire(&m, 5, None)).await);

        drop(g1);
        drop(g2);
        assert_eq!(manager.snapshot().running, 1);
        assert!(stays_pending(manager.acquire(&m, 5, None)).await, "cap is 1 and one guard is still held");

        drop(g3);
        let ok = tokio::time::timeout(Duration::from_millis(500), manager.acquire(&m, 5, None)).await;
        assert!(ok.is_ok(), "with everything released the new cap of 1 admits one caller");
    }

    #[tokio::test]
    async fn shrinking_the_cap_with_free_permits_removes_them_immediately() {
        let manager = ResourceManager::new(budget(100_000, 3));
        let m = model(10);
        manager.set_budget(budget(100_000, 1)).unwrap();
        let _only = manager.acquire(&m, 5, None).await;
        assert!(stays_pending(manager.acquire(&m, 5, None)).await, "the cap really is 1, not 3");
        assert_eq!(manager.snapshot().running, 1);
    }

    #[tokio::test]
    async fn growing_the_cap_again_cancels_an_unpaid_shrink() {
        let manager = ResourceManager::new(budget(100_000, 3));
        let m = model(10);
        let (g1, g2, g3) = (manager.acquire(&m, 5, None).await, manager.acquire(&m, 5, None).await, manager.acquire(&m, 5, None).await);
        manager.set_budget(budget(100_000, 1)).unwrap();
        manager.set_budget(budget(100_000, 3)).unwrap();
        drop(g1);
        let g4 = tokio::time::timeout(Duration::from_millis(500), manager.acquire(&m, 5, None)).await;
        assert!(g4.is_ok(), "the freed permit must come back, not be forgotten against stale debt");
        assert!(stays_pending(manager.acquire(&m, 5, None)).await, "and the cap is exactly 3 again");
        drop((g2, g3, g4));
    }

    #[tokio::test]
    async fn invalid_resizes_are_rejected_and_leave_the_budget_alone() {
        let manager = ResourceManager::new(budget(1_000, 4));
        assert!(matches!(manager.set_budget(budget(1_000, 0)), Err(ResizeError::Invalid(_))));
        assert!(matches!(manager.set_budget(budget(1_000, MAX_CONCURRENT_LIMIT + 1)), Err(ResizeError::Invalid(_))));
        assert_eq!(manager.budget().max_concurrent, 4);
        // the upper bound itself is allowed, and zero memory is a legitimate CPU-only setup
        manager.set_budget(budget(0, MAX_CONCURRENT_LIMIT)).unwrap();
        assert_eq!(manager.budget().vram_mb, 0);
    }

    #[tokio::test]
    async fn fits_in_budget_follows_the_live_budget() {
        let manager = ResourceManager::new(budget(1_000, 4));
        assert!(manager.fits_in_budget(&model(800)));
        manager.set_budget(budget(500, 4)).unwrap();
        assert!(!manager.fits_in_budget(&model(800)));
    }

    #[tokio::test]
    async fn resizing_to_the_same_values_is_a_harmless_noop() {
        let manager = ResourceManager::new(budget(1_000, 2));
        let g = manager.acquire(&model(100), 5, None).await;
        let previous = manager.set_budget(budget(1_000, 2)).unwrap();
        assert_eq!((previous.vram_mb, previous.max_concurrent), (1_000, 2));
        assert_eq!(manager.snapshot().running, 1);
        drop(g);
        assert_eq!(manager.snapshot().running, 0);
    }

    #[tokio::test]
    async fn a_budget_exactly_the_size_of_a_queued_model_is_not_a_strand() {
        let manager = Arc::new(ResourceManager::new(budget(1_000, 8)));
        let holder = manager.acquire(&model(800), 5, None).await;
        let waiter = tokio::spawn({
            let manager = manager.clone();
            async move {
                let _g = manager.acquire(&model(800), 5, None).await;
            }
        });
        tokio::time::sleep(Duration::from_millis(40)).await;
        manager.set_budget(budget(800, 8)).expect("800 MB budget still fits an 800 MB waiter exactly");
        drop(holder);
        tokio::time::timeout(Duration::from_millis(500), waiter).await.expect("an exact fit must be served").unwrap();
    }

    #[tokio::test]
    async fn a_model_exactly_the_size_of_the_budget_still_runs_after_a_resize() {
        let manager = ResourceManager::new(budget(5_000, 2));
        manager.set_budget(budget(800, 2)).unwrap();
        let g = tokio::time::timeout(Duration::from_millis(500), manager.acquire_cancellable(&model(800), 5, None, None)).await;
        assert!(matches!(g, Ok(Ok(_))), "an exact fit must acquire, not be called ExceedsBudget");
    }

    #[tokio::test]
    async fn the_ram_side_of_the_budget_is_protected_too() {
        // `model()` always needs 1_000 MB of RAM.
        let manager = Arc::new(ResourceManager::new(budget(1_000, 8)));
        let holder = manager.acquire(&model(800), 5, None).await;
        let waiter = tokio::spawn({
            let manager = manager.clone();
            async move {
                let _g = manager.acquire(&model(800), 5, None).await;
            }
        });
        tokio::time::sleep(Duration::from_millis(40)).await;
        let err = manager.set_budget(ResourceBudget { vram_mb: 1_000, ram_mb: 500, max_concurrent: 8 }).unwrap_err();
        assert_eq!(err, ResizeError::WouldStrand { waiting: 1 }, "VRAM fits but RAM would not");
        drop(holder);
        tokio::time::timeout(Duration::from_millis(500), waiter).await.unwrap().unwrap();

        // and the loud-failure backstop checks RAM as well
        let manager = Arc::new(ResourceManager::new(budget(100_000, 1)));
        let first = manager.acquire(&model(10), 5, None).await;
        let invisible = tokio::spawn({
            let manager = manager.clone();
            async move { manager.acquire_cancellable(&model(10), 5, None, None).await.map(|_g| ()) }
        });
        tokio::time::sleep(Duration::from_millis(40)).await;
        manager.set_budget(ResourceBudget { vram_mb: 100_000, ram_mb: 500, max_concurrent: 1 }).unwrap();
        drop(first);
        let outcome = tokio::time::timeout(Duration::from_millis(500), invisible).await.expect("must not hang").unwrap();
        assert_eq!(outcome, Err(AcquireError::ExceedsBudget));
    }

    // ---- round 61: one definition of "an acceptable budget" ----------------

    #[test]
    fn validate_accepts_the_bounds_and_rejects_outside_them() {
        assert!(budget(0, 1).validate().is_ok(), "zero memory with one slot is a valid CPU-only budget");
        assert!(budget(1_000, MAX_CONCURRENT_LIMIT).validate().is_ok());
        assert!(budget(1_000, 0).validate().is_err());
        assert!(budget(1_000, MAX_CONCURRENT_LIMIT + 1).validate().is_err());
        assert!(ResourceBudget::default().validate().is_ok(), "the shipped default must itself be valid");
    }
}
