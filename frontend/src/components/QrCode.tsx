import { useEffect, useRef } from "react";
import QRCode from "qrcode";
import { useStore } from "../store";

/**
 * Renders a QR code canvas encoding the server URL + auth token for mobile
 * access. Returns nothing if no auth token is available (local-only mode).
 */
export default function QrCode() {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const authToken = useStore((s) => s.authToken);
  const sessionId = useStore((s) => s.sessionId);
  const publicUrl = useStore((s) => s.projectInfo?.public_url ?? null);
  const origin = publicUrl || (typeof window !== "undefined" ? window.location.origin : "");

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas || !authToken) return;
    let url = `${origin}/?token=${encodeURIComponent(authToken)}`;
    if (sessionId) url += `&session=${encodeURIComponent(sessionId)}`;
    QRCode.toCanvas(canvas, url, {
      width: 280,
      margin: 4,
      color: { dark: "#000000", light: "#ffffff" },
    });
  }, [authToken, origin]);

  if (!authToken) {
    return (
      <div style={{ color: "var(--faint)", padding: 12, fontSize: 13 }}>
        no auth token — server may be in local-only mode
      </div>
    );
  }
  return (
    <canvas
      ref={canvasRef}
      style={{
        display: "block",
        borderRadius: 12,
        background: "#070a0f",
        padding: 8,
      }}
    />
  );
}
