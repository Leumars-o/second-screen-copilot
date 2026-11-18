"use client";
import { useEffect, useRef, useState } from "react";
import { defaultDeviceName } from "../lib/connection";

/** QR scan with the camera (@zxing/browser), or paste the QR text shown on the PC. */
export function PairScreen({ onPair, error }: { onPair: (qrText: string, deviceName: string) => void; error: string | null }) {
  const [deviceName, setDeviceName] = useState("");
  const [text, setText] = useState("");
  const [scanning, setScanning] = useState(false);
  const [camError, setCamError] = useState<string | null>(null);
  const videoRef = useRef<HTMLVideoElement>(null);

  useEffect(() => setDeviceName(defaultDeviceName()), []);

  useEffect(() => {
    if (!scanning) return;
    let stop: (() => void) | undefined;
    let cancelled = false;
    (async () => {
      // Safari/Chrome hide the camera (navigator.mediaDevices) on plain http://.
      if (!window.isSecureContext || !navigator.mediaDevices?.getUserMedia) {
        setCamError(
          window.isSecureContext
            ? "This browser has no camera access. Scan the QR with the phone's camera app, or paste the code."
            : `The camera needs a secure page. Open https://${location.host} (or scan the QR with the phone's camera app).`,
        );
        setScanning(false);
        return;
      }
      try {
        const { BrowserQRCodeReader } = await import("@zxing/browser");
        const reader = new BrowserQRCodeReader();
        const controls = await reader.decodeFromVideoDevice(undefined, videoRef.current!, (result, _err, c) => {
          if (result && !cancelled) {
            c.stop();
            setScanning(false);
            onPair(result.getText(), deviceName);
          }
        });
        if (cancelled) controls.stop();
        else stop = () => controls.stop();
      } catch (e) {
        setCamError(`Camera unavailable: ${(e as Error).message}. Paste the code instead.`);
        setScanning(false);
      }
    })();
    return () => {
      cancelled = true;
      stop?.();
    };
  }, [scanning, deviceName, onPair]);

  return (
    <section className="screen pair" data-testid="pair-screen">
      <h1>Pair with your PC</h1>
      <p className="muted">Open Settings → Pair phone on the PC. The code works once and expires after 2 minutes.</p>

      <label className="field">
        <span>This phone&apos;s name</span>
        <input value={deviceName} onChange={(e) => setDeviceName(e.target.value)} maxLength={64} data-testid="device-name" />
      </label>

      {scanning ? (
        <div className="scanner">
          <video ref={videoRef} muted playsInline />
          <button type="button" onClick={() => setScanning(false)}>
            Cancel
          </button>
        </div>
      ) : (
        <button type="button" className="primary big" onClick={() => (setCamError(null), setScanning(true))} data-testid="scan">
          Scan QR code
        </button>
      )}
      {camError && <p className="error">{camError}</p>}

      <details className="paste" open={!!camError}>
        <summary>No camera? Paste the code</summary>
        <textarea
          value={text}
          onChange={(e) => setText(e.target.value)}
          rows={5}
          placeholder='{"v":1,"urls":[...],...}'
          spellCheck={false}
          data-testid="qr-text"
        />
        <button type="button" className="primary" disabled={!text.trim()} onClick={() => onPair(text, deviceName)} data-testid="pair-submit">
          Pair
        </button>
      </details>
      {error && (
        <p className="error" role="alert" data-testid="pair-error">
          {error}
        </p>
      )}
    </section>
  );
}
