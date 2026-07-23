//! NonoClaw CLI. Mirrors the externally-visible flags from `src/main.tsx` /
//! `src/entrypoints/cli.tsx`. Runs headless (`--print`, piped input, or any
//! positional prompt) or starts the web UI (`--serve-http`).

mod attachments;
mod project_info;
mod remote;
mod serve_http;
mod skill_watcher;
mod skills;

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::{anyhow, Context, Result};
use clap::{Parser, ValueEnum};
use nonoclaw_core::{MessageContent, PermissionMode, Usage};
use nonoclaw_engine::{
    ClientPurpose, ConfigSource, EngineEvent, EventEnvelope, QueryEngine, RunConfigOverrides,
    RunController, RunTerminalStatus, SessionService, SkillsManager,
};
use nonoclaw_tools::register_all;
use serde_json::json;

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
    about = "NonoClaw — Rust rewrite of Claude Code (agent CLI)"
)]
struct Cli {
    /// The prompt. If omitted, read from stdin.
    prompt: Vec<String>,

    /// Preserve the `-p`/`--print` compatibility entry for explicit headless mode.
    #[arg(short = 'p', long, default_value_t = false)]
    print: bool,

    /// Override the main-loop model.
    #[arg(long, value_name = "ID")]
    model: Option<String>,

    /// Permission mode.
    #[arg(long, value_name = "MODE", default_value = "default")]
    permission_mode: String,

    /// Comma-separated tool allowlist (e.g. "Read,Grep,Bash").
    #[arg(long, value_name = "LIST", value_delimiter = ',')]
    allowed_tools: Vec<String>,

    /// Comma-separated tool denylist.
    #[arg(long, value_name = "LIST", value_delimiter = ',')]
    disallowed_tools: Vec<String>,

    /// Maximum agent turns.
    #[arg(long, value_name = "N")]
    max_turns: Option<u32>,

    /// Max output tokens per turn.
    #[arg(long, value_name = "N")]
    max_tokens: Option<u32>,

    /// Extra text appended to the system prompt.
    #[arg(long, value_name = "TXT")]
    append_system_prompt: Option<String>,

    /// Additional directory for NONOCLAW.md discovery (repeatable).
    #[arg(long, value_name = "PATH")]
    add_dir: Vec<PathBuf>,

    /// Skip all permission prompts (sets permission-mode = bypassPermissions).
    #[arg(long)]
    dangerously_skip_permissions: bool,

    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    output_format: OutputFormat,

    /// MCP config path. Servers are merged into the canonical resolved config.
    #[arg(long, value_name = "PATH")]
    mcp_config: Option<PathBuf>,

    /// Resume a prior session by id (loads its transcript and continues).
    #[arg(long, value_name = "ID")]
    resume: Option<String>,

    /// Resume the most recent session for this directory.
    #[arg(long = "continue")]
    continue_session: bool,

    /// List stored sessions for this directory and exit.
    #[arg(long)]
    list_sessions: bool,

    /// Disable session persistence for this run.
    #[arg(long)]
    no_session: bool,

    /// Disable auto-compaction of long transcripts.
    #[arg(long)]
    no_auto_compact: bool,

    /// Estimated-token threshold above which auto-compact fires.
    #[arg(long)]
    compact_threshold: Option<usize>,

    /// Model context window in tokens. When set, auto-compact fires at
    /// window − maxTokens − margin (unless --compact-threshold is given).
    #[arg(long)]
    context_window: Option<usize>,

    /// Explicit settings file path (highest priority after CLI flags).
    #[arg(long, value_name = "PATH")]
    settings: Option<PathBuf>,

    /// Run as a remote session server (TCP, JSON-lines) on ADDR (e.g. 127.0.0.1:8765).
    #[arg(long, value_name = "ADDR")]
    serve: Option<String>,

    /// Start the web UI server (HTTP + WebSocket) on ADDR and open the browser.
    #[arg(long, value_name = "ADDR")]
    serve_http: Option<String>,

    /// Public URL used in the QR code for mobile access (e.g.
    /// http://192.168.1.42:8765). If not set, the QR defaults to
    /// `window.location.origin`.
    #[arg(long, value_name = "URL")]
    public_url: Option<String>,

    /// Auto-spawn cloudflared tunnel for public internet access. Requires
    /// cloudflared in PATH. The generated *.trycloudflare.com URL replaces
    /// --public-url automatically.
    #[arg(long)]
    tunnel: bool,

    /// Connect to a remote session server at ADDR and run the prompt.
    #[arg(long, value_name = "ADDR")]
    remote: Option<String>,

    /// Run as an MCP server over stdio (expose tools to an MCP client).
    #[arg(long)]
    mcp_serve: bool,

    /// Install a plugin from SOURCE (local dir or git URL) into .nonoclaw/plugins.
    #[arg(long, value_name = "SOURCE")]
    plugin_add: Option<String>,

    /// Verbose logging (RUST_LOG=debug also works).
    #[arg(long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // `--print` is a preserved explicit-headless compatibility flag. All
    // non-server local invocations are headless, so reading it is sufficient.
    let _explicit_headless = cli.print;

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

    if let Some(addr) = &cli.serve {
        return remote::serve(addr, Arc::clone(&resolved)).await;
    }

    let permission_mode = if cli.dangerously_skip_permissions {
        PermissionMode::BypassPermissions
    } else {
        PermissionMode::from_kebab(&cli.permission_mode)
            .ok_or_else(|| anyhow!("unknown --permission-mode `{}`", cli.permission_mode))?
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
    options.background_registry = Some(background_registry);

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
    let registry = Arc::new(registry);

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
        } => {
            if json {
                emit_json(
                    &json!({"type":"compacted","removed":removed,"kept":kept,"tokens_before":tokens_before,"tokens_after":tokens_after}),
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
