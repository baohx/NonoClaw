//! NonoClaw CLI. Mirrors the externally-visible flags from `src/main.tsx` /
//! `src/entrypoints/cli.tsx`. Runs headless (`--print`, piped input, or any
//! positional prompt) or starts the web UI (`--serve-http`).

mod acp;
mod attachments;
mod billing;
mod project_info;
mod remote;
mod serve_http;
mod skill_watcher;
mod skills;

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use clap::{ArgGroup, Parser, ValueEnum};
use nonoclaw_core::{MessageContent, PermissionMode, Usage};
use nonoclaw_engine::{
    ClientPurpose, ConfigSource, EngineEvent, EngineSkillSource, EventEnvelope, QueryEngine,
    RunConfigOverrides, RunController, RunTerminalStatus, SessionService, SkillsManager,
};
use nonoclaw_tools::register_all;
use serde_json::json;

/// `--jev` mode: which path assigns run-outcome reward labels.
#[derive(Copy, Clone, Debug, ValueEnum, PartialEq)]
enum JevMode {
    /// Follow settings.json (on when jev.apiKey is present and enabled).
    Auto,
    /// Force Jev classification (requires a configured key).
    On,
    /// Force the traditional heuristic path.
    Off,
}

impl JevMode {
    fn override_flag(self) -> Option<bool> {
        match self {
            JevMode::Auto => None,
            JevMode::On => Some(true),
            JevMode::Off => Some(false),
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum PermissionModeArg {
    Default,
    #[value(alias = "acceptEdits")]
    AcceptEdits,
    Auto,
    #[value(alias = "sandboxWorkspaceWrite")]
    SandboxWorkspaceWrite,
    #[value(alias = "sandboxReadOnly")]
    SandboxReadOnly,
    #[value(alias = "bypassPermissions")]
    BypassPermissions,
    Plan,
}

impl From<PermissionModeArg> for PermissionMode {
    fn from(value: PermissionModeArg) -> Self {
        match value {
            PermissionModeArg::Default => Self::Default,
            PermissionModeArg::AcceptEdits => Self::AcceptEdits,
            PermissionModeArg::Auto => Self::Auto,
            PermissionModeArg::SandboxWorkspaceWrite => Self::SandboxWorkspaceWrite,
            PermissionModeArg::SandboxReadOnly => Self::SandboxReadOnly,
            PermissionModeArg::BypassPermissions => Self::BypassPermissions,
            PermissionModeArg::Plan => Self::Plan,
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum OutputFormat {
    Text,
    Json,
}

#[derive(Parser, Debug)]
#[command(
    name = "nonoclaw",
    version,
    about = "NonoClaw — Rust rewrite of Claude Code (agent CLI)",
    group(
        ArgGroup::new("operating_mode")
            .args([
                "plugin_add",
                "remote",
                "mcp_serve",
                "mcp_serve_memory",
                "list_sessions",
                "serve",
                "acp",
                "serve_http",
            ])
            .multiple(false)
    )
)]
struct Cli {
    /// The prompt. If omitted, read from stdin.
    #[arg(help_heading = "Input & headless")]
    prompt: Vec<String>,

    /// Compatibility marker for explicit headless mode; local prompt/stdin runs are already headless.
    #[arg(
        short = 'p',
        long,
        default_value_t = false,
        help_heading = "Input & headless"
    )]
    print: bool,

    /// Tag the created session (e.g. `bench-smoke`, `eval`). Tagged sessions
    /// are skipped by auto-resume and by dream outcome scanning.
    #[arg(long, value_name = "TAG", help_heading = "Advanced diagnostics")]
    tag: Option<String>,

    /// Override the main-loop model.
    #[arg(long, value_name = "ID", help_heading = "Model & limits")]
    model: Option<String>,

    /// Permission mode.
    #[arg(
        long,
        value_enum,
        value_name = "MODE",
        default_value_t = PermissionModeArg::Default,
        help_heading = "Permissions"
    )]
    permission_mode: PermissionModeArg,

    /// Comma-separated tool allowlist (e.g. "Read,Grep,Bash").
    #[arg(
        long,
        value_name = "LIST",
        value_delimiter = ',',
        help_heading = "Permissions"
    )]
    allowed_tools: Vec<String>,

    /// Comma-separated tool denylist.
    #[arg(
        long,
        value_name = "LIST",
        value_delimiter = ',',
        help_heading = "Permissions"
    )]
    disallowed_tools: Vec<String>,

    /// Maximum agent turns.
    #[arg(long, value_name = "N", help_heading = "Model & limits")]
    max_turns: Option<u32>,

    /// Max output tokens per turn.
    #[arg(long, value_name = "N", help_heading = "Model & limits")]
    max_tokens: Option<u32>,

    /// Extra text appended to the system prompt.
    #[arg(long, value_name = "TXT", help_heading = "Input & headless")]
    append_system_prompt: Option<String>,

    /// Additional directory for NONOCLAW.md discovery (repeatable).
    #[arg(long, value_name = "PATH", help_heading = "Configuration & extensions")]
    add_dir: Vec<PathBuf>,

    /// Skip all permission prompts (sets permission-mode = bypass-permissions).
    #[arg(long, conflicts_with = "permission_mode", help_heading = "Permissions")]
    dangerously_skip_permissions: bool,

    /// Output format.
    #[arg(
        long,
        value_enum,
        default_value_t = OutputFormat::Text,
        help_heading = "Input & headless"
    )]
    output_format: OutputFormat,

    /// MCP config path. Servers are merged into the canonical resolved config.
    #[arg(long, value_name = "PATH", help_heading = "Configuration & extensions")]
    mcp_config: Option<PathBuf>,

    /// Resume a prior session by id (loads its transcript and continues).
    #[arg(
        long,
        value_name = "ID",
        conflicts_with_all = ["continue_session", "no_session", "list_sessions"],
        help_heading = "Sessions"
    )]
    resume: Option<String>,

    /// Resume the most recent session for this directory.
    #[arg(
        long = "continue",
        conflicts_with_all = ["no_session", "list_sessions"],
        help_heading = "Sessions"
    )]
    continue_session: bool,

    /// List stored sessions for this directory and exit.
    #[arg(
        long,
        conflicts_with_all = ["resume", "continue_session", "no_session"],
        help_heading = "Sessions"
    )]
    list_sessions: bool,

    /// Disable session persistence for this run.
    #[arg(
        long,
        conflicts_with_all = ["resume", "continue_session", "list_sessions"],
        help_heading = "Sessions"
    )]
    no_session: bool,

    /// Disable auto-compaction of long transcripts.
    #[arg(long, help_heading = "Model & limits")]
    no_auto_compact: bool,

    /// Log full unredacted API traffic (requests + raw SSE responses +
    /// usage summaries) to .nonoclaw/logs/api/. Diagnostics only: payloads
    /// contain complete prompts. API keys are never logged (headers only).
    #[arg(long, help_heading = "Advanced diagnostics")]
    log_raw_api: bool,

    /// Estimated-token threshold above which auto-compact fires.
    #[arg(long, help_heading = "Model & limits")]
    compact_threshold: Option<usize>,

    /// Model context window in tokens. When set, auto-compact fires at
    /// window − maxTokens − margin (unless --compact-threshold is given).
    #[arg(long, help_heading = "Model & limits")]
    context_window: Option<usize>,

    /// Explicit settings file path (highest priority after CLI flags).
    #[arg(long, value_name = "PATH", help_heading = "Configuration & extensions")]
    settings: Option<PathBuf>,

    /// Jev (TypeSafe AI System One) mode for run-outcome reward labels:
    /// `auto` follows settings.json (on when jev.apiKey is set), `on` forces
    /// Jev classification, `off` forces the traditional heuristic path.
    #[arg(long, value_enum, default_value_t = JevMode::Auto, help_heading = "Configuration & extensions")]
    jev: JevMode,

    /// Run as a remote session server (TCP, JSON-lines) on ADDR (e.g. 127.0.0.1:8765).
    #[arg(long, value_name = "ADDR", help_heading = "Advanced integrations")]
    serve: Option<String>,

    /// Start the web UI server (HTTP + WebSocket) on ADDR and open the browser.
    #[arg(long, value_name = "ADDR", help_heading = "Web UI")]
    serve_http: Option<String>,

    /// Public URL used in the QR code for mobile access (e.g.
    /// http://192.168.1.42:8765). If not set, the QR defaults to
    /// `window.location.origin`.
    #[arg(
        long,
        value_name = "URL",
        requires = "serve_http",
        help_heading = "Web UI"
    )]
    public_url: Option<String>,

    /// Auto-spawn cloudflared tunnel for public internet access. Requires
    /// cloudflared in PATH. The generated *.trycloudflare.com URL replaces
    /// --public-url automatically.
    #[arg(long, requires = "serve_http", help_heading = "Web UI")]
    tunnel: bool,

    /// Connect to a remote session server at ADDR and run the prompt.
    #[arg(long, value_name = "ADDR", help_heading = "Advanced integrations")]
    remote: Option<String>,

    /// Run as an MCP server over stdio (expose tools to an MCP client).
    #[arg(long, help_heading = "Advanced integrations")]
    mcp_serve: bool,

    /// Run as an Agent Client Protocol (ACP) server over stdio (Zed editor).
    #[arg(long, help_heading = "Advanced integrations")]
    acp: bool,

    /// Run as an MCP server exposing only the Mneme memory system
    /// (facts/beads/wiki/goals) so an external harness can mount it.
    #[arg(long, help_heading = "Advanced integrations")]
    mcp_serve_memory: bool,

    /// Install a plugin from SOURCE (local dir or git URL) into .nonoclaw/plugins.
    #[arg(
        long,
        value_name = "SOURCE",
        help_heading = "Configuration & extensions"
    )]
    plugin_add: Option<String>,

    /// Verbose logging (RUST_LOG=debug also works).
    #[arg(long, help_heading = "Advanced diagnostics")]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // `--print` is a preserved explicit-headless compatibility flag. All
    // non-server local invocations are headless, so reading it is sufficient.
    let _explicit_headless = cli.print;

    // Must run before any Client is built: the api crate reads this env var
    // once per request to decide whether to open raw traffic log files.
    if cli.log_raw_api {
        std::env::set_var("NONOCLAW_RAW_API_LOG", "1");
    }

    // `--verbose` shows NonoClaw debug logs but keeps the noisy HTTP stack
    // (rustls/hyper/reqwest) at warn: these emit benign TLS teardown warnings
    // ("peer closed connection without sending TLS close_notify") on every
    // connection pool cleanup, which would drown the signal.
    let filter = if cli.verbose {
        "debug,hyper=warn,hyper_util=warn,reqwest=warn,h2=warn,rustls=warn,tokio_tungstenite=warn,tungstenite=warn"
    } else {
        "nonoclaw_api=info,warn"
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
        )
        .with_writer(std::io::stderr)
        .init();

    if let Some(p) = &cli.mcp_config {
        tracing::info!("--mcp-config {:?}: connecting to configured MCP servers", p);
    }

    // Plugin install: copy/clone into .nonoclaw/plugins.
    if let Some(src) = &cli.plugin_add {
        add_plugin(src)?;
        return Ok(());
    }

    // Remote client mode forwards the request without constructing a local run.
    if let Some(addr) = &cli.remote {
        let prompt = cli.prompt.join(" ");
        let trimmed = prompt.trim();
        if trimmed.is_empty() {
            anyhow::bail!("--remote requires a prompt argument");
        }
        let req = remote::RunRequest {
            prompt: trimmed.to_string(),
            model: cli.model.clone(),
            max_turns: cli.max_turns,
        };
        return remote::connect(addr, &req).await;
    }

    // MCP server mode: speak JSON-RPC over stdio, expose built-in tools.
    if cli.mcp_serve {
        let (registry, _todos) = register_all();
        let cwd = std::env::current_dir().context("no current directory")?;
        return Ok(nonoclaw_tools::mcp_server::serve_stdin(&registry, &cwd).await?);
    }

    // Memory-only MCP server: expose just the Mneme memory system + Read (for
    // spill retrieval) so an external harness can mount cross-session memory.
    if cli.mcp_serve_memory {
        let mut registry = nonoclaw_tools::ToolRegistry::new();
        registry.register(std::sync::Arc::new(nonoclaw_tools::builtin::MemoryTool));
        registry.register(std::sync::Arc::new(nonoclaw_tools::builtin::ReadTool));
        let cwd = std::env::current_dir().context("no current directory")?;
        return Ok(nonoclaw_tools::mcp_server::serve_stdin(&registry, &cwd).await?);
    }

    let cwd = std::env::current_dir().context("no current directory")?;
    let session_service = SessionService::new();

    // --list-sessions prints and exits before any model call.
    if cli.list_sessions {
        list_and_exit(&session_service, &cwd);
    }

    // Resolve every file/environment/MCP layer once. The immutable snapshot is
    // shared by headless, Web, remote server, compact, subagent, and doc-model
    // paths; resolution itself does not mutate process environment.
    let resolved = Arc::new(nonoclaw_engine::load_resolved_config(
        &cwd,
        cli.settings.as_deref(),
        cli.mcp_config.as_deref(),
    ));
    resolved.log_diagnostics();

    // Export proxy env vars before any reqwest client is built (reqwest
    // snapshots proxy config at client build time).
    nonoclaw_engine::apply_proxy_env(resolved.settings());

    // Initialize the Jev decision-model client for run-outcome reward
    // labels (process-wide; every write site degrades to heuristics when
    // this leaves it unset).
    nonoclaw_engine::jev_reward::init_global_jev(&resolved, cli.jev.override_flag());

    if let Some(addr) = &cli.serve {
        return remote::serve(addr, Arc::clone(&resolved)).await;
    }

    let permission_mode = if cli.dangerously_skip_permissions {
        PermissionMode::BypassPermissions
    } else {
        cli.permission_mode.into()
    };
    let model = cli
        .model
        .clone()
        .unwrap_or_else(|| resolved.active_model.value.clone());
    let client = resolved
        .client_for(ClientPurpose::Conversation, Some(&model))
        .context("failed to build API client from resolved configuration")?;

    let skills_manager = Arc::new(RwLock::new(SkillsManager::new(&cwd)));
    let background_registry = Arc::new(std::sync::Mutex::new(
        nonoclaw_tools::BackgroundTaskRegistry::new(),
    ));

    // Spawn file watcher for hot-reloading skills in headless mode.
    skill_watcher::spawn_skill_watcher(Arc::clone(&skills_manager), cwd.clone());

    let mut options = resolved
        .resolve_run(RunConfigOverrides {
            source: ConfigSource::CommandLine {
                field: "run options".into(),
            },
            model: cli.model.clone(),
            max_turns: cli.max_turns,
            max_tokens: cli.max_tokens,
            context_window: cli.context_window,
            compact_threshold: cli.compact_threshold,
            auto_compact: cli.no_auto_compact.then_some(false),
            permission_mode: Some(permission_mode),
            allowed_tools: (!cli.allowed_tools.is_empty()).then(|| cli.allowed_tools.clone()),
            disallowed_tools: (!cli.disallowed_tools.is_empty())
                .then(|| cli.disallowed_tools.clone()),
            append_system_prompt: cli.append_system_prompt.clone(),
            add_dirs: cli.add_dir.clone(),
            arguments: None,
            is_non_interactive: true,
        })
        .options;
    options.skills_manager = Some(Arc::clone(&skills_manager));
    options.background_registry = Some(Arc::clone(&background_registry));

    let (context_window, compact_threshold_tokens) = resolved.model_budget(&model);
    tracing::info!(
        context_window,
        compact_threshold = compact_threshold_tokens,
        max_tokens = options.max_tokens,
        "resolved context budget"
    );

    // Build the tool registry once: builtins + all resolved MCP sources.
    let (mut registry, todos) = register_all();
    let mcp_configs = resolved.mcp_configs();
    nonoclaw_tools::register_mcp(&mut registry, &mcp_configs).await;
    // Register ToolSearch with a snapshot of all tools (including MCP).
    let tool_search = nonoclaw_tools::builtin::ToolSearchTool::new(registry.search_entries());
    registry.register(Arc::new(tool_search));
    // Register skill discovery/loading after MCP discovery. SkillSearch exposes
    // only bounded metadata; Skill loads one selected body on demand.
    let skill_source = Arc::new(EngineSkillSource::new(Arc::clone(&skills_manager)));
    registry.register(Arc::new(nonoclaw_tools::builtin::SkillSearchTool::new(
        skill_source.clone(),
    )));
    registry.register(Arc::new(nonoclaw_tools::builtin::SkillTool::new(
        skill_source,
    )));
    let registry = Arc::new(registry);

    // ACP server mode: speak Agent Client Protocol over stdio (Zed editor).
    if cli.acp {
        tracing::info!("ACP server over stdio");
        return acp::serve_stdin(
            registry,
            todos,
            cwd,
            Arc::clone(&resolved),
            skills_manager,
            background_registry,
        )
        .await
        .map_err(anyhow::Error::from);
    }

    // Web UI server: HTTP + WebSocket. All model, compact, document, media,
    // permission, and MCP values are derived from this same resolved snapshot.
    if let Some(addr) = &cli.serve_http {
        tracing::info!("open http://{addr} in your browser");
        serve_http::serve(
            addr,
            registry,
            todos,
            cwd,
            model,
            Arc::clone(&resolved),
            cli.public_url.clone(),
            cli.tunnel,
        )
        .await?;
        return Ok(());
    }

    // --- Headless path ---
    let prompt = read_prompt(&cli)?;
    let session = resolve_session(&session_service, &cli, &cwd, &model).await?;
    // Keep a handle for the Level-1 RL outcome label written after the run.
    let outcome_session = session.as_ref().map(|(s, _)| s.clone());
    let engine = match session {
        Some((session, snapshot)) => {
            QueryEngine::with_session(client, registry, todos, options, session, snapshot)
        }
        None => QueryEngine::new(client, registry, todos, options),
    };

    let json = matches!(cli.output_format, OutputFormat::Json);
    let controller = RunController::for_engine(&engine, cwd.clone());
    let completion = controller
        .start(
            engine,
            MessageContent::from_text(&prompt),
            move |sequenced| async move {
                handle_event(json, &sequenced);
            },
        )
        .wait()
        .await;

    // Level-1 RL label: persist the terminal outcome + heuristic reward for
    // headless runs too (parity with the WS and REST write sites).
    if let Some(outcome_session) = &outcome_session {
        let terminal = &completion.terminal;
        let (status, detail, turns) = match (&terminal.status, &terminal.reason) {
            (RunTerminalStatus::Done, reason) => {
                let turns = terminal.result.as_ref().map(|r| r.turns).unwrap_or(0);
                let detail = match reason {
                    nonoclaw_engine::RunFinishReason::Completed { detail } => detail.clone(),
                    other => format!("{other:?}"),
                };
                ("done", detail, turns)
            }
            (RunTerminalStatus::Cancelled, reason) => {
                let detail = match reason {
                    nonoclaw_engine::RunFinishReason::Cancelled { reason } => reason.clone(),
                    other => format!("{other:?}"),
                };
                ("cancelled", detail, 0)
            }
            (RunTerminalStatus::Error, reason) => {
                let detail = match reason {
                    nonoclaw_engine::RunFinishReason::Error { message, .. } => {
                        nonoclaw_core::redact_text(message)
                    }
                    other => format!("{other:?}"),
                };
                ("error", detail, 0)
            }
        };
        let (reward, detail) = nonoclaw_engine::jev_reward::run_reward_with_jev(
            status,
            &detail,
            turns,
            &nonoclaw_engine::session::RewardSignals::default(),
        )
        .await;
        if let Err(e) = outcome_session
            .write_run_outcome(
                &terminal.run_id,
                status,
                reward,
                turns,
                &detail,
                &terminal.usage(),
            )
            .await
        {
            tracing::warn!(error = %e, "failed to persist run outcome");
        }
    }

    let result = match completion.terminal.status {
        RunTerminalStatus::Done => completion
            .terminal
            .result
            .context("run completed without a result")?,
        RunTerminalStatus::Cancelled => {
            anyhow::bail!("agent run cancelled: {:?}", completion.terminal.reason)
        }
        RunTerminalStatus::Error => {
            anyhow::bail!("agent run failed: {:?}", completion.terminal.reason)
        }
    };

    if json {
        emit_json(&json!({
            "type": "result",
            "text": result.text,
            "turns": result.turns,
            "usage": usage_json(&result.usage),
            "stop_reason": result.stop_reason.as_ref().map(|s| s.as_str()),
        }));
    } else {
        // Text was streamed live; just print the usage summary on stderr.
        eprintln!(
            "\n[turns: {}, in: {}, out: {}, cache read: {}, cache write: {}]",
            result.turns,
            result.usage.input_tokens,
            result.usage.output_tokens,
            result.usage.cache_read_input_tokens,
            result.usage.cache_creation_input_tokens,
        );
    }

    Ok(())
}

/// Print stored sessions for `cwd` and exit.
fn list_and_exit(service: &SessionService, cwd: &std::path::Path) -> ! {
    match service.list_sessions(cwd) {
        Ok(list) if list.is_empty() => {
            println!("No sessions found for {}.", cwd.display());
        }
        Ok(list) => {
            for s in list {
                println!(
                    "{}\t{}\t{} msgs\t{}",
                    s.id,
                    s.started.as_deref().unwrap_or("-"),
                    s.message_count,
                    preview_one_line(&s.summary, 60),
                );
            }
        }
        Err(e) => eprintln!("error listing sessions: {e}"),
    }
    std::process::exit(0);
}

/// Resolve the canonical session actor and its current snapshot for this run.
async fn resolve_session(
    service: &SessionService,
    cli: &Cli,
    cwd: &std::path::Path,
    model: &str,
) -> Result<Option<(nonoclaw_engine::Session, nonoclaw_engine::SessionSnapshot)>> {
    if cli.no_session {
        return Ok(None);
    }
    let session = if let Some(id) = &cli.resume {
        service
            .resume(cwd, id)
            .with_context(|| format!("load session {id}"))?
    } else if cli.continue_session {
        match service
            .most_recent_session(cwd)
            .context("failed to look up most recent session")?
        {
            Some(id) => service
                .resume(cwd, &id)
                .with_context(|| format!("load session {id}"))?,
            None => service.create(cwd, nonoclaw_engine::new_session_id(), model)?,
        }
    } else {
        service.create(cwd, nonoclaw_engine::new_session_id(), model)?
    };
    let snapshot = session.snapshot().await?;
    if let Some(tag) = &cli.tag {
        // Only tag freshly created sessions; a --tag + --resume combination
        // would retroactively relabel prior work.
        if cli.resume.is_none() && !cli.continue_session {
            session
                .write_tag(tag.clone())
                .await
                .context("failed to tag session")?;
        }
    }
    Ok(Some((session, snapshot)))
}

fn add_plugin(src: &str) -> Result<()> {
    let home = nonoclaw_core::nonoclaw_data_dir()
        .context("cannot resolve nonoclaw data dir (set HOME or USERPROFILE)")?;
    let plugins = home.join("plugins");
    std::fs::create_dir_all(&plugins)?;
    if src.starts_with("http://") || src.starts_with("https://") || src.starts_with("git@") {
        let name = src
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("plugin")
            .trim_end_matches(".git");
        let dest = plugins.join(name);
        if dest.exists() {
            anyhow::bail!("{dest:?} already exists; remove it first");
        }
        let status = std::process::Command::new("git")
            .arg("clone")
            .arg(src)
            .arg(&dest)
            .status()
            .context("git clone")?;
        if !status.success() {
            anyhow::bail!("git clone failed");
        }
        eprintln!("plugin `{name}` cloned to {:?}", dest);
    } else {
        let src_path = std::path::Path::new(src);
        let name = src_path.file_name().context("bad source path")?;
        let dest = plugins.join(name);
        if dest.exists() {
            anyhow::bail!("{dest:?} already exists; remove it first");
        }
        copy_dir(src_path, &dest)?;
        eprintln!("plugin `{}` copied to {:?}", name.to_string_lossy(), dest);
    }
    Ok(())
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let dest = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &dest)?;
        } else {
            std::fs::copy(e.path(), &dest)?;
        }
    }
    Ok(())
}

fn preview_one_line(s: &str, max: usize) -> String {
    let one = s.lines().next().unwrap_or("").replace('\t', " ");
    if one.chars().count() <= max {
        one
    } else {
        let mut t: String = one.chars().take(max).collect();
        t.push('…');
        t
    }
}

fn read_prompt(cli: &Cli) -> Result<String> {
    if !cli.prompt.is_empty() {
        return Ok(cli.prompt.join(" "));
    }
    // Read stdin if piped (not a TTY).
    let mut buf = String::new();
    if !std::io::stdin().is_terminal() {
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("failed to read prompt from stdin")?;
    }
    let trimmed = buf.trim().to_string();
    if trimmed.is_empty() {
        anyhow::bail!("no prompt provided (pass arguments or pipe via stdin)");
    }
    Ok(trimmed)
}

fn handle_event(json: bool, envelope: &EventEnvelope) {
    let ev = &envelope.event;
    match ev {
        EngineEvent::TextDelta { text } => {
            if json {
                emit_json(&json!({"type": "text_delta", "text": text}));
            } else {
                let mut stdout = std::io::stdout();
                let _ = stdout.write_all(text.as_bytes());
                let _ = stdout.flush();
            }
        }
        EngineEvent::ToolUseStart { id, name, input } => {
            if json {
                emit_json(&json!({"type":"tool_use","id":id,"name":name,"input":input}));
            } else {
                eprintln!("\n▶ {name}");
            }
        }
        EngineEvent::ToolResult { id, ok, preview } => {
            if json {
                emit_json(&json!({"type":"tool_result","id":id,"ok":ok,"preview":preview}));
            } else {
                eprintln!("  ↳ {}: {}", if *ok { "ok" } else { "ERR" }, preview);
            }
        }
        EngineEvent::AssistantDone { text: _ } => {
            if !json {
                eprintln!();
            }
        }
        EngineEvent::Compacted {
            removed,
            kept,
            tokens_before,
            tokens_after,
            pruned_results,
        } => {
            if json {
                emit_json(
                    &json!({"type":"compacted","removed":removed,"kept":kept,"tokens_before":tokens_before,"tokens_after":tokens_after,"pruned_results":pruned_results}),
                );
            } else {
                eprintln!(
                    "[compacted: removed {removed}, kept {kept}, ~{tokens_before}→{tokens_after} tokens]"
                );
            }
        }
        EngineEvent::ModelInfo { model } => {
            // The model the API actually used (resolves aliases / endpoints).
            // Only meaningful in JSON/SDK output; stay quiet in text mode.
            if json {
                emit_json(&json!({"type":"model_info","model":model}));
            }
        }
        EngineEvent::SkillActivated {
            name,
            reason,
            source,
            version,
        } => {
            if json {
                emit_json(&json!({
                    "type":"skill_activated",
                    "name":name,
                    "reason":reason,
                    "source":source,
                    "version":version,
                }));
            } else {
                eprintln!("[skill: {name} ({reason}) from {source}]");
            }
        }
        EngineEvent::SessionRepair { repair } => {
            if json {
                emit_json(&json!({"type":"session_repair","repair":repair}));
            } else {
                eprintln!("[session repair: {:?}: {}]", repair.kind, repair.detail);
            }
        }
        EngineEvent::TaskChanged { change } => {
            if json {
                emit_json(&json!({"type":"task_changed","change":change}));
            } else {
                eprintln!(
                    "[tasks: {:?} {:?}, scope={}, count={}]",
                    change.source,
                    change.change,
                    change.scope,
                    change.tasks.len()
                );
            }
        }
        _ => {
            if json {
                emit_json(&json!({"type":"run_event","envelope":envelope}));
            }
        }
    }
}

fn emit_json(v: &serde_json::Value) {
    // Inline the serialization to avoid panicking on broken pipes.
    if let Ok(s) = serde_json::to_string(v) {
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(s.as_bytes());
        let _ = stdout.write_all(b"\n");
        let _ = stdout.flush();
    }
}

fn usage_json(u: &Usage) -> serde_json::Value {
    json!({
        "input_tokens": u.input_tokens,
        "output_tokens": u.output_tokens,
        "cache_creation_input_tokens": u.cache_creation_input_tokens,
        "cache_read_input_tokens": u.cache_read_input_tokens,
    })
}
