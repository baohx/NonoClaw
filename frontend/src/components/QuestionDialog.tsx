import { useState } from "react";

interface Props {
  prompt: string;
  options: string[];
  context?: string | null;
  urgency?: string | null;
  format?: string | null;
  onAnswer: (answer: string | null) => void;
}

export default function QuestionDialog({ prompt, options, context, urgency, format, onAnswer }: Props) {
  const isFreeText = format === "free_text";
  const [selected, setSelected] = useState<string | null>(null);
  const [text, setText] = useState("");

  const isHighStakes = urgency === "high";

  return (
    <div className="dialog-overlay">
      <div className="dialog">
        <div className={`dialog__eyebrow ${isHighStakes ? "coral" : "mint"}`}>
          {isHighStakes ? "high-stakes question" : "question"}
        </div>
        {context && (
          <div className="dialog__body" style={{ whiteSpace: "pre-wrap", marginBottom: 8, opacity: 0.7, fontSize: "0.9em" }}>
            {context}
          </div>
        )}
        <div className="dialog__title">{isFreeText ? "Your response" : "Pick one"}</div>
        <div className="dialog__body" style={{ whiteSpace: "pre-wrap", marginBottom: 12 }}>
          {prompt}
        </div>
        <div>
          {isFreeText ? (
            <textarea
              className="dialog__textarea"
              value={text}
              onChange={(e) => setText(e.target.value)}
              placeholder="Type your answer..."
              rows={3}
              autoFocus
            />
          ) : (
            options.map((opt) => (
              <label key={opt} className="q-opt">
                <input
                  type="radio"
                  name="question-option"
                  value={opt}
                  checked={selected === opt}
                  onChange={() => setSelected(opt)}
                />
                {opt}
              </label>
            ))
          )}
        </div>
        <div className="dialog__actions">
          <button className="btn btn--ghost" onClick={() => onAnswer(null)}>
            Cancel
          </button>
          <button
            className="btn btn--primary"
            onClick={() => onAnswer(isFreeText ? (text.trim() || null) : selected)}
            disabled={isFreeText ? !text.trim() : !selected}
          >
            Answer
          </button>
        </div>
      </div>
    </div>
  );
}
