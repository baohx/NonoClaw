import { useRef, useCallback, useEffect, useState } from "react";
import type { AttachmentRef, ImageRef, UploadResponse } from "../types";

interface PendingAttachment {
  id: string;
  filename: string;
  extracted_text: string;
  image_count: number;
  images?: ImageRef[];
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
  const [recording, setRecording] = useState(false);
  const recorderRef = useRef<MediaRecorder | null>(null);
  const spaceTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const spaceDownRef = useRef(false);

  useEffect(() => {
    textareaRef.current?.focus();
  }, []);

  // ── Voice input (ElevenLabs STT) ────────────────────────────────────────

  const startRecording = useCallback(async () => {
    try {
      const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
      const recorder = new MediaRecorder(stream, { mimeType: "audio/webm" });
      recorderRef.current = recorder;
      const chunks: BlobPart[] = [];
      recorder.ondataavailable = (e) => chunks.push(e.data);
      recorder.onstop = async () => {
        stream.getTracks().forEach((t) => t.stop());
        const blob = new Blob(chunks, { type: "audio/webm" });
        if (blob.size < 500) return; // too short — ignore
        try {
          const form = new FormData();
          form.append("audio", blob, "recording.webm");
          const resp = await fetch("/api/stt", { method: "POST", body: form });
          const data = await resp.json();
          const text: string = data.text || data.error || "";
          if (text && text !== "error") {
            const el = textareaRef.current;
            if (!el) return;
            const start = el.selectionStart ?? el.value.length;
            const end = el.selectionEnd ?? el.value.length;
            el.value = el.value.slice(0, start) + text + el.value.slice(end);
            el.focus();
            setHasText(el.value.trim().length > 0);
          }
        } catch { /* STT failed silently */ }
      };
      recorder.start();
      setRecording(true);
    } catch { /* mic denied */ }
  }, []);

  const stopRecording = useCallback(() => {
    if (recorderRef.current?.state === "recording") {
      recorderRef.current.stop();
    }
    setRecording(false);
  }, []);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      const el = textareaRef.current;
      if (!el) return;

      if ((e.ctrlKey || e.metaKey) && e.key === "Enter") {
        e.preventDefault();
        submit();
        return;
      }

      // Space bar: long press to start recording (only when not already typing).
      if (e.key === " " && !el.value.trim() && !recording && !disabled) {
        // Let the space go through for normal typing if the user already
        // has text (they're typing, not wanting to dictate).
        if (el.value.length > 0) return;
        e.preventDefault();
        if (spaceTimerRef.current) clearTimeout(spaceTimerRef.current);
        spaceTimerRef.current = setTimeout(() => {
          if (spaceDownRef.current) startRecording();
        }, 400);
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
    [submit, recording, disabled, startRecording]
  );

  const handleKeyUp = useCallback(
    (e: React.KeyboardEvent) => {
      if (e.key === " ") {
        spaceDownRef.current = false;
        if (spaceTimerRef.current) clearTimeout(spaceTimerRef.current);
        if (recording) stopRecording();
      }
    },
    [recording, stopRecording]
  );

  const handleKeyDownCapture = useCallback((e: React.KeyboardEvent) => {
    if (e.key === " ") spaceDownRef.current = true;
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
      const errMsg = data.error || (!data.extracted_text ? "no text extracted from file" : null);
      if (errMsg) throw new Error(errMsg);
      setAttachments((prev) =>
        prev.map((a) =>
          a.id === id
            ? { ...a, uploading: false, extracted_text: data.extracted_text, image_count: data.image_count, images: data.images }
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
      .map((a) => ({ id: a.id, filename: a.filename, extracted_text: a.extracted_text, images: a.images }));
    onSubmit(text, ready);
    setAttachments([]);
  }, [disabled, onSubmit, attachments]);

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
              className={`composer__chip${a.uploading ? " composer__chip--uploading" : ""}${(a.error || !a.extracted_text) ? " composer__chip--error" : ""}`}
              title={a.error || (!a.extracted_text ? "no text extracted" : a.uploading ? "uploading…" : `${a.image_count} image(s) extracted`)}
            >
              <span className="composer__chip__icon">
                {a.uploading ? "◌" : (a.error || !a.extracted_text) ? "✕" : "✓"}
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
          onKeyUp={handleKeyUp}
          onKeyDownCapture={handleKeyDownCapture}
          onInput={handleInput}
          disabled={disabled}
          placeholder={recording ? "🎙️ listening…" : disabled ? "connecting…" : "message NonoClaw…"}
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
            <button
              className={`composer__mic${recording ? " composer__mic--active" : ""}`}
              onMouseDown={(e) => { e.preventDefault(); startRecording(); }}
              onMouseUp={stopRecording}
              onMouseLeave={stopRecording}
              onTouchStart={(e) => { e.preventDefault(); startRecording(); }}
              onTouchEnd={stopRecording}
              disabled={disabled}
              aria-label="Voice input"
              title="Hold to record voice · Space to dictate"
            >
              <svg width="14" height="14" viewBox="0 0 24 24" fill={recording ? "currentColor" : "none"} stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                <path d="M12 1a3 3 0 00-3 3v8a3 3 0 006 0V4a3 3 0 00-3-3z"/>
                <path d="M19 10v2a7 7 0 01-14 0v-2"/>
                <line x1="12" y1="19" x2="12" y2="23"/>
                <line x1="8" y1="23" x2="16" y2="23"/>
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
