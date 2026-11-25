import { useState } from "react";
import type { ClientMsg, RegisteredProject } from "../types";

interface Props {
  projects: RegisteredProject[];
  activePath: string;
  send: (msg: ClientMsg) => void;
  /** True while any run/compaction of the active session is in flight. */
  busy?: boolean;
}

/** Clickable project list in the left rail. Clicking a project switches the
 *  active project; its sessions already live in the conversations rail. */
export default function ProjectTree({
  projects,
  activePath,
  send,
  busy,
}: Props) {
  const [adding, setAdding] = useState(false);
  const [addPath, setAddPath] = useState("");

  const submitAdd = () => {
    const path = addPath.trim();
    if (!path) return;
    send({ type: "add_project", path });
    setAddPath("");
    setAdding(false);
  };

  return (
    <div className="projecttree">
      <div className="projecttree__head">
        <span className="projecttree__title">Projects</span>
        <button
          className="projecttree__add"
          onClick={() => setAdding(!adding)}
          title="Add a project directory"
        >
          {adding ? "×" : "+"}
        </button>
      </div>
      {adding && (
        <div className="projecttree__addform">
          <input
            className="projecttree__input"
            placeholder="/absolute/project/path"
            value={addPath}
            onChange={(e) => setAddPath(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") submitAdd();
              if (e.key === "Escape") setAdding(false);
            }}
            autoFocus
          />
          <button className="projecttree__confirm" onClick={submitAdd}>
            Add
          </button>
        </div>
      )}
      <div className="projecttree__list">
        {projects.map((p) => {
          const isActive = p.path === activePath;
          return (
            <div key={p.path} className="projecttree__node">
              <div className="projecttree__row">
                <button
                  className={`projecttree__name${isActive ? " projecttree__name--active" : ""}`}
                  onClick={() => {
                    if (isActive) return;
                    if (
                      !busy ||
                      window.confirm(
                        "A run is active. Switch projects and cancel it?",
                      )
                    ) {
                      send({ type: "switch_project", path: p.path, force: busy === true });
                    }
                  }}
                  title={p.path}
                >
                  {p.name}
                  {isActive && <span className="projecttree__activemark"> ●</span>}
                </button>
                {!isActive && (
                  <button
                    className="projecttree__remove"
                    onClick={() => send({ type: "remove_project", path: p.path })}
                    title={`Remove ${p.name} from the registry (does not delete files)`}
                  >
                    ×
                  </button>
                )}
              </div>
            </div>
          );
        })}
        {projects.length === 0 && (
          <div className="projecttree__empty">No projects registered</div>
        )}
      </div>
    </div>
  );
}
