import { useRef, useCallback, useEffect, useState } from "react";
import { useStore } from "../store";
import { getBrowserAccessToken, sanitizeBrowserText } from "../security";
import type { MediaAttachment } from "../store/slices";
import type { AttachmentRef, UploadResponse } from "../types";

interface Props {
  onSubmit: (text: string, attachments: AttachmentRef[]) => void;
  disabled?: boolean;
}

const ALLOWED_EXT = ".pdf,.docx,.doc,.txt,.md,.markdown,.png,.jpg,.jpeg";

function authenticatedApiUrl(path: string): string {
  const token = getBrowserAccessToken(window.location.search);
  return token ? `${path}?token=${encodeURIComponent(token)}` : path;
}

export default function InputBox({ onSubmit, disabled }: Props) {
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const historyRef = useRef<string[]>([]);
  const historyIdx = useRef(-1);
  const draftRef = useRef("");
  const draft = useStore((state) => state.draft);
  const setDraft = useStore((state) => state.setDraft);
  const attachments = useStore((state) => state.attachments);
  const addAttachment = useStore((state) => state.addAttachment);
  const updateAttachment = useStore((state) => state.updateAttachment);
  const removeMediaAttachment = useStore((state) => state.removeAttachment);
  const clearAttachments = useStore((state) => state.clearAttachments);
  const recording = useStore((state) => state.recording);
  const setRecording = useStore((state) => state.setRecording);
  const sessionId = useStore((state) => state.sessionId);
  const [dragOver, setDragOver] = useState(false);
  const dragCounter = useRef(0);
  const recorderRef = useRef<MediaRecorder | null>(null);
  const recordingStreamRef = useRef<MediaStream | null>(null);
  const cancelRecordingRef = useRef(false);

  useEffect(() => {
    textareaRef.current?.focus();
  }, []);

  useEffect(() => {
    return () => {
      cancelRecordingRef.current = true;
      const recorder = recorderRef.current;
      recorderRef.current = null;
      if (recorder?.state === "recording") {
        try { recorder.stop(); } catch {}
      }
      recordingStreamRef.current?.getTracks().forEach((track) => track.stop());
      recordingStreamRef.current = null;
      const state = useStore.getState();
      state.setRecording(false);
      state.clearAttachments();
    };
  }, [sessionId]);

  // ── Voice input (ElevenLabs STT) ────────────────────────────────────────

  const startRecording = useCallback(async () => {
    try {
      const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
      recordingStreamRef.current = stream;
      cancelRecordingRef.current = false;
      const recorder = new MediaRecorder(stream, { mimeType: "audio/webm" });
      recorderRef.current = recorder;
      const chunks: BlobPart[] = [];
      recorder.ondataavailable = (e) => chunks.push(e.data);
      recorder.onstop = async () => {
        stream.getTracks().forEach((track) => track.stop());
        recordingStreamRef.current = null;
        if (cancelRecordingRef.current) return;
        const blob = new Blob(chunks, { type: "audio/webm" });
        if (blob.size < 500) return; // too short — ignore
        try {
          const form = new FormData();
          form.append("audio", blob, "recording.webm");
          const resp = await fetch(authenticatedApiUrl("/api/stt"), { method: "POST", body: form });
          if (!resp.ok) return;
          const data = await resp.json();
          const text = typeof data.text === "string" ? data.text : "";
          if (text && text !== "error") {
            const el = textareaRef.current;
            if (!el) return;
            const start = el.selectionStart ?? el.value.length;
            const end = el.selectionEnd ?? el.value.length;
            const value = el.value.slice(0, start) + text + el.value.slice(end);
            setDraft(value);
            window.requestAnimationFrame(() => {
              el.focus();
              el.setSelectionRange(start + text.length, start + text.length);
            });
          }
        } catch { /* STT failed silently */ }
      };
      recorder.start();
      setRecording(true);
    } catch { /* mic denied */ }
  }, [setDraft, setRecording]);

  const stopRecording = useCallback(() => {
    if (recorderRef.current?.state === "recording") {
      recorderRef.current.stop();
    }
    setRecording(false);
  }, [setRecording]);

  // ── File upload ─────────────────────────────────────────────────────────

  const uploadFile = useCallback(async (file: File) => {
    const id = crypto.randomUUID();
    const chip: MediaAttachment = {
      id,
      filename: file.name,
      extracted_text: "",
      image_count: 0,
      uploading: true,
    };
    addAttachment(chip);

    try {
      const form = new FormData();
      form.append("file", file);
      const resp = await fetch(authenticatedApiUrl("/api/upload"), { method: "POST", body: form });
      if (!resp.ok) {
        const err = await resp.json().catch(() => ({ error: `HTTP ${resp.status}` }));
        throw new Error(err.error || `HTTP ${resp.status}`);
      }
      const data: UploadResponse = await resp.json();
      const errMsg = data.error || null;
      if (errMsg) throw new Error(errMsg);
      updateAttachment(id, {
        id: data.id,
        filename: data.filename,
        uploading: false,
        extracted_text: "",
        image_count: data.image_count,
        images: undefined,
      });
    } catch (e: unknown) {
      const msg = e instanceof Error ? sanitizeBrowserText(e.message) : "upload failed";
      updateAttachment(id, { uploading: false, error: msg });
    }
  }, [addAttachment, updateAttachment]);

  const removeAttachment = useCallback((id: string) => {
    removeMediaAttachment(id);
  }, [removeMediaAttachment]);

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
    const text = draft.trim();
    if (!text || disabled || attachments.some((attachment) => attachment.uploading)) return;
    historyRef.current.push(text);
    historyIdx.current = historyRef.current.length;
    draftRef.current = "";
    const ready: AttachmentRef[] = attachments
      .filter((attachment) => !attachment.error && !attachment.uploading)
      .map((attachment) => ({
        id: attachment.id,
        filename: attachment.filename,
        extracted_text: "",
      }));
    setDraft("");
    clearAttachments();
    onSubmit(text, ready);
  }, [attachments, clearAttachments, disabled, draft, onSubmit, setDraft]);

  const handleKeyDown = useCallback(
    (event: React.KeyboardEvent) => {
      if ((event.ctrlKey || event.metaKey) && event.key === "Enter") {
        event.preventDefault();
        submit();
        return;
      }
      if (event.key === "ArrowUp" && !draft) {
        event.preventDefault();
        if (historyIdx.current === historyRef.current.length) draftRef.current = draft;
        if (historyIdx.current > 0) {
          historyIdx.current -= 1;
          setDraft(historyRef.current[historyIdx.current] || "");
        }
        return;
      }
      if (event.key === "ArrowDown" && !draft) {
        event.preventDefault();
        if (historyIdx.current < historyRef.current.length - 1) {
          historyIdx.current += 1;
          setDraft(historyRef.current[historyIdx.current] || "");
        } else {
          historyIdx.current = historyRef.current.length;
          setDraft(draftRef.current);
        }
      }
    },
    [draft, setDraft, submit]
  );

  const canSend = draft.trim().length > 0 && !disabled && !attachments.some((attachment) => attachment.uploading);

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
              title={a.error || (a.uploading ? "uploading…" : `${a.image_count} image(s) extracted`) }
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
          value={draft}
          onKeyDown={handleKeyDown}
          onChange={(event) => setDraft(event.target.value)}
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
