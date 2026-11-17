"use client";
import { useState } from "react";
import type { SessionSummary } from "@ssc/protocol";
import type { Copilot } from "../lib/copilot";

export function ContactSuggestions({ copilot, summary, confirmed }: { copilot: Copilot; summary: SessionSummary; confirmed: Record<string, boolean> }) {
  if (summary.contact_suggestions.length === 0) return null;
  return (
    <ul className="contacts" data-testid="contact-suggestions">
      {summary.contact_suggestions.map((c) => {
        const state = confirmed[c.id];
        return (
          <li key={c.id} className="contact">
            <div>
              <strong>{c.name}</strong>
              {c.company && <span className="muted"> · {c.company}</span>}
              {c.notes && <p className="muted small">{c.notes}</p>}
            </div>
            {state === undefined ? (
              <div className="row">
                <button type="button" className="primary" onClick={() => copilot.confirmContact(c.id, true)} data-testid="contact-save">
                  Save
                </button>
                <button type="button" onClick={() => copilot.confirmContact(c.id, false)} data-testid="contact-skip">
                  Skip
                </button>
              </div>
            ) : (
              <span className="muted" data-testid="contact-done">
                {state ? "Saved" : "Skipped"}
              </span>
            )}
          </li>
        );
      })}
    </ul>
  );
}

export function SummaryScreen({ copilot, summaries, confirmed }: { copilot: Copilot; summaries: SessionSummary[]; confirmed: Record<string, boolean> }) {
  const [copied, setCopied] = useState(false);
  const s = summaries[0];
  if (!s) {
    return (
      <section className="screen" data-testid="summary-screen">
        <h1>Session summary</h1>
        <p className="muted">The summary appears here when a call ends.</p>
      </section>
    );
  }
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(s.followup_email);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch {
      setCopied(false);
    }
  };
  return (
    <section className="screen" data-testid="summary-screen">
      <h1>Session summary</h1>
      <p className="summary-text" data-testid="summary-text">
        {s.summary}
      </p>
      {s.action_items.length > 0 && (
        <>
          <h2>Action items</h2>
          <ul className="action-items" data-testid="action-items">
            {s.action_items.map((a, i) => (
              <li key={i}>{a}</li>
            ))}
          </ul>
        </>
      )}
      {s.followup_email && (
        <>
          <h2>Follow-up email</h2>
          <pre className="email" data-testid="followup-email">
            {s.followup_email}
          </pre>
          <button type="button" className="primary" onClick={copy} data-testid="copy-email">
            {copied ? "Copied" : "Copy email"}
          </button>
        </>
      )}
      {s.contact_suggestions.length > 0 && (
        <>
          <h2>New contacts</h2>
          <ContactSuggestions copilot={copilot} summary={s} confirmed={confirmed} />
        </>
      )}
    </section>
  );
}

/** Contact confirmation: every pending suggestion from recent sessions, one tap each. */
export function ContactsScreen({ copilot, summaries, confirmed }: { copilot: Copilot; summaries: SessionSummary[]; confirmed: Record<string, boolean> }) {
  const withSuggestions = summaries.filter((s) => s.contact_suggestions.length > 0);
  return (
    <section className="screen" data-testid="contacts-screen">
      <h1>Confirm contacts</h1>
      <p className="muted">Contacts are only saved to memory after you confirm them.</p>
      {withSuggestions.length === 0 && <p className="muted">Nothing to confirm.</p>}
      {withSuggestions.map((s) => (
        <ContactSuggestions key={s.session_id} copilot={copilot} summary={s} confirmed={confirmed} />
      ))}
    </section>
  );
}
