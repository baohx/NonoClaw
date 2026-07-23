import { useState } from "react";
import { useStore } from "../store";
import type { ProjectInfo, ToolInfo } from "../types";
import TechnicalTrace from "./TechnicalTrace";

interface Props {
  info: ProjectInfo | null;
  onOpen: (path: string, forceCode: boolean) => void;
  onRefresh: () => void;
}

const DEFAULT_OPEN = new Set<string>();

export default function InsightRail({ info, onOpen, onRefresh }: Props) {
  const [open, setOpen] = useState<Set<string>>(DEFAULT_OPEN);
  const toggle = (id: string) =>
    setOpen((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  const tools = info?.tools ?? [];
  const builtin = tools.filter((t) => t.kind === "builtin");
  const mcpTools = tools.filter((t) => t.kind === "mcp");

  return (
    <>
      <div className="filetree__head">
        <span className="filetree__root">
          <span className="filetree__rootmark">◆</span>
          insight
        </span>
        <span className="filetree__actions">
          <button className="iconbtn" title="Refresh" onClick={onRefresh}>
            ↻
          </button>
        </span>
      </div>
      <TechnicalTrace />
      <div className="acc">
        <Section
          id="tools"
          label="Tools"
          count={tools.length}
          open={open.has("tools")}
          onToggle={toggle}
        >
          {builtin.length > 0 && (
            <>
              <div className="acc-empty">built-in · {builtin.length}</div>
              {builtin.map((t) => (
                <ToolRow key={t.name} t={t} />
              ))}
            </>
          )}
          {mcpTools.length > 0 && (
            <>
              <div className="acc-empty" style={{ marginTop: 8 }}>
                mcp · {mcpTools.length}
              </div>
              {mcpTools.map((t) => (
                <ToolRow key={t.name} t={t} />
              ))}
            </>
          )}
          {tools.length === 0 && <div className="acc-empty">no tools registered</div>}
        </Section>

        <Section
          id="mcp"
          label="MCP servers"
          count={info?.mcp_servers.length ?? 0}
          open={open.has("mcp")}
          onToggle={toggle}
        >
          {(info?.mcp_servers.length ?? 0) === 0 ? (
            <div className="acc-empty">
              none configured — add via settings.json <code>mcpServers</code> or --mcp-config
            </div>
          ) : (
            info?.mcp_servers.map((s) => (
              <div key={s.name} className="insight-row">
                <div className="insight-row__top">
                  <span className={`dot ${s.connected ? "on" : "bad"}`} />
                  <span className="insight-row__name">{s.name}</span>
                  <span className="tag">{s.tool_count} tool{s.tool_count === 1 ? "" : "s"}</span>
                </div>
                <div className="insight-row__meta">$ {s.command}</div>
                {s.config_source && (
                  <div className="insight-row__meta">src: {s.config_source}</div>
                )}
              </div>
            ))
          )}
        </Section>

        <ModelsSection open={open.has("models")} onToggle={() => toggle("models")} />

        <Section
          id="skills"
          label="Skills"
          count={info?.skills.length ?? 0}
          open={open.has("skills")}
          onToggle={toggle}
        >
          {(info?.skills.length ?? 0) === 0 ? (
            <div className="acc-empty">
              none — drop a <code>SKILL.md</code> in .nonoclaw/skills/&lt;name&gt;/
            </div>
          ) : (
            info?.skills.map((s) => (
              <button
                key={s.source}
                className="insight-row"
                onClick={(ev) => onOpen(s.source, ev.shiftKey)}
                title={`${s.source} — click to open`}
              >
                <div className="insight-row__top">
                  <span className="tag">/{s.name}</span>
                  <span className="insight-row__name">{s.description || s.name}</span>
                </div>
                <div className="insight-row__meta">{s.source}</div>
              </button>
            ))
          )}
        </Section>

        <Section
          id="plugins"
          label="Plugins"
          count={info?.plugins.length ?? 0}
          open={open.has("plugins")}
          onToggle={toggle}
        >
          {(info?.plugins.length ?? 0) === 0 ? (
            <div className="acc-empty">none installed — nonoclaw --plugin-add &lt;src&gt;</div>
          ) : (
            info?.plugins.map((p) => (
              <button
                key={p.dir}
                className="insight-row"
                onClick={(ev) => onOpen(p.dir, ev.shiftKey)}
                title={`${p.dir} — click to open`}
              >
                <div className="insight-row__top">
                  <span className="tag">{p.skill_count} skill{p.skill_count === 1 ? "" : "s"}</span>
                  <span className="insight-row__name">{p.name}</span>
                </div>
                <div className="insight-row__meta">{p.dir}</div>
              </button>
            ))
          )}
        </Section>

        <Section
          id="extension-diagnostics"
          label="Extension diagnostics"
          count={info?.extension_diagnostics.length ?? 0}
          open={open.has("extension-diagnostics")}
          onToggle={toggle}
        >
          {(info?.extension_diagnostics.length ?? 0) === 0 ? (
            <div className="acc-empty">no extension conflicts or load failures</div>
          ) : (
            info?.extension_diagnostics.map((diagnostic, index) => (
              <div className="insight-row" key={`${diagnostic.code}-${diagnostic.source}-${index}`}>
                <div className="insight-row__top">
                  <span className={`dot ${diagnostic.severity === "error" ? "bad" : "off"}`} />
                  <span className="tag">{diagnostic.kind}</span>
                  <span className="insight-row__name">{diagnostic.name ?? diagnostic.code}</span>
                </div>
                <div className="insight-row__meta">{diagnostic.message}</div>
                <div className="insight-row__meta">fix: {diagnostic.suggestion}</div>
              </div>
            ))
          )}
          {(info?.extensions.length ?? 0) > 0 && (
            <div className="acc-empty" style={{ marginTop: 8 }}>
              higher precedence wins; shadowed and failed sources remain visible for diagnosis
            </div>
          )}
        </Section>

        <Section
          id="hooks"
          label="Hooks"
          count={info?.hooks.length ?? 0}
          open={open.has("hooks")}
          onToggle={toggle}
        >
          <HooksSection configured={info?.hooks ?? []} />
        </Section>

        <Section
          id="docs"
          label="Docs & config"
          count={(info?.docs.length ?? 0) + (info?.settings.length ?? 0)}
          open={open.has("docs")}
          onToggle={toggle}
        >
          <PathRows label="docs" layers={info?.docs ?? []} onOpen={onOpen} />
          <PathRows label="config" layers={info?.settings ?? []} onOpen={onOpen} />
          <ConfigRef items={info?.config_reference ?? []} />
        </Section>

        <Section id="slash" label="Slash commands" count={3} open={open.has("slash")} onToggle={toggle}>
          <SlashRef />
        </Section>

        <Section id="cli" label="CLI reference" count={info?.cli_reference.length ?? 0} open={open.has("cli")} onToggle={toggle}>
          <CliRef items={info?.cli_reference ?? []} />
        </Section>

        <Section
          id="project"
          label="Project"
          count={null}
          open={open.has("project")}
          onToggle={toggle}
        >
          <ProjectKv info={info} />
        </Section>
      </div>
    </>
  );
}

// ── Accordion section wrapper ──────────────────────────────────────────────

function Section({
  id,
  label,
  count,
  open,
  onToggle,
  children,
}: {
  id: string;
  label: string;
  count: number | null;
  open: boolean;
  onToggle: (id: string) => void;
  children: React.ReactNode;
}) {
  return (
    <div className="acc-section">
      <button className="acc-head" onClick={() => onToggle(id)}>
        <span className="acc-head__caret">{open ? "▾" : "▸"}</span>
        <span className="acc-head__label">{label}</span>
        {count !== null && (
          <span className={`acc-head__count${count === 0 ? " zero" : ""}`}>{count}</span>
        )}
      </button>
      {open && <div className="acc-body">{children}</div>}
    </div>
  );
}

// ── Rows ────────────────────────────────────────────────────────────────────

function ToolRow({ t }: { t: ToolInfo }) {
  const [open, setOpen] = useState(false);
  const props = schemaProps(t.input_schema);
  const required = schemaRequired(t.input_schema);
  return (
    <div
      className="insight-row insight-row--tool"
      title={open ? t.prompt_preview : `${t.prompt_preview}\n— click to view parameters`}
      onClick={() => setOpen((o) => !o)}
    >
      <div className="insight-row__top">
        <span className={`tag ${t.kind}`}>{t.kind}</span>
        <span className="insight-row__name">{t.name}</span>
        <span className={`tag ${t.read_only ? "ro" : "write"}`}>{t.read_only ? "RO" : "✎"}</span>
        <span className="acc-head__caret">{open ? "▾" : "▸"}</span>
      </div>
      {t.description && !open && <div className="insight-row__meta">{t.description}</div>}
      {open && (
        <div className="schema">
          {t.prompt_preview && (
            <>
              <div className="schema__label">prompt</div>
              <div className="schema__prompt">{t.prompt_preview}</div>
            </>
          )}
          <div className="schema__label">parameters</div>
          {props.length === 0 ? (
            <div className="schema__none">no parameters</div>
          ) : (
            props.map(([name, def]) => (
              <SchemaProp key={name} name={name} def={def} req={required.includes(name)} />
            ))
          )}
        </div>
      )}
    </div>
  );
}

function schemaProps(s: Record<string, unknown>): [string, Record<string, unknown>][] {
  const p = s?.properties;
  return p && typeof p === "object" ? Object.entries(p as object) : [];
}
function schemaRequired(s: Record<string, unknown>): string[] {
  return Array.isArray(s?.required) ? (s.required as string[]) : [];
}

function SchemaProp({
  name,
  def,
  req,
}: {
  name: string;
  def: Record<string, unknown>;
  req: boolean;
}) {
  const rawType = def.type;
  const type = Array.isArray(rawType)
    ? rawType.join("|")
    : typeof rawType === "string"
    ? rawType
    : Array.isArray(def.enum)
    ? "enum"
    : "any";
  const desc = typeof def.description === "string" ? def.description : "";
  return (
    <div className="schema__prop">
      <div className="schema__head">
        <span className="schema__name">
          {name}
          {req && <span className="schema__req">*</span>}
        </span>
        <span className="schema__type">{type}</span>
      </div>
      {desc && <div className="schema__desc">{desc}</div>}
    </div>
  );
}

function PathRows({
  label,
  layers,
  onOpen,
}: {
  label: string;
  layers: { label: string; path: string; exists: boolean }[];
  onOpen: (path: string, forceCode: boolean) => void;
}) {
  if (layers.length === 0) return null;
  return (
    <>
      <div className="acc-empty" style={{ marginTop: 6 }}>
        {label}
      </div>
      {layers.map((l) => (
        <button
          key={l.path}
          className="insight-row"
          onClick={(ev) => onOpen(l.path, ev.shiftKey)}
          title={`${l.path} — click to open · shift for VS Code`}
        >
          <div className="insight-row__top">
            <span className={`dot ${l.exists ? "on" : "off"}`} />
            <span className="insight-row__name">{l.label}</span>
          </div>
          <div className="insight-row__meta">{l.path}</div>
        </button>
      ))}
    </>
  );
}

// ── Hooks: configured list + lifecycle reference ───────────────────────────

const HOOK_TYPES: { name: string; when: string; deny: boolean; match: boolean }[] = [
  { name: "PreToolUse", when: "before a tool runs", deny: true, match: true },
  { name: "PostToolUse", when: "after a tool succeeds", deny: false, match: true },
  { name: "PostToolUseFailure", when: "after a tool fails", deny: false, match: true },
  { name: "Notification", when: "runtime notification", deny: false, match: false },
  { name: "UserPromptSubmit", when: "user sends a prompt", deny: false, match: false },
  { name: "SessionStart", when: "session begins", deny: false, match: false },
  { name: "SessionEnd", when: "session ends", deny: false, match: false },
  { name: "Stop", when: "main run stops", deny: false, match: false },
  { name: "SubagentStart", when: "a subagent begins", deny: false, match: false },
  { name: "SubagentStop", when: "a subagent finishes", deny: false, match: false },
  { name: "PreCompact", when: "before compaction", deny: false, match: false },
  { name: "PostCompact", when: "after compaction", deny: false, match: false },
];

function HooksSection({ configured }: { configured: { hook_type: string; matcher: string; command: string }[] }) {
  return (
    <>
      {configured.length > 0 ? (
        <>
          <div className="acc-empty">configured · {configured.length}</div>
          {configured.map((h, i) => (
            <div className="insight-row" key={i}>
              <div className="insight-row__top">
                <span className="tag mcp">{h.hook_type}</span>
                {h.matcher && <span className="tag">{h.matcher}</span>}
              </div>
              <div className="insight-row__meta">$ {h.command}</div>
            </div>
          ))}
        </>
      ) : (
        <div className="acc-empty">none configured — create .nonoclaw/hooks.json (clickable in Docs & config)</div>
      )}

      <div className="schema__label" style={{ marginTop: 10 }}>hook types · {HOOK_TYPES.length}</div>
      {HOOK_TYPES.map((t) => (
        <div className="cli-ref__flag" key={t.name}>
          <span className="cli-ref__name">{t.name}</span>
          <span className="cli-ref__desc">
            {t.when}
            {t.deny && <span className="tag ro" style={{ marginLeft: 6 }}>can deny</span>}
            {t.match && <span className="tag" style={{ marginLeft: 6 }}>matcher</span>}
          </span>
        </div>
      ))}

      <div className="schema__label" style={{ marginTop: 10 }}>config</div>
      <div className="cli-ref__ex">
{`// ~/.nonoclaw/hooks.json or <cwd>/.nonoclaw/hooks.json
{
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash*", "command": "scripts/guard.sh" }
    ],
    "PostToolUse": [
      { "matcher": "*", "command": "notify-send", "args": ["done"] }
    ],
    "UserPromptSubmit": [
      { "command": "scripts/log-prompt.sh" }
    ]
  }
}`}
      </div>
      <div className="acc-empty" style={{ marginTop: 8, lineHeight: 1.6 }}>
        matcher: <code>*</code> = all · <code>Bash*</code> = prefix · else exact (tool hooks only).<br />
        each command receives a <b>JSON context on stdin</b> (tool: tool_name+tool_input · prompt: prompt · compact: removed/kept/tokens).<br />
        <b>only PreToolUse</b> can block — non-zero exit denies the call.
      </div>
    </>
  );
}

// ── Models section ──────────────────────────────────────────────────────────

function ModelsSection({ open, onToggle }: { open: boolean; onToggle: () => void }) {
  const models = useStore((s) => s.availableModels);
  const active = useStore((s) => s.model);
  if (models.length === 0) return null;
  return (
    <Section id="models" label="Models" count={models.length} open={open} onToggle={onToggle}>
      <div className="acc-body">
        {models.map((m) => (
          <div key={m.name} className="insight-row">
            <div className="insight-row__top">
              <span className={`dot ${m.name === active ? "on" : "off"}`} />
              <span className="insight-row__name">{m.label || m.name}</span>
              {m.name === active && <span className="tag mcp">active</span>}
            </div>
            <div className="insight-row__meta">{m.name}</div>
          </div>
        ))}
      </div>
    </Section>
  );
}

// ── Slash commands reference ────────────────────────────────────────────────

function SlashRef() {
  return (
    <div className="acc-body">
      <div className="insight-row">
        <div className="insight-row__top">
          <span className="tag mcp">/clear</span>
          <span className="insight-row__name">Reset conversation</span>
        </div>
        <div className="insight-row__meta">Clear all messages and start fresh</div>
      </div>
      <div className="insight-row">
        <div className="insight-row__top">
          <span className="tag mcp">/compact</span>
          <span className="insight-row__name">Summarise long context</span>
        </div>
        <div className="insight-row__meta">Compress older messages into a summary to free context window</div>
      </div>
      <div className="insight-row">
        <div className="insight-row__top">
          <span className="tag mcp">/multi</span>
          <span className="insight-row__name">Multi-model compare</span>
        </div>
        <div className="insight-row__meta">
          Syntax: /multi deepseek-chat,glm-4-plus {"<prompt>"} — compare model answers sequentially
        </div>
      </div>
    </div>
  );
}

// ── Generated CLI/config references ────────────────────────────────────────

function CliRef({ items }: { items: { name: string; description: string }[] }) {
  return (
    <div className="cli-ref">
      <div className="cli-ref__ex">
{`# web UI
nonoclaw --serve-http 127.0.0.1:8765 --model deepseek-chat

# headless
nonoclaw -p "summarize rust/README.md"
echo "fix the bug" | nonoclaw -p --allowed-tools Read,Edit,Bash

# sessions
nonoclaw --continue "keep going"
nonoclaw --list-sessions`}
      </div>
      {items.map((item) => (
        <div key={item.name} className="cli-ref__flag">
          <span className="cli-ref__name">{item.name}</span>
          <span className="cli-ref__desc">{item.description}</span>
        </div>
      ))}
      {items.length === 0 && <div className="acc-empty">CLI metadata unavailable</div>}
    </div>
  );
}

function ConfigRef({ items }: { items: { name: string; description: string }[] }) {
  return (
    <>
      <div className="acc-empty" style={{ marginTop: 8 }}>settings fields</div>
      {items.map((item) => (
        <div key={item.name} className="cli-ref__flag">
          <span className="cli-ref__name">{item.name}</span>
          <span className="cli-ref__desc">{item.description}</span>
        </div>
      ))}
    </>
  );
}

// ── Project summary (reads live session/token state from the store) ──────────

function ProjectKv({ info }: { info: ProjectInfo | null }) {
  const sessionId = useStore((s) => s.sessionId);
  const inputTokens = useStore((s) => s.inputTokens);
  const outputTokens = useStore((s) => s.outputTokens);
  const cwd = info?.cwd ?? "—";
  const short = (p: string) => (p.length > 40 ? "…" + p.slice(-39) : p);

  return (
    <div className="kv">
      <div className="kv__row">
        <span className="kv__k">cwd</span>
        <span className="kv__v" title={cwd}>
          {short(cwd)}
        </span>
      </div>
      <div className="kv__row">
        <span className="kv__k">window</span>
        <span className="kv__v">
          {info?.context_window ? `${info.context_window.toLocaleString()} tok` : "—"}
        </span>
      </div>
      <div className="kv__row">
        <span className="kv__k">compact at</span>
        <span className="kv__v">
          {info?.compact_threshold.toLocaleString()} tok
        </span>
      </div>
      <div className="kv__row">
        <span className="kv__k">session</span>
        <span className="kv__v">{sessionId ? sessionId.slice(0, 8) : "—"}</span>
      </div>
      <div className="kv__row">
        <span className="kv__k">tools</span>
        <span className="kv__v">{info?.tools.length ?? 0}</span>
      </div>
      <div className="kv__row">
        <span className="kv__k">mcp</span>
        <span className="kv__v">{info?.mcp_servers.length ?? 0}</span>
      </div>
      <div className="kv__row">
        <span className="kv__k">skills</span>
        <span className="kv__v">{info?.skills.length ?? 0}</span>
      </div>
      <div className="kv__row">
        <span className="kv__k">git</span>
        <span className="kv__v">
          {info?.git ? (info.git.is_empty ? `${info.git.branch} · empty` : info.git.branch) : "—"}
        </span>
      </div>
      {(inputTokens > 0 || outputTokens > 0) && (
        <div className="kv__row">
          <span className="kv__k">tokens</span>
          <span className="kv__v">
            in {inputTokens.toLocaleString()} · out {outputTokens.toLocaleString()}
          </span>
        </div>
      )}
    </div>
  );
}
