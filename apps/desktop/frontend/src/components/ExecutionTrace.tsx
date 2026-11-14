import type { EventDto, EventType } from "../lib/types";
import { relativeTime, titleCase } from "../lib/format";
import { useI18n } from "../i18n";

// The signature element (per the product's own UI/UX brief: "the
// execution trace is a core UX feature... users can click every
// stage"). Rendered as a vertical trace line, literally the circuit-
// board motif the whole product's visual identity is built on (see the
// app icon), with each event a node on the line. This isn't a chat
// log: order and causation are the content, so a connected line is the
// only honest way to show it.

const TERMINAL_GOOD: EventType[] = ["verified", "task_completed", "approved"];
const NEEDS_ATTENTION: EventType[] = [
  "task_blocked",
  "revision_requested",
  "escalated",
  "rejected",
  "task_failed",
];

function nodeColor(type: EventType): string {
  if (TERMINAL_GOOD.includes(type)) return "bg-teal ring-teal/30";
  if (NEEDS_ATTENTION.includes(type)) return "bg-rose ring-rose/30";
  return "bg-amber ring-amber/30";
}

export function ExecutionTrace({
  events,
  taskTitle,
  isLive,
}: {
  events: EventDto[];
  taskTitle: string | null;
  isLive: boolean;
}) {
  const { t, locale, has } = useI18n();
  if (!taskTitle) {
    return (
      <div className="flex h-full items-center justify-center rounded-lg border border-dashed border-line px-6 text-center">
        <p className="text-sm text-muted">{t("trace.empty")}</p>
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col">
      <div className="mb-4">
        <p className="text-xs font-mono uppercase tracking-wide text-muted">{t("trace.title")}</p>
        <h2 className="text-sm font-medium text-ink">{taskTitle}</h2>
      </div>

      {events.length === 0 ? (
        <p className="text-sm text-muted">{t("trace.waiting")}</p>
      ) : (
        <ol className="relative flex-1 space-y-0 overflow-y-auto pr-1">
          {buildDisplayItems(events).map((item, i, all) => {
            const isLast = i === all.length - 1;
            if (item.kind === "stream") {
              return (
                <li key={item.key} className="relative flex gap-3 pb-5 last:pb-0">
                  {!isLast && (
                    <span className="absolute left-[5px] top-3 h-full w-px bg-line" aria-hidden />
                  )}
                  <span
                    className={`relative mt-1.5 h-[11px] w-[11px] shrink-0 rounded-full bg-amber ring-4 ring-amber/30 ${
                      isLast && isLive ? "animate-pulse" : ""
                    }`}
                    aria-hidden
                  />
                  <div className="min-w-0 flex-1">
                    <p className="text-sm text-ink">{t("trace.streaming")}</p>
                    <p className="whitespace-pre-wrap text-xs text-muted">{item.text}</p>
                  </div>
                </li>
              );
            }
            const event = item.event;
            return (
              <li key={event.event_id} className="relative flex gap-3 pb-5 last:pb-0">
                {!isLast && (
                  <span
                    className="absolute left-[5px] top-3 h-full w-px bg-line"
                    aria-hidden
                  />
                )}
                <span
                  className={`relative mt-1.5 h-[11px] w-[11px] shrink-0 rounded-full ring-4 ${nodeColor(
                    event.event_type,
                  )} ${isLast && isLive ? "animate-pulse" : ""}`}
                  aria-hidden
                />
                <div className="min-w-0 flex-1">
                  <div className="flex items-baseline justify-between gap-2">
                    <p className="text-sm text-ink">
                      {has(`event.${event.event_type}`)
                        ? t(`event.${event.event_type}`)
                        : titleCase(event.event_type)}
                    </p>
                    <time className="shrink-0 font-mono text-[11px] text-muted">
                      {relativeTime(event.timestamp, locale)}
                    </time>
                  </div>
                  <p className="text-xs text-muted">
                    {event.actor_type === "human" ? t("trace.human") : event.actor_id}
                  </p>
                  {hasVisiblePayload(event.payload) && (
                    <pre className="mt-1 max-h-24 overflow-auto rounded bg-graphite px-2 py-1 font-mono text-[11px] text-muted">
                      {formatPayload(event.payload)}
                    </pre>
                  )}
                </div>
              </li>
            );
          })}
        </ol>
      )}
    </div>
  );
}

function hasVisiblePayload(payload: unknown): boolean {
  return typeof payload === "object" && payload !== null && Object.keys(payload).length > 0;
}

function formatPayload(payload: unknown): string {
  try {
    return JSON.stringify(payload, null, 2);
  } catch {
    return String(payload);
  }
}

// Round 39: `model_token_generated` events arrive one per chunk, often
// dozens per generation, and durably persisting or rendering each as
// its own trace node would be exactly the noise the product brief's
// "chat should not become the database" principle warns against (see
// `EventBus::publish_ephemeral`'s own doc comment on the backend side
// of this same call). So consecutive token events are coalesced here
// into a single growing "stream" item, in place in the timeline
// wherever they occurred, one live-updating node, not N discrete ones.
type DisplayItem = { kind: "event"; event: EventDto } | { kind: "stream"; key: string; text: string };

function chunkText(payload: unknown): string {
  if (typeof payload === "object" && payload !== null && "chunk" in payload) {
    return String((payload as { chunk: unknown }).chunk ?? "");
  }
  return "";
}

function buildDisplayItems(events: EventDto[]): DisplayItem[] {
  const items: DisplayItem[] = [];
  for (const event of events) {
    if (event.event_type === "model_token_generated") {
      const last = items[items.length - 1];
      if (last && last.kind === "stream") {
        last.text += chunkText(event.payload);
      } else {
        items.push({ kind: "stream", key: event.event_id, text: chunkText(event.payload) });
      }
      continue;
    }
    items.push({ kind: "event", event });
  }
  return items;
}
