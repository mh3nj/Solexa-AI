import { useEffect, useRef, useState } from "react";
import {
  exportSpecialistPackage,
  exportTeamPackage,
  importSpecialistPackage,
  importTeamPackage,
} from "../lib/api";
import type { SpecialistDto, TeamDto } from "../lib/types";
import { useI18n } from "../i18n";

// Specialist packaging: export a specialist's configuration and measured
// track record as a JSON document, and import one back. A team package
// only carries AI members and their specialists.
//
// Saving and opening files uses plain browser APIs rather than a Tauri
// file dialog plugin, so the flow works in the desktop app exactly as it
// does in a plain preview. Pasting JSON stays available next to the file
// flow, since that is the simplest way to move a package between
// machines.

function slugify(name: string): string {
  const slug = name
    .toLowerCase()
    .trim()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "");
  return slug || "package";
}

function downloadJson(json: string, filename: string) {
  const blob = new Blob([json], { type: "application/json" });
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = filename;
  document.body.appendChild(anchor);
  anchor.click();
  document.body.removeChild(anchor);
  URL.revokeObjectURL(url);
}

export function SpecialistPackagePanel({
  exportingSpecialist,
  onCloseExportSpecialist,
  exportingTeam,
  onCloseExportTeam,
  onImported,
}: {
  exportingSpecialist: SpecialistDto | null;
  onCloseExportSpecialist: () => void;
  exportingTeam: TeamDto | null;
  onCloseExportTeam: () => void;
  onImported: () => void;
}) {
  const { t } = useI18n();
  const [specialistExportJson, setSpecialistExportJson] = useState("");
  const [specialistExportError, setSpecialistExportError] = useState<string | null>(null);
  const [specialistCopyLabel, setSpecialistCopyLabel] = useState<string | null>(null);

  const [teamExportJson, setTeamExportJson] = useState("");
  const [teamExportError, setTeamExportError] = useState<string | null>(null);
  const [teamCopyLabel, setTeamCopyLabel] = useState<string | null>(null);

  const [importSpecialistOpen, setImportSpecialistOpen] = useState(false);
  const [importSpecialistText, setImportSpecialistText] = useState("");
  const [importSpecialistError, setImportSpecialistError] = useState<string | null>(null);
  const [importSpecialistBusy, setImportSpecialistBusy] = useState(false);
  const specialistFileInputRef = useRef<HTMLInputElement>(null);

  const [importTeamOpen, setImportTeamOpen] = useState(false);
  const [importTeamText, setImportTeamText] = useState("");
  const [importTeamError, setImportTeamError] = useState<string | null>(null);
  const [importTeamBusy, setImportTeamBusy] = useState(false);
  const teamFileInputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (!exportingSpecialist) {
      setSpecialistExportJson("");
      setSpecialistExportError(null);
      return;
    }
    setSpecialistCopyLabel(null);
    exportSpecialistPackage(exportingSpecialist.id)
      .then(setSpecialistExportJson)
      .catch((err) => setSpecialistExportError(err instanceof Error ? err.message : String(err)));
  }, [exportingSpecialist]);

  useEffect(() => {
    if (!exportingTeam) {
      setTeamExportJson("");
      setTeamExportError(null);
      return;
    }
    setTeamCopyLabel(null);
    exportTeamPackage(exportingTeam.id)
      .then(setTeamExportJson)
      .catch((err) => setTeamExportError(err instanceof Error ? err.message : String(err)));
  }, [exportingTeam]);

  async function copyText(text: string, setLabel: (label: string | null) => void) {
    try {
      await navigator.clipboard.writeText(text);
      setLabel(t("common.copied"));
      setTimeout(() => setLabel(null), 1500);
    } catch {
      // Clipboard access can be blocked. The JSON is already sitting in a
      // readonly textarea, and there is a download button too.
      setLabel(t("packages.copyManually"));
    }
  }

  function readChosenFile(file: File, setText: (text: string) => void, setError: (err: string | null) => void) {
    const reader = new FileReader();
    reader.onload = () => {
      setText(typeof reader.result === "string" ? reader.result : "");
      setError(null);
    };
    reader.onerror = () => setError(t("packages.fileReadError"));
    reader.readAsText(file);
  }

  async function handleImportSpecialist() {
    if (!importSpecialistText.trim()) {
      setImportSpecialistError(t("packages.pasteFirstSpecialist"));
      return;
    }
    setImportSpecialistBusy(true);
    setImportSpecialistError(null);
    try {
      await importSpecialistPackage(importSpecialistText);
      setImportSpecialistText("");
      setImportSpecialistOpen(false);
      onImported();
    } catch (err) {
      setImportSpecialistError(err instanceof Error ? err.message : String(err));
    } finally {
      setImportSpecialistBusy(false);
    }
  }

  async function handleImportTeam() {
    if (!importTeamText.trim()) {
      setImportTeamError(t("packages.pasteFirstTeam"));
      return;
    }
    setImportTeamBusy(true);
    setImportTeamError(null);
    try {
      await importTeamPackage(importTeamText);
      setImportTeamText("");
      setImportTeamOpen(false);
      onImported();
    } catch (err) {
      setImportTeamError(err instanceof Error ? err.message : String(err));
    } finally {
      setImportTeamBusy(false);
    }
  }

  return (
    <div className="space-y-3">
      {exportingSpecialist && (
        <div className="rounded-lg border border-line bg-panel p-4">
          <div className="mb-2 flex items-center justify-between">
            <p className="text-xs font-mono uppercase tracking-wide text-muted">
              {t("packages.exportingSpecialist", { name: exportingSpecialist.name })}
            </p>
            <button
              type="button"
              onClick={onCloseExportSpecialist}
              className="text-[10px] text-muted underline decoration-dotted transition hover:text-ink"
            >
              {t("common.close")}
            </button>
          </div>
          {specialistExportError ? (
            <p className="text-[11px] text-rose">{specialistExportError}</p>
          ) : (
            <>
              <textarea
                readOnly
                value={specialistExportJson}
                rows={10}
                className="w-full resize-y rounded border border-line bg-graphite px-2 py-1 font-mono text-[11px] text-ink"
              />
              <div className="mt-2 flex gap-2">
                <button
                  type="button"
                  onClick={() => copyText(specialistExportJson, setSpecialistCopyLabel)}
                  className="rounded border border-line px-3 py-1 text-xs text-muted transition hover:text-ink"
                >
                  {specialistCopyLabel ?? t("common.copy")}
                </button>
                <button
                  type="button"
                  onClick={() =>
                    downloadJson(specialistExportJson, `specialist-${slugify(exportingSpecialist.name)}.json`)
                  }
                  className="rounded border border-line px-3 py-1 text-xs text-muted transition hover:text-ink"
                >
                  {t("common.download")}
                </button>
              </div>
            </>
          )}
        </div>
      )}

      {exportingTeam && (
        <div className="rounded-lg border border-line bg-panel p-4">
          <div className="mb-2 flex items-center justify-between">
            <p className="text-xs font-mono uppercase tracking-wide text-muted">
              {t("packages.exportingTeam", { name: exportingTeam.name })}
            </p>
            <button
              type="button"
              onClick={onCloseExportTeam}
              className="text-[10px] text-muted underline decoration-dotted transition hover:text-ink"
            >
              {t("common.close")}
            </button>
          </div>
          {teamExportError ? (
            <p className="text-[11px] text-rose">{teamExportError}</p>
          ) : (
            <>
              <p className="mb-1.5 text-[10px] text-muted">{t("packages.teamNote")}</p>
              <textarea
                readOnly
                value={teamExportJson}
                rows={10}
                className="w-full resize-y rounded border border-line bg-graphite px-2 py-1 font-mono text-[11px] text-ink"
              />
              <div className="mt-2 flex gap-2">
                <button
                  type="button"
                  onClick={() => copyText(teamExportJson, setTeamCopyLabel)}
                  className="rounded border border-line px-3 py-1 text-xs text-muted transition hover:text-ink"
                >
                  {teamCopyLabel ?? t("common.copy")}
                </button>
                <button
                  type="button"
                  onClick={() => downloadJson(teamExportJson, `team-${slugify(exportingTeam.name)}.json`)}
                  className="rounded border border-line px-3 py-1 text-xs text-muted transition hover:text-ink"
                >
                  {t("common.download")}
                </button>
              </div>
            </>
          )}
        </div>
      )}

      <div className="rounded-lg border border-line bg-panel">
        <button
          type="button"
          onClick={() => setImportSpecialistOpen((v) => !v)}
          className="w-full cursor-pointer select-none px-4 py-3 text-start text-xs font-mono uppercase tracking-wide text-muted"
        >
          {t("packages.importSpecialist")}
        </button>
        {importSpecialistOpen && (
          <div className="space-y-2 px-4 pb-4">
            <input
              ref={specialistFileInputRef}
              type="file"
              accept=".json,application/json"
              className="hidden"
              onChange={(e) => {
                const file = e.target.files?.[0];
                if (file) readChosenFile(file, setImportSpecialistText, setImportSpecialistError);
                e.target.value = "";
              }}
            />
            <button
              type="button"
              onClick={() => specialistFileInputRef.current?.click()}
              className="rounded border border-line px-3 py-1 text-xs text-muted transition hover:text-ink"
            >
              {t("packages.chooseFile")}
            </button>
            <textarea
              value={importSpecialistText}
              onChange={(e) => setImportSpecialistText(e.target.value)}
              placeholder={t("packages.pasteSpecialist")}
              rows={8}
              className="w-full resize-y rounded border border-line bg-graphite px-2 py-1 font-mono text-[11px] text-ink placeholder:text-muted/60"
            />
            <button
              type="button"
              onClick={handleImportSpecialist}
              disabled={importSpecialistBusy}
              className="rounded bg-amber/90 px-3 py-1 text-xs font-medium text-graphite transition hover:bg-amber disabled:cursor-not-allowed disabled:opacity-40"
            >
              {importSpecialistBusy ? t("packages.importing") : t("packages.import")}
            </button>
            {importSpecialistError && <p className="text-[11px] text-rose">{importSpecialistError}</p>}
          </div>
        )}
      </div>

      <div className="rounded-lg border border-line bg-panel">
        <button
          type="button"
          onClick={() => setImportTeamOpen((v) => !v)}
          className="w-full cursor-pointer select-none px-4 py-3 text-start text-xs font-mono uppercase tracking-wide text-muted"
        >
          {t("packages.importTeam")}
        </button>
        {importTeamOpen && (
          <div className="space-y-2 px-4 pb-4">
            <input
              ref={teamFileInputRef}
              type="file"
              accept=".json,application/json"
              className="hidden"
              onChange={(e) => {
                const file = e.target.files?.[0];
                if (file) readChosenFile(file, setImportTeamText, setImportTeamError);
                e.target.value = "";
              }}
            />
            <button
              type="button"
              onClick={() => teamFileInputRef.current?.click()}
              className="rounded border border-line px-3 py-1 text-xs text-muted transition hover:text-ink"
            >
              {t("packages.chooseFile")}
            </button>
            <textarea
              value={importTeamText}
              onChange={(e) => setImportTeamText(e.target.value)}
              placeholder={t("packages.pasteTeam")}
              rows={8}
              className="w-full resize-y rounded border border-line bg-graphite px-2 py-1 font-mono text-[11px] text-ink placeholder:text-muted/60"
            />
            <button
              type="button"
              onClick={handleImportTeam}
              disabled={importTeamBusy}
              className="rounded bg-amber/90 px-3 py-1 text-xs font-medium text-graphite transition hover:bg-amber disabled:cursor-not-allowed disabled:opacity-40"
            >
              {importTeamBusy ? t("packages.importing") : t("packages.import")}
            </button>
            {importTeamError && <p className="text-[11px] text-rose">{importTeamError}</p>}
          </div>
        )}
      </div>
    </div>
  );
}
