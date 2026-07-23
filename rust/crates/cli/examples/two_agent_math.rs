//! Two-agent math quiz demo.
//!
//! Agent A (questioner): generates a fresh 2-digit addition/subtraction problem.
//! Agent B (answerer):   solves the problem.
//!
//! They alternate for 4 rounds, printing each Q&A pair.
//!
//! Requires `ANTHROPIC_API_KEY` (and `ANTHROPIC_BASE_URL` if using a proxy).
//!
//! Run:  cargo run --example two_agent_math

use std::path::PathBuf;
use std::sync::Arc;

use nonoclaw_api::Client;
use nonoclaw_core::MessageContent;
use nonoclaw_engine::{EngineOptions, FinalResult, QueryEngine, RunController, RunTerminalStatus};
use nonoclaw_tools::{register_all, ToolRegistry};

const ROUNDS: usize = 4;

/// System-level hint appended for each agent so they stay on-task.
const QUESTIONER_HINT: &str =
    "你是一个出题官。你只出一道**2位数加减法**题目（两个两位数之间的加法或减法）。\n\
     输出**只能是一行纯题目**，格式如 `34 + 58 = ?` 或 `76 - 29 = ?`。不要加任何解释、标点、或额外文字。\n\
     确保被减数大于减数（减法时结果不为负）。";

const ANSWERER_HINT: &str =
    "你是一个答题机器人。你会收到一道数学题，你只需要输出**最终答案**（一个数字）。\n\
     格式如 `34 + 58 = 92`。不要加任何解释或其他文字。";

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    // ---- Build shared infrastructure ----
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let client = Arc::new(
        Client::from_env().expect("ANTHROPIC_API_KEY not set (and no credentials file found)"),
    );
    let (registry, todos) = register_all();
    let registry = Arc::new(registry);

    let mut questioner = build_engine(
        Arc::clone(&client),
        Arc::clone(&registry),
        Arc::clone(&todos),
        QUESTIONER_HINT,
    );
    let mut answerer = build_engine(
        Arc::clone(&client),
        Arc::clone(&registry),
        Arc::clone(&todos),
        ANSWERER_HINT,
    );

    let mut last_problem = String::new();

    for round in 1..=ROUNDS {
        println!("══════ Round {}/{} ══════", round, ROUNDS);

        // --- Agent A: generate a problem ---
        questioner.clear().await.expect("clear questioner");
        let (next_questioner, q_result) = run_once(
            questioner,
            &cwd,
            MessageContent::from_text("出一道2位数加减法题目"),
        )
        .await;
        questioner = next_questioner;
        let problem = q_result.text.trim().to_string();
        println!("🧑‍🏫 出题官: {problem}");

        // --- Agent B: answer the problem ---
        answerer.clear().await.expect("clear answerer");
        let (next_answerer, a_result) =
            run_once(answerer, &cwd, MessageContent::from_text(&problem)).await;
        answerer = next_answerer;
        let answer = a_result.text.trim().to_string();
        println!("🎓 答题者: {answer}");

        last_problem = problem;
        println!();
    }

    println!("✅ 所有 {ROUNDS} 轮完成！最后一道题: {last_problem}");
}

async fn run_once(
    engine: QueryEngine,
    cwd: &std::path::Path,
    content: MessageContent,
) -> (QueryEngine, FinalResult) {
    let controller = RunController::for_engine(&engine, cwd.to_path_buf());
    let completion = controller.start(engine, content, |_| async {}).wait().await;
    let engine = completion.engine.expect("run task lost its engine");
    match completion.terminal.status {
        RunTerminalStatus::Done => (
            engine,
            completion
                .terminal
                .result
                .expect("completed run missing result"),
        ),
        RunTerminalStatus::Cancelled | RunTerminalStatus::Error => {
            panic!("agent failed: {:?}", completion.terminal.reason)
        }
    }
}

fn build_engine(
    client: Arc<Client>,
    registry: Arc<ToolRegistry>,
    todos: Arc<nonoclaw_tools::TodoStore>,
    hint: &str,
) -> QueryEngine {
    let options = EngineOptions {
        model: std::env::var("NONOCLAW_MODEL")
            .unwrap_or_else(|_| "claude-sonnet-4-5-20250929".into()),
        max_tokens: 256,
        max_turns: 1, // single turn per Q / per A
        append_system_prompt: Some(hint.to_string()),
        auto_compact: false,
        ..EngineOptions::default()
    };
    QueryEngine::new(client, registry, todos, options)
}
