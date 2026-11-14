// SOLEXA desktop shell.
//
// This binary's only job is wiring: open/seed local storage, build a
// Coordinator, forward its live event stream into the window, and
// expose a small set of Tauri commands the React frontend calls. All
// orchestration logic lives in solexa-coordinator, nothing here
// decides *what* happens, only how the UI talks to the engine that
// does (spec rule: "the frontend sends commands and consumes
// state/events... do not put orchestration logic inside React").
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod seed;
mod state;

use solexa_coordinator::UserRequest;
use solexa_events::Event;
use state::AppState;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager, State, WindowEvent};

// Set only by the tray's Quit item, so the close-to-tray handler can tell
// a user closing the window (keep running) from a real quit (exit).
static ALLOW_EXIT: AtomicBool = AtomicBool::new(false);

#[derive(serde::Serialize)]
struct SubmissionDto {
    task_id: String,
    correlation_id: String,
}

// ---- projects (round 35) ---------------------------------------------
//
// The engine has been multi-project since Phase 1, every storage and
// coordinator call takes a `project_id`. Only the desktop shell was
// pinned to the single project `seed.rs` creates at first launch;
// these four commands are what let a user actually have more than one.

#[tauri::command]
fn list_projects(state: State<'_, AppState>) -> Result<Vec<serde_json::Value>, String> {
    let projects = state.coordinator.list_projects().map_err(|e| e.to_string())?;
    let active = state.project_id();
    Ok(projects
        .into_iter()
        .map(|p| {
            serde_json::json!({
                "id": p.id,
                "name": p.name,
                "description": p.description,
                "is_active": p.id == active,
            })
        })
        .collect())
}

#[tauri::command]
fn create_project(state: State<'_, AppState>, name: String, description: String) -> Result<String, String> {
    let project_id = state
        .coordinator
        .create_project(name, description)
        .map_err(|e| e.to_string())?;
    Ok(project_id.to_string())
}

/// Switches which project every other command operates on. Deliberately
/// validates the project exists first, a bad id would otherwise leave
/// the whole app pointed at a project that returns empty results for
/// everything, with no obvious cause.
#[tauri::command]
fn switch_project(state: State<'_, AppState>, project_id: String) -> Result<(), String> {
    let project_id = uuid::Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;
    if state.coordinator.get_project(project_id).map_err(|e| e.to_string())?.is_none() {
        return Err(format!("no project with id {project_id}"));
    }
    state.set_project_id(project_id);
    Ok(())
}

#[tauri::command]
async fn submit_request(
    state: State<'_, AppState>,
    text: String,
    priority: Option<u8>,
) -> Result<SubmissionDto, String> {
    if text.trim().is_empty() {
        return Err("request text must not be empty".into());
    }
    let submission = state
        .coordinator
        .submit_request(UserRequest {
            project_id: state.project_id(),
            text,
            requested_by: "human".into(),
        })
        .await
        .map_err(|e| e.to_string())?;
    // Set priority as a follow-up write rather than threading it
    // through UserRequest/the core loop, Task.priority isn't read by
    // any scheduling logic yet (see the README), so this only needs to
    // be "set after creation," not "known before the pipeline runs."
    if let Some(priority) = priority {
        state
            .coordinator
            .set_task_priority(submission.task_id, priority)
            .map_err(|e| e.to_string())?;
    }
    Ok(SubmissionDto {
        task_id: submission.task_id.to_string(),
        correlation_id: submission.correlation_id.to_string(),
    })
}

#[tauri::command]
fn list_tasks(state: State<'_, AppState>) -> Result<Vec<serde_json::Value>, String> {
    let tasks = state
        .coordinator
        .list_tasks(state.project_id())
        .map_err(|e| e.to_string())?;
    Ok(tasks.iter().map(task_to_json).collect())
}

#[tauri::command]
fn get_task(state: State<'_, AppState>, task_id: String) -> Result<Option<serde_json::Value>, String> {
    let id = uuid::Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let task = state.coordinator.get_task(id).map_err(|e| e.to_string())?;
    Ok(task.as_ref().map(task_to_json))
}

#[tauri::command]
fn get_trace(state: State<'_, AppState>, correlation_id: String) -> Result<Vec<serde_json::Value>, String> {
    let id = uuid::Uuid::parse_str(&correlation_id).map_err(|e| e.to_string())?;
    let events = state
        .coordinator
        .events_for_correlation(id)
        .map_err(|e| e.to_string())?;
    Ok(events.iter().map(event_to_json).collect())
}

/// Round 52: the form sends an empty field as `null`, but a stray `0`
/// would be a real (and useless) "generate zero tokens" limit, so it is
/// treated as unset rather than stored.
fn positive_limit(value: Option<u32>) -> Option<u32> {
    value.filter(|v| *v > 0)
}

#[tauri::command]
fn list_specialists(state: State<'_, AppState>) -> Result<Vec<serde_json::Value>, String> {
    let specialists = state.coordinator.list_specialists(state.project_id()).map_err(|e| e.to_string())?;
    specialists
        .iter()
        .map(|s| {
            let benchmark = state.coordinator.storage().get_benchmark(s.id).map_err(|e| e.to_string())?;
            Ok(serde_json::json!({
                "id": s.id,
                "name": s.name,
                "role": s.role,
                "domain": s.domain,
                "capabilities": s.capabilities,
                "preferred_model_ids": s.preferred_model_ids,
                "tools": s.tools,
                "knowledge_scope": s.knowledge_scope,
                "permissions": s.permissions,
                "output_schema": s.output_schema,
                "escalate_on_weak_confidence": s.escalation_policy.escalate_on_weak_confidence,
                "escalate_on_verification_failure": s.escalation_policy.escalate_on_verification_failure,
                "requires_model_review": s.escalation_policy.requires_model_review,
                "requires_specialist_review": s.escalation_policy.requires_specialist_review,
                "active": s.active,
                "role_ids": s.role_ids,
                "max_tokens": s.max_tokens,
                "temperature": s.temperature,
                "benchmark": {
                    "verification_passed_count": benchmark.verification_passed_count,
                    "verification_failed_count": benchmark.verification_failed_count,
                    "success_rate": benchmark.success_rate(),
                },
            }))
        })
        .collect()
}

#[tauri::command]
fn list_models(state: State<'_, AppState>) -> Result<Vec<serde_json::Value>, String> {
    // Round 41: scoped to what this project actually has available,
    // a specialist-creation picker shouldn't offer a model this project
    // has turned off, even if some other project sharing the same
    // daemon still uses it. See `list_models_with_status` for the full,
    // unfiltered list a settings view needs instead.
    let models = state.coordinator.models_available_to(state.project_id()).map_err(|e| e.to_string())?;
    Ok(models
        .iter()
        .map(|m| {
            serde_json::json!({
                "id": m.id,
                "name": m.name,
                "provider": m.provider,
                "parameter_count_billions": m.parameter_count_billions,
                "local": m.local,
            })
        })
        .collect())
}

#[tauri::command]
fn list_models_with_status(state: State<'_, AppState>) -> Result<Vec<serde_json::Value>, String> {
    let disabled: std::collections::HashSet<uuid::Uuid> = state
        .coordinator
        .disabled_model_ids(state.project_id())
        .map_err(|e| e.to_string())?
        .into_iter()
        .collect();
    Ok(state
        .coordinator
        .list_models()
        .iter()
        .map(|m| {
            serde_json::json!({
                "id": m.id,
                "name": m.name,
                "provider": m.provider,
                "parameter_count_billions": m.parameter_count_billions,
                "context_capacity_tokens": m.context_capacity_tokens,
                "tool_support": m.tool_support,
                "local": m.local,
                "enabled": !disabled.contains(&m.id),
            })
        })
        .collect())
}

/// Round 49: cancels a task. Since round 53 this interrupts it wherever it
/// is, waiting on hardware, not yet started, or already generating (the
/// in-flight model call is aborted); it errors only for a task that has
/// already finished, see `Coordinator::cancel_task`'s doc comment.
#[tauri::command]
fn cancel_task(state: State<'_, AppState>, task_id: String) -> Result<(), String> {
    let task_id = uuid::Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    state.coordinator.cancel_task(task_id).map_err(|e| e.to_string())
}

#[tauri::command]
fn set_model_enabled(state: State<'_, AppState>, model_id: String, enabled: bool) -> Result<(), String> {
    let model_id = uuid::Uuid::parse_str(&model_id).map_err(|e| e.to_string())?;
    state.coordinator.set_model_enabled(state.project_id(), model_id, enabled).map_err(|e| e.to_string())
}

/// Adopted from a parallel build: the Resource Center.
/// `Coordinator::resource_snapshot()` has existed since Phase 3 (used
/// internally to log what a task acquired), but was never exposed to
/// the frontend, this is the first command that reads it. Combines it
/// with `resource_budget()` in one payload rather than two round-trips,
/// since the used numbers mean nothing to a live view without the
/// ceiling they're measured against.
#[tauri::command]
fn get_resource_snapshot(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let snapshot = state.coordinator.resource_snapshot();
    let budget = state.coordinator.resource_budget();
    Ok(serde_json::json!({
        "used_vram_mb": snapshot.used_vram_mb,
        "used_ram_mb": snapshot.used_ram_mb,
        "running": snapshot.running,
        "queued": snapshot.queued,
        "budget_vram_mb": budget.vram_mb,
        "budget_ram_mb": budget.ram_mb,
        "max_concurrent": budget.max_concurrent,
    }))
}

/// Round 60: the Resource Center's controls, resizes the engine's
/// VRAM / RAM / concurrency budget while it is running. Running work is
/// never evicted; a shrink that would strand an already-queued task is
/// refused with a message (see `ResourceManager::set_budget`). The change
/// is recorded as a `resource_budget_changed` event and (round 61) saved, so
/// `Coordinator::new` restores it on the next start; if saving fails the
/// change still applies this session and the command returns an error saying
/// it will revert on restart.
#[tauri::command]
fn set_resource_budget(
    state: State<'_, AppState>,
    vram_mb: u32,
    ram_mb: u32,
    max_concurrent: usize,
) -> Result<(), String> {
    state
        .coordinator
        .set_resource_limits(state.project_id(), vram_mb, ram_mb, max_concurrent)
        .map_err(|e| e.to_string())
}

/// Round 62: puts the resource budget back to the built-in default and
/// forgets the saved override (see `Coordinator::reset_resource_budget`).
/// Refused, with a message, if the default would strand an already-queued
/// task. If the saved override can't be cleared the reset still applies this
/// session and the command returns an error saying the old budget returns on
/// restart.
#[tauri::command]
fn reset_resource_budget(state: State<'_, AppState>) -> Result<(), String> {
    state
        .coordinator
        .reset_resource_budget(state.project_id())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Creates a specialist through the structured flow the spec calls for
/// (name/role/domain/capabilities/models/tools/knowledge scope/
/// permissions/output format/escalation rules, "not a giant
/// system-prompt textbox"), then wraps it in an AI `Member` and adds
/// that member to `add_to_team_id` if given, the same two extra steps
/// `seed.rs` performs for every specialist it creates. Without this, a
/// specialist registered on its own would be immediately routable
/// (engine-side, proven in `solexa-coordinator`'s tests) but invisible
/// in the Team rail, which resolves specialists only via a member's
/// `specialist_ids`, see `SpecialistRail.tsx`.
#[tauri::command]
fn create_specialist(
    state: State<'_, AppState>,
    name: String,
    role: String,
    domain: String,
    capabilities: Vec<String>,
    preferred_model_ids: Vec<String>,
    tools: Vec<String>,
    knowledge_scope: Vec<String>,
    permissions: Vec<String>,
    output_schema: Option<String>,
    escalate_on_weak_confidence: bool,
    escalate_on_verification_failure: bool,
    requires_model_review: bool,
    requires_specialist_review: bool,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    member_display_name: String,
    add_to_team_id: Option<String>,
) -> Result<serde_json::Value, String> {
    let preferred_model_ids = preferred_model_ids
        .into_iter()
        .map(|id| uuid::Uuid::parse_str(&id).map_err(|e| e.to_string()))
        .collect::<Result<Vec<_>, _>>()?;

    let permissions = permissions
        .into_iter()
        .map(|p| {
            serde_json::from_value::<solexa_domain::Permission>(serde_json::Value::String(p.clone()))
                .map_err(|_| format!("\"{p}\" is not a valid permission"))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let output_schema = match output_schema {
        Some(text) if !text.trim().is_empty() => Some(
            serde_json::from_str::<serde_json::Value>(&text)
                .map_err(|e| format!("output format must be valid JSON Schema: {e}"))?,
        ),
        _ => None,
    };

    let specialist = solexa_domain::Specialist {
        id: uuid::Uuid::new_v4(),
        name,
        role,
        domain,
        capabilities,
        preferred_model_ids,
        tools,
        knowledge_scope,
        permissions,
        output_schema,
        escalation_policy: solexa_domain::EscalationPolicy {
            escalate_on_weak_confidence,
            escalate_on_verification_failure,
            requires_model_review,
            requires_specialist_review,
        },
        active: true,
        role_ids: Vec::new(), // a brand-new specialist holds no roles; assign_role_to_specialist adds them
        max_tokens: positive_limit(max_tokens),
        temperature,
    };
    let specialist_id = specialist.id;
    let specialist_name = specialist.name.clone();

    state
        .coordinator
        .register_specialist(state.project_id(), specialist)
        .map_err(|e| e.to_string())?;

    let member_name = if member_display_name.trim().is_empty() {
        format!("{specialist_name} Bot")
    } else {
        member_display_name
    };
    let mut member = solexa_domain::Member::new_ai(
        state.project_id(),
        member_name,
        format!("runs the {specialist_name}"),
    );
    member.specialist_ids = vec![specialist_id];
    let member_id = member.id;
    state.coordinator.register_member(member).map_err(|e| e.to_string())?;

    if let Some(team_id) = add_to_team_id {
        let team_id = uuid::Uuid::parse_str(&team_id).map_err(|e| e.to_string())?;
        state
            .coordinator
            .add_member_to_team(team_id, member_id)
            .map_err(|e| e.to_string())?;
    }

    Ok(serde_json::json!({ "specialist_id": specialist_id, "member_id": member_id }))
}

/// Rewrites an existing specialist's configuration through the same
/// structured shape `create_specialist` accepts, `SpecialistCreationForm`
/// is reused in "edit" mode for this, pre-filled from `list_specialists`'
/// now-fuller JSON rather than duplicating the form. Does not touch the
/// AI member or team membership `create_specialist` set up; `active` is
/// changed separately via `set_specialist_active` so archiving isn't
/// silently reversible by an unrelated field edit.
#[tauri::command]
fn update_specialist(
    state: State<'_, AppState>,
    specialist_id: String,
    name: String,
    role: String,
    domain: String,
    capabilities: Vec<String>,
    preferred_model_ids: Vec<String>,
    tools: Vec<String>,
    knowledge_scope: Vec<String>,
    permissions: Vec<String>,
    output_schema: Option<String>,
    escalate_on_weak_confidence: bool,
    escalate_on_verification_failure: bool,
    requires_model_review: bool,
    requires_specialist_review: bool,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
) -> Result<(), String> {
    let specialist_id = uuid::Uuid::parse_str(&specialist_id).map_err(|e| e.to_string())?;

    let preferred_model_ids = preferred_model_ids
        .into_iter()
        .map(|id| uuid::Uuid::parse_str(&id).map_err(|e| e.to_string()))
        .collect::<Result<Vec<_>, _>>()?;

    let permissions = permissions
        .into_iter()
        .map(|p| {
            serde_json::from_value::<solexa_domain::Permission>(serde_json::Value::String(p.clone()))
                .map_err(|_| format!("\"{p}\" is not a valid permission"))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let output_schema = match output_schema {
        Some(text) if !text.trim().is_empty() => Some(
            serde_json::from_str::<serde_json::Value>(&text)
                .map_err(|e| format!("output format must be valid JSON Schema: {e}"))?,
        ),
        _ => None,
    };

    // `active` is intentionally not a parameter here, carried over from
    // whatever it already was so this command can never accidentally
    // flip it; `id` is overwritten by `Coordinator::edit_specialist`
    // regardless of what's put here, but set explicitly for clarity.
    let updated = solexa_domain::Specialist {
        id: specialist_id,
        name,
        role,
        domain,
        capabilities,
        preferred_model_ids,
        tools,
        knowledge_scope,
        permissions,
        output_schema,
        escalation_policy: solexa_domain::EscalationPolicy {
            escalate_on_weak_confidence,
            escalate_on_verification_failure,
            requires_model_review,
            requires_specialist_review,
        },
        active: true,
        role_ids: Vec::new(), // overridden by edit_specialist regardless (round 52), roles survive an edit
        max_tokens: positive_limit(max_tokens),
        temperature,
    };

    state
        .coordinator
        .edit_specialist(state.project_id(), specialist_id, updated)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn set_specialist_active(state: State<'_, AppState>, specialist_id: String, active: bool) -> Result<(), String> {
    let specialist_id = uuid::Uuid::parse_str(&specialist_id).map_err(|e| e.to_string())?;
    state
        .coordinator
        .set_specialist_active(state.project_id(), specialist_id, active)
        .map_err(|e| e.to_string())
}

/// Serializes a specialist's full configuration and measured benchmark
/// into a portable, pretty-printed JSON document, the "packaging"
/// half of spec Phase 7. Returns the JSON as a plain string; there's
/// no file-dialog plumbing here, the frontend decides what to do with
/// the text (show it, offer a download, whatever fits).
#[tauri::command]
fn export_specialist_package(state: State<'_, AppState>, specialist_id: String) -> Result<String, String> {
    let specialist_id = uuid::Uuid::parse_str(&specialist_id).map_err(|e| e.to_string())?;
    let package = state
        .coordinator
        .export_specialist_package(specialist_id)
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&package).map_err(|e| e.to_string())
}

/// The "installation" half, parses a `SpecialistPackage` JSON document
/// and registers a new specialist from it, with a fresh id and always
/// active regardless of what the package says. Rejects malformed JSON
/// or an unrecognized package format with a clear error rather than
/// guessing.
#[tauri::command]
fn import_specialist_package(state: State<'_, AppState>, package_json: String) -> Result<serde_json::Value, String> {
    let package: solexa_domain::SpecialistPackage =
        serde_json::from_str(&package_json).map_err(|e| format!("not a valid specialist package: {e}"))?;
    let specialist_id = state
        .coordinator
        .import_specialist_package(state.project_id(), package)
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "specialist_id": specialist_id }))
}

/// Serializes a team's AI structure, name, mission, and every AI
/// member's full packaged specialist, into a portable, pretty-printed
/// JSON document. Human members and hierarchy are excluded (see
/// `TeamPackage`'s doc comment).
#[tauri::command]
fn export_team_package(state: State<'_, AppState>, team_id: String) -> Result<String, String> {
    let team_id = uuid::Uuid::parse_str(&team_id).map_err(|e| e.to_string())?;
    let package = state.coordinator.export_team_package(team_id).map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&package).map_err(|e| e.to_string())
}

/// Installs a new team, and every AI member/specialist it packaged,
/// from a `TeamPackage` JSON document.
#[tauri::command]
fn import_team_package(state: State<'_, AppState>, package_json: String) -> Result<serde_json::Value, String> {
    let package: solexa_domain::TeamPackage =
        serde_json::from_str(&package_json).map_err(|e| format!("not a valid team package: {e}"))?;
    let team_id = state
        .coordinator
        .import_team_package(state.project_id(), package)
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "team_id": team_id }))
}

/// Saves a copy of a specialist into the local registry, a personal
/// library independent of any one project (spec's "Specialist
/// Discovery"/"Specialist Marketplace", local-first slice: no hosting,
/// no accounts, no browsing other people's creations).
#[tauri::command]
fn save_specialist_to_registry(state: State<'_, AppState>, specialist_id: String) -> Result<serde_json::Value, String> {
    let specialist_id = uuid::Uuid::parse_str(&specialist_id).map_err(|e| e.to_string())?;
    let entry_id = state
        .coordinator
        .save_specialist_to_registry(state.project_id(), specialist_id)
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "registry_entry_id": entry_id }))
}

/// Saves a copy of a team's AI structure into the local registry.
#[tauri::command]
fn save_team_to_registry(state: State<'_, AppState>, team_id: String) -> Result<serde_json::Value, String> {
    let team_id = uuid::Uuid::parse_str(&team_id).map_err(|e| e.to_string())?;
    let entry_id = state
        .coordinator
        .save_team_to_registry(state.project_id(), team_id)
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "registry_entry_id": entry_id }))
}

/// Lists every entry in the local registry, global, not scoped to the
/// currently open project. Doesn't include each entry's full
/// `package_json` (kept light for a list view); use
/// `get_registry_entry_package_json` to fetch one entry's document,
/// the same JSON `install_from_registry` would install from.
#[tauri::command]
fn list_registry_entries(state: State<'_, AppState>) -> Result<Vec<serde_json::Value>, String> {
    let entries = state.coordinator.list_registry_entries().map_err(|e| e.to_string())?;
    Ok(entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "id": e.id,
                "kind": e.kind,
                "name": e.name,
                "domain": e.domain,
                "capabilities": e.capabilities,
                "added_at": e.added_at.to_rfc3339(),
            })
        })
        .collect())
}

/// Returns one registry entry's full package JSON, the same document
/// `install_from_registry` installs from, displayable the same way a
/// live specialist's/team's export is (copy, download).
#[tauri::command]
fn get_registry_entry_package_json(state: State<'_, AppState>, entry_id: String) -> Result<String, String> {
    let entry_id = uuid::Uuid::parse_str(&entry_id).map_err(|e| e.to_string())?;
    let entry = state
        .coordinator
        .storage()
        .get_registry_entry(entry_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("registry entry {entry_id} not found"))?;
    Ok(entry.package_json)
}

/// Installs a saved registry entry into the currently open project,
/// deserializes its package and hands it to the same
/// `import_specialist_package`/`import_team_package` a pasted or
/// file-loaded package already goes through. Returns the new
/// specialist's or team's id alongside which kind it was, since the
/// frontend can't otherwise tell which list to refresh.
#[tauri::command]
fn install_from_registry(state: State<'_, AppState>, entry_id: String) -> Result<serde_json::Value, String> {
    let entry_id = uuid::Uuid::parse_str(&entry_id).map_err(|e| e.to_string())?;
    let kind = state
        .coordinator
        .storage()
        .get_registry_entry(entry_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("registry entry {entry_id} not found"))?
        .kind;
    let installed_id = state
        .coordinator
        .install_from_registry(state.project_id(), entry_id)
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "kind": kind, "id": installed_id }))
}

/// Permanently removes an entry from the local registry, a genuine
/// delete, not an archive (see `Coordinator::remove_from_registry`'s
/// doc comment on why).
#[tauri::command]
fn remove_from_registry(state: State<'_, AppState>, entry_id: String) -> Result<(), String> {
    let entry_id = uuid::Uuid::parse_str(&entry_id).map_err(|e| e.to_string())?;
    state.coordinator.remove_from_registry(entry_id).map_err(|e| e.to_string())
}

#[tauri::command]
fn list_members(state: State<'_, AppState>) -> Result<Vec<serde_json::Value>, String> {
    let members = state.coordinator.list_members(state.project_id()).map_err(|e| e.to_string())?;
    Ok(members.iter().map(member_to_json).collect())
}

/// Renames a member and/or, for an AI member, swaps which
/// specialist(s) it runs (spec section 11, "Model Swapping"). `member_type`
/// isn't a parameter here at all, `Coordinator::edit_member` always
/// overrides it from the existing stored value regardless of what's
/// constructed below, so the placeholder type here is never actually
/// used; identity (human vs. AI vs. hybrid) isn't something this
/// command can change.
#[tauri::command]
fn update_member(
    state: State<'_, AppState>,
    member_id: String,
    display_name: String,
    description: String,
    specialist_ids: Vec<String>,
) -> Result<(), String> {
    let member_id = uuid::Uuid::parse_str(&member_id).map_err(|e| e.to_string())?;
    let specialist_ids = specialist_ids
        .into_iter()
        .map(|id| uuid::Uuid::parse_str(&id).map_err(|e| e.to_string()))
        .collect::<Result<Vec<_>, _>>()?;

    let updated = solexa_domain::Member {
        id: member_id,
        project_id: state.project_id(),
        display_name,
        description,
        member_type: solexa_domain::MemberType::Ai, // overridden by edit_member regardless
        specialist_ids,
        deleted_at: None, // overridden by edit_member regardless, see its doc comment
        role_ids: Vec::new(), // overridden by edit_member regardless (round 46), see its doc comment
    };

    state
        .coordinator
        .edit_member(state.project_id(), member_id, updated)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn list_teams(state: State<'_, AppState>) -> Result<Vec<serde_json::Value>, String> {
    let teams = state.coordinator.list_teams(state.project_id()).map_err(|e| e.to_string())?;
    Ok(teams
        .iter()
        .map(|t| {
            serde_json::json!({
                "id": t.id,
                "name": t.name,
                "mission": t.mission,
                "member_ids": t.member_ids,
                "parent_team_id": t.parent_team_id,
                "deleted_at": t.deleted_at,
            })
        })
        .collect())
}

/// Renames a team and/or updates its mission statement. `member_ids`
/// and `parent_team_id` aren't parameters, `Coordinator::edit_team`
/// always overrides both from the existing stored row, so this command
/// has no way to change membership (use `add_member_to_team`) or
/// re-parent a team through this path.
#[tauri::command]
fn update_team(state: State<'_, AppState>, team_id: String, name: String, mission: String) -> Result<(), String> {
    let team_id = uuid::Uuid::parse_str(&team_id).map_err(|e| e.to_string())?;
    let updated = solexa_domain::Team::new(state.project_id(), name, mission);
    state
        .coordinator
        .edit_team(state.project_id(), team_id, updated)
        .map_err(|e| e.to_string())
}

/// Registers a new team, `Coordinator::register_team` has existed
/// since Phase 6, but until this round the only way to call it was
/// `seed.rs` at startup, so the desktop app has always had exactly one
/// team. This is the first reachable way to create a second one, which
/// is also what finally makes `set_team_parent` (below) meaningful from
/// the UI, re-parenting needs somewhere else to move a team to.
#[tauri::command]
fn create_team(
    state: State<'_, AppState>,
    name: String,
    mission: String,
    parent_team_id: Option<String>,
) -> Result<serde_json::Value, String> {
    let parent_team_id = parent_team_id
        .map(|id| uuid::Uuid::parse_str(&id).map_err(|e| e.to_string()))
        .transpose()?;
    let mut team = solexa_domain::Team::new(state.project_id(), name, mission);
    if let Some(parent_id) = parent_team_id {
        team = team.nested_under(parent_id);
    }
    let team_id = team.id;
    state.coordinator.register_team(team).map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "team_id": team_id }))
}

/// Moves a team to a new place in the hierarchy, or detaches it to the
/// top level when `new_parent_id` is `None`. Rejected (with a clear
/// error, not a partial update) if it would make the team its own
/// parent or its own descendant's descendant, see
/// `Coordinator::set_team_parent`'s cycle check.
#[tauri::command]
fn set_team_parent(state: State<'_, AppState>, team_id: String, new_parent_id: Option<String>) -> Result<(), String> {
    let team_id = uuid::Uuid::parse_str(&team_id).map_err(|e| e.to_string())?;
    let new_parent_id = new_parent_id
        .map(|id| uuid::Uuid::parse_str(&id).map_err(|e| e.to_string()))
        .transpose()?;
    state
        .coordinator
        .set_team_parent(state.project_id(), team_id, new_parent_id)
        .map_err(|e| e.to_string())
}

/// Adds an existing, already-registered member to a team's roster,
/// the same `Coordinator::add_member_to_team` `create_specialist` has
/// always used internally to join a newly created specialist's member
/// to a team, now reachable as its own action instead of only ever
/// firing as a side effect of creating something else.
#[tauri::command]
fn add_member_to_team(state: State<'_, AppState>, team_id: String, member_id: String) -> Result<(), String> {
    let team_id = uuid::Uuid::parse_str(&team_id).map_err(|e| e.to_string())?;
    let member_id = uuid::Uuid::parse_str(&member_id).map_err(|e| e.to_string())?;
    state
        .coordinator
        .add_member_to_team(team_id, member_id)
        .map_err(|e| e.to_string())
}

/// The inverse of `add_member_to_team`, removes a member from a
/// team's roster without deleting the member.
#[tauri::command]
fn remove_member_from_team(state: State<'_, AppState>, team_id: String, member_id: String) -> Result<(), String> {
    let team_id = uuid::Uuid::parse_str(&team_id).map_err(|e| e.to_string())?;
    let member_id = uuid::Uuid::parse_str(&member_id).map_err(|e| e.to_string())?;
    state
        .coordinator
        .remove_member_from_team(team_id, member_id)
        .map_err(|e| e.to_string())
}

// ---- soft delete (round 27) -------------------------------------------

#[tauri::command]
fn delete_member(state: State<'_, AppState>, member_id: String) -> Result<(), String> {
    let member_id = uuid::Uuid::parse_str(&member_id).map_err(|e| e.to_string())?;
    state.coordinator.delete_member(member_id).map_err(|e| e.to_string())
}

#[tauri::command]
fn restore_member(state: State<'_, AppState>, member_id: String) -> Result<(), String> {
    let member_id = uuid::Uuid::parse_str(&member_id).map_err(|e| e.to_string())?;
    state.coordinator.restore_member(member_id).map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_team(state: State<'_, AppState>, team_id: String) -> Result<(), String> {
    let team_id = uuid::Uuid::parse_str(&team_id).map_err(|e| e.to_string())?;
    state.coordinator.delete_team(team_id).map_err(|e| e.to_string())
}

#[tauri::command]
fn restore_team(state: State<'_, AppState>, team_id: String) -> Result<(), String> {
    let team_id = uuid::Uuid::parse_str(&team_id).map_err(|e| e.to_string())?;
    state.coordinator.restore_team(team_id).map_err(|e| e.to_string())
}

#[tauri::command]
fn list_conflicts(state: State<'_, AppState>) -> Result<Vec<serde_json::Value>, String> {
    let conflicts = state.coordinator.list_conflicts(state.project_id()).map_err(|e| e.to_string())?;
    Ok(conflicts.iter().map(conflict_to_json).collect())
}

#[tauri::command]
fn resolve_conflict(
    state: State<'_, AppState>,
    conflict_id: String,
    chosen_specialist_id: String,
    approving_member_id: String,
) -> Result<(), String> {
    let conflict_id = uuid::Uuid::parse_str(&conflict_id).map_err(|e| e.to_string())?;
    let chosen_specialist_id = uuid::Uuid::parse_str(&chosen_specialist_id).map_err(|e| e.to_string())?;
    let approving_member_id = uuid::Uuid::parse_str(&approving_member_id).map_err(|e| e.to_string())?;
    state
        .coordinator
        .resolve_conflict(conflict_id, chosen_specialist_id, approving_member_id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn resolve_sync_conflict(
    state: State<'_, AppState>,
    conflict_id: String,
    chosen_specialist_id: String,
    approving_member_id: String,
) -> Result<(), String> {
    let conflict_id = uuid::Uuid::parse_str(&conflict_id).map_err(|e| e.to_string())?;
    let chosen_specialist_id = uuid::Uuid::parse_str(&chosen_specialist_id).map_err(|e| e.to_string())?;
    let approving_member_id = uuid::Uuid::parse_str(&approving_member_id).map_err(|e| e.to_string())?;
    state
        .coordinator
        .resolve_sync_conflict(conflict_id, chosen_specialist_id, approving_member_id)
        .map_err(|e| e.to_string())
}

// ---- GitHub PR mirroring (round 30, Phase 8) -------------------------

#[tauri::command]
fn send_conflict_to_github(state: State<'_, AppState>, conflict_id: String) -> Result<String, String> {
    let conflict_id = uuid::Uuid::parse_str(&conflict_id).map_err(|e| e.to_string())?;
    state.coordinator.send_conflict_to_github(conflict_id).map_err(|e| e.to_string())
}

#[tauri::command]
fn check_conflict_github_pr_status(
    state: State<'_, AppState>,
    conflict_id: String,
    checking_member_id: String,
) -> Result<String, String> {
    let conflict_id = uuid::Uuid::parse_str(&conflict_id).map_err(|e| e.to_string())?;
    let checking_member_id = uuid::Uuid::parse_str(&checking_member_id).map_err(|e| e.to_string())?;
    let outcome = state
        .coordinator
        .check_conflict_github_pr_status(conflict_id, checking_member_id)
        .map_err(|e| e.to_string())?;
    Ok(github_pr_check_outcome_to_str(outcome).to_string())
}

fn github_pr_check_outcome_to_str(outcome: solexa_coordinator::GithubPrCheckOutcome) -> &'static str {
    match outcome {
        solexa_coordinator::GithubPrCheckOutcome::StillOpen => "still_open",
        solexa_coordinator::GithubPrCheckOutcome::ResolvedAcceptingLocal => "resolved_accepting_local",
        solexa_coordinator::GithubPrCheckOutcome::ResolvedAcceptingRemote => "resolved_accepting_remote",
        solexa_coordinator::GithubPrCheckOutcome::AlreadyResolved => "already_resolved",
    }
}

/// Round 32: the automatic "check every open, GitHub-mirrored
/// conflict" sweep, the counterpart callers use right after
/// `pull_from_git` instead of clicking "Check PR status" per conflict.
#[tauri::command]
fn check_all_conflict_github_pr_statuses(state: State<'_, AppState>, checking_member_id: String) -> Result<serde_json::Value, String> {
    let checking_member_id = uuid::Uuid::parse_str(&checking_member_id).map_err(|e| e.to_string())?;
    let sweep = state
        .coordinator
        .check_all_conflict_github_pr_statuses(state.project_id(), checking_member_id)
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({
        "checked": sweep.checked.into_iter().map(|(id, outcome)| serde_json::json!({
            "conflict_id": id,
            "outcome": github_pr_check_outcome_to_str(outcome),
        })).collect::<Vec<_>>(),
        "errors": sweep.errors.into_iter().map(|(id, message)| serde_json::json!({
            "conflict_id": id,
            "message": message,
        })).collect::<Vec<_>>(),
    }))
}

// ---- git sync (round 22, Phase 8) ------------------------------------

#[tauri::command]
fn link_git_repo(state: State<'_, AppState>, repo_path: String) -> Result<(), String> {
    state.coordinator.link_git_repo(state.project_id(), repo_path).map_err(|e| e.to_string())
}

#[tauri::command]
fn unlink_git_repo(state: State<'_, AppState>) -> Result<(), String> {
    state.coordinator.unlink_git_repo(state.project_id()).map_err(|e| e.to_string())
}

#[tauri::command]
fn get_git_sync_status(state: State<'_, AppState>) -> Result<Option<serde_json::Value>, String> {
    let status = state.coordinator.git_sync_status(state.project_id()).map_err(|e| e.to_string())?;
    Ok(status.map(|s| {
        serde_json::json!({
            "repo_path": s.config.repo_path,
            "linked_at": s.config.linked_at,
            "last_synced_commit": s.config.last_synced_commit,
            "last_synced_at": s.config.last_synced_at,
            "local_dirty": s.local_dirty,
            "require_review_for_incoming_changes": s.config.require_review_for_incoming_changes,
        })
    }))
}

/// Round 46: toggles the review gate. Requires a linked repo (the
/// underlying `GitSyncConfig` is where the flag lives), link one
/// first via `link_git_repo`.
#[tauri::command]
fn set_require_review_for_incoming_changes(state: State<'_, AppState>, enabled: bool) -> Result<(), String> {
    state
        .coordinator
        .set_require_review_for_incoming_changes(state.project_id(), enabled)
        .map_err(|e| e.to_string())
}

/// Round 55: whether this project enforces `Permission::Approve` on
/// deciding a Decision and resolving a Conflict. Off by default.
#[tauri::command]
fn get_enforce_approver_roles(state: State<'_, AppState>) -> Result<bool, String> {
    state.coordinator.enforce_approver_roles(state.project_id()).map_err(|e| e.to_string())
}

/// Round 55: turns that enforcement on or off. Turning it ON errors if no
/// member holds `Permission::Approve` (it would leave nothing
/// resolvable); turning it OFF always works.
#[tauri::command]
fn set_enforce_approver_roles(state: State<'_, AppState>, enabled: bool) -> Result<(), String> {
    state
        .coordinator
        .set_enforce_approver_roles(state.project_id(), enabled)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn list_pending_sync_changes(state: State<'_, AppState>) -> Result<Vec<serde_json::Value>, String> {
    let changes = state.coordinator.list_pending_sync_changes(state.project_id()).map_err(|e| e.to_string())?;
    Ok(changes.iter().map(pending_sync_change_to_json).collect())
}

#[tauri::command]
fn apply_pending_sync_change(state: State<'_, AppState>, pending_change_id: String, deciding_member_id: String) -> Result<(), String> {
    let pending_change_id = uuid::Uuid::parse_str(&pending_change_id).map_err(|e| e.to_string())?;
    let deciding_member_id = uuid::Uuid::parse_str(&deciding_member_id).map_err(|e| e.to_string())?;
    state.coordinator.apply_pending_sync_change(pending_change_id, deciding_member_id).map_err(|e| e.to_string())
}

/// `reason` is optional but, unlike closing a GitHub PR, gives whoever
/// denied it a real place to say why (round 46).
#[tauri::command]
fn deny_pending_sync_change(state: State<'_, AppState>, pending_change_id: String, deciding_member_id: String, reason: Option<String>) -> Result<(), String> {
    let pending_change_id = uuid::Uuid::parse_str(&pending_change_id).map_err(|e| e.to_string())?;
    let deciding_member_id = uuid::Uuid::parse_str(&deciding_member_id).map_err(|e| e.to_string())?;
    state.coordinator.deny_pending_sync_change(pending_change_id, deciding_member_id, reason).map_err(|e| e.to_string())
}

// ---- roles (round 46) -------------------------------------------------
//
// "Discord roles" for SOLEXA, see solexa-domain::role's doc comment
// for the full rationale and this round's deliberate scope boundary.

fn parse_permissions(permissions: Vec<String>) -> Result<Vec<solexa_domain::Permission>, String> {
    permissions
        .into_iter()
        .map(|p| {
            serde_json::from_value::<solexa_domain::Permission>(serde_json::Value::String(p.clone()))
                .map_err(|_| format!("\"{p}\" is not a valid permission"))
        })
        .collect()
}

#[tauri::command]
fn create_role(state: State<'_, AppState>, name: String, permissions: Vec<String>) -> Result<serde_json::Value, String> {
    let permissions = parse_permissions(permissions)?;
    let role = state.coordinator.create_role(state.project_id(), name, permissions).map_err(|e| e.to_string())?;
    Ok(role_to_json(&role))
}

#[tauri::command]
fn update_role(state: State<'_, AppState>, role_id: String, name: String, permissions: Vec<String>) -> Result<(), String> {
    let role_id = uuid::Uuid::parse_str(&role_id).map_err(|e| e.to_string())?;
    let permissions = parse_permissions(permissions)?;
    state.coordinator.update_role(role_id, name, permissions).map_err(|e| e.to_string())
}

#[tauri::command]
fn list_roles(state: State<'_, AppState>) -> Result<Vec<serde_json::Value>, String> {
    let roles = state.coordinator.list_roles(state.project_id()).map_err(|e| e.to_string())?;
    Ok(roles.iter().map(role_to_json).collect())
}

#[tauri::command]
fn assign_role(state: State<'_, AppState>, member_id: String, role_id: String) -> Result<(), String> {
    let member_id = uuid::Uuid::parse_str(&member_id).map_err(|e| e.to_string())?;
    let role_id = uuid::Uuid::parse_str(&role_id).map_err(|e| e.to_string())?;
    state.coordinator.assign_role(member_id, role_id).map_err(|e| e.to_string())
}

#[tauri::command]
fn unassign_role(state: State<'_, AppState>, member_id: String, role_id: String) -> Result<(), String> {
    let member_id = uuid::Uuid::parse_str(&member_id).map_err(|e| e.to_string())?;
    let role_id = uuid::Uuid::parse_str(&role_id).map_err(|e| e.to_string())?;
    state.coordinator.unassign_role(member_id, role_id).map_err(|e| e.to_string())
}

/// Round 47: extends the role system to Specialist, additive to, never
/// a replacement for, its own separately-tested `permissions` field.
#[tauri::command]
fn assign_role_to_specialist(state: State<'_, AppState>, specialist_id: String, role_id: String) -> Result<(), String> {
    let specialist_id = uuid::Uuid::parse_str(&specialist_id).map_err(|e| e.to_string())?;
    let role_id = uuid::Uuid::parse_str(&role_id).map_err(|e| e.to_string())?;
    state.coordinator.assign_role_to_specialist(specialist_id, role_id).map_err(|e| e.to_string())
}

#[tauri::command]
fn unassign_role_from_specialist(state: State<'_, AppState>, specialist_id: String, role_id: String) -> Result<(), String> {
    let specialist_id = uuid::Uuid::parse_str(&specialist_id).map_err(|e| e.to_string())?;
    let role_id = uuid::Uuid::parse_str(&role_id).map_err(|e| e.to_string())?;
    state.coordinator.unassign_role_from_specialist(specialist_id, role_id).map_err(|e| e.to_string())
}

#[tauri::command]
fn push_to_git(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let result = state.coordinator.push_to_git(state.project_id()).map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "pushed": result.pushed, "commit": result.commit }))
}

#[tauri::command]
fn pull_from_git(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let result = state.coordinator.pull_from_git(state.project_id()).map_err(|e| e.to_string())?;
    Ok(serde_json::json!({
        "commit": result.commit,
        "had_remote_changes": result.had_remote_changes,
        "members_applied": result.members_applied,
        "teams_applied": result.teams_applied,
        "specialists_applied": result.specialists_applied,
        "knowledge_items_applied": result.knowledge_items_applied,
        "decisions_applied": result.decisions_applied,
        "conflicts_created": result.conflicts_created,
        "pending_changes_created": result.pending_changes_created,
        "github_prs_created": result.github_prs_created,
        "github_pr_errors": result.github_pr_errors,
    }))
}

#[tauri::command]
fn set_task_priority(state: State<'_, AppState>, task_id: String, priority: u8) -> Result<(), String> {
    let task_id = uuid::Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    state.coordinator.set_task_priority(task_id, priority).map_err(|e| e.to_string())
}

/// Round 44: `deadline` is an RFC 3339 timestamp string to set, or
/// `None`/omitted to clear an existing deadline, mirrors
/// `Task.deadline`'s own `Option` shape rather than needing a separate
/// clear command.
#[tauri::command]
fn set_task_deadline(state: State<'_, AppState>, task_id: String, deadline: Option<String>) -> Result<(), String> {
    let task_id = uuid::Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let deadline = deadline
        .map(|d| chrono::DateTime::parse_from_rfc3339(&d).map(|dt| dt.with_timezone(&chrono::Utc)))
        .transpose()
        .map_err(|e| e.to_string())?;
    state.coordinator.set_task_deadline(task_id, deadline).map_err(|e| e.to_string())
}

#[tauri::command]
fn list_knowledge_items(state: State<'_, AppState>) -> Result<Vec<serde_json::Value>, String> {
    let items = state.coordinator.list_knowledge(state.project_id()).map_err(|e| e.to_string())?;
    Ok(items
        .iter()
        .map(|item| {
            serde_json::json!({
                "id": item.id,
                "kind": item.kind,
                "title": item.title,
                "content": item.content,
                "status": item.status,
                "tags": item.tags,
                "created_at": item.created_at,
            })
        })
        .collect())
}

/// Adds a fact/constraint/artifact to the Project Brain from the UI.
/// `kind` is one of "fact" | "constraint" | "artifact" (matching
/// `solexa_domain::KnowledgeKind`'s serde representation); provenance
/// is always `Provenance::human("you")` since only a human can add one
/// through this command today, an AI specialist proposing a fact is a
/// real future feature, not something this command pretends to do.
#[tauri::command]
fn add_knowledge_item(
    state: State<'_, AppState>,
    kind: String,
    title: String,
    content: String,
    tags: Vec<String>,
) -> Result<(), String> {
    let kind: solexa_domain::KnowledgeKind = serde_json::from_value(serde_json::Value::String(kind))
        .map_err(|_| "kind must be one of \"fact\", \"constraint\", or \"artifact\"".to_string())?;
    if title.trim().is_empty() || content.trim().is_empty() {
        return Err("title and content must not be empty".into());
    }
    let item = solexa_domain::KnowledgeItem::new(
        state.project_id(),
        kind,
        title,
        content,
        solexa_domain::Provenance::human("you"),
        tags,
    );
    state.coordinator.record_knowledge(item).map_err(|e| e.to_string())
}

#[tauri::command]
fn archive_knowledge_item(state: State<'_, AppState>, item_id: String) -> Result<(), String> {
    let item_id = uuid::Uuid::parse_str(&item_id).map_err(|e| e.to_string())?;
    state.coordinator.archive_knowledge_item(item_id).map_err(|e| e.to_string())
}

#[tauri::command]
fn list_decisions(state: State<'_, AppState>) -> Result<Vec<serde_json::Value>, String> {
    let decisions = state.coordinator.list_decisions(state.project_id()).map_err(|e| e.to_string())?;
    Ok(decisions
        .iter()
        .map(|d| {
            serde_json::json!({
                "id": d.id,
                "question": d.question,
                "options": d.options,
                "resolution": d.resolution,
                "approved_by": d.approved_by,
                "status": d.status,
                "tags": d.tags,
                "created_at": d.created_at,
                "github_issue_url": d.github_issue_url,
            })
        })
        .collect())
}

#[tauri::command]
fn add_decision(
    state: State<'_, AppState>,
    question: String,
    options: Vec<String>,
    tags: Vec<String>,
) -> Result<(), String> {
    if question.trim().is_empty() || options.len() < 2 {
        return Err("a decision needs a question and at least two options".into());
    }
    let decision = solexa_domain::Decision::new(state.project_id(), question, options, tags);
    state.coordinator.record_decision(decision).map_err(|e| e.to_string())
}

#[tauri::command]
fn resolve_decision(
    state: State<'_, AppState>,
    decision_id: String,
    resolution: String,
    approving_member_id: String,
) -> Result<(), String> {
    let decision_id = uuid::Uuid::parse_str(&decision_id).map_err(|e| e.to_string())?;
    let approving_member_id = uuid::Uuid::parse_str(&approving_member_id).map_err(|e| e.to_string())?;
    state
        .coordinator
        .resolve_decision(decision_id, resolution, approving_member_id)
        .map_err(|e| e.to_string())
}

/// Round 33: mirrors a recorded Decision as a GitHub Issue.
#[tauri::command]
fn send_decision_to_github(state: State<'_, AppState>, decision_id: String) -> Result<String, String> {
    let decision_id = uuid::Uuid::parse_str(&decision_id).map_err(|e| e.to_string())?;
    state.coordinator.send_decision_to_github(decision_id).map_err(|e| e.to_string())
}

fn member_to_json(member: &solexa_domain::Member) -> serde_json::Value {
    serde_json::json!({
        "id": member.id,
        "display_name": member.display_name,
        "description": member.description,
        "member_type": member.member_type,
        "specialist_ids": member.specialist_ids,
        "deleted_at": member.deleted_at,
        "role_ids": member.role_ids,
    })
}

fn role_to_json(role: &solexa_domain::Role) -> serde_json::Value {
    serde_json::json!({
        "id": role.id,
        "name": role.name,
        "permissions": role.permissions,
        "created_at": role.created_at,
    })
}

fn pending_sync_change_to_json(change: &solexa_domain::PendingSyncChange) -> serde_json::Value {
    serde_json::json!({
        "id": change.id,
        "kind": change.kind,
        "object_id": change.object_id,
        "label": change.label,
        "is_new": change.is_new,
        "source_commit": change.source_commit,
        "discovered_at": change.discovered_at,
        "status": change.status,
        "decided_by": change.decided_by,
        "decided_at": change.decided_at,
        "denial_reason": change.denial_reason,
    })
}

fn conflict_to_json(conflict: &solexa_domain::Conflict) -> serde_json::Value {
    serde_json::json!({
        "id": conflict.id,
        "task_id": conflict.task_id,
        "question": conflict.question,
        "positions": conflict.positions,
        "resolution": conflict.resolution,
        "approved_by": conflict.approved_by,
        "status": conflict.status,
        "created_at": conflict.created_at,
        "sync_object": conflict.sync_object,
        "github_pr_url": conflict.github_pr_url,
    })
}

fn task_to_json(task: &solexa_domain::Task) -> serde_json::Value {
    serde_json::json!({
        "id": task.id,
        "title": task.title,
        "description": task.description,
        "state": task.state,
        "assigned_specialist_id": task.assigned_specialist_id,
        "required_capabilities": task.required_capabilities,
        "priority": task.priority,
        "deadline": task.deadline,
        "result": task.result,
        "confidence": task.confidence,
        "verification": task.verification,
        "created_at": task.created_at,
        "updated_at": task.updated_at,
    })
}

fn event_to_json(event: &Event) -> serde_json::Value {
    serde_json::json!({
        "event_id": event.event_id,
        "timestamp": event.timestamp,
        "actor_id": event.actor_id,
        "actor_type": event.actor_type,
        "event_type": event.event_type,
        "payload": event.payload,
        "correlation_id": event.correlation_id,
        "causation_id": event.causation_id,
    })
}

// ---- settings, tray, startup (round 67) -------------------------------
//
// The UI keeps its own copy of these in localStorage for a fast first
// paint, but SQLite is the source of truth: the tray and the window's
// close/minimize handlers read them here, and they survive a reinstall
// as long as the app data folder does.

const APP_SETTING_KEYS: [&str; 7] = [
    "ui.theme_id",
    "ui.theme_accent",
    "ui.custom_themes",
    "ui.language",
    "ui.close_to_tray",
    "ui.minimize_to_tray",
    "ui.startup_mode",
];

fn setting_string(state: &AppState, key: &str) -> Option<String> {
    state.coordinator.storage().get_setting(key).ok().flatten()
}

fn setting_bool(state: &AppState, key: &str) -> bool {
    matches!(setting_string(state, key).as_deref(), Some("true"))
}

/// 32x32 straight-alpha mask of the SOLEXA logo, generated from
/// `solexa.png` by `scripts/generate_app_icons.py`. The tray icon is
/// tinted with the active theme's accent colour at runtime, so a custom
/// theme gets a matching tray icon without shipping one PNG per theme.
const TRAY_MASK: &[u8] = include_bytes!("../icons/tray-mask.rgba");

fn parse_hex_rgb(hex: &str) -> (u8, u8, u8) {
    let digits = hex.trim().trim_start_matches('#');
    if digits.len() == 6 && digits.is_ascii() {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&digits[0..2], 16),
            u8::from_str_radix(&digits[2..4], 16),
            u8::from_str_radix(&digits[4..6], 16),
        ) {
            return (r, g, b);
        }
    }
    (0xE3, 0xA8, 0x57)
}

fn tray_icon(accent: &str) -> tauri::image::Image<'static> {
    let (r, g, b) = parse_hex_rgb(accent);
    let mut rgba = Vec::with_capacity(TRAY_MASK.len());
    for pixel in TRAY_MASK.chunks_exact(4) {
        rgba.extend_from_slice(&[r, g, b, pixel[3]]);
    }
    tauri::image::Image::new_owned(rgba, 32, 32)
}

fn tray_labels(language: &str) -> (&'static str, &'static str) {
    match language {
        "de" => ("SOLEXA öffnen", "Beenden"),
        "fa" => ("باز کردن سولکسا", "خروج"),
        _ => ("Open SOLEXA", "Quit"),
    }
}

fn build_tray_menu(app: &tauri::AppHandle, language: &str) -> tauri::Result<Menu<tauri::Wry>> {
    let (open_label, quit_label) = tray_labels(language);
    let open = MenuItem::with_id(app, "tray-open", open_label, true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "tray-quit", quit_label, true, None::<&str>)?;
    Menu::with_items(app, &[&open, &quit])
}

fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

#[tauri::command]
fn get_app_settings(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let mut values = serde_json::Map::new();
    for key in APP_SETTING_KEYS {
        if let Some(value) = state
            .coordinator
            .storage()
            .get_setting(key)
            .map_err(|e| e.to_string())?
        {
            values.insert(key.to_string(), serde_json::Value::String(value));
        }
    }
    Ok(serde_json::Value::Object(values))
}

/// Persists one UI setting. `ui.theme_accent` also retints the tray icon
/// immediately, so switching themes visibly changes the logo in the tray.
#[tauri::command]
fn set_app_setting(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    key: String,
    value: String,
) -> Result<(), String> {
    if !APP_SETTING_KEYS.contains(&key.as_str()) {
        return Err(format!("unknown setting: {key}"));
    }
    state
        .coordinator
        .storage()
        .set_setting(&key, &value)
        .map_err(|e| e.to_string())?;
    if key == "ui.theme_accent" {
        if let Some(tray) = app.tray_by_id("main") {
            if let Err(err) = tray.set_icon(Some(tray_icon(&value))) {
                tracing::warn!("could not retint the tray icon: {err}");
            }
        }
    }
    Ok(())
}

/// Enables or disables launch at login. The autostart registration always
/// passes `--autostart`; whether that means "show the window" or "stay in
/// the tray" is decided at startup from `ui.startup_mode`, so the two
/// choices can share one registration.
#[tauri::command]
fn set_startup_mode(app: tauri::AppHandle, mode: String) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt as _;
    let manager = app.autolaunch();
    match mode.as_str() {
        "off" => {
            if manager.is_enabled().unwrap_or(false) {
                manager.disable().map_err(|e| e.to_string())?;
            }
            Ok(())
        }
        "app" | "tray" => manager.enable().map_err(|e| e.to_string()),
        _ => Err("startup mode must be off, app or tray".into()),
    }
}

#[tauri::command]
fn startup_enabled(app: tauri::AppHandle) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt as _;
    app.autolaunch().is_enabled().map_err(|e| e.to_string())
}

#[tauri::command]
fn update_tray_menu(app: tauri::AppHandle, language: String) -> Result<(), String> {
    let menu = build_tray_menu(&app, &language).map_err(|e| e.to_string())?;
    if let Some(tray) = app.tray_by_id("main") {
        tray.set_menu(Some(menu)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--autostart"]),
        ))
        .on_window_event(|window, event| match event {
            WindowEvent::CloseRequested { api, .. } => {
                let app = window.app_handle();
                let state = app.state::<AppState>();
                if !ALLOW_EXIT.load(Ordering::SeqCst) && setting_bool(&state, "ui.close_to_tray") {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
            WindowEvent::Resized(_) => {
                let app = window.app_handle();
                let state = app.state::<AppState>();
                if setting_bool(&state, "ui.minimize_to_tray")
                    && window.is_minimized().unwrap_or(false)
                {
                    let _ = window.hide();
                }
            }
            _ => {}
        })
        .setup(|app| {
            let app_state = state::build(app.handle())?;
            let language = setting_string(&app_state, "ui.language").unwrap_or_else(|| "en".into());
            let accent =
                setting_string(&app_state, "ui.theme_accent").unwrap_or_else(|| "#e3a857".into());
            let startup_mode =
                setting_string(&app_state, "ui.startup_mode").unwrap_or_else(|| "off".into());

            // Forward every published event straight to the window as
            // it happens, so the Execution Trace panel updates live
            // instead of polling. For almost every event type, the bus
            // already persisted it to SQLite via its EventSink before
            // this receiver ever sees it (see state::build), the live
            // stream is purely additive, never the only record of what
            // happened. The one exception (round 39) is
            // `ModelTokenGenerated`: those are published via
            // `publish_ephemeral`, which deliberately skips every
            // durable sink, for that event type specifically, this
            // live stream genuinely is the only place it ever exists.
            let mut live = app_state.coordinator_events.subscribe();
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                while let Ok(event) = live.recv().await {
                    let _ = handle.emit("solexa://event", event_to_json(&event));
                }
            });

            app.manage(app_state);

            // Tray icon: same logo as the window, tinted with the active
            // theme accent. Failure is not fatal (some sessions simply
            // cannot register one); the window itself still works.
            match build_tray_menu(app.handle(), &language) {
                Ok(menu) => {
                    let built = TrayIconBuilder::with_id("main")
                        .icon(tray_icon(&accent))
                        .tooltip("SOLEXA")
                        .menu(&menu)
                        .show_menu_on_left_click(false)
                        .on_menu_event(|app, event| match event.id.as_ref() {
                            "tray-open" => show_main_window(app),
                            "tray-quit" => {
                                ALLOW_EXIT.store(true, Ordering::SeqCst);
                                app.exit(0);
                            }
                            _ => {}
                        })
                        .on_tray_icon_event(|tray, event| {
                            if let TrayIconEvent::Click {
                                button: MouseButton::Left,
                                button_state: MouseButtonState::Up,
                                ..
                            } = event
                            {
                                let app = tray.app_handle();
                                match app.get_webview_window("main") {
                                    Some(window) if window.is_visible().unwrap_or(false) => {
                                        let _ = window.hide();
                                    }
                                    _ => show_main_window(app),
                                }
                            }
                        })
                        .build(app.handle());
                    if let Err(err) = built {
                        tracing::warn!("could not create the tray icon: {err}");
                    }
                }
                Err(err) => tracing::warn!("could not build the tray menu: {err}"),
            }

            // "Run at startup in tray" means the same autostart entry as
            // "run at startup"; only this decides whether to show the
            // window. `--hidden` exists for manual testing.
            let args: Vec<String> = std::env::args().collect();
            let autostart = args.iter().any(|arg| arg == "--autostart");
            let explicit_hidden = args.iter().any(|arg| arg == "--hidden");
            if explicit_hidden || (autostart && startup_mode == "tray") {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_projects,
            create_project,
            switch_project,
            submit_request,
            list_tasks,
            get_task,
            get_trace,
            list_specialists,
            list_models,
            list_models_with_status,
            set_model_enabled,
            cancel_task,
            get_resource_snapshot,
            set_resource_budget,
            reset_resource_budget,
            create_specialist,
            update_specialist,
            set_specialist_active,
            export_specialist_package,
            import_specialist_package,
            export_team_package,
            import_team_package,
            save_specialist_to_registry,
            save_team_to_registry,
            list_registry_entries,
            get_registry_entry_package_json,
            install_from_registry,
            remove_from_registry,
            list_members,
            update_member,
            list_teams,
            update_team,
            create_team,
            set_team_parent,
            add_member_to_team,
            remove_member_from_team,
            delete_member,
            restore_member,
            delete_team,
            restore_team,
            list_conflicts,
            resolve_conflict,
            resolve_sync_conflict,
            send_conflict_to_github,
            check_conflict_github_pr_status,
            check_all_conflict_github_pr_statuses,
            link_git_repo,
            unlink_git_repo,
            get_git_sync_status,
            set_require_review_for_incoming_changes,
            get_enforce_approver_roles,
            set_enforce_approver_roles,
            list_pending_sync_changes,
            apply_pending_sync_change,
            deny_pending_sync_change,
            create_role,
            update_role,
            list_roles,
            assign_role,
            unassign_role,
            assign_role_to_specialist,
            unassign_role_from_specialist,
            push_to_git,
            pull_from_git,
            set_task_priority,
            set_task_deadline,
            list_knowledge_items,
            add_knowledge_item,
            list_decisions,
            add_decision,
            resolve_decision,
            send_decision_to_github,
            archive_knowledge_item,
            get_app_settings,
            set_app_setting,
            set_startup_mode,
            startup_enabled,
            update_tray_menu,
        ])
        .run(tauri::generate_context!())
        .expect("error while running SOLEXA desktop");
}
