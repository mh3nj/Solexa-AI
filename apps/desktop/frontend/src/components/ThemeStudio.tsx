import { useMemo, useState, type CSSProperties } from "react";
import { useI18n } from "../i18n";
import { useSettings } from "../lib/settings";
import {
  BUILTIN,
  THEME_TOKENS,
  isHexColor,
  themeStyle,
  type Theme,
  type ThemeToken,
} from "../lib/theme";
import { CheckIcon, PencilIcon, PlusIcon, TrashIcon } from "./icons";

function draftFrom(theme: Theme): Theme {
  return { ...theme, id: `custom-${Date.now()}`, name: `${theme.name} copy`, builtin: false };
}

export function ThemeStudio() {
  const {
    settings,
    activeTheme,
    saveCustomTheme,
    deleteCustomTheme,
    setThemeId,
    applyTransientTheme,
    clearTransientTheme,
  } = useSettings();
  const { t } = useI18n();

  const [selectedId, setSelectedId] = useState(activeTheme.id);
  const [draft, setDraft] = useState<Theme | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const allThemes = useMemo(() => [...BUILTIN, ...settings.customThemes], [settings.customThemes]);
  const selected = draft ?? allThemes.find((theme) => theme.id === selectedId) ?? activeTheme;
  const preview = draft ?? selected;

  function select(theme: Theme) {
    setSelectedId(theme.id);
    setDraft(null);
    setError(null);
    setMessage(null);
  }

  function startNew() {
    const created = draftFrom(activeTheme);
    created.name = "";
    setDraft(created);
    setError(null);
    setMessage(null);
  }

  function startEdit(theme: Theme) {
    if (theme.builtin) return;
    setDraft({ ...theme });
    setError(null);
    setMessage(null);
  }

  function changeToken(token: ThemeToken, value: string) {
    setDraft((current) => (current ? { ...current, [token]: value } : current));
  }

  function validate(theme: Theme): string | null {
    if (!theme.name.trim()) return t("themes.nameRequired");
    if (THEME_TOKENS.some((token) => !isHexColor(theme[token]))) return t("themes.invalidColor");
    return null;
  }

  function save(apply: boolean): Theme | null {
    if (!draft) return null;
    const problem = validate(draft);
    if (problem) {
      setError(problem);
      return null;
    }
    const clean: Theme = {
      ...draft,
      name: draft.name.trim(),
      id: draft.id.startsWith("custom-") ? draft.id : `custom-${Date.now()}`,
      builtin: false,
    };
    saveCustomTheme(clean);
    setDraft(null);
    setSelectedId(clean.id);
    setError(null);
    if (apply) {
      setThemeId(clean.id);
      setMessage(t("themes.saved"));
    } else {
      setMessage(t("themes.saved"));
    }
    return clean;
  }

  function apply() {
    if (draft) {
      const problem = validate(draft);
      if (problem) {
        setError(problem);
        return;
      }
      if (settings.customThemes.some((theme) => theme.id === draft.id)) {
        saveCustomTheme(draft);
        setThemeId(draft.id);
      } else {
        applyTransientTheme(draft);
      }
      setMessage(t("themes.applied"));
      setError(null);
      return;
    }
    setThemeId(selected.id);
    clearTransientTheme();
    setMessage(t("themes.applied"));
    setError(null);
  }

  function remove() {
    if (selected.builtin) return;
    if (!window.confirm(t("themes.deleteConfirm"))) return;
    deleteCustomTheme(selected.id);
    setDraft(null);
    setSelectedId(activeTheme.id === selected.id ? BUILTIN[0].id : activeTheme.id);
    setMessage(t("themes.deleted"));
    setError(null);
  }

  const custom = settings.customThemes;
  const isEditing = draft !== null;

  return (
    <div className="flex min-h-0 flex-1 gap-4">
      <div className="flex w-64 shrink-0 flex-col gap-3 overflow-y-auto pr-1">
        <div className="flex items-center justify-between">
          <p className="text-[11px] font-mono uppercase tracking-wide text-muted">
            {t("themes.builtin")}
          </p>
        </div>
        <div className="space-y-1">
          {BUILTIN.map((theme) => (
            <ThemeChoice
              key={theme.id}
              theme={theme}
              selected={!isEditing && selected.id === theme.id}
              active={activeTheme.id === theme.id}
              onClick={() => select(theme)}
            />
          ))}
        </div>

        <div className="mt-2 flex items-center justify-between">
          <p className="text-[11px] font-mono uppercase tracking-wide text-muted">
            {t("themes.custom")}
          </p>
          <button
            onClick={startNew}
            className="inline-flex items-center gap-1 rounded border border-line px-2 py-0.5 text-[11px] text-muted transition hover:border-amber/60 hover:text-amber"
          >
            <PlusIcon size={12} />
            {t("themes.new")}
          </button>
        </div>
        {custom.length === 0 ? (
          <p className="rounded border border-dashed border-line px-2 py-3 text-[11px] leading-relaxed text-muted">
            {t("themes.noCustom")}
          </p>
        ) : (
          <div className="space-y-1">
            {custom.map((theme) => (
              <ThemeChoice
                key={theme.id}
                theme={theme}
                selected={!isEditing && selected.id === theme.id}
                active={activeTheme.id === theme.id}
                onClick={() => select(theme)}
              />
            ))}
          </div>
        )}
      </div>

      <div className="flex min-w-0 flex-1 flex-col gap-3">
        <div className="rounded-lg border border-line bg-panel/60 p-3">
          <div className="mb-2 flex items-center justify-between">
            <p className="text-[11px] font-mono uppercase tracking-wide text-muted">
              {t("themes.preview")}
            </p>
            <p className="truncate text-xs text-ink">{preview.name || t("themes.new")}</p>
          </div>
          <ThemePreview theme={preview} />
        </div>

        <div className="rounded-lg border border-line bg-panel/60 p-3">
          <div className="mb-2 flex items-center justify-between gap-2">
            <p className="text-[11px] font-mono uppercase tracking-wide text-muted">
              {isEditing ? t("themes.edit") : t("themes.colors")}
            </p>
            {isEditing ? (
              <input
                value={draft.name}
                onChange={(event) => setDraft({ ...draft, name: event.target.value })}
                placeholder={t("themes.name")}
                className="w-56 rounded border border-line bg-graphite px-2 py-1 text-xs text-ink placeholder:text-muted/60"
              />
            ) : null}
          </div>

          {isEditing ? (
            <div className="grid grid-cols-2 gap-2">
              {THEME_TOKENS.map((token) => (
                <label key={token} className="flex items-center gap-2 text-[11px] text-muted">
                  <input
                    type="color"
                    value={isHexColor(draft[token]) ? draft[token] : "#000000"}
                    onChange={(event) => changeToken(token, event.target.value)}
                    className="h-6 w-8 cursor-pointer rounded border border-line bg-transparent p-0"
                    aria-label={t(`themes.token.${token}`)}
                  />
                  <span className="w-24 shrink-0">{t(`themes.token.${token}`)}</span>
                  <input
                    value={draft[token]}
                    onChange={(event) => changeToken(token, event.target.value)}
                    className={`w-24 rounded border bg-graphite px-1.5 py-0.5 font-mono text-[11px] text-ink ${
                      isHexColor(draft[token]) ? "border-line" : "border-rose"
                    }`}
                  />
                </label>
              ))}
            </div>
          ) : (
            <div className="flex flex-wrap gap-1.5">
              {THEME_TOKENS.map((token) => (
                <span
                  key={token}
                  title={t(`themes.token.${token}`)}
                  className="inline-flex items-center gap-1.5 rounded border border-line/70 px-1.5 py-0.5 text-[10px] text-muted"
                >
                  <span
                    className="h-2.5 w-2.5 rounded-full border border-line"
                    style={{ backgroundColor: selected[token] }}
                  />
                  {selected[token]}
                </span>
              ))}
            </div>
          )}
        </div>

        <div className="mt-auto flex flex-wrap items-center gap-2">
          <button
            onClick={apply}
            className="inline-flex items-center gap-1.5 rounded-md bg-amber px-3 py-1.5 text-xs font-medium text-onaccent transition hover:bg-amber/90"
          >
            <CheckIcon size={13} />
            {t("themes.apply")}
          </button>
          {isEditing ? (
            <>
              <button
                onClick={() => save(false)}
                className="rounded-md border border-line px-3 py-1.5 text-xs text-ink transition hover:border-amber/60"
              >
                {t("themes.save")}
              </button>
              <button
                onClick={() => save(true)}
                className="rounded-md border border-amber/60 px-3 py-1.5 text-xs text-amber transition hover:bg-amber/10"
              >
                {t("themes.saveApply")}
              </button>
              <button
                onClick={() => {
                  setDraft(null);
                  setError(null);
                }}
                className="rounded-md border border-line px-3 py-1.5 text-xs text-muted transition hover:text-ink"
              >
                {t("common.cancel")}
              </button>
            </>
          ) : (
            <>
              <button
                onClick={() => startEdit(selected)}
                disabled={selected.builtin}
                className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-1.5 text-xs text-ink transition hover:border-amber/60 disabled:cursor-not-allowed disabled:opacity-40"
              >
                <PencilIcon size={13} />
                {t("themes.edit")}
              </button>
              <button
                onClick={remove}
                disabled={selected.builtin}
                className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-1.5 text-xs text-muted transition hover:border-rose/60 hover:text-rose disabled:cursor-not-allowed disabled:opacity-40"
              >
                <TrashIcon size={13} />
                {t("themes.delete")}
              </button>
            </>
          )}
          {activeTheme.id === selected.id && !isEditing ? (
            <span className="ml-auto text-[11px] text-teal">{t("themes.active")}</span>
          ) : null}
        </div>

        {error ? <p className="text-xs text-rose">{error}</p> : null}
        {message ? <p className="text-xs text-teal">{message}</p> : null}
      </div>
    </div>
  );
}

function ThemeChoice({
  theme,
  selected,
  active,
  onClick,
}: {
  theme: Theme;
  selected: boolean;
  active: boolean;
  onClick: () => void;
}) {
  return (
    <button
      onClick={onClick}
      aria-pressed={selected}
      className={`flex w-full items-center gap-2 rounded-md border px-2 py-1.5 text-left transition ${
        selected ? "border-amber/60 bg-amber/10" : "border-line hover:border-line/70 hover:bg-panel/60"
      }`}
    >
      <span
        className="flex h-5 w-5 shrink-0 overflow-hidden rounded-full border border-line"
        style={{ backgroundColor: theme.bg }}
        aria-hidden
      >
        <span className="m-auto h-2.5 w-2.5 rounded-full" style={{ backgroundColor: theme.accent }} />
      </span>
      <span className="min-w-0 flex-1 truncate text-xs text-ink">{theme.name}</span>
      {active ? <span className="h-1.5 w-1.5 rounded-full bg-teal" aria-hidden /> : null}
    </button>
  );
}

/** A miniature of the real interface, painted with the candidate palette. */
function ThemePreview({ theme }: { theme: Theme }) {
  const { t } = useI18n();
  return (
    <div
      style={themeStyle(theme) as CSSProperties}
      className="rounded-md border border-line bg-graphite p-3 font-display"
    >
      <div className="mb-2 flex items-center justify-between">
        <span className="text-[11px] font-medium text-ink">{t("themes.previewTitle")}</span>
        <span className="rounded-full bg-teal/15 px-2 py-0.5 text-[9px] uppercase tracking-wide text-teal ring-1 ring-teal/30">
          verified
        </span>
      </div>
      <div className="rounded border border-line bg-panel p-2">
        <div className="flex items-center justify-between gap-2">
          <span className="truncate text-[11px] text-ink">{t("themes.previewTask")}</span>
          <span className="rounded bg-amber/15 px-1.5 py-0.5 text-[9px] text-amber ring-1 ring-amber/30">
            running
          </span>
        </div>
        <p className="mt-1 text-[10px] text-muted">{t("themes.previewNote")}</p>
        <div className="mt-2 flex items-center gap-2">
          <span className="rounded bg-line/40 px-1.5 py-0.5 font-mono text-[9px] text-muted">
            {t("themes.previewTag")}
          </span>
          <span className="ml-auto rounded bg-amber px-2 py-0.5 text-[10px] font-medium text-onaccent">
            {t("themes.previewButton")}
          </span>
        </div>
      </div>
      <div className="mt-2 flex items-center gap-1.5">
        <span className="h-1.5 w-1.5 rounded-full bg-rose" />
        <span className="h-1.5 w-1.5 rounded-full bg-amber" />
        <span className="h-1.5 w-1.5 rounded-full bg-teal" />
        <span className="ml-1 text-[9px] text-muted">{theme.kind}</span>
      </div>
    </div>
  );
}
