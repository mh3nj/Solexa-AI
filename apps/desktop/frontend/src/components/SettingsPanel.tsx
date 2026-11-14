import { useEffect, useState, type ReactNode } from "react";
import { useI18n, LANGUAGES } from "../i18n";
import { useSettings, type StartupMode } from "../lib/settings";
import {
  getEnforceApproverRoles,
  getGitSyncStatus,
  getResourceSnapshot,
  resetResourceBudget,
  setEnforceApproverRoles,
  setRequireReviewForIncomingChanges,
  setResourceBudget,
} from "../lib/api";
import { CloseIcon, GlobeIcon, InfoIcon, Logo, PaletteIcon, SlidersIcon } from "./icons";
import { ThemeStudio } from "./ThemeStudio";

type Tab = "general" | "appearance" | "engine" | "about";

export function SettingsPanel({ open, onClose }: { open: boolean; onClose: () => void }) {
  const { t } = useI18n();
  const { settings, setLanguage, setCloseToTray, setMinimizeToTray, setStartup } = useSettings();
  const [tab, setTab] = useState<Tab>("general");
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setError(null);
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  if (!open) return null;

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-graphite/80 p-6 backdrop-blur-sm"
      role="dialog"
      aria-modal="true"
      aria-label={t("settings.title")}
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <div className="flex h-[min(88vh,760px)] w-[min(96vw,1020px)] flex-col overflow-hidden rounded-xl border border-line bg-graphite shadow-2xl md:flex-row">
        <aside className="flex shrink-0 flex-row gap-1 border-b border-line bg-panel/50 p-2 md:w-52 md:flex-col md:border-b-0 md:border-e md:p-3">
          <TabButton icon={<GlobeIcon size={15} />} active={tab === "general"} onClick={() => setTab("general")}>
            {t("settings.tab.general")}
          </TabButton>
          <TabButton icon={<PaletteIcon size={15} />} active={tab === "appearance"} onClick={() => setTab("appearance")}>
            {t("settings.tab.appearance")}
          </TabButton>
          <TabButton icon={<SlidersIcon size={15} />} active={tab === "engine"} onClick={() => setTab("engine")}>
            {t("settings.tab.engine")}
          </TabButton>
          <TabButton icon={<InfoIcon size={15} />} active={tab === "about"} onClick={() => setTab("about")}>
            {t("settings.tab.about")}
          </TabButton>
        </aside>

        <div className="flex min-h-0 min-w-0 flex-1 flex-col">
          <header className="flex items-center justify-between border-b border-line px-4 py-3">
            <h2 className="text-sm font-medium text-ink">
              {tab === "appearance" ? t("themes.title") : t(`settings.tab.${tab}`)}
            </h2>
            <button
              onClick={onClose}
              aria-label={t("app.close")}
              className="rounded p-1 text-muted transition hover:bg-panel hover:text-ink"
            >
              <CloseIcon size={16} />
            </button>
          </header>

          <div className="flex min-h-0 flex-1 flex-col overflow-y-auto p-4">
            {error ? <p className="mb-3 text-xs text-rose">{error}</p> : null}
            {tab === "general" ? (
              <div className="space-y-5">
                <Section title={t("settings.language")} help={t("settings.languageHelp")}>
                  <div className="flex flex-wrap gap-2">
                    {LANGUAGES.map((language) => (
                      <button
                        key={language.id}
                        onClick={() => setLanguage(language.id)}
                        aria-pressed={settings.language === language.id}
                        className={chipClass(settings.language === language.id)}
                      >
                        {language.label}
                      </button>
                    ))}
                  </div>
                </Section>

                <Section title={t("settings.tray")}>
                  <Toggle
                    label={t("settings.closeToTray")}
                    help={t("settings.closeToTrayHelp")}
                    checked={settings.closeToTray}
                    onChange={setCloseToTray}
                  />
                  <Toggle
                    label={t("settings.minimizeToTray")}
                    help={t("settings.minimizeToTrayHelp")}
                    checked={settings.minimizeToTray}
                    onChange={setMinimizeToTray}
                  />
                </Section>

                <Section title={t("settings.startup")} help={t("settings.startupHelp")}>
                  <div className="space-y-2">
                    {(
                      [
                        ["off", t("settings.startup.off")],
                        ["app", t("settings.startup.app")],
                        ["tray", t("settings.startup.tray")],
                      ] as Array<[StartupMode, string]>
                    ).map(([mode, label]) => (
                      <label
                        key={mode}
                        className="flex cursor-pointer items-center gap-2 rounded-md border border-line px-3 py-2 text-sm text-ink transition hover:border-amber/40"
                      >
                        <input
                          type="radio"
                          name="startup-mode"
                          value={mode}
                          checked={settings.startupMode === mode}
                          onChange={() => {
                            setStartup(mode).catch(() => setError(t("settings.startupUnavailable")));
                          }}
                          className="accent-amber"
                        />
                        {label}
                      </label>
                    ))}
                  </div>
                </Section>
              </div>
            ) : null}

            {tab === "appearance" ? <ThemeStudio /> : null}

            {tab === "engine" ? <EngineTab onError={setError} /> : null}

            {tab === "about" ? (
              <div className="flex flex-col items-start gap-4">
                <div className="flex items-center gap-4">
                  <span className="flex h-16 w-16 items-center justify-center rounded-xl border border-line bg-panel">
                    <Logo className="h-10 w-10 text-amber" />
                  </span>
                  <div>
                    <p className="text-lg font-medium text-ink">SOLEXA</p>
                    <p className="text-xs text-muted">{t("app.tagline")}</p>
                    <p className="mt-1 font-mono text-[11px] text-muted">
                      {t("settings.version")} 0.1.0
                    </p>
                  </div>
                </div>
                <p className="max-w-prose text-sm leading-relaxed text-muted">
                  {t("settings.aboutText")}
                </p>
                <p className="max-w-prose text-xs leading-relaxed text-muted">
                  {t("settings.logoNote")}
                </p>
              </div>
            ) : null}
          </div>
        </div>
      </div>
    </div>
  );
}

function TabButton({
  icon,
  active,
  onClick,
  children,
}: {
  icon: ReactNode;
  active: boolean;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <button
      onClick={onClick}
      aria-current={active ? "page" : undefined}
      className={`flex items-center gap-2 rounded-md px-3 py-2 text-xs transition ${
        active ? "bg-amber/10 text-amber" : "text-muted hover:bg-panel hover:text-ink"
      }`}
    >
      {icon}
      <span className="truncate">{children}</span>
    </button>
  );
}

function Section({
  title,
  help,
  children,
}: {
  title: string;
  help?: string;
  children: ReactNode;
}) {
  return (
    <section className="space-y-2">
      <div>
        <h3 className="text-xs font-mono uppercase tracking-wide text-muted">{title}</h3>
        {help ? <p className="mt-0.5 max-w-prose text-[11px] leading-relaxed text-muted/80">{help}</p> : null}
      </div>
      {children}
    </section>
  );
}

function Toggle({
  label,
  help,
  checked,
  onChange,
}: {
  label: string;
  help?: string;
  checked: boolean;
  onChange: (value: boolean) => void;
}) {
  return (
    <label className="flex cursor-pointer items-start justify-between gap-4 rounded-md border border-line px-3 py-2">
      <span className="min-w-0">
        <span className="block text-sm text-ink">{label}</span>
        {help ? <span className="mt-0.5 block text-[11px] leading-relaxed text-muted">{help}</span> : null}
      </span>
      <span className="relative mt-0.5 shrink-0">
        <input
          type="checkbox"
          checked={checked}
          onChange={(event) => onChange(event.target.checked)}
          className="peer sr-only"
        />
        <span className="block h-5 w-9 rounded-full bg-line transition peer-checked:bg-amber" />
        <span className="pointer-events-none absolute left-0.5 top-0.5 block h-4 w-4 rounded-full bg-ink transition peer-checked:translate-x-4 peer-checked:bg-onaccent" />
      </span>
    </label>
  );
}

function chipClass(active: boolean): string {
  return `rounded-md border px-3 py-1.5 text-xs transition ${
    active ? "border-amber/60 bg-amber/10 text-amber" : "border-line text-muted hover:text-ink"
  }`;
}

function EngineTab({ onError }: { onError: (message: string | null) => void }) {
  const { t } = useI18n();
  const [vram, setVram] = useState("24000");
  const [ram, setRam] = useState("64000");
  const [concurrency, setConcurrency] = useState("4");
  const [loaded, setLoaded] = useState(false);
  const [note, setNote] = useState<string | null>(null);
  const [enforceRoles, setEnforceRoles] = useState(false);
  const [reviewGate, setReviewGate] = useState(false);
  const [hasRepo, setHasRepo] = useState(false);

  useEffect(() => {
    let cancelled = false;
    getResourceSnapshot()
      .then((snapshot) => {
        if (cancelled) return;
        setVram(String(snapshot.budget_vram_mb));
        setRam(String(snapshot.budget_ram_mb));
        setConcurrency(String(snapshot.max_concurrent));
        setLoaded(true);
      })
      .catch(() => setLoaded(true));
    getEnforceApproverRoles()
      .then((value) => !cancelled && setEnforceRoles(value))
      .catch(() => {});
    getGitSyncStatus()
      .then((status) => {
        if (cancelled) return;
        setHasRepo(status !== null);
        setReviewGate(status?.require_review_for_incoming_changes ?? false);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, []);

  async function saveBudget() {
    onError(null);
    setNote(null);
    try {
      await setResourceBudget(Number(vram), Number(ram), Number(concurrency));
      setNote(t("resources.updated"));
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    }
  }

  async function resetBudget() {
    onError(null);
    setNote(null);
    try {
      await resetResourceBudget();
      const snapshot = await getResourceSnapshot();
      setVram(String(snapshot.budget_vram_mb));
      setRam(String(snapshot.budget_ram_mb));
      setConcurrency(String(snapshot.max_concurrent));
      setNote(t("settings.resetDone"));
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    }
  }

  async function toggleRoles(next: boolean) {
    onError(null);
    try {
      await setEnforceApproverRoles(next);
      setEnforceRoles(next);
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    }
  }

  async function toggleReviewGate(next: boolean) {
    onError(null);
    try {
      await setRequireReviewForIncomingChanges(next);
      setReviewGate(next);
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    }
  }

  return (
    <div className="space-y-5">
      <p className="text-[11px] leading-relaxed text-muted">{t("settings.engineHelp")}</p>

      <Section title={t("resources.title")} help={t("resources.help")}>
        <div className="grid grid-cols-3 gap-2">
          <Field label={t("resources.vramMb")} value={vram} onChange={setVram} disabled={!loaded} />
          <Field label={t("resources.ramMb")} value={ram} onChange={setRam} disabled={!loaded} />
          <Field
            label={t("resources.concurrency")}
            value={concurrency}
            onChange={setConcurrency}
            disabled={!loaded}
          />
        </div>
        <div className="flex items-center gap-2">
          <button
            onClick={saveBudget}
            disabled={!loaded}
            className="rounded-md bg-amber px-3 py-1.5 text-xs font-medium text-onaccent transition hover:bg-amber/90 disabled:opacity-40"
          >
            {t("common.save")}
          </button>
          <button
            onClick={resetBudget}
            disabled={!loaded}
            title={t("resources.resetHint")}
            className="rounded-md border border-line px-3 py-1.5 text-xs text-muted transition hover:text-ink disabled:opacity-40"
          >
            {t("resources.reset")}
          </button>
          {note ? <span className="text-[11px] text-teal">{note}</span> : null}
        </div>
      </Section>

      <Section title={t("roles.title")} help={t("roles.requireApproveHelp")}>
        <Toggle
          label={t("roles.requireApprove")}
          checked={enforceRoles}
          onChange={(value) => void toggleRoles(value)}
        />
      </Section>

      {hasRepo ? (
        <Section title={t("git.title")}>
          <Toggle
            label={t("git.reviewGate")}
            help={t("git.reviewGateHelp")}
            checked={reviewGate}
            onChange={(value) => void toggleReviewGate(value)}
          />
        </Section>
      ) : null}

    </div>
  );
}

function Field({
  label,
  value,
  onChange,
  disabled,
}: {
  label: string;
  value: string;
  onChange: (value: string) => void;
  disabled?: boolean;
}) {
  return (
    <label className="block">
      <span className="mb-1 block text-[11px] text-muted">{label}</span>
      <input
        value={value}
        onChange={(event) => onChange(event.target.value)}
        inputMode="numeric"
        disabled={disabled}
        className="w-full rounded border border-line bg-graphite px-2 py-1 text-xs text-ink disabled:opacity-40"
      />
    </label>
  );
}
