use anyhow::{Context, Result};
use solexa_coordinator::Coordinator;
use solexa_events::EventBus;
use solexa_models::{llamacpp::LlamaCppProvider, ollama::OllamaProvider, MockLocalProvider, ProviderRegistry};
use solexa_storage::Storage;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager};
use uuid::Uuid;

pub struct AppState {
    pub coordinator: Arc<Coordinator>,
    /// Kept alongside `coordinator` (same underlying bus) purely so
    /// `main.rs`'s setup hook can `subscribe()` a live listener for the
    /// UI without the coordinator crate needing to expose that.
    pub coordinator_events: Arc<EventBus>,
    /// Round 35: which project the UI is currently looking at. Behind a
    /// lock rather than a plain `Uuid` because the app can now switch
    /// between projects at runtime, the engine has always been
    /// multi-project (every storage/coordinator call takes a
    /// `project_id`), this is just the desktop shell catching up.
    /// Read it through `project_id()`; never hold the guard across a
    /// coordinator call.
    active_project_id: Mutex<Uuid>,
}

impl AppState {
    pub fn project_id(&self) -> Uuid {
        *self.active_project_id.lock().unwrap()
    }

    pub fn set_project_id(&self, project_id: Uuid) {
        *self.active_project_id.lock().unwrap() = project_id;
    }
}

pub fn build(app_handle: &AppHandle) -> Result<AppState> {
    let data_dir = app_handle
        .path()
        .app_data_dir()
        .context("resolving app data directory")?;
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("creating app data dir at {}", data_dir.display()))?;
    let db_path = data_dir.join("solexa.sqlite3");

    let storage = Arc::new(Storage::open(&db_path)?);
    let (project_id, model) = crate::seed::ensure_seeded(&storage)?;

    let mut bus = EventBus::new();
    bus.add_sink(Box::new(solexa_events::ArcSink(storage.clone())));
    let events = Arc::new(bus);

    let mut providers = ProviderRegistry::new();

    // Round 37: if the user is already running Ollama, register it and
    // use their real local models. This is the same principle round 22
    // applied to git and round 30 to GitHub, SOLEXA talks to the thing
    // you already run, it never ships or manages it.
    //
    // Failure here is deliberately non-fatal and quiet-ish: "Ollama
    // isn't running" is the normal state for someone who hasn't
    // installed it, not an error worth blocking app startup over. The
    // mock provider is always registered too, so the app is fully
    // usable either way and the difference is visible in the Models
    // list rather than hidden.
    match try_register_ollama(OllamaProvider::local(), &mut providers) {
        Ok(0) => tracing::info!("Ollama is running but has no models installed (`ollama pull <model>` to add one)"),
        Ok(n) => tracing::info!("registered {n} model(s) from the local Ollama daemon"),
        Err(err) => tracing::info!("no local Ollama daemon detected ({err:#}), continuing with the built-in mock provider"),
    }

    // Round 42: same principle, second adapter, a llama.cpp
    // `llama-server` running in router mode (`--models-dir`), if the
    // user has one going. `try_register_llamacpp` is a near-duplicate
    // of `try_register_ollama` rather than something shared behind a
    // trait, two call sites doesn't earn an abstraction yet; if a
    // third adapter shows up wanting the identical "probe on startup,
    // register only if it found something, never fatal" shape, that's
    // the point where factoring it out actually pays for itself (the
    // same judgment call round 40 made about `stream_generation`, just
    // landing on "not yet" here instead of "yes").
    match try_register_llamacpp(LlamaCppProvider::local(), &mut providers) {
        Ok(0) => tracing::info!("llama.cpp is running but has no models configured (start it with --models-dir)"),
        Ok(n) => tracing::info!("registered {n} model(s) from the local llama.cpp server"),
        Err(err) => tracing::info!("no local llama.cpp server detected ({err:#}), continuing with the built-in mock provider"),
    }

    providers.register(Arc::new(MockLocalProvider::new(vec![model])));

    let coordinator = Arc::new(Coordinator::new(storage, events.clone(), providers));

    Ok(AppState {
        coordinator,
        coordinator_events: events,
        active_project_id: Mutex::new(project_id),
    })
}

/// Runs `OllamaProvider::refresh` to completion on a short-lived
/// runtime and registers the provider if it found anything.
///
/// `build` is called from Tauri's synchronous `setup` hook, before the
/// app's own async runtime is handling requests, so there's no ambient
/// runtime to `await` on here. A dedicated current-thread runtime for
/// one startup probe is simpler and more honest than restructuring
/// setup to be async for a call that takes milliseconds against
/// localhost.
fn try_register_ollama(provider: OllamaProvider, providers: &mut ProviderRegistry) -> Result<usize> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let count = runtime.block_on(provider.refresh())?;
    if count > 0 {
        providers.register(Arc::new(provider));
    }
    Ok(count)
}

/// Round 42's counterpart to `try_register_ollama`, for
/// `LlamaCppProvider`, same contract, same reasoning, a different
/// concrete provider type.
fn try_register_llamacpp(provider: LlamaCppProvider, providers: &mut ProviderRegistry) -> Result<usize> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let count = runtime.block_on(provider.refresh())?;
    if count > 0 {
        providers.register(Arc::new(provider));
    }
    Ok(count)
}
