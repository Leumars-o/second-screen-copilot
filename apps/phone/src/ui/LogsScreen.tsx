"use client";
import type { AgentLog, AgentStatus } from "@ssc/protocol";
import { useEffect, useState } from "react";

const STATE_ICON = { ok: "●", warn: "▲", error: "■", off: "○" } as const;
/** The PC sends status every 15 s: three missed beats means it has gone quiet. */
export const STATUS_STALE_MS = 45_000;

function clock(t: number): string {
  return new Date(t).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
}

function ago(ms: number): string {
  const s = Math.max(0, Math.round(ms / 1000));
  return s < 60 ? `${s} s` : s < 3600 ? `${Math.round(s / 60)} min` : `${Math.round(s / 3600)} h`;
}

/** Re-render every few seconds so "x s ago" and staleness stay current. */
export function useNow(every = 5000): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), every);
    return () => clearInterval(id);
  }, [every]);
  return now;
}

export type HealthView = { level: "ok" | "warn" | "error" | "off"; text: string };

/** One line for the main screen: link first, then the PC's own health. Pure, for tests. */
export function healthLine(online: boolean, status: AgentStatus | null, statusAt: number | null, now: number): HealthView {
  if (!online) return { level: "off", text: "Not connected to the PC" };
  if (!status || statusAt === null) return { level: "warn", text: "Connected · waiting for the PC's status…" };
  const age = now - statusAt;
  if (age > STATUS_STALE_MS) return { level: "error", text: `No word from the PC for ${ago(age)}: agent stopped or busy?` };
  return { level: status.health, text: status.summary };
}

export function HealthLine({ view }: { view: HealthView }) {
  return (
    <p className={`health health-${view.level}`} data-testid="health-line" data-level={view.level}>
      <span className="health-icon" aria-hidden>
        {STATE_ICON[view.level]}
      </span>
      {view.text}
    </p>
  );
}

export function LogsScreen({
  status,
  statusAt,
  logs,
  onRefresh,
}: {
  status: AgentStatus | null;
  statusAt: number | null;
  logs: AgentLog["entries"];
  onRefresh: () => void;
}) {
  const now = useNow();
  const [level, setLevel] = useState<"all" | "problems">("all");
  const shown = [...logs].reverse().filter((e) => level === "all" || e.level !== "info");
  return (
    <section className="screen logs" data-testid="logs-screen">
      <div className="logs-head">
        <h1>PC health</h1>
        <button type="button" className="small" onClick={onRefresh} data-testid="logs-refresh">
          Refresh
        </button>
      </div>
      {status ? (
        <>
          <p className="muted small">Updated {statusAt ? `${ago(now - statusAt)} ago` : "—"}</p>
          <ul className="checks" data-testid="health-checks">
            {status.checks.map((c) => (
              <li key={c.id} className={`check check-${c.state}`} data-state={c.state}>
                <span className="health-icon" aria-hidden>
                  {STATE_ICON[c.state]}
                </span>
                <b>{c.label}</b>
                <span className="muted"> {c.detail}</span>
              </li>
            ))}
          </ul>
        </>
      ) : (
        <p className="muted">No status from the PC yet.</p>
      )}

      <div className="logs-head">
        <h2>Logs</h2>
        <div className="seg" role="group" aria-label="Filter">
          {(["all", "problems"] as const).map((l) => (
            <button key={l} type="button" aria-pressed={level === l} className="small" onClick={() => setLevel(l)}>
              {l === "all" ? "All" : "Problems"}
            </button>
          ))}
        </div>
      </div>
      {shown.length === 0 ? (
        <p className="muted">{level === "all" ? "No log lines yet." : "No problems logged."}</p>
      ) : (
        <ol className="log" data-testid="log-lines">
          {shown.map((e) => (
            <li key={`${e.t}|${e.source}|${e.text}`} className={`log-${e.level}`} data-level={e.level}>
              <span className="log-t">{clock(e.t)}</span> <span className="log-src">{e.source}</span> {e.text}
            </li>
          ))}
        </ol>
      )}
    </section>
  );
}
