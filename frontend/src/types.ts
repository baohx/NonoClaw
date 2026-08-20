// Type definitions matching the Rust wire protocol.
// Keep in sync with `rust/crates/cli/src/serve_http.rs`.

// ── Server → Browser messages ─────────────────────────────────────────────

/** Structured task mutation from TodoWrite or Task*. */
export type TaskStatus = "pending" | "in_progress" | "completed";
export interface TaskSnapshot {
  id: string;
  subject: string;
  status: TaskStatus;
  active_form?: string;
  owner?: string;
  blocks?: string[];
  blocked_by?: string[];
}
export interface TaskChange {
  scope: string;
  source: "todo_write" | "task_create" | "task_update";
  change: "replaced" | "created" | "updated";
  tasks: TaskSnapshot[];
}

export interface TokenBudgetComponent {
  name: string;
  chars: number;
  estimated_tokens: number;
}

/** Engine event (streamed per-turn). */
export interface EngineEvent {
  kind:
    | "text_delta" | "tool_use_start" | "tool_result" | "assistant_done"
    | "compacted" | "compacting" | "model_info" | "skill_activated"
    | "session_repair" | "task_changed" | "run_started" | "context_prepared"
    | "token_budget_breakdown" | "model_request_started" | "model_resolved" | "provider_diagnostic"
    | "stream_state_changed" | "thinking_state" | "retry_scheduled"
    | "tool_queued" | "tool_validation" | "permission_requested"
    | "permission_resolved" | "tool_execution_started" | "tool_execution_finished"
    | "tool_result_normalized" | "hook_started" | "hook_finished"
    | "subagent_started" | "subagent_finished" | "background_task_changed"
    | "compaction_started" | "recovery_applied" | "extension_diagnostic"
    | "mcp_diagnostic" | "config_diagnostic" | "usage_updated"
    | "cancellation_requested" | "run_error" | "run_finished"
    | "subagent_event";
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
  /** Canonical task mutation (task_changed only). */
  change?: TaskChange;
  /** Skill activation provenance (skill_activated only). */
  reason?: string;
  source?: string;
  version?: string | null;
  requested_model?: string;
  actual_model?: string;
  provider?: string;
  turn?: number;
  max_turns?: number;
  max_budget_usd?: number | null;
  estimated_tokens?: number;
  chars_per_token?: number;
  system_chars?: number;
  tools_chars?: number;
  messages_chars?: number;
  system?: TokenBudgetComponent[];
  tools?: TokenBudgetComponent[];
  messages?: TokenBudgetComponent[];
  context_window?: number | null;
  tool_count?: number;
  skill_count?: number;
  status?: string;
  state?: string;
  active?: boolean;
  category?: string;
  detail?: string;
  attempt?: number;
  delay_ms?: number;
  operation?: string;
  tool_use_id?: string;
  tool_name?: string;
  index?: number | null;
  waiting_on?: string;
  decision?: string;
  elapsed_ms?: number;
  read_only?: boolean | null;
  destructive?: boolean | null;
  original_chars?: number;
  visible_chars?: number;
  truncated?: boolean;
  local_reference?: string | null;
  hook_type?: string;
  action?: string;
  matcher?: string;
  description?: string;
  task_id?: string;
  exit_code?: number | null;
  automatic?: boolean;
  messages_before?: number;
  items_affected?: number;
  diagnostic?: Record<string, unknown>;
  repair?: Record<string, unknown>;
  server?: string;
  severity?: string;
  code?: string;
  field?: string | null;
  message?: string;
  suggestion?: string;
  turn_usage?: Record<string, unknown>;
  total?: Record<string, unknown>;
  usage?: Record<string, unknown>;
  duration_ms?: number;
  turns?: number;
  retryable?: boolean;
  /** Scoped child-run metadata (subagent_event only). The enclosing run wire
   * metadata continues to belong exclusively to the parent run. */
  subagent_id?: string;
  parent_tool_use_id?: string;
  profile?: string | null;
  child_sequence?: number;
  event?: EngineEvent;
}

/** A child EngineEvent scoped to one Agent/Coordinator tool invocation. */
export interface ScopedSubagentEvent extends EngineEvent {
  kind: "subagent_event";
  subagent_id: string;
  parent_tool_use_id: string;
  description: string;
  profile?: string | null;
  /** Coordinator child position; null/absent for a direct Agent call. */
  index?: number | null;
  child_sequence: number;
  event: EngineEvent;
}

export type SubagentRunStatus =
  | "running"
  | "waiting_permission"
  | "succeeded"
  | "failed"
  | "cancelled"
  | "interrupted";

export interface SubagentTool {
  id: string;
  name: string;
  input?: unknown;
  result: string;
  ok?: boolean;
  status: string;
  permission?: string;
  truncated: boolean;
}

export interface SubagentRun {
  id: string;
  parentToolUseId: string;
  description: string;
  profile?: string;
  index: number;
  childSequence: number;
  status: SubagentRunStatus;
  output: string;
  outputTruncated: boolean;
  segmentCount: number;
  toolsById: Record<string, SubagentTool>;
  toolOrder: string[];
}

/** Versioned ordering metadata added in protocol v1. Optional fields preserve
 * compatibility with servers that still emit the original unversioned tags. */
export interface RunWireMeta {
  protocol_version?: number;
  event_id?: string;
  run_id?: string;
  parent_run_id?: string;
  session_id?: string;
  session_revision?: number;
  sequence?: number;
  timestamp_ms?: number;
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
  context?: string | null;
  urgency?: string | null;
  format?: string | null;
}

export interface DoneResult extends RunWireMeta {
  type: "done";
  text: string;
  usage: { input_tokens: number; output_tokens: number; cache_read_input_tokens: number; cache_creation_input_tokens: number };
  turns: number;
  stop_reason: string | null;
}

export interface ErrorMsg extends RunWireMeta {
  type: "error";
  message: string;
  code?: "authentication" | "payload_too_large" | "unsupported_format" | "invalid_request" | "path_denied" | "not_found" | "configuration" | "provider_unavailable" | "storage" | "cancelled" | "internal";
  retryable?: boolean;
  operation?: string;
  trace_id?: string;
  safe_details?: Record<string, unknown>;
}

export interface ModelInfo { name: string; label: string; context_window?: number; }

export interface InfoMsg {
  type: "info";
  model: string;
  session_id: string;
  /** Auth token for QR-code remote access. */
  auth_token?: string;
  available_models: ModelInfo[];
}

export interface EventMsg extends RunWireMeta {
  type: "event";
  event: EngineEvent;
}

export interface SessionInfoWire {
  id: string;
  started: string | null;
  message_count: number;
  summary: string;
  /** Custom title (optional, newer servers). */
  title?: string;
  /** Metadata tag; forks carry "fork:<sid>#<index>" lineage. */
  tag?: string | null;
}

export interface SessionListMsg {
  type: "session_list";
  sessions: SessionInfoWire[];
}

export interface MessagesLoadedMsg {
  type: "messages_loaded";
  protocol_version?: number;
  session_id?: string;
  /** Canonical SessionSnapshot.revision. */
  revision?: number;
  timestamp_ms?: number;
  /** Each entry is a serialized engine Message ({role, content}). */
  messages: unknown[];
  /** Cumulative token usage from all completed runs (restores the in/out display). */
  cumulative_usage?: { input_tokens?: number; output_tokens?: number; cache_read_input_tokens?: number; cache_creation_input_tokens?: number };
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

/** First user prompt of one run within a session (for the rail browser). */
export interface SessionRunPrompt {
  preview: string;
  message_index: number;
  timestamp_ms?: number | null;
}

export interface SessionPromptsMsg {
  type: "session_prompts";
  session_id: string;
  prompts: SessionRunPrompt[];
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
  /** Full markdown body (injected as append_system_prompt when /skill-name is used). */
  body: string;
}
export interface PluginInfo {
  name: string;
  dir: string;
  skill_count: number;
}
export interface ExtensionDescriptor {
  kind: "skill" | "profile" | "plugin" | "mcp";
  name: string;
  source: string;
  source_kind: "bundled" | "user" | "project" | "plugin" | "explicit" | "dynamic";
  precedence: number;
  version: string | null;
  status: "active" | "pending" | "shadowed" | "failed" | "disconnected";
  shadowed_by?: string;
  detail?: string;
}
export interface ExtensionDiagnostic {
  severity: "warning" | "error";
  code: string;
  kind: ExtensionDescriptor["kind"];
  name: string | null;
  source: string | null;
  message: string;
  suggestion: string;
}
export interface HookEntry {
  hook_type: string;
  matcher: string;
  command: string;
}
export interface ReferenceItem {
  name: string;
  description: string;
}
export interface ConfigFieldReference {
  name: string;
  description: string;
}
export interface ConfigDiagnosticInfo {
  severity: "warning" | "error";
  code: string;
  message: string;
  field: string | null;
  source: string | null;
  related_source: string | null;
  suggestion: string;
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
export interface RuntimeExecutableProbe {
  name: string;
  status: "checking" | "available" | "missing" | "invalid" | "version mismatch";
  path: string | null;
  version: string | null;
  expected_version: string | null;
  resolution_source: string;
  diagnostic: string | null;
  suggestion: string | null;
}
export interface RuntimeProbeReport {
  fingerprint: string;
  completed_at_ms: number;
  timeout_ms: number;
  output_limit_bytes: number;
  entries: RuntimeExecutableProbe[];
  python_venv: {
    status: "available" | "missing" | "invalid";
    python_path: string | null;
    required: boolean;
    suggestion: string | null;
  };
  /** Probe result for the MarkItDown CLI
   *  (https://github.com/microsoft/markitdown), used to convert
   *  office documents, HTML, EPUB, and other formats to structured
   *  Markdown for LLM consumption. */
  markitdown: {
    status: "available" | "missing" | "invalid";
    path: string | null;
    version: string | null;
    suggestion: string | null;
  };
}
export interface ProjectInfo {
  cwd: string;
  model: string;
  tools: ToolInfo[];
  mcp_servers: McpServerInfo[];
  skills: SkillInfo[];
  plugins: PluginInfo[];
  extensions: ExtensionDescriptor[];
  extension_diagnostics: ExtensionDiagnostic[];
  hooks: HookEntry[];
  /** Mneme memory facts, importance desc. */
  facts: FactInfo[];
  /** Active (non-done) beads, priority desc. */
  beads: BeadInfo[];
  docs: PathLayer[];
  settings: PathLayer[];
  /** Generated from the executable's Clap command definition. */
  cli_reference: ReferenceItem[];
  /** Shared top-level settings metadata used by server diagnostics. */
  config_reference: ConfigFieldReference[];
  config_diagnostics: ConfigDiagnosticInfo[];
  system: RuntimeProbeReport;
  /** Configured model context window (tokens), if set. */
  context_window: number | null;
  /** Effective auto-compact threshold (tokens). */
  compact_threshold: number;
  /** Public URL for QR-code mobile access, if set via --public-url. */
  public_url: string | null;
  git: GitInfo | null;
  /** API balances for configured billing providers. */
  provider_balances: ProviderBalance[];
  /** Model name → billing provider key (only models with configured billing). */
  model_providers: ModelProviderMapping[];
}

export interface ProviderBalance {
  provider: string;
  summary: string;
  ok: boolean;
  error?: string;
}

export interface FactInfo {
  name: string;
  title: string;
  fact_type: string;
  importance: number;
  confidence: number;
  path: string;
}

export interface BeadInfo {
  id: string;
  title: string;
  status: string;
  priority: number;
  path: string;
}

export interface ModelProviderMapping {
  model: string;
  provider: string;
}
export interface ProjectInfoMsg {
  type: "project_info";
  info: ProjectInfo;
}

export interface SystemProbeMsg {
  type: "system_probe";
  system: RuntimeProbeReport;
}

export interface GitShowRequest {
  type: "git_show";
  sha: string;
}

export interface SessionPromptsRequest {
  type: "session_prompts";
  session_id: string;
}

export type PermissionMode = "default" | "acceptEdits" | "auto" | "bypassPermissions" | "plan";

export interface SetPermissionModeRequest {
  type: "set_permission_mode";
  mode: PermissionMode;
}

export interface SetModelRequest {
  type: "set_model";
  name: string;
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
  | SessionPromptsMsg
  | ProjectInfoMsg
  | SystemProbeMsg
  | GitShowMsg;

// ── Browser → Server messages ─────────────────────────────────────────────

export interface ImageRef {
  media_type: string;
  data: string;
}

export interface AttachmentRef {
  id: string;
  filename: string;
  extracted_text: string;
  images?: ImageRef[];
}

export interface UploadResponse {
  id: string;
  filename: string;
  extracted_text: string;
  image_count: number;
  images?: ImageRef[];
  error?: string;
}

export interface RunRequest {
  type: "run";
  prompt: string;
  model?: string;
  max_turns?: number;
  /** Skill body injected into system prompt (from /skill-name). */
  append_system_prompt?: string;
  /** Pre-extracted file attachments. */
  attachments?: AttachmentRef[];
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
  | GitShowRequest
  | SessionPromptsRequest
  | SetPermissionModeRequest
  | SetModelRequest;

// ── Application types ─────────────────────────────────────────────────────

export interface MessageAttachment {
  filename: string;
  /** Local object-URL for image preview (images only). */
  previewUrl?: string;
}

export interface ChatMessage {
  id: string;
  role: "user" | "assistant" | "tool" | "system";
  content: string;
  /** When this message was created (epoch ms). */
  timestamp?: number;
  /** Attachment names associated with this user turn. */
  attachments?: MessageAttachment[];
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
  /** Index of the source JSONL message this chat entry derives from
   * (restored sessions only — used by the session rail to locate prompts). */
  srcIndex?: number;
}
