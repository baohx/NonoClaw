interface Props {
  toolName: string;
  message: string;
  input: unknown;
  onAllow: () => void;
  onDeny: () => void;
}

export default function PermissionDialog({ toolName, message, input, onAllow, onDeny }: Props) {
  return (
    <div className="dialog-overlay">
      <div className="dialog">
        <div className="dialog__eyebrow">permission required</div>
        <div className="dialog__title">Allow this action?</div>
        <div className="dialog__sub">{toolName}</div>
        <div className="dialog__body">{message}</div>
        {input !== undefined && input !== null && (
          <pre className="dialog__pre">{JSON.stringify(input, null, 2)}</pre>
        )}
        <div className="dialog__actions">
          <button className="btn btn--danger" onClick={onDeny}>
            Deny
          </button>
          <button className="btn btn--primary" onClick={onAllow} autoFocus>
            Allow
          </button>
        </div>
      </div>
    </div>
  );
}
