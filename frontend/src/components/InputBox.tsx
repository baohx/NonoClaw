import { useRef, useCallback, useEffect, useState } from "react";

interface Props {
  onSubmit: (text: string) => void;
  disabled?: boolean;
}

export default function InputBox({ onSubmit, disabled }: Props) {
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const historyRef = useRef<string[]>([]);
  const historyIdx = useRef(-1);
  const draftRef = useRef("");
  const [hasText, setHasText] = useState(false);

  useEffect(() => {
    textareaRef.current?.focus();
  }, []);

  // resize is handled by CSS `field-sizing: content` — no JS reflow per keystroke.

  // Shared by both ⌘/Ctrl+Enter and the Send button.
  const submit = useCallback(() => {
    const el = textareaRef.current;
    if (!el) return;
    const text = el.value.trim();
    if (!text || disabled) return;
    historyRef.current.push(text);
    historyIdx.current = historyRef.current.length;
    draftRef.current = "";
    el.value = "";
    setHasText(false);
    onSubmit(text);
  }, [disabled, onSubmit]);

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

  return (
    <div className="composer">
      <div className="composer__shell">
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
          <span>
            <kbd>⌘/Ctrl</kbd> + <kbd>Enter</kbd> · /clear /compact
          </span>
          <button
            className="composer__send"
            onClick={submit}
            disabled={disabled || !hasText}
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
