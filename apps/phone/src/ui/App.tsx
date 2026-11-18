"use client";
import { useCallback, useEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";
import type { LinkStatus } from "@ssc/protocol";
import { Copilot } from "../lib/copilot";
import { connect, defaultDeviceName, forgetPairing, loadPairing, pairFromQr, takePairFromLocation, type LinkHandle, type StoredPairing } from "../lib/connection";
import { browserVibrate } from "../lib/vibrate";
import { AnswerCard } from "./AnswerCard";
import { FollowUp } from "./FollowUp";
import { MemoryScreen } from "./MemoryScreen";
import { PairScreen } from "./PairScreen";
import { ContactsScreen, SummaryScreen } from "./SummaryScreen";

type Screen = "live" | "summary" | "memory" | "contacts";

function StatusLine({ status, paired, onUnpair }: { status: LinkStatus | null; paired: boolean; onUnpair: () => void }) {
  const [menu, setMenu] = useState(false);
  let label = "Starting…";
  let cls = "off";
  if (status?.state === "connected") {
    label = paired ? (status.via === "lan" ? "Connected · Wi-Fi" : "Connected · relay") : "Waiting for PC…";
    cls = status.via === "lan" ? "ok" : "relay";
  } else if (status?.state === "connecting") label = "Connecting…";
  else if (status?.state === "offline") label = "Offline · retrying";
  return (
    <div className="status">
      <button type="button" className="status-btn" onClick={() => setMenu(!menu)} data-testid="status" data-state={status?.state ?? "none"}>
        <span className={`dot dot-${cls}`} />
        {label}
      </button>
      {menu && (
        <button type="button" className="danger small" onClick={onUnpair} data-testid="unpair">
          Unpair
        </button>
      )}
    </div>
  );
}

export function App() {
  const linkRef = useRef<LinkHandle | null>(null);
  const copilot = useMemo(
    () =>
      new Copilot({
        send: (msg) => linkRef.current?.link.send(msg) ?? false,
        vibrate: browserVibrate,
      }),
    [],
  );
  const state = useSyncExternalStore(copilot.subscribe, copilot.getState, copilot.getState);
  const [stored, setStored] = useState<StoredPairing | null | undefined>(undefined);
  const [status, setStatus] = useState<LinkStatus | null>(null);
  const [paired, setPaired] = useState(false);
  const [pairError, setPairError] = useState<string | null>(null);
  const [screen, setScreen] = useState<Screen>("live");
  const [openAnswer, setOpenAnswer] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    // CR-19: opened from the camera by a pairing link. A link can come from anywhere (a message,
    // a web page), so replacing an existing pairing needs the user's yes.
    const link = takePairFromLocation();
    void loadPairing().then(async (s) => {
      if (!live) return;
      if (link && (!s || window.confirm("Pair with this PC? It replaces your current pairing."))) {
        try {
          const fresh = await pairFromQr(link, defaultDeviceName());
          if (!live) return;
          setStored(fresh);
          setPaired(false);
          return;
        } catch (e) {
          setPairError((e as Error).message);
        }
      }
      setStored(s);
      setPaired(!!s?.confirmed);
    });
    (window as unknown as { __ssc?: Copilot }).__ssc = copilot; // debug + E2E render timing
    return () => {
      live = false;
    };
  }, [copilot]);

  useEffect(() => {
    if (!stored) return;
    const h = connect(stored, copilot, { onStatus: setStatus, onPaired: () => setPaired(true) });
    linkRef.current = h;
    return () => {
      h.stop();
      if (linkRef.current === h) linkRef.current = null;
    };
  }, [stored, copilot]);

  // Show the summary as soon as one arrives.
  const summaryCount = state.summaries.length;
  useEffect(() => {
    if (summaryCount > 0) setScreen("summary");
  }, [summaryCount]);

  const onPair = useCallback(async (qrText: string, deviceName: string) => {
    setPairError(null);
    try {
      setStored(await pairFromQr(qrText, deviceName));
    } catch (e) {
      setPairError((e as Error).message);
    }
  }, []);

  const unpair = () => {
    linkRef.current?.stop();
    linkRef.current = null;
    void forgetPairing();
    setStored(null);
    setPaired(false);
    setStatus(null);
  };

  if (stored === undefined) return <main className="app" />; // first client render: pairing not loaded yet
  if (stored === null) {
    return (
      <main className="app">
        <PairScreen onPair={onPair} error={pairError} />
      </main>
    );
  }

  const online = status?.state === "connected" && paired;
  const [current, ...older] = state.answers;
  const pinned = older.filter((a) => a.pinned);
  const pendingContacts = state.summaries.flatMap((s) => s.contact_suggestions).filter((c) => state.confirmedContacts[c.id] === undefined).length;
  const lastThem = [...state.transcript].reverse().find((t) => t.speaker === "them");

  return (
    <main className="app">
      <StatusLine status={status} paired={paired} onUnpair={unpair} />

      {screen === "live" && (
        <section className="screen live" data-testid="live-screen">
          {state.session && <p className="session muted small">{state.session.mode === "local" ? "Local mode" : "Cloud mode"} · {state.session.profile_id}</p>}
          {lastThem && (
            <p className="them" data-testid="transcript">
              {lastThem.text}
            </p>
          )}
          {current ? (
            <AnswerCard key={current.id} copilot={copilot} answer={current} current />
          ) : (
            <p className="empty muted">{online ? "Listening. Answers appear here." : "Not connected to the PC yet."}</p>
          )}
          {current && <FollowUp onSend={(t) => copilot.followup(t, current.id)} />}
          {pinned.length > 0 && (
            <>
              <h2>Pinned</h2>
              {pinned.map((a) => (
                <AnswerCard key={a.id} copilot={copilot} answer={a} current={false} />
              ))}
            </>
          )}
          {older.length > 0 && (
            <>
              <h2>Earlier</h2>
              <ul className="history">
                {older
                  .filter((a) => !a.pinned)
                  .map((a) => (
                    <li key={a.id}>
                      <button type="button" className="history-item" onClick={() => setOpenAnswer(openAnswer === a.id ? null : a.id)}>
                        {a.end?.cues[0]?.text ?? a.end?.fact ?? (a.text.cue.split("\n")[0] || a.start.intent)}
                      </button>
                      {openAnswer === a.id && <AnswerCard copilot={copilot} answer={a} current={false} />}
                    </li>
                  ))}
              </ul>
            </>
          )}
        </section>
      )}
      {screen === "summary" && <SummaryScreen copilot={copilot} summaries={state.summaries} confirmed={state.confirmedContacts} />}
      {screen === "memory" && <MemoryScreen copilot={copilot} items={state.memory} online={online} />}
      {screen === "contacts" && <ContactsScreen copilot={copilot} summaries={state.summaries} confirmed={state.confirmedContacts} />}

      <nav className="tabs" aria-label="Screens">
        {(
          [
            ["live", "Live"],
            ["summary", "Summary"],
            ["memory", "Memory"],
            ["contacts", pendingContacts ? `Contacts ${pendingContacts}` : "Contacts"],
          ] as const
        ).map(([id, label]) => (
          <button key={id} type="button" aria-current={screen === id ? "page" : undefined} onClick={() => setScreen(id)} data-testid={`tab-${id}`}>
            {label}
          </button>
        ))}
      </nav>
    </main>
  );
}
