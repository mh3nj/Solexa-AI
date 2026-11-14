import { useCallback, useEffect, useRef, useState } from "react";
import {
  getTrace,
  listConflicts,
  listDecisions,
  listKnowledgeItems,
  listMembers,
  listSpecialists,
  listTasks,
  listTeams,
  saveSpecialistToRegistry,
  saveTeamToRegistry,
  setSpecialistActive,
  subscribeToEvents,
} from "./lib/api";
import type {
  ConflictDto,
  DecisionDto,
  EventDto,
  KnowledgeItemDto,
  MemberDto,
  SpecialistDto,
  SubmissionDto,
  TaskDto,
  TeamDto,
} from "./lib/types";
import { RequestForm } from "./components/RequestForm";
import { TaskList } from "./components/TaskList";
import { ExecutionTrace } from "./components/ExecutionTrace";
import { ResultPanel } from "./components/ResultPanel";
import { SpecialistRail } from "./components/SpecialistRail";
import { ConflictCenter } from "./components/ConflictCenter";
import { ProjectBrainPanel } from "./components/ProjectBrainPanel";
import { SpecialistCreationForm } from "./components/SpecialistCreationForm";
import { SpecialistPackagePanel } from "./components/SpecialistPackagePanel";
import { GitSyncPanel } from "./components/GitSyncPanel";
import { RegistryPanel } from "./components/RegistryPanel";
import { DeletedItemsPanel } from "./components/DeletedItemsPanel";
import { ModelsPanel } from "./components/ModelsPanel";
import { RolesPanel } from "./components/RolesPanel";
import { ResourceCenter } from "./components/ResourceCenter";
import { SettingsPanel } from "./components/SettingsPanel";
import { GearIcon, Logo } from "./components/icons";
import { useI18n } from "./i18n";

export default function App() {
  const { t } = useI18n();
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [tasks, setTasks] = useState<TaskDto[]>([]);
  const [specialists, setSpecialists] = useState<SpecialistDto[]>([]);
  const [members, setMembers] = useState<MemberDto[]>([]);
  const [teams, setTeams] = useState<TeamDto[]>([]);
  const [selectedTeamId, setSelectedTeamId] = useState<string | null>(null);
  const [conflicts, setConflicts] = useState<ConflictDto[]>([]);
  const [knowledgeItems, setKnowledgeItems] = useState<KnowledgeItemDto[]>([]);
  const [decisions, setDecisions] = useState<DecisionDto[]>([]);
  const [activeTaskId, setActiveTaskId] = useState<string | null>(null);
  const [trace, setTrace] = useState<EventDto[]>([]);
  const [editingSpecialist, setEditingSpecialist] = useState<SpecialistDto | null>(null);
  const [exportingSpecialist, setExportingSpecialist] = useState<SpecialistDto | null>(null);
  const [exportingTeam, setExportingTeam] = useState<TeamDto | null>(null);
  const [registryRefreshSignal, setRegistryRefreshSignal] = useState(0);
  // Round 45: bumped whenever ModelsPanel enables/disables a model, so
  // SpecialistCreationForm's picker (which fetches once on mount) can
  // refetch instead of holding a list that's since gone stale.
  const [modelsRefreshSignal, setModelsRefreshSignal] = useState(0);
  // Adopted from a parallel build (its Resource Center): bumped whenever
  // a `resource_acquired`/`resource_released` event arrives, so the
  // panel refreshes off the app's single event subscription instead of
  // its own polling timer.
  const [resourceRefreshSignal, setResourceRefreshSignal] = useState(0);

  // Phase 1 has no server-side lookup from task -> correlation id (see
  // the note in solexa-coordinator); the frontend just remembers the
  // mapping for whatever it has submitted this session.
  const correlationByTask = useRef(new Map<string, string>());

  const refreshTasks = useCallback(() => {
    listTasks().then(setTasks).catch(console.error);
  }, []);

  const refreshTeamRoster = useCallback(() => {
    listSpecialists().then(setSpecialists).catch(console.error);
    listMembers().then(setMembers).catch(console.error);
    listTeams().then(setTeams).catch(console.error);
  }, []);

  const refreshConflicts = useCallback(() => {
    listConflicts().then(setConflicts).catch(console.error);
  }, []);

  const refreshKnowledge = useCallback(() => {
    listKnowledgeItems().then(setKnowledgeItems).catch(console.error);
    listDecisions().then(setDecisions).catch(console.error);
  }, []);

  useEffect(() => {
    refreshTasks();
    refreshTeamRoster();
    refreshConflicts();
    refreshKnowledge();
  }, [refreshTasks, refreshTeamRoster, refreshConflicts, refreshKnowledge]);

  // Live trace + task-state updates: one subscription, filtered/applied
  // as events arrive, so the trace panel, task badges, conflict center,
  // specialist benchmarks, and Project Brain list all move in real time
  // without polling.
  useEffect(() => {
    let unsubscribe: (() => void) | undefined;
    subscribeToEvents((event) => {
      const activeCorrelationId = activeTaskId ? correlationByTask.current.get(activeTaskId) : null;
      if (activeCorrelationId && event.correlation_id === activeCorrelationId) {
        setTrace((prev) => [...prev, event]);
      }
      refreshTasks();
      if (event.event_type === "conflict_detected" || event.event_type === "conflict_resolved") {
        refreshConflicts();
      }
      if (
        event.event_type === "verified" ||
        event.event_type === "member_registered" ||
        event.event_type === "team_registered" ||
        event.event_type === "specialist_registered" ||
        event.event_type === "team_member_added" ||
        event.event_type === "team_member_removed" ||
        event.event_type === "team_reparented" ||
        event.event_type === "specialist_updated" ||
        event.event_type === "specialist_active_changed" ||
        event.event_type === "specialist_imported" ||
        event.event_type === "team_imported" ||
        event.event_type === "member_updated" ||
        event.event_type === "team_updated"
      ) {
        refreshTeamRoster();
      }
      if (event.event_type === "knowledge_item_added" || event.event_type === "knowledge_item_archived" || event.event_type === "decision_recorded" || event.event_type === "decision_resolved") {
        refreshKnowledge();
      }
      if (event.event_type === "saved_to_registry" || event.event_type === "installed_from_registry") {
        setRegistryRefreshSignal((n) => n + 1);
      }
      if (event.event_type === "installed_from_registry") {
        refreshTeamRoster();
      }
      if (
        event.event_type === "resource_acquired" ||
        event.event_type === "resource_released" ||
        event.event_type === "resource_budget_changed"
      ) {
        setResourceRefreshSignal((n) => n + 1);
      }
    }).then((fn) => (unsubscribe = fn));
    return () => unsubscribe?.();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeTaskId, refreshTasks, refreshConflicts, refreshTeamRoster, refreshKnowledge]);

  function handleSubmitted(submission: SubmissionDto) {
    correlationByTask.current.set(submission.task_id, submission.correlation_id);
    setActiveTaskId(submission.task_id);
    setTrace([]);
    refreshTasks();
  }

  function handleSelectTask(task: TaskDto) {
    setActiveTaskId(task.id);
    const correlationId = correlationByTask.current.get(task.id);
    if (correlationId) {
      getTrace(correlationId).then(setTrace).catch(console.error);
    } else {
      setTrace([]);
    }
  }

  function handleConflictResolved() {
    refreshConflicts();
    refreshTasks();
    refreshTeamRoster();
  }

  function handleGitSyncChanged() {
    // A pull can update Members/Teams/Specialists directly (fast-
    // forwards) and/or create new Conflicts (genuine divergence), both
    // lists need to be current for the UI to reflect what just happened.
    refreshTeamRoster();
    refreshConflicts();
  }

  function handleToggleSpecialistActive(specialist: SpecialistDto) {
    const reactivating = specialist.active === false;
    setSpecialistActive(specialist.id, reactivating).then(refreshTeamRoster).catch(console.error);
  }

  function handleSaveSpecialistToRegistry(specialist: SpecialistDto) {
    saveSpecialistToRegistry(specialist.id)
      .then(() => setRegistryRefreshSignal((n) => n + 1))
      .catch(console.error);
  }

  function handleSaveTeamToRegistry(team: TeamDto) {
    saveTeamToRegistry(team.id)
      .then(() => setRegistryRefreshSignal((n) => n + 1))
      .catch(console.error);
  }

  // Round 35: everything on screen, tasks, roster, conflicts, the
  // Project Brain, the selected team and task, belongs to the project
  // that was active a moment ago. Rather than refresh each list and
  // hope nothing stale survives, clear the two selections that point at
  // now-invisible rows and reload every list from scratch.
  const handleProjectSwitched = useCallback(() => {
    setActiveTaskId(null);
    setSelectedTeamId(null);
    refreshTasks();
    refreshTeamRoster();
    refreshConflicts();
    refreshKnowledge();
  }, [refreshTasks, refreshTeamRoster, refreshConflicts, refreshKnowledge]);

  const activeTask = tasks.find((t) => t.id === activeTaskId) ?? null;
  const isLive = activeTask ? !["completed", "failed", "cancelled"].includes(activeTask.state) : false;

  return (
    <div className="flex h-screen w-screen flex-col overflow-hidden">
      <header className="flex h-11 shrink-0 items-center gap-2.5 border-b border-line bg-panel/40 px-4">
        <Logo className="h-5 w-5 shrink-0 text-amber" />
        <span className="text-sm font-semibold tracking-wide text-ink">SOLEXA</span>
        <span className="hidden truncate text-[11px] text-muted sm:inline">{t("app.tagline")}</span>
        <button
          onClick={() => setSettingsOpen(true)}
          aria-label={t("app.settings")}
          title={t("app.settings")}
          className="ml-auto rounded-md border border-line p-1.5 text-muted transition hover:border-amber/60 hover:text-amber"
        >
          <GearIcon size={16} />
        </button>
      </header>

      <div className="flex min-h-0 flex-1 overflow-hidden">
      <SpecialistRail
        team={teams.find((t) => t.id === selectedTeamId) ?? teams[0] ?? null}
        teams={teams}
        members={members}
        specialists={specialists}
        onEditSpecialist={setEditingSpecialist}
        onExportSpecialist={setExportingSpecialist}
        onExportTeam={setExportingTeam}
        onSaveSpecialistToRegistry={handleSaveSpecialistToRegistry}
        onSaveTeamToRegistry={handleSaveTeamToRegistry}
        onToggleSpecialistActive={handleToggleSpecialistActive}
        onChanged={refreshTeamRoster}
        onSelectTeam={setSelectedTeamId}
        onProjectSwitched={handleProjectSwitched}
      />

      <main className="flex min-w-0 flex-1 flex-col gap-4 overflow-y-auto p-6">
        <RequestForm onSubmitted={handleSubmitted} />
        <ConflictCenter conflicts={conflicts} members={members} onResolved={handleConflictResolved} />
        <GitSyncPanel onChanged={handleGitSyncChanged} members={members} />
        <RolesPanel members={members} specialists={specialists} onChanged={refreshTeamRoster} />
        <ResourceCenter refreshSignal={resourceRefreshSignal} />
        <ModelsPanel onChanged={() => setModelsRefreshSignal((s) => s + 1)} />
        <SpecialistCreationForm
          teams={teams}
          editingSpecialist={editingSpecialist}
          modelsRefreshSignal={modelsRefreshSignal}
          onChanged={() => {
            refreshTeamRoster();
            setEditingSpecialist(null);
          }}
          onCancelEdit={() => setEditingSpecialist(null)}
        />
        <ProjectBrainPanel items={knowledgeItems} decisions={decisions} members={members} onChanged={refreshKnowledge} />
        <SpecialistPackagePanel
          exportingSpecialist={exportingSpecialist}
          onCloseExportSpecialist={() => setExportingSpecialist(null)}
          exportingTeam={exportingTeam}
          onCloseExportTeam={() => setExportingTeam(null)}
          onImported={refreshTeamRoster}
        />
        <RegistryPanel key={registryRefreshSignal} onChanged={refreshTeamRoster} />
        <DeletedItemsPanel members={members} teams={teams} onChanged={refreshTeamRoster} />
        <div>
          <p className="mb-2 text-xs font-mono uppercase tracking-wide text-muted">
            {t("tasks.title", { count: tasks.length })}
          </p>
          <TaskList tasks={tasks} activeTaskId={activeTaskId} onSelect={handleSelectTask} onPriorityChanged={refreshTasks} />
        </div>
        {activeTask && <ResultPanel task={activeTask} />}
      </main>

      <section className="w-[26rem] shrink-0 overflow-hidden border-s border-line bg-panel/40 p-6">
        <ExecutionTrace events={trace} taskTitle={activeTask?.title ?? null} isLive={isLive} />
      </section>
      </div>

      <SettingsPanel open={settingsOpen} onClose={() => setSettingsOpen(false)} />
    </div>
  );
}
