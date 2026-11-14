//! The Planner, turns a raw user request into task(s).
//!
//! Phase 1 scope was one request -> one task. Phase 2 ("Multiple
//! Specialists", per the roadmap) extends this into real decomposition:
//! when a request implies more than one distinct capability, the
//! planner returns `[parent, subtask_1, subtask_2, ...]`, one subtask
//! per capability, each independently routed, executed, and verified by
//! the coordinator, with the parent's result assembled from all of
//! them. A request needing only one capability still gets exactly one
//! task, unchanged from Phase 1 (`plan()` always returns at least one
//! task; callers branch on `len() == 1` vs `> 1`).
//!
//! `infer_capabilities` remains a placeholder for what a real planner
//! would do with a reasoning model: a keyword heuristic, not an attempt
//! at real NLU, see the note on each function below.

use crate::UserRequest;
use solexa_domain::Task;

#[derive(Default)]
pub struct Planner;

impl Planner {
    pub fn plan(&self, request: &UserRequest) -> Vec<Task> {
        let capabilities = infer_capabilities(&request.text);

        if capabilities.len() <= 1 {
            let mut task = Task::new(request.project_id, title_from(&request.text), request.text.clone());
            task.required_capabilities = capabilities;
            return vec![task];
        }

        // More than one distinct capability implied: decompose into one
        // subtask per capability under a parent task that represents
        // the whole request. The parent's own required_capabilities is
        // the union, useful for anyone inspecting the parent alone,
        // but the coordinator never assigns a specialist to the parent
        // itself; it aggregates its subtasks' outcomes instead.
        let mut parent = Task::new(request.project_id, title_from(&request.text), request.text.clone());
        parent.required_capabilities = capabilities.clone();
        let parent_id = parent.id;

        let mut tasks = vec![parent];
        for capability in capabilities {
            let mut subtask = Task::new(
                request.project_id,
                format!("{title}, {capability}", title = title_from(&request.text)),
                request.text.clone(),
            );
            subtask.required_capabilities = vec![capability];
            subtask.parent_task_id = Some(parent_id);
            tasks.push(subtask);
        }
        tasks
    }
}

fn title_from(text: &str) -> String {
    let first_line = text.lines().next().unwrap_or(text).trim();
    if first_line.chars().count() > 80 {
        format!("{}...", first_line.chars().take(77).collect::<String>())
    } else if first_line.is_empty() {
        "Untitled request".to_string()
    } else {
        first_line.to_string()
    }
}

/// Naive keyword -> capability heuristic. Deliberately simple: this is
/// standing in for what should eventually be a reasoning-model planner
/// call, not an attempt at real NLU. Returns capabilities in the order
/// their first keyword appears, deduplicated; falls back to `general`
/// when nothing matches.
fn infer_capabilities(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let table: &[(&str, &str)] = &[
        ("rust", "backend"),
        ("backend", "backend"),
        ("api", "backend"),
        ("sql", "sql"),
        ("database", "sql"),
        ("migration", "sql"),
        ("design", "design"),
        ("ui", "design"),
        ("logo", "design"),
        ("typography", "design"),
        ("security", "security"),
        ("audio", "audio"),
        ("test", "qa"),
        ("bug", "qa"),
    ];

    let mut caps: Vec<String> = table
        .iter()
        .filter(|(keyword, _)| lower.contains(keyword))
        .map(|(_, cap)| cap.to_string())
        .collect();
    caps.dedup();

    if caps.is_empty() {
        caps.push("general".to_string());
    }
    caps
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn request(text: &str) -> UserRequest {
        UserRequest {
            project_id: Uuid::new_v4(),
            text: text.into(),
            requested_by: "human".into(),
        }
    }

    #[test]
    fn single_capability_request_yields_exactly_one_task() {
        let tasks = Planner.plan(&request("Refactor the Rust backend auth module."));
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].required_capabilities, vec!["backend".to_string()]);
        assert!(tasks[0].parent_task_id.is_none());
    }

    #[test]
    fn falls_back_to_general_capability() {
        let tasks = Planner.plan(&request("Say hello to the team."));
        assert_eq!(tasks[0].required_capabilities, vec!["general".to_string()]);
    }

    #[test]
    fn multi_capability_request_decomposes_into_parent_plus_subtasks() {
        let tasks = Planner.plan(&request(
            "Refactor the Rust backend and write the SQL migration for the new auth table.",
        ));
        assert_eq!(tasks.len(), 3, "parent + backend subtask + sql subtask");

        let parent = &tasks[0];
        assert!(parent.parent_task_id.is_none());
        assert_eq!(parent.required_capabilities, vec!["backend".to_string(), "sql".to_string()]);

        let subtasks = &tasks[1..];
        assert!(subtasks.iter().all(|t| t.parent_task_id == Some(parent.id)));
        assert!(subtasks.iter().any(|t| t.required_capabilities == vec!["backend".to_string()]));
        assert!(subtasks.iter().any(|t| t.required_capabilities == vec!["sql".to_string()]));
    }
}
