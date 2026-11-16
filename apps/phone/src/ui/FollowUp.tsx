"use client";
import { useEffect, useRef, useState } from "react";

interface Recognition {
  lang: string;
  interimResults: boolean;
  continuous: boolean;
  onresult: ((e: { results: ArrayLike<ArrayLike<{ transcript: string }>> }) => void) | null;
  onend: (() => void) | null;
  onerror: (() => void) | null;
  start(): void;
  stop(): void;
}
type RecognitionCtor = new () => Recognition;

function speechCtor(): RecognitionCtor | null {
  if (typeof window === "undefined") return null;
  const w = window as unknown as { SpeechRecognition?: RecognitionCtor; webkitSpeechRecognition?: RecognitionCtor };
  return w.SpeechRecognition ?? w.webkitSpeechRecognition ?? null;
}

/** Follow-up by typing, or by voice where the Web Speech API exists. */
export function FollowUp({ onSend }: { onSend: (text: string) => Promise<boolean> }) {
  const [text, setText] = useState("");
  const [listening, setListening] = useState(false);
  const [canSpeak, setCanSpeak] = useState(false);
  const rec = useRef<Recognition | null>(null);

  useEffect(() => setCanSpeak(!!speechCtor()), []);

  const send = async () => {
    if (await onSend(text)) setText("");
  };

  const toggleVoice = () => {
    if (listening) return rec.current?.stop();
    const Ctor = speechCtor();
    if (!Ctor) return;
    const r = new Ctor();
    r.lang = navigator.language || "en-US";
    r.interimResults = true;
    r.continuous = false;
    r.onresult = (e) => setText(Array.from(e.results, (res) => res[0]?.transcript ?? "").join(" "));
    r.onend = () => setListening(false);
    r.onerror = () => setListening(false);
    rec.current = r;
    setListening(true);
    r.start();
  };

  return (
    <form
      className="followup"
      onSubmit={(e) => {
        e.preventDefault();
        void send();
      }}
    >
      <input
        value={text}
        onChange={(e) => setText(e.target.value)}
        placeholder="Ask a follow-up"
        maxLength={2000}
        enterKeyHint="send"
        data-testid="followup-input"
      />
      {canSpeak && (
        <button type="button" onClick={toggleVoice} aria-pressed={listening} aria-label="Speak a follow-up" data-testid="followup-voice">
          {listening ? "Stop" : "Mic"}
        </button>
      )}
      <button type="submit" className="primary" disabled={!text.trim()} data-testid="followup-send">
        Ask
      </button>
    </form>
  );
}
