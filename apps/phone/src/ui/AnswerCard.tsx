"use client";
import { useLayoutEffect, useRef, useState } from "react";
import type { Answer, Copilot } from "../lib/copilot";

/** Streamed text, appended imperatively (one text node per delta). React never re-renders it. */
function StreamBody({ copilot, answer, showCue }: { copilot: Copilot; answer: Answer; showCue: boolean }) {
  const cueRef = useRef<HTMLDivElement>(null);
  const detailRef = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const cue = cueRef.current!;
    const detail = detailRef.current!;
    // Render what arrived before mount, then append from here on.
    cue.textContent = answer.text.cue;
    detail.textContent = answer.text.detail;
    return copilot.registerSink(answer.id, {
      append(block, text) {
        (block === "cue" ? cue : detail).appendChild(document.createTextNode(text));
      },
    });
  }, [copilot, answer.id]); // eslint-disable-line react-hooks/exhaustive-deps -- text is read once on mount by design
  return (
    <>
      <div ref={cueRef} className="stream stream-cue" data-testid="stream-cue" hidden={!showCue} />
      <div ref={detailRef} className="stream stream-detail" data-testid="stream-detail" />
    </>
  );
}

/** CR-23b: what the model was given. Tap to enlarge; tap again to shrink. */
function ScreenThumb({ src }: { src: string }) {
  const [big, setBig] = useState(false);
  return (
    <button type="button" className={`thumb${big ? " thumb-big" : ""}`} onClick={() => setBig(!big)} aria-label={big ? "Shrink screenshot" : "Show the screenshot the answer used"} data-testid="screen-thumb">
      <img src={src} alt="What the PC captured" />
    </button>
  );
}

const FORMAT_LABEL = { cue: "Cues", fact: "Fact", objection: "Objection", screen: "Screen" } as const;

export function AnswerCard({ copilot, answer, current }: { copilot: Copilot; answer: Answer; current: boolean }) {
  const end = answer.end;
  // CR-20: a screen answer is free text (answer, then working), shown as streamed.
  const structured = !!end && end.cues.length > 0 && answer.start.format !== "screen";
  return (
    <article
      className={`card card-${answer.start.format}${current ? " card-current" : ""}${answer.pinned ? " card-pinned" : ""}`}
      data-testid={current ? "answer-current" : "answer"}
      data-answer-id={answer.id}
      data-state={end ? "done" : "streaming"}
    >
      <header className="card-head">
        <span className={`chip urgency-${answer.start.urgency}`}>{FORMAT_LABEL[answer.start.format]}</span>
        {answer.start.intent !== "none" && <span className="chip">{answer.start.intent.replace("_", " ")}</span>}
        {answer.pinned && <span className="chip chip-pin">Pinned</span>}
        {!end && <span className="live-dot" aria-label="streaming" />}
      </header>

      {answer.start.thumb && <ScreenThumb src={answer.start.thumb} />}

      {end?.not_in_docs && (
        <p className="not-in-docs" data-testid="not-in-docs">
          Not in your docs
        </p>
      )}

      {end?.fact && (
        <p className="fact" data-testid="fact">
          <strong>{end.fact}</strong>
          {end.source_doc && <span className="source">{end.source_doc}</span>}
        </p>
      )}

      {structured && (
        <ol className="cues" data-testid="cues">
          {end.cues.map((c, i) => {
            const open = answer.expandAll || answer.expanded === i;
            return (
              <li key={i}>
                <button
                  type="button"
                  className="cue"
                  aria-expanded={c.expand ? open : undefined}
                  disabled={!c.expand}
                  onClick={() => copilot.toggleCue(answer.id, i)}
                  data-testid={`cue-${i}`}
                >
                  <span className="cue-n">{i + 1}</span>
                  <span className="cue-t">{c.text}</span>
                </button>
                {open && c.expand && (
                  <p className="cue-expand" data-testid={`cue-expand-${i}`}>
                    {c.expand}
                  </p>
                )}
              </li>
            );
          })}
        </ol>
      )}

      <StreamBody copilot={copilot} answer={answer} showCue={!structured} />

      {end && end.reason !== "done" && <p className="muted">{end.reason === "cancelled" ? "Cancelled" : "Answer failed"}</p>}

      {current && (
        <div className="actions" role="toolbar" aria-label="Answer actions">
          <button type="button" onClick={() => copilot.pin(answer.id)} data-testid="act-pin" aria-pressed={answer.pinned}>
            {answer.pinned ? "Unpin" : "Pin"}
          </button>
          <button type="button" onClick={() => copilot.simple("regenerate", answer.id)} data-testid="act-regenerate">
            Again
          </button>
          <button type="button" onClick={() => copilot.simple("shorter", answer.id)} data-testid="act-shorter">
            Shorter
          </button>
          <button type="button" onClick={() => copilot.simple("number", answer.id)} data-testid="act-number">
            Number
          </button>
          {answer.start.format !== "screen" && (
            <button type="button" onClick={() => copilot.expandAll(answer.id)} data-testid="act-expand-all">
              Expand all
            </button>
          )}
          <button type="button" onClick={() => copilot.rate(answer.id, "up")} aria-pressed={answer.rating === "up"} data-testid="act-up" aria-label="Useful">
            +1
          </button>
          <button type="button" onClick={() => copilot.rate(answer.id, "down")} aria-pressed={answer.rating === "down"} data-testid="act-down" aria-label="Not useful">
            -1
          </button>
        </div>
      )}
    </article>
  );
}
