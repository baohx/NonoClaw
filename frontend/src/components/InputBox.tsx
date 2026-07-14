import { useRef, useCallback, useEffect, useState } from "react";
import type { AttachmentRef, UploadResponse } from "../types";

interface PendingAttachment {
  id: string;
  filename: string;
  extracted_text: string;
  image_count: number;
  uploading: boolean;
  error?: string;
}

interface Props {
  onSubmit: (text: string, attachments: AttachmentRef[]) => void;
  disabled?: boolean;
}

const ALLOWED_EXT = ".pdf,.docx,.doc,.txt,.md,.markdown,.png,.jpg,.jpeg";

export default function InputBox({ onSubmit, disabled }: Props) {
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const historyRef = useRef<string[]>([]);
  const historyIdx = useRef(-1);
  const draftRef = useRef("");
  const [hasText, setHasText] = useState(false);
  const [attachments, setAttachments] = useState<PendingAttachment[]>([]);
  const [dragOver, setDragOver] = useState(false);
  const dragCounter = useRef(0);

  useEffect(() => {
    textareaRef.current?.focus();
  }, []);

  // ── File upload ─────────────────────────────────────────────────────────

  const uploadFile = useCallback(async (file: File) => {
    const id = crypto.randomUUID();
    const chip: PendingAttachment = {
      id,
      filename: file.name,
      extracted_text: "",
      image_count: 0,
      uploading: true,
    };
    setAttachments((prev) => [...prev, chip]);

    try {
      const form = new FormData();
      form.append("file", file);
      const resp = await fetch("/api/upload", { method: "POST", body: form });
      if (!resp.ok) {
        const err = await resp.json().catch(() => ({ error: `HTTP ${resp.status}` }));
        throw new Error(err.error || `HTTP ${resp.status}`);
      }
      const data: UploadResponse = await resp.json();
      if (data.error) throw new Error(data.error);
      setAttachments((prev) =>
        prev.map((a) =>
          a.id === id
            ? { ...a, uploading: false, extracted_text: data.extracted_text, image_count: data.image_count }
            : a
        )
      );
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : "upload failed";
      setAttachments((prev) =>
        prev.map((a) => (a.id === id ? { ...a, uploading: false, error: msg } : a))
      );
    }
  }, []);

  const removeAttachment = useCallback((id: string) => {
    setAttachments((prev) => prev.filter((a) => a.id !== id));
  }, []);

  const handleFiles = useCallback(
    (files: FileList | File[]) => {
      for (const f of files) {
        const ext = "." + (f.name.split(".").pop() || "").toLowerCase();
        const allowed = ALLOWED_EXT.split(",").map((s) => s.trim());
        if (!allowed.includes(ext)) continue;
        uploadFile(f);
      }
    },
    [uploadFile]
  );

  // ── Drag & drop ─────────────────────────────────────────────────────────

  const handleDragEnter = useCallback((e: React.DragEvent) => {
    e.preventDefault();
    dragCounter.current++;
    setDragOver(true);
  }, []);

  const handleDragLeave = useCallback((e: React.DragEvent) => {
    e.preventDefault();
    dragCounter.current--;
    if (dragCounter.current <= 0) {
      dragCounter.current = 0;
      setDragOver(false);
    }
  }, []);

  const handleDragOver = useCallback((e: React.DragEvent) => {
    e.preventDefault();
  }, []);

  const handleDrop = useCallback(
    (e: React.DragEvent) => {
      e.preventDefault();
      dragCounter.current = 0;
      setDragOver(false);
      if (e.dataTransfer.files.length) handleFiles(e.dataTransfer.files);
    },
    [handleFiles]
  );

  // ── Paste handler ───────────────────────────────────────────────────────

  const handlePaste = useCallback(
    (e: React.ClipboardEvent) => {
      if (e.clipboardData.files.length) {
        e.preventDefault();
        handleFiles(e.clipboardData.files);
      }
    },
    [handleFiles]
  );

  // ── Submit ──────────────────────────────────────────────────────────────

  const submit = useCallback(() => {
    const el = textareaRef.current;
    if (!el) return;
    const text = el.value.trim();
    if (!text || disabled) return;
    // Only submit if all attachments are done uploading.
    if (attachments.some((a) => a.uploading)) return;
    historyRef.current.push(text);
    historyIdx.current = historyRef.current.length;
    draftRef.current = "";
    el.value = "";
    setHasText(false);
    const ready: AttachmentRef[] = attachments
      .filter((a) => !a.error && a.extracted_text)
      .map((a) => ({ id: a.id, filename: a.filename, extracted_text: a.extracted_text }));
    onSubmit(text, ready);
    setAttachments([]);
  }, [disabled, onSubmit, attachments]);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      const el = textareaRef.current;
      if (!el) return;

      if ((e.ctrlKey || e.metaKey) && e.key === "Enter") {
        e.preventDefault();
        submit();
        return;
      }

      if (e.key === "ArrowUp" && !el.value) {
        e.preventDefault();
        if (historyIdx.current === historyRef.current.length) draftRef.current = el.value;
        if (historyIdx.current > 0) {
          historyIdx.current--;
          el.value = historyRef.current[historyIdx.current] || "";
          setHasText(el.value.trim().length > 0);
        }
        return;
      }
      if (e.key === "ArrowDown" && !el.value) {
        e.preventDefault();
        if (historyIdx.current < historyRef.current.length - 1) {
          historyIdx.current++;
          el.value = historyRef.current[historyIdx.current] || "";
        } else {
          historyIdx.current = historyRef.current.length;
          el.value = draftRef.current;
        }
        setHasText(el.value.trim().length > 0);
        return;
      }
    },
    [submit]
  );

  const handleInput = useCallback(() => {
    const el = textareaRef.current;
    setHasText(!!el && el.value.trim().length > 0);
  }, []);

  const canSend = hasText && !disabled && !attachments.some((a) => a.uploading);

  return (
    <div className="composer">
      {/* Hidden file input */}
      <input
        ref={fileInputRef}
        type="file"
        multiple
        accept={ALLOWED_EXT}
        style={{ display: "none" }}
        onChange={(e) => {
          if (e.target.files?.length) handleFiles(e.target.files);
          e.target.value = "";
        }}
      />

      {/* Attachment chips */}
      {attachments.length > 0 && (
        <div className="composer__attachments">
          {attachments.map((a) => (
            <span
              key={a.id}
              className={`composer__chip${a.uploading ? " composer__chip--uploading" : ""}${a.error ? " composer__chip--error" : ""}`}
              title={a.error || (a.uploading ? "uploading…" : `${a.image_count} image(s) extracted`)}
            >
              <span className="composer__chip__icon">
                {a.uploading ? "◌" : a.error ? "✕" : "✓"}
              </span>
              <span className="composer__chip__name">{a.filename}</span>
              {!a.uploading && (
                <button
                  className="composer__chip__remove"
                  onClick={() => removeAttachment(a.id)}
                  aria-label={`Remove ${a.filename}`}
                >
                  ×
                </button>
              )}
            </span>
          ))}
        </div>
      )}

      <div
        className={`composer__shell${dragOver ? " composer__shell--dragover" : ""}`}
        onDragEnter={handleDragEnter}
        onDragLeave={handleDragLeave}
        onDragOver={handleDragOver}
        onDrop={handleDrop}
        onPaste={handlePaste}
      >
        <textarea
          ref={textareaRef}
          className="composer__textarea"
          onKeyDown={handleKeyDown}
          onInput={handleInput}
          disabled={disabled}
          placeholder={disabled ? "connecting…" : "message NonoClaw…"}
          rows={1}
        />
        <div className="composer__hint">
          <span style={{ display: "flex", alignItems: "center", gap: 6 }}>
            <button
              className="composer__attach"
              onClick={() => fileInputRef.current?.click()}
              disabled={disabled}
              aria-label="Attach files"
              title="Attach files (PDF, DOCX, TXT, MD, images)"
            >
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                <path d="M21.44 11.05l-9.19 9.19a6 6 0 01-8.49-8.49l9.19-9.19a4 4 0 015.66 5.66l-9.2 9.19a2 2 0 01-2.83-2.83l8.49-8.48" />
              </svg>
            </button>
            <kbd>⌘/Ctrl</kbd> + <kbd>Enter</kbd> · /clear /compact
          </span>
          <button
            className="composer__send"
            onClick={submit}
            disabled={!canSend}
            aria-label="Send message"
          >
            send
            <span className="composer__send__arrow">↗</span>
          </button>
        </div>
      </div>
    </div>
  );
}
