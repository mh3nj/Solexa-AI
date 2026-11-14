# SOLEXA, Agent Task Book

**Read this first.** This file is the work order for the tasks that could *not* be done
in the sandbox where rounds 1–65 were built (no real model, no access to `ollama.com` /
`lmstudio.ai` / `huggingface.co`, no desktop to run the Tauri window, `main.rs` never
compiled). It is written so an agent can pick up any one task cold.

> **Prompt to give the agent**
>
> You are continuing SOLEXA (Rust workspace + Tauri/React desktop app, a local-first AI
> Team Engine). Read, in order: `README.md`, `AGENT_TASKS.md` (this file), `HANDBOOK.md`
> §4 (standing conventions), §6 (lessons), §7 (current state). Bootstrap per
> `SOLEXA_RESUME.md` §4 and confirm the baseline (**363 backend tests green**,
> `tsc`/`vite build` clean) before touching anything. Then do the tasks in §3 in the order
> given, one at a time, following §1 for every change. Update `HANDBOOK.md` (one new row
> per round), `README.md`, and the test counts when you finish each task, and ship a new
> zip. Never skip the mutation step. Never claim something is verified that you did not
> actually run.

---

## 0. Where the project stands

* **No roadmap phases remain.** Phases 1–8 of the original spec have been complete since
  round 34. Rounds 35–65 were all post-roadmap hardening: find a real limitation, fix it
  test-first, break the fix on purpose to prove the tests bite, verify it for real, document it.
* Baseline: **363 backend tests** (round 66 added one net: one machine-dependent test replaced by two deterministic ones), `tsc -b` + `vite build` clean.
* "Finished" has no clean meaning here. What is left falls into three buckets:
  **(A)** things that need a real model or network (§3, tasks T1–T5),
  **(B)** things that need a real desktop (T6), and
  **(C)** continuing bug-hunting (T7), which never fully ends.
* **Permanently out of scope** (do not propose): multi-repo sync (HANDBOOK §1 rule 12);
  any hosted infrastructure, accounts, telemetry, or paid tier. SOLEXA is free forever, for
  everyone, local-first.

---

## 1. Rules of the road (apply to EVERY change)

These are distilled from HANDBOOK §4/§6. They exist because each one caught a real bug.

1. **Test first.** Write the failing test, run it, *watch it fail for the right reason*
   (an assertion failure, not a compile error), then fix. If a test cannot be red first,
   say so in the HANDBOOK row.
2. **Mutation-check every fix.** Break the fix on purpose and confirm a test fails. Use
   `scripts/mutation_harness.py` (crash-safe; see its docstring). It reports four outcomes:
   * `KILLED-BY-TEST` / `KILLED-BY-HANG`, good.
   * `SURVIVED`, **a test is missing. Add one.** (Rounds 59–65 each found real gaps this way.)
   * `COMPILE-ERROR(not a kill)`, the mutant is invalid; rewrite it. A compile error is
     not evidence of anything. (Three false "kills" happened before the harness learned this.)
   Test matchers/comparators **in both argument orders** (a substring bug hid in one direction).
3. **Prove tests bite the *original* bug**: revert the fix at each call site and watch the
   end-to-end test fail.
4. **Verify against something real** whenever the claim is about the outside world (a real
   daemon, a real model, a real window). Recording provider ≠ what arrives on the wire:
   use `scripts/request_logging_proxy.py` to inspect the actual HTTP request bodies.
5. **Never invent enforcement.** A field/permission with no real capability to gate
   (`Permission::Propose`, `Permission::Edit`, `Specialist.tools`) is *documented* as
   reserved/descriptive, not given fake checks.
6. **Prefer "no value" to a wrong-typed value** (`None`, not a stand-in id). Treat every
   persisted value as untrusted input at load time: anything wrong ⇒ use the default,
   never fail startup.
7. **When fixing wrongly-typed data, also fix what is already on disk** (idempotent
   `UPDATE` in `Storage::migrate`), if the bad values are provably identifiable.
8. **Name what you did NOT verify** in the HANDBOOK row. The honest-limitations lists in
   §5 exist because earlier rounds did.
9. **Every round ends with:** full `cargo test --workspace` count, `tsc`/`vite build` if
   the frontend changed, a clean-copy re-run from a fresh unzip of the shipped zip,
   updated `HANDBOOK.md` row + lesson (if any), `README.md`, `SOLEXA_STATUS_HANDBOOK.md`,
   `SOLEXA_RESUME.md`, and the new zip.
10. **Beware the sandbox traps** that bit earlier rounds: a tool call that exceeds its
    time limit can kill a mutation run mid-flight (use `setsid nohup … &` and poll, the
    harness auto-recovers a leftover mutation); `cargo test` takes one filter at a time;
    `Cargo.lock` is regenerated automatically.

### Quick reference
```bash
# bootstrap + baseline
unzip -o <latest>.zip && cd solexa
cargo test --workspace            # expect 363 passed
cd apps/desktop/frontend && npm install && npx tsc --noEmit && npm run build

# mutation testing (create mutants.json per the harness docstring)
setsid nohup python3 scripts/mutation_harness.py mutants.json > mut.log 2>&1 < /dev/null &

# watch the real HTTP bodies a daemon receives (proxy :8090 -> daemon :8080)
python3 scripts/request_logging_proxy.py   # logs POST bodies to /tmp/proxy_requests.jsonl
cargo run -p solexa-coordinator --example project_context_smoke_test
```

---

## 2. Orientation (where things live)

| Concern | Where |
|---|---|
| Domain types (Task, Specialist, Member, KnowledgeItem, …) | `crates/solexa-domain` |
| Planner (request → tasks), pipeline, all coordinator actions | `crates/solexa-coordinator/src/{planner.rs,lib.rs}` |
| SQLite storage, migrations, list queries | `crates/solexa-storage` (`schema.sql` + `lib.rs` `migrate`) |
| Model providers | `crates/solexa-models/src/{lib.rs,ollama.rs,llamacpp.rs,http.rs,provider_util.rs}` |
| Provider registration at startup | `apps/desktop/src-tauri/src/state.rs` (`try_register_*`) |
| Resource scheduling (VRAM/RAM/concurrency, live resize) | `crates/solexa-resources` |
| Project Brain (context packaging) | `crates/solexa-brain` + `ContextPackage::render_for_prompt` in domain |
| Tauri commands | `apps/desktop/src-tauri/src/main.rs` |
| React UI + browser mock engine | `apps/desktop/frontend/src` (`lib/api.ts` has `MockEngine`) |
| Live-check helpers | `scripts/`, `crates/solexa-coordinator/examples/` |

Provider contract (`solexa_models::ModelProvider`): `list_models()`, `generate()`,
`generate_streaming()` (per-chunk output over an `UnboundedSender<String>`), plus the
cancellation behavior introduced in round 53 (see `cancel_inflight_smoke_test`).

---

## 3. The tasks

Order matters: T1 → T2 → T3 → T4 are model/network tasks; do T5/T6 on a desktop; T7 any time.

### T1, Prove the round-53 in-flight cancellation against a real Ollama

**Why open:** `cancel_task` interrupts an in-flight generation (drops the HTTP stream, frees
the GPU). It is verified against `llama-server` and against fakes, **not against Ollama**:
in the sandbox no generation ran long enough to cancel. Two workarounds were tried and
**did not help, do not repeat them:** (a) omitting the EOS id from the synthetic model, and
(b) making `</s>` an ordinary token. Ollama's repetition guard / stop handling ended the
generations anyway.

**Do:** on a machine with Ollama and a real model (any small one, e.g. a 1–3B instruct model):
1. Start `ollama serve`; confirm `OllamaProvider::local()` lists the model.
2. Run `cancel_inflight_smoke_test` (read its header) adapted to an Ollama model, with a
   prompt that guarantees a long answer (ask for a 2,000-word essay; set a high `max_tokens`).
3. Cancel ~2 s in. Observe with `nvidia-smi`/`ollama ps`/server log that generation actually
   **stops** (not merely that the coordinator stopped waiting).
4. Also cancel *before the first token* and *after the last token* (the race edges).

**Done when:** the smoke test passes against Ollama; the HANDBOOK row records the model,
Ollama version and observed time-to-stop; any Ollama-specific behavior found is fixed
test-first (e.g. a provider that must close the connection differently).

### T2, LM Studio adapter

**Why open:** `lmstudio.ai` is outside the sandbox allowlist, so it could not be built or
checked. It is a **third provider** next to `ollama.rs` and `llamacpp.rs`.

**Build it as `crates/solexa-models/src/lmstudio.rs`**, mirroring the structure, tests and
docs of `llamacpp.rs` (the closest analogue: both speak OpenAI-style HTTP):

* **API surface, VERIFY against LM Studio's current docs before coding; this is from memory
  and may be stale:** an OpenAI-compatible local server (default `http://localhost:1234`):
  `GET /v1/models`, `POST /v1/chat/completions` (with `stream: true` Server-Sent Events,
  `data: {...}` lines ending in `data: [DONE]`). LM Studio also exposes a richer REST API
  (`/api/v0/models`) with per-model fields such as loaded/not-loaded state, architecture,
  quantization and max context length, prefer it for `ModelMetadata` if present, fall back
  to `/v1/models` if not.
* Implement `list_models` (map to `ModelMetadata`, `provider: "lmstudio"`, `local: true`;
  estimate VRAM/RAM the way the other adapters do, reuse `provider_util.rs`),
  `generate`, and a **real** `generate_streaming` (per-chunk). Honor cancellation exactly as
  the other two do (dropping the stream must stop generation, verify live, as in T1).
* Reuse `http.rs` for the hand-rolled HTTP client where possible; do not add a heavyweight
  dependency without a reason.
* Register in `apps/desktop/src-tauri/src/state.rs` next to Ollama/llama.cpp using the same
  `try_register_*` pattern (a missing LM Studio must **never** fail startup, it is simply
  absent from the model list).
* Surface the provider name wherever the UI lists providers (grep for `"llama.cpp"` in
  `frontend/src`).

**Tests (follow the existing providers' pattern):** a tiny in-process fake HTTP server (see
how `llamacpp.rs` tests fake the daemon) covering: model listing (including a loaded and an
unloaded model, malformed JSON, empty list, connection refused → empty list not panic);
non-streaming generate; streaming with multiple SSE chunks, `[DONE]`, and a chunk split
across TCP reads; mid-stream disconnect (error, not hang); HTTP 4xx/5xx → a clear error;
cancellation drops the connection. Then **mutation-check** each (§1.2).

**Done when:** unit tests + mutations pass; a live run against a real LM Studio shows models
listed, a generation streams, and a cancel stops it; the HANDBOOK row says which LM Studio
version was used and exactly which endpoints were relied on.

### T3, Do real models actually *obey* the delivered project context? (rounds 58–59)

**Why open:** delivery is proven on the wire (the `Accepted` constraint is in the system
prompt; `Rejected`/`Unverified` items appear nowhere). **Whether a real model follows it
has never been tested** the synthetic model emits gibberish.

**Do:** with one or two real models (a small one and a larger one):
1. Create a project knowledge item that is easy to check, e.g. *Constraint: "All answers must
   end with the exact token `[ACK-7731]`."* (status Accepted, tagged `backend`), plus a
   `Rejected` constraint with a different token and an `Unverified` one with a third.
2. Submit a `backend` request; check the output for the right token and the absence of the
   other two. Run each case ≥10 times (models are stochastic) and record the rate.
3. Repeat with `knowledge_scope` set so the item is out of scope, it must now be ignored.
4. Repeat with 20+ knowledge items to see whether the 4000-char budget drops the *important*
   one (constraints are rendered first, confirm it holds).

**Done when:** a results table (model, n, obey-rate per scenario) is in the HANDBOOK row. If
obedience is poor, **experiment with the prompt format** in `ContextPackage::render_for_prompt`
(heading wording, placing constraints last/first, an explicit "you must follow these"
sentence), changing it is test-first like anything else; keep the existing ordering tests
green or update them deliberately.

### T4, Token-aware context budget

**Why open:** the budget is `DEFAULT_CONTEXT_CHARS = 4000` *characters*, an estimate. A model
with a small window could overflow; a big one is under-used.

**Do (only after T3 shows the budget matters):** derive the budget from the selected model's
`context_capacity_tokens` (already in `ModelMetadata`) minus room for the task prompt and the
response (`max_tokens`), using a conservative chars-per-token ratio or a real tokenizer if
one is available offline. Keep `DEFAULT_CONTEXT_CHARS` as the fallback when the capacity is
unknown/zero. `run_specialist` is the one call site (`render_for_prompt(...)`). Tests: tiny
window ⇒ fewer items and an accurate "(N more item(s) omitted)"; huge window ⇒ everything;
unknown ⇒ fallback. Mutation-check the arithmetic (off-by-one, wrong sign, ignoring
`max_tokens`).

### T5, Subtask scoping prompt (investigate; may be a real product bug)

**Observed in round 65:** for a request like *"Refactor the Rust backend and write the SQL
migration…"* the planner (`planner.rs`, ~line 44) creates a parent plus one subtask per
capability. Subtasks get distinct **titles** (`"…, backend"`) but their **description is
the full request text**, and `task.description` is what becomes the model's prompt. So each
specialist is asked to do *everything*, differing only by its system prompt (role/domain).

**Do:** test with a real model whether a SQL specialist, given the whole combined request,
does the backend work too (duplicated/overlapping output, wasted tokens, conflicting
answers). If yes, change the subtask prompt to tell the specialist which part is theirs
(e.g. the original request plus an explicit "Your part: <capability>" scope line, or split
the request by capability if a reliable split exists). This changes model-facing prompt
semantics, so: test the new prompt shape end to end with a recording provider, check the
real-model result before/after, and keep parent/subtask `title` behavior stable (the UI
uses it). **If real-model testing shows no problem, record that and leave it.**

### T6, Verify everything that has only been type-checked (needs a desktop)

**Round 66 did step 1 and the packaging half of step 2 on Windows** (the first time this crate
was ever compiled): three real errors were fixed, a missing `icons/icon.ico`, stale `Specialist`
literals in `seed.rs`, and an undeclared `chrono` dependency, `cargo check --all-targets` is
clean, the release binary launches and seeds its database, and **both NSIS and MSI installers
build**. The UI checklist in step 2 is still open, as is everything below it.

`apps/desktop/src-tauri/src/main.rs` and `state.rs` were **never compiled** in the original
sandbox (they need the system webkit/GTK libraries for Tauri; see the round-66 note above for
where that stands now); the UI has never been seen in a real window. Every round since ~R46
has said "not seen in a real Tauri window".

1. **Compile the shell.** *Toolchain note:* the shell needs a Rust toolchain new enough for
   **edition 2024**; the original build sandbox had cargo 1.75, which is why this was never
   compiled there (and why `main.rs` was once stale for five rounds before anyone noticed).
   Running `cargo check` here will **rewrite `apps/desktop/src-tauri/Cargo.lock`** expected on
   a modern toolchain; review and commit that diff deliberately, do not discard it blindly.
   Install Tauri's OS prerequisites, then `cd apps/desktop/src-tauri && cargo check` and fix
   anything (expect small things:
   commands added in R60/R62, `set_resource_budget`, `reset_resource_budget`, and earlier
   ones, were checked only by reading). Every `#[tauri::command]` must also appear in the
   `invoke_handler!` list.
2. **Run it:** `cargo tauri dev` and walk this checklist, fixing and testing what breaks:
   * Resource Center: usage bars; **Adjust budget** opens seeded with live values; grow while a
     task is queued ⇒ it starts; shrink under a queued task ⇒ clear refusal text; invalid
     input ⇒ message; **Reset to default** works; quit and relaunch ⇒ saved budget restored
     (and reset ⇒ default after relaunch).
   * Specialist form: capability typed as `Backend` ⇒ routes; scope/tools placeholder texts;
     permissions note; creating/editing doesn't blank any field (R52 lesson).
   * Knowledge: tag typed `Backend` ⇒ reaches the model; reject/accept/archive affect delivery.
   * Cancel a running task from the UI (R49/R53).
   * Members/teams/roles actions, including quick repeated clicks (R65 gate).
3. Add Playwright/Tauri-driver tests only if they are cheap; otherwise a written manual
   checklist result in the HANDBOOK row is acceptable.

### T7, Continue the bug hunt (never fully ends)

Prior bug hunts found real defects by *systematic audit*, not by luck. Repeat them, and
invent new ones. For each finding follow §1.

**Standing audits (commands to start from):**
1. *Declared but never consumed* every stored/editable field must change behavior or be
   documented as descriptive. Triage the heuristic's noise (serde-carried fields look unread):
   read the decision site before declaring a defect. Then ALSO check what is *written* into
   each field against its documented type (R64 found `owner_id` holding a specialist id).
2. *Whole-row overwrite* `grep -n "storage.update_" crates/solexa-coordinator/src/lib.rs`.
   Since R65 every such caller must hold `entity_gate`; the lint test
   `every_function_that_rewrites_a_whole_row_takes_the_entity_gate` enforces it. Task rows
   are protected separately (`cancel_gate`, `persist_task`).
3. *Unordered queries* `grep -n 'prepare(' crates/solexa-storage/src/lib.rs | grep -v ORDER`.
   List queries must have an explicit order (R65). Any new one must too.
4. *Exact comparison of user-typed text* capability/tag/scope names go through
   `solexa_domain::tags_match`. Grep for `==` against user strings elsewhere (names, roles,
   model names, provider names).
5. *Swallowed errors* `grep -rn "let _ =\|\.ok()\|unwrap_or_default()" crates --include=*.rs`
   in non-test code; each must be deliberate. Look especially at event emission and
   persistence calls.
6. *Panics on user paths* `grep -rn "\.unwrap()\|\.expect(" crates/*/src` outside tests,
   on anything reachable from a Tauri command or provider response.
7. *Startup robustness* corrupt/old/partial database, missing settings, daemon absent,
   daemon returns junk. Startup must never fail because of persisted or external data.
8. `cargo clippy --workspace --all-targets` and `cargo test --workspace` under
   `RUSTFLAGS="-D warnings"`; fix or justify each.

**High-priority suspects to evaluate with a real model (from the known-gaps list):**
* **`review_verdict` defaults to *approved*** when a reviewer's output has no `VERDICT:` line
  and finished normally (only a reported `Length` finish reason ⇒ Inconclusive). A reviewing
  model that rambles or ignores the format therefore silently approves. Measure how often real
  reviewers omit the verdict; if non-trivial, consider "no verdict ⇒ Inconclusive" (test-first,
  and check every place that treats Inconclusive).
* **A late cancel doesn't roll back non-task-write side effects** (events already emitted, a
  benchmark attempt, a `Conflict` row). Decide whether any is user-visible wrong state.
* **`ResourceSnapshot.running` counts permits held**, including a caller that holds a
  concurrency permit while still waiting for VRAM, so `running` can overstate real generations.
  Decide whether the UI label should change or the count should.
* **Sync trust:** a specialist's own `permissions` field still syncs (R3-era design; the
  review gate only covers *new* specialists arriving), and a project that pulled a hostile
  snapshot between R46–R55 may hold a wrong `role_ids` (undetectable). Review whether a pulled
  specialist could grant itself `Approve`/`Execute` and whether that matters for your threat model.

**Ideas worth trying that have not been done:**
* **Property tests** (`proptest`) for the `TaskState` machine (no illegal transition ever
  succeeds; terminal states are terminal), `ResourceManager` (random interleavings of
  acquire/release/cancel/resize never violate `used <= budget` except by shrink, never leak a
  permit, never deadlock), and `tags_match` (symmetric, reflexive for non-blank).
* **Sync/conflict end-to-end** with a real git repo in a temp dir (two clones editing the
  same specialist), the sync code is heavily unit-tested but little integration-tested.
* **Large inputs:** huge request text, 10k knowledge items, 1k tasks, time and memory.
* **Event log integrity:** every state change should have exactly one event; look for
  silent mutations (state changes with no event) and the reverse.
* **Upgrade test on a real old database** (create with an old zip's binary, open with the
  new one), migrations were only tested synthetically.
* **Frontend ↔ backend parity:** the browser `MockEngine` and the real engine share rules by
  hand-copied constants (e.g. default budget 24000/64000/4, validation messages). Diff them.

---

## 4. Definition of done for the whole project

The project is "done enough to ship" when: T6 passes on at least Linux + one other OS; T1
and T2 are verified live; T3/T5 have recorded real-model results and any needed fixes; the
baseline suite is green with the mutation harness reporting no survivors on the latest
changes; and the HANDBOOK honestly lists what remains unverified. After that, T7 continues
indefinitely. Packaging/installers (`cargo tauri build`, per-OS signing) are a separate,
optional release task, the project's principle is *free forever, local-first, no accounts*.

---

## 5. Honest limitations register (consolidated; each is in a HANDBOOK row too)

* **Not proven with a real model:** that delivered context is obeyed (T3); subtask scoping
  quality (T5); the 4000-*character* context budget vs real windows (T4).
* **Not verified live:** Ollama cancellation (T1); LM Studio does not exist yet (T2).
* **Never compiled/seen:** `main.rs`/`state.rs` and the whole UI in a real Tauri window (T6).
* Two R57 mutants (write order in `resolve_conflict`; bypassing `persist_task`) were
  unkillable without a concurrency harness. R65's `entity_gate` covers the *entity* rows
  `resolve_conflict` touches, but the **task-row** side (`persist_task`/`cancel_gate`) is
  still "closed by construction, no reproduced race". The new stress-test pattern (real
  threads + barrier, in the coordinator tests) could finally attack it.
* `ExceedsBudget` (a budget shrunk under a task that was waiting on a concurrency permit)
  fails the **whole request** with a clear message, not just the one subtask, matching the
  pre-existing up-front "model doesn't fit" behavior.
* The resource budget is **process-wide** while audit events are per-project (filed under
  the active project); nothing probes hardware, so a saved budget larger than the machine can
  run is the user's call.
* Capability/tag matching folds **case and whitespace only** no plurals, synonyms or typo
  tolerance (`backends` ≠ `backend`, deliberately). The team-registry capability union
  (`save_team_to_registry`, display-only dedupe) is still case-sensitive and can show
  `Backend` and `backend` separately.
* `Task.owner_id`: nothing in the UI shows an owner yet. In a consensus round the owner
  follows the *primary* specialist (first match after the track-record sort; ties by
  creation order, R65). If several members run one specialist the smallest member id wins.
* The entity gate is re-entrant **per thread**, not per `Coordinator`: holding it on one
  coordinator and calling a mutator on a different one from the same thread would skip the
  second lock. Nothing does that; do not start. It must never be held across an `.await`.
* `ORDER BY rowid` was added to `models` and `project_disabled_models` for consistency, but
  those two have no test that can distinguish it (the other seven list queries do).
* The mock engine duplicates `ResourceBudget::default()` (24000/64000/4) by hand.
* `Storage::sabotage_settings_table_for_tests` is a `#[doc(hidden)]` test hook in production
  code (needed because `cfg(test)` does not cross crate boundaries). Never call it outside tests.
* `ResourceBudget::validate`'s own test, and the brain tests for case-insensitive matching,
  were written alongside their code rather than red-first; their bite was shown by mutation.

---

## 6. How to report back

For each task: one new row in `HANDBOOK.md` §3 (what you found, what you changed, how it was
verified, **what you did not verify**), a lesson in §6 if you learned one, updated test
counts everywhere (`README.md`, §7), and a fresh zip verified from a clean unzip. If a task
turns out not to be needed (e.g. T5 finds no problem), say so with the evidence and move on.
