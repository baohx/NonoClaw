interface Props {
  sha: string;
  output: string;
  onClose: () => void;
}

/** Shows the `git show` stat + patch for a commit clicked in the Git pane. */
export default function CommitDialog({ sha, output, onClose }: Props) {
  return (
    <div className="dialog-overlay" onClick={onClose}>
      <div
        className="dialog"
        style={{ maxWidth: 840 }}
        onClick={(e) => e.stopPropagation()}
      >
        <div className="dialog__eyebrow mint">git show</div>
        <div className="dialog__title">{sha}</div>
        <pre className="dialog__pre commit-pre">{output}</pre>
        <div className="dialog__actions">
          <button className="btn btn--ghost" onClick={onClose}>
            Close
          </button>
        </div>
      </div>
    </div>
  );
}
