import { useStore } from "../store";
import QrCode from "./QrCode";

interface Props {
  onClose: () => void;
}

export default function QrDialog({ onClose }: Props) {
  const authToken = useStore((s) => s.authToken);
  const publicUrl = useStore((s) => s.projectInfo?.public_url ?? null);
  const origin = publicUrl || (typeof window !== "undefined" ? window.location.origin : "");
  const url = authToken ? `${origin}/?token=${encodeURIComponent(authToken)}` : "";

  return (
    <div className="dialog-overlay" onClick={onClose}>
      <div
        className="dialog"
        style={{ maxWidth: 420, textAlign: "center" }}
        onClick={(e) => e.stopPropagation()}
      >
        <div className="dialog__eyebrow mint">mobile access</div>
        <div className="dialog__title">Scan with your phone</div>
        {url ? (
          <>
            <div style={{ margin: "12px 0 8px", display: "flex", justifyContent: "center" }}>
              <QrCode />
            </div>
            <div
              style={{
                fontFamily: "var(--font-mono)",
                fontSize: 11,
                wordBreak: "break-all",
                color: "var(--muted)",
                padding: "0 8px",
              }}
            >
              {url}
            </div>
          </>
        ) : (
          <div style={{ padding: 24 }}>
            <QrCode />
          </div>
        )}
        <div className="dialog__actions" style={{ justifyContent: "center", marginTop: 16 }}>
          <button className="btn btn--ghost" onClick={onClose}>
            Close
          </button>
          {url && (
            <button
              className="btn btn--primary"
              onClick={() => {
                // navigator.clipboard works only in secure contexts (HTTPS).
                // On plain HTTP fall back to execCommand.
                if (navigator.clipboard && window.isSecureContext) {
                  navigator.clipboard.writeText(url);
                } else {
                  const ta = document.createElement("textarea");
                  ta.value = url;
                  ta.style.position = "fixed"; ta.style.opacity = "0";
                  document.body.appendChild(ta);
                  ta.select();
                  document.execCommand("copy");
                  document.body.removeChild(ta);
                }
                // Brief "Copied!" feedback.
                const btn = (event as MouseEvent).target as HTMLButtonElement;
                const prev = btn.textContent;
                btn.textContent = "Copied!";
                btn.disabled = true;
                setTimeout(() => { btn.textContent = prev; btn.disabled = false; }, 1500);
              }}
            >
              Copy URL
            </button>
          )}
        </div>
        <div style={{
          marginTop: 14,
          fontSize: 11,
          color: "var(--faint)",
          lineHeight: 1.6,
        }}>
          add to Home Screen for PWA<br />
          <b>auto-tunnel: </b><code>nonoclaw --serve-http … --tunnel</code><br />
          <span style={{fontSize:10}}>Requires cloudflared: curl -L … -o ~/bin/cloudflared</span>
        </div>
      </div>
    </div>
  );
}
