import { useMemo } from "react";
import { useSettings, type Language } from "../lib/settings";
import { de } from "./de";
import { en } from "./en";
import { fa } from "./fa";

const DICTIONARIES: Record<Language, Record<string, string>> = { en, de, fa };

export const LANGUAGES: Array<{ id: Language; label: string }> = [
  { id: "en", label: "English" },
  { id: "de", label: "Deutsch" },
  { id: "fa", label: "فارسی" },
];

export function localeFor(language: Language): string {
  if (language === "fa") return "fa-IR";
  if (language === "de") return "de-DE";
  return "en-US";
}

/** Looks up a key and substitutes `{name}` placeholders. Unknown keys fall
 * back to English, then to the key itself, so a missed translation can
 * never blank out a screen. */
export function translate(
  language: Language,
  key: string,
  vars?: Record<string, string | number>,
): string {
  const template = DICTIONARIES[language][key] ?? DICTIONARIES.en[key] ?? key;
  if (!vars) return template;
  return template.replace(/\{(\w+)\}/g, (match, name: string) =>
    Object.prototype.hasOwnProperty.call(vars, name) ? String(vars[name]) : match,
  );
}

export function useI18n() {
  const { settings } = useSettings();
  const language = settings.language;
  return useMemo(
    () => ({
      language,
      locale: localeFor(language),
      dir: language === "fa" ? ("rtl" as const) : ("ltr" as const),
      t: (key: string, vars?: Record<string, string | number>) => translate(language, key, vars),
      has: (key: string) => key in DICTIONARIES[language] || key in DICTIONARIES.en,
    }),
    [language],
  );
}
