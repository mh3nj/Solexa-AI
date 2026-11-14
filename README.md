# SOLEXA, AI Team Engine

A local-first orchestration engine where humans and specialized local AI
models collaborate as accountable members of structured teams. Free
forever, for everyone, with no premium tier.

`cargo test`: **363 tests, all green** (up from 362, the one
machine-dependent test was replaced by two deterministic ones). `tsc -b` /
`vite build`: clean. **`apps/desktop/src-tauri` now compiles on a modern
toolchain, the release binary has been launched, and both Windows
installers build**: NSIS (`SOLEXA_0.1.0_x64-setup.exe`) and MSI
(`SOLEXA_0.1.0_x64_en-US.msi`).

**Handing this to an agent?** Give it this zip and read **`AGENT_TASKS.md`** 
the work order for everything that needs a real model, network or desktop
(live Ollama cancel proof, LM Studio adapter, real-model obedience tests,
token-aware budget, subtask-scoping prompt, the rest of the UI checklist,
and a bug-hunt playbook). `scripts/mutation_harness.py` is the crash-safe
mutation-testing tool its rules require.

## Settings, themes and languages

The gear in the title bar opens Settings, and everything in it is written
to `%APPDATA%\engine.solexa.desktop\solexa.sqlite3`, so the choices come
back after a restart or a reinstall.

- **General**: English, German, or Persian (Persian flips the layout to
  right to left), plus Close to tray, Minimize to tray, and the startup
  choice. "Run at startup" and "Run at startup in tray" are exclusive;
  the second one keeps the window hidden until the tray icon is clicked.
- **Appearance**: Theme Studio. The bundled themes come from the
  `mh3nj-themes` VS Code collection, each reduced to the eight colors this
  interface uses. New theme opens an editor with a live preview; Save,
  Save and apply, Edit and Delete all work on your own themes.
- **Engine**: resource budget, the approver requirement, and the review
  gate for incoming sync changes.
- **About**: version, and the note that the logo and the tray icon follow
  the active theme accent.

The window and the tray read those same rows directly, which is what lets
close/minimize to tray and the tray menu language work before the webview
has painted. The app icon, the installer icons, the tray icon and the
window logo are all built from `solexa.png` / `solexa.svg`.

## Building the Windows installers

Build the frontend first, then let the Tauri 2 CLI bundle. The override
skips `beforeBuildCommand`, which would otherwise run in `apps/desktop`
where there is no `package.json`.

```
cd apps/desktop/frontend
npm run build
cd ..
tauri build --config '{"build":{"beforeBuildCommand":""}}' --bundles nsis,msi --ignore-version-mismatches
```

Both files land in `apps/desktop/src-tauri/target/release/bundle/`, and
`installers/` keeps a copy of each.

## Where the project stands

Phases 1–8 of the original spec have been complete since round 34. Every
round since is post-roadmap: find a real limitation, close it, verify it
for real, document it. Round 66 is T6 from `AGENT_TASKS.md`, the first
time the Tauri shell was ever compiled, plus the Windows packaging step
that had never been attempted.

## This round, in one sentence

The desktop shell was compiled, launched and packaged on Windows for the
first time, which surfaced and fixed three genuine compile errors that
"reviewed by eye" had missed for many rounds, and both installers now
build.

## What changed

1. **`icons/icon.ico` was missing** `tauri-build`'s Windows resource
   step aborts without it (`icon.png` alone is not enough for an
   `.exe`). Generated the standard desktop icon set with `tauri icon`
   (`icon.ico`, `icon.icns`, `32x32.png`, `128x128.png`,
   `128x128@2x.png`), listed all five in `bundle.icon`, and deleted the
   mobile/Microsoft-Store assets the generator also emits.
2. **`seed.rs`'s two `Specialist` literals were stale** missing
   `role_ids`, `max_tokens` and `temperature`, the fields added in
   rounds 47/52. Nothing had compiled the seeder since, so the very
   first launch path would not have built. Now `Vec::new()` / `None` /
   `None`, matching a brand-new specialist created through the UI.
3. **`main.rs` used `chrono` without declaring it** `set_task_deadline`
   parses RFC 3339; the workspace pins `chrono`, the standalone Tauri
   crate did not. Added `chrono = { version = "0.4", features = ["serde"] }`.
4. **The suite could not be green on a normal dev machine.**
   `solexa-github`'s `gh_available_is_false_when_gh_is_installed_but_not_logged_in`
   asserted `!gh_available()`, which is only true where `gh` is installed
   but never authenticated (the original sandbox). On a machine that has
   run `gh auth login` it fails for no reason. The logic is now in
   `gh_available_from(Command)` and tested deterministically for the two
   outcomes that are this function's job (exit 0 ⇒ true, non-zero or
   unspawnable ⇒ false); the machine-dependent assertion is gone.
5. **Both Windows installers build** `npx tauri build --bundles nsis,msi`.

## What was verified, and what wasn't

- `cargo test --workspace --no-fail-fast`: **363 passed, 0 failed**.
- `npx tsc --noEmit` and `npm run build`: clean.
- `cargo check --all-targets` in `apps/desktop/src-tauri`: clean on rustc
  1.99.1 / Tauri 2.11.5. Its `Cargo.lock` was rewritten by the modern
  toolchain and **kept deliberately** (that had been an explicit warning
  in the old README; on a current toolchain it is the correct resolution).
- The release binary was **launched**: the process stayed up, wrote no
  errors, and created/seeded `%APPDATA%\engine.solexa.desktop\solexa.sqlite3`
  with 1 project, 2 specialists, 3 members, 1 team, 1 model and 1 knowledge
  item, the first-run path this crate has never executed before.
- Bundles: NSIS archive lists `solexa-desktop.exe` (7-Zip); the MSI was
  administrative-extracted with `msiexec /a` and contains
  `PFiles\SOLEXA\solexa-desktop.exe`. Both carry the generated icons.
- The two new `gh` tests were **mutation-checked** (exit-status mapping
  forced to `true`; `unwrap_or(true)`): each mutant was killed by the test
  that names it, and the file was restored byte-identical afterwards.
- **Not verified:** the T6 UI checklist (Resource Center adjust/reset,
  specialist form round trip, knowledge delivery, cancelling from the UI,
  members/teams/roles, budget persistence across relaunch), only that the
  window's process starts and seeds its database. No installer was actually
  run to completion, and neither is code-signed (SmartScreen will warn).
  No live Ollama/llama.cpp/LM Studio work (T1/T2/T3/T5) and no
  `cargo clippy` pass.

## Honest inventory

| Piece | Status |
|---|---|
| Desktop shell compiles (`cargo check --all-targets`) | **Real, verified (R66)** |
| Release binary launches and seeds its database | **Real, verified (R66)** |
| NSIS + MSI installers | **Built and inspected (R66); not installed; unsigned** |
| Full UI checklist in a real window | **Still open (T6 second half)** |
| Project Brain context reaches the model | **Real, test-first, live-verified (delivery)** |
| `Permission::Read` gates context | **Real** (first check `Read` has ever had) |
| Context budget | **Character estimate, not tokens** |
| `Propose`, `Edit` permissions | **Reserved, documented, not enforced** |
| `Specialist.knowledge_scope` | **Real, test-first, live-verified (narrows context)** |
| `Specialist.tools` | **Descriptive labels only, nothing executes them** |
| List ordering explicit; concurrent UI edits can't revert each other | **Real, differential-tested, mutation-checked (R65)** |
| Task owner is a member (with data heal) | **Real, test-first (R64); consensus case untested; no UI yet** |
| Capability/tag matching ignores case and whitespace | **Real, mutation-checked, end-to-end tested (R63)** |
| Resource budget resize | **Real, test-first, mutation-checked; saved across restarts (R61)** |
| `gh` precondition check | **Real, deterministic tests (R66)** |
| Model *obeys* the context | **Not proven** |
| Ollama live cancel | **Not verified** |
| LM Studio adapter | **Not built** |
| Multi-repo sync | **Permanently rejected** (§1 rule 12) |

## Next steps

`AGENT_TASKS.md` remains the work order: T6's **UI checklist in a running
window** is the natural next step now that the shell compiles; then T1
(Ollama cancel), T2 (LM Studio), T3/T5 (real-model obedience and subtask
scoping), T4 (token-aware budget), and T7 (continued bug-hunting, starting
with `cargo clippy -D warnings` and the property/upgrade tests).

Anything proposing hosted infrastructure, accounts, or a paid tier is out
of scope: SOLEXA is free forever, for everyone.
>>>>>>> 0dcbdd5 (Solexa Project)
