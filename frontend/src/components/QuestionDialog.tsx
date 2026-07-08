import { useState } from "react";

interface Props {
  prompt: string;
  options: string[];
  onAnswer: (answer: string | null) => void;
}

export default function QuestionDialog({ prompt, options, onAnswer }: Props) {
  const [selected, setSelected] = useState<string | null>(null);

  return (
    <div className="dialog-overlay">
      <div className="dialog">
        <div className="dialog__eyebrow mint">question</div>
        <div className="dialog__title">Pick one</div>
        <div className="dialog__body" style={{ whiteSpace: "pre-wrap", marginBottom: 12 }}>
          {prompt}
        </div>
        <div>
          {options.map((opt) => (
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
          ))}
        </div>
        <div className="dialog__actions">
          <button className="btn btn--ghost" onClick={() => onAnswer(null)}>
            Cancel
          </button>
          <button className="btn btn--primary" onClick={() => onAnswer(selected)} disabled={!selected}>
            Answer
          </button>
        </div>
      </div>
    </div>
  );
}
