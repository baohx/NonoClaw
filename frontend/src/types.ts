// Type definitions matching the Rust wire protocol.
// Keep in sync with `rust/crates/cli/src/serve_http.rs`.

// ── Server → Browser messages ─────────────────────────────────────────────

/** Engine event (streamed per-turn). */
export interface EngineEvent {
  kind: "text_delta" | "tool_use_start" | "tool_result" | "assistant_done" | "compacted" | "compacting" | "model_info";
  text?: string;
  id?: string;
  name?: string;
  input?: unknown;
  ok?: boolean;
  preview?: string;
  removed?: number;
  kept?: number;
  tokens_before?: number;
  tokens_after?: number;
  /** Real model the API reported (model_info only), e.g. "deepseek-chat". */
  model?: string;
}

export interface PermissionRequired {
  type: "permission_required";
  request_id: string;
  tool_name: string;
  message: string;
  input: unknown;
}

export interface QuestionRequired {
  type: "question_required";
  request_id: string;
  prompt: string;
  options: string[];
}

export interface DoneResult {
  type: "done";
  text: string;
  usage: { input_tokens: number; output_tokens: number; cache_read_input_tokens: number; cache_creation_input_tokens: number };
  turns: number;
  stop_reason: string | null;
}

export interface ErrorMsg {
  type: "error";
  message: string;
}

export interface InfoMsg {
  type: "info";
  model: string;
  session_id: string;
}

export interface EventMsg {
  type: "event";
  event: EngineEvent;
}

export interface SessionInfoWire {
  id: string;
  started: string | null;
  message_count: number;
  summary: string;
}

export interface SessionListMsg {
  type: "session_list";
  sessions: SessionInfoWire[];
}

export interface MessagesLoadedMsg {
  type: "messages_loaded";
  /** Each entry is a serialized engine Message ({role, content}). */
  messages: unknown[];
}

/** One node of the flattened project file tree. */
export interface FileEntry {
  /** Path relative to cwd, forward slashes. */
  path: string;
  /** Display name (final segment). */
  name: string;
  is_dir: boolean;
  /** Indentation depth (0 = direct child of cwd). */
  depth: number;
}

export interface FileTreeMsg {
  type: "file_tree";
  root: string;
  entries: FileEntry[];
}

// ── Project context (Insight rail + Git pane) ──────────────────────────────

export interface ToolInfo {
  name: string;
  description: string;
  kind: "builtin" | "mcp";
  mcp_server: string | null;
  read_only: boolean;
  aliases: string[];
  prompt_preview: string;
  /** JSON Schema (object) describing the tool's input parameters. */
  input_schema: Record<string, unknown>;
}
export interface McpServerInfo {
  name: string;
  command: string;
  config_source: string | null;
  connected: boolean;
  tool_count: number;
}
export interface SkillInfo {
  name: string;
  description: string;
  source: string;
}
export interface PluginInfo {
  name: string;
  dir: string;
  skill_count: number;
}
export interface HookEntry {
  hook_type: string;
  matcher: string;
  command: string;
}
export interface PathLayer {
  label: string;
  path: string;
  exists: boolean;
}
export interface CommitInfo {
  sha: string;
  author: string;
  date: string;
  subject: string;
}
export interface GitInfo {
  branch: string | null;
  ahead: number;
  behind: number;
  staged: number;
  modified: number;
  untracked: number;
  conflicts: number;
  is_empty: boolean;
  recent_commits: CommitInfo[];
  user: string | null;
}
export interface ProjectInfo {
  cwd: string;
  model: string;
  tools: ToolInfo[];
  mcp_servers: McpServerInfo[];
  skills: SkillInfo[];
  plugins: PluginInfo[];
  hooks: HookEntry[];
  docs: PathLayer[];
  settings: PathLayer[];
  /** Configured model context window (tokens), if set. */
  context_window: number | null;
  /** Effective auto-compact threshold (tokens). */
  compact_threshold: number;
  git: GitInfo | null;
}
export interface ProjectInfoMsg {
  type: "project_info";
  info: ProjectInfo;
}

export interface GitShowRequest {
  type: "git_show";
  sha: string;
}

export interface GitShowMsg {
  type: "git_show";
  sha: string;
  output: string;
}

export type ServerMsg =
  | EventMsg
  | PermissionRequired
  | QuestionRequired
  | DoneResult
  | ErrorMsg
  | InfoMsg
  | SessionListMsg
  | MessagesLoadedMsg
  | FileTreeMsg
  | ProjectInfoMsg
  | GitShowMsg;

// ── Browser → Server messages ─────────────────────────────────────────────

export interface RunRequest {
  type: "run";
  prompt: string;
  model?: string;
  max_turns?: number;
}

export interface CancelRequest {
  type: "cancel";
}

export interface ClearRequest {
  type: "clear";
}

export interface NewSessionRequest {
  type: "new_session";
}

export interface ResumeSessionRequest {
  type: "resume_session";
  id: string;
}

export interface CompactRequest {
  type: "compact";
}

export interface PermissionDecision {
  type: "permission_decision";
  request_id: string;
  decision: "allow" | "deny";
}

export interface QuestionAnswer {
  type: "question_answer";
  request_id: string;
  answer: string | null;
}

export interface FileTreeRequest {
  type: "file_tree";
}

export interface OpenFileRequest {
  type: "open_file";
  path: string;
  force_code?: boolean;
}

export interface ProjectInfoRefreshRequest {
  type: "project_info_refresh";
}

export type ClientMsg =
  | RunRequest
  | CancelRequest
  | ClearRequest
  | NewSessionRequest
  | ResumeSessionRequest
  | CompactRequest
  | PermissionDecision
  | QuestionAnswer
  | FileTreeRequest
  | OpenFileRequest
  | ProjectInfoRefreshRequest
  | GitShowRequest;

// ── Application types ─────────────────────────────────────────────────────

export interface ChatMessage {
  id: string;
  role: "user" | "assistant" | "tool" | "system";
  content: string;
  /** Tool name (only for tool messages). */
  toolName?: string;
  /** Tool input JSON (only for tool messages). */
  toolInput?: unknown;
  /** Whether the tool succeeded. */
  toolOk?: boolean;
  /** Streaming text buffer (assistant messages in progress). */
  streaming?: boolean;
  /** Collapsed tool output. */
  collapsed?: boolean;
}
