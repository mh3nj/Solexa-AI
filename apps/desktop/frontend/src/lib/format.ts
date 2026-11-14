export function titleCase(snake: string): string {
  return snake
    .split("_")
    .map((word) => word.charAt(0).toUpperCase() + word.slice(1))
    .join(" ");
}

/** "3 minutes ago", "vor 3 Minuten", "۳ دقیقه پیش". Intl handles the
 * plural forms, which hand-written English strings cannot do for German
 * or Persian. */
export function relativeTime(iso: string, locale = "en-US"): string {
  const diffMs = Date.now() - new Date(iso).getTime();
  const rtf = new Intl.RelativeTimeFormat(locale, { numeric: "auto" });
  const seconds = Math.round(diffMs / 1000);
  if (seconds < 45) return rtf.format(-Math.max(seconds, 0), "second");
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return rtf.format(-minutes, "minute");
  const hours = Math.round(minutes / 60);
  if (hours < 24) return rtf.format(-hours, "hour");
  return new Date(iso).toLocaleDateString(locale);
}

/** A deadline is a future instant, so it gets its own formatter rather
 * than feeding negative numbers through the past-tense helper. */
export function dueIn(iso: string, locale = "en-US", overdueLabel = "overdue"): string {
  const diffMs = new Date(iso).getTime() - Date.now();
  if (diffMs <= 0) return overdueLabel;
  const rtf = new Intl.RelativeTimeFormat(locale, { numeric: "always" });
  const seconds = Math.round(diffMs / 1000);
  if (seconds < 60) return rtf.format(seconds, "second");
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return rtf.format(minutes, "minute");
  const hours = Math.round(minutes / 60);
  if (hours < 24) return rtf.format(hours, "hour");
  const days = Math.round(hours / 24);
  return rtf.format(days, "day");
}
