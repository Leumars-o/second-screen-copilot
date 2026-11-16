"use client";
import { useEffect, useState } from "react";
import type { MemorySnapshot } from "@ssc/protocol";
import type { Copilot } from "../lib/copilot";

type Item = MemorySnapshot["items"][number];
type Layer = Item["layer"];

const LAYERS: { id: Layer; label: string }[] = [
  { id: "about_me", label: "About me" },
  { id: "profile", label: "Profiles" },
  { id: "contact", label: "Contacts" },
  { id: "account", label: "Accounts" },
  { id: "session", label: "Sessions" },
];

interface Draft {
  id?: string;
  layer: Layer;
  title: string;
  body: string;
}

/** Every memory item is visible and editable here (spec: Memory and context). */
export function MemoryScreen({ copilot, items, online }: { copilot: Copilot; items: Item[] | null; online: boolean }) {
  const [layer, setLayer] = useState<Layer | "all">("all");
  const [draft, setDraft] = useState<Draft | null>(null);
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);

  useEffect(() => {
    if (online) copilot.listMemory();
  }, [copilot, online]);

  const save = () => {
    if (!draft || !draft.title.trim()) return;
    const item = { layer: draft.layer, title: draft.title.trim().slice(0, 200), body: draft.body.slice(0, 20000) };
    void copilot.command({ action: "memory.update", item: draft.id ? { id: draft.id, ...item } : item });
    setDraft(null);
  };

  const remove = (id: string) => {
    if (confirmDelete !== id) return setConfirmDelete(id);
    setConfirmDelete(null);
    void copilot.command({ action: "memory.delete", id });
  };

  if (draft) {
    return (
      <section className="screen" data-testid="memory-edit">
        <h1>{draft.id ? "Edit memory" : "New memory"}</h1>
        <label className="field">
          <span>Layer</span>
          <select value={draft.layer} onChange={(e) => setDraft({ ...draft, layer: e.target.value as Layer })}>
            {LAYERS.map((l) => (
              <option key={l.id} value={l.id}>
                {l.label}
              </option>
            ))}
          </select>
        </label>
        <label className="field">
          <span>Title</span>
          <input value={draft.title} maxLength={200} onChange={(e) => setDraft({ ...draft, title: e.target.value })} data-testid="memory-title" />
        </label>
        <label className="field">
          <span>Details</span>
          <textarea value={draft.body} rows={8} maxLength={20000} onChange={(e) => setDraft({ ...draft, body: e.target.value })} data-testid="memory-body" />
        </label>
        <div className="row">
          <button type="button" className="primary" disabled={!draft.title.trim()} onClick={save} data-testid="memory-save">
            Save
          </button>
          <button type="button" onClick={() => setDraft(null)}>
            Cancel
          </button>
        </div>
      </section>
    );
  }

  const shown = (items ?? []).filter((i) => layer === "all" || i.layer === layer).sort((a, b) => b.updated_at - a.updated_at);
  return (
    <section className="screen" data-testid="memory-screen">
      <div className="row spread">
        <h1>Memory</h1>
        <button type="button" onClick={() => setDraft({ layer: layer === "all" ? "about_me" : layer, title: "", body: "" })} data-testid="memory-new">
          New
        </button>
      </div>
      <div className="chips" role="tablist">
        {[{ id: "all" as const, label: "All" }, ...LAYERS].map((l) => (
          <button key={l.id} type="button" role="tab" aria-selected={layer === l.id} onClick={() => setLayer(l.id)}>
            {l.label}
          </button>
        ))}
      </div>
      {items === null && <p className="muted">{online ? "Loading…" : "Connect to the PC to see memory."}</p>}
      {items !== null && shown.length === 0 && <p className="muted">No items.</p>}
      <ul className="memory-list">
        {shown.map((i) => (
          <li key={i.id} className="memory-item" data-testid="memory-item">
            <button type="button" className="memory-open" onClick={() => setDraft({ id: i.id, layer: i.layer, title: i.title, body: i.body })}>
              <span className="muted small">{LAYERS.find((l) => l.id === i.layer)?.label}</span>
              <strong>{i.title}</strong>
              {i.body && <span className="muted clamp">{i.body}</span>}
            </button>
            <button type="button" className={confirmDelete === i.id ? "danger" : ""} onClick={() => remove(i.id)} data-testid="memory-delete">
              {confirmDelete === i.id ? "Confirm" : "Delete"}
            </button>
          </li>
        ))}
      </ul>
    </section>
  );
}
