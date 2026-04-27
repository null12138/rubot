mod agent;
mod channel;
mod config;
mod llm;
mod markdown;
mod memory;
mod personality;
mod planner;
mod subagent;
mod tools;

use config::Config;
use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::FileHistory;
use rustyline::validate::Validator;
use rustyline::Helper;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

struct RubotHelper {
    commands: Vec<String>,
}
impl Helper for RubotHelper {}
impl Completer for RubotHelper {
    type Candidate = Pair;
    fn complete(
        &self,
        line: &str,
        _pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        if line.starts_with('/') {
            let m: Vec<_> = self
                .commands
                .iter()
                .filter(|c| c.starts_with(line))
                .map(|c| Pair {
                    display: c.clone(),
                    replacement: c.clone(),
                })
                .collect();
            return Ok((0, m));
        }
        Ok((0, Vec::new()))
    }
}
impl Hinter for RubotHelper {
    type Hint = String;
    fn hint(&self, line: &str, pos: usize, _ctx: &rustyline::Context<'_>) -> Option<String> {
        if line.starts_with('/') && line.len() > 1 {
            self.commands.iter().find(|c| c.starts_with(line)).map(|c| {
                if c.len() > pos {
                    c[pos..].to_string()
                } else {
                    String::new()
                }
            })
        } else {
            None
        }
    }
}
impl Highlighter for RubotHelper {
    fn highlight_hint<'h>(&self, h: &'h str) -> std::borrow::Cow<'h, str> {
        std::borrow::Cow::Owned(format!("\x1b[2m{}\x1b[0m", h))
    }
}
impl Validator for RubotHelper {}
impl RubotHelper {
    fn new() -> Self {
        Self {
            commands: [
                "/quit", "/exit", "/plan", "/memory", "/model", "/config", "/clear", "/loop",
                "/wechat", "/usage",
            ]
            .into_iter()
            .map(Into::into)
            .collect(),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let action = parse_startup_action(env::args().skip(1))?;
    if action == StartupAction::Help {
        print_help();
        return Ok(());
    }
    if action == StartupAction::Version {
        println!("rubot {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("rubot=warn")),
        )
        .with_target(false)
        .compact()
        .init();

    if action == StartupAction::Wechat {
        return run_wechat().await;
    }

    // ── REPL mode ──
    let config = Config::load()?;
    config.ensure_workspace_dirs()?;
    let agent = Arc::new(Mutex::new(agent::Agent::new(config).await?));
    let restored_messages = agent.lock().await.restored_session_messages();

    println!("rubot {} — /quit to exit", env!("CARGO_PKG_VERSION"));
    if restored_messages > 0 {
        println!(
            "{}[restored {} session messages]{}",
            markdown::DIM,
            restored_messages,
            markdown::R
        );
    }
    tokio::task::spawn_blocking(move || run_repl(agent)).await??;
    Ok(())
}

async fn run_wechat() -> anyhow::Result<()> {
    let config = Config::load()?;
    config.ensure_workspace_dirs()?;

    if config.wechat_bot_token.is_empty() {
        anyhow::bail!(
            "RUBOT_WECHAT_BOT_TOKEN is not set. Run `rubot` and use /wechat setup to scan QR code, or set the token in {} manually.",
            config::env_file_path()?.display()
        );
    }

    println!("rubot wechat — starting WeChat bot...");
    let agent = Arc::new(Mutex::new(agent::Agent::new(config.clone()).await?));
    channel::start(agent, config).await?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupAction {
    Repl,
    Help,
    Version,
    Wechat,
}

fn parse_startup_action(args: impl IntoIterator<Item = String>) -> anyhow::Result<StartupAction> {
    let mut args = args.into_iter();
    match args.next().as_deref() {
        None => Ok(StartupAction::Repl),
        Some("-h" | "--help") => Ok(StartupAction::Help),
        Some("-V" | "--version") => Ok(StartupAction::Version),
        Some("wechat") => Ok(StartupAction::Wechat),
        Some(other) => {
            anyhow::bail!("unsupported argument: {other}\n\nRun `rubot --help` for usage.")
        }
    }
}

fn print_help() {
    println!(
        "\
rubot {version}

Usage:
  rubot                 Start interactive REPL
  rubot wechat          Start WeChat bot (requires RUBOT_WECHAT_BOT_TOKEN)
  rubot --help
  rubot --version

Description:
  Terminal AI agent with built-in tools, memory, planning, and subagents.

REPL commands:
  /quit / /exit              Save session memory and exit
  /clear                     Clear the conversation and screen
  /memory                    Show memory index
  /model [name]              Show or set the heavy model
  /config                    List effective config and .env path
  /config get <key>          Show one config value
  /config set <key> <value>  Save config to .env and apply it
  /plan                      Show the last executed plan
  /usage                     Show session token usage and estimated cost
  /loop <task>|<stop>        Auto-loop until stop condition
  /wechat                    Show WeChat setup instructions

Config:
  Reads a global .env from the Rubot config directory.
  Common keys: RUBOT_API_BASE_URL, RUBOT_API_KEY, RUBOT_MODEL,
  RUBOT_FAST_MODEL, RUBOT_TAVILY_API_KEY, RUBOT_WORKSPACE,
  RUBOT_MAX_RETRIES, RUBOT_CODE_EXEC_TIMEOUT.
  WeChat keys: RUBOT_WECHAT_BOT_TOKEN, RUBOT_WECHAT_BASE_URL.
",
        version = env!("CARGO_PKG_VERSION")
    );
}

fn run_repl(agent: Arc<Mutex<agent::Agent>>) -> anyhow::Result<()> {
    use rustyline::{error::ReadlineError, Config as RLConfig, Editor};
    let rt = tokio::runtime::Handle::current();
    let history_path =
        rt.block_on(async { repl_history_path(&agent.lock().await.config().workspace_path) });
    let mut rl: Editor<RubotHelper, FileHistory> =
        Editor::with_config(RLConfig::builder().auto_add_history(false).build())?;
    rl.set_helper(Some(RubotHelper::new()));
    let _ = rl.load_history(&history_path);

    let mut loop_mode = false;
    let mut last_input = String::new();
    let mut stop_condition = String::new();

    loop {
        let (line, synthetic_input) = if loop_mode && !last_input.is_empty() {
            (
                Ok(format!(
                    "Continue. STOP: {}. End with 'TASK COMPLETE'.",
                    stop_condition
                )),
                true,
            )
        } else {
            (rl.readline("> "), false)
        };

        match line {
            Ok(line) => {
                let input = line.trim();
                if input.is_empty() {
                    continue;
                }
                if input.starts_with('/') {
                    let parts: Vec<&str> = input.split_whitespace().collect();
                    match parts[0] {
                        "/quit" | "/exit" => {
                            rt.block_on(async { agent.lock().await.shutdown().await });
                            let _ = rl.save_history(&history_path);
                            println!("bye.");
                            return Ok(());
                        }
                        "/loop" => {
                            loop_mode = !loop_mode;
                            if loop_mode {
                                let full = parts[1..].join(" ");
                                if let Some((t, s)) = full.split_once('|') {
                                    last_input = t.trim().into();
                                    stop_condition = s.trim().into();
                                } else {
                                    last_input = full;
                                    stop_condition = "done".into();
                                }
                                println!(
                                    "{}[Loop ON | stop: {}]{}",
                                    markdown::YELLOW,
                                    stop_condition,
                                    markdown::R
                                );
                            } else {
                                println!("{}[Loop OFF]{}", markdown::DIM, markdown::R);
                            }
                            continue;
                        }
                        "/clear" => {
                            match rt
                                .block_on(async { agent.lock().await.clear_conversation().await })
                            {
                                Ok(()) => {}
                                Err(e) => eprintln!(
                                    "{}error:{} failed to clear conversation: {:#}",
                                    markdown::RED,
                                    markdown::R,
                                    e
                                ),
                            }
                            print!("\x1b[2J\x1b[H");
                            continue;
                        }
                        "/plan" => {
                            let p = rt.block_on(async {
                                agent.lock().await.last_plan().map(String::from)
                            });
                            match p {
                                Some(p) => println!("\n{}\n", markdown::render(&p)),
                                None => println!("(no plan yet)"),
                            }
                            continue;
                        }
                        "/memory" => {
                            let sub = parts.get(1).copied().unwrap_or("");
                            let arg = parts.get(2..).map(|s| s.join(" ")).unwrap_or_default();
                            match sub {
                                "" => {
                                    if let Ok(t) = rt.block_on(async { agent.lock().await.memory().get_index_text().await }) {
                                        println!("\n{}\n", markdown::render(&t));
                                    }
                                }
                                "search" if !arg.is_empty() => {
                                    let hits = rt.block_on(async { agent.lock().await.memory().quick_search(&arg).await }).unwrap_or_default();
                                    if hits.is_empty() {
                                        println!("(no matches)");
                                    } else {
                                        println!("\n# Search: {}\n", arg);
                                        for e in &hits {
                                            let tg = if e.tags.is_empty() { String::new() } else { format!(" [{}]", e.tags.join(", ")) };
                                            println!("- `{}` — {}{}", e.file, e.summary, tg);
                                        }
                                        println!();
                                    }
                                }
                                "delete" | "rm" if !arg.is_empty() => {
                                    match rt.block_on(async { agent.lock().await.memory().delete_entry(&arg).await }) {
                                        Ok(true) => println!("deleted {}", arg),
                                        Ok(false) => eprintln!("not found: {}", arg),
                                        Err(e) => eprintln!("{}error:{} {:#}", markdown::RED, markdown::R, e),
                                    }
                                }
                                "clear" => {
                                    match rt.block_on(async { agent.lock().await.memory().clear_all().await }) {
                                        Ok(n) => println!("cleared {} memories", n),
                                        Err(e) => eprintln!("{}error:{} {:#}", markdown::RED, markdown::R, e),
                                    }
                                }
                                "show" if !arg.is_empty() => {
                                    match rt.block_on(async { agent.lock().await.memory().get_entry(&arg).await }) {
                                        Ok(Some(t)) => println!("\n{}\n", markdown::render(&t)),
                                        Ok(None) => eprintln!("not found: {}", arg),
                                        Err(e) => eprintln!("{}error:{} {:#}", markdown::RED, markdown::R, e),
                                    }
                                }
                                "due" => {
                                    match rt.block_on(async { agent.lock().await.memory().due().await }) {
                                        Ok(hits) if hits.is_empty() => println!("(nothing due)"),
                                        Ok(hits) => {
                                            let mut body = String::from("# Due for review\n\n");
                                            for e in &hits {
                                                let t = if e.tags.is_empty() { String::new() } else { format!(" [{}]", e.tags.join(", ")) };
                                                body.push_str(&format!("- `{}` — {} (s{}){}\n", e.file, e.summary, e.strength, t));
                                            }
                                            println!("\n{}\n", markdown::render(&body));
                                        }
                                        Err(e) => eprintln!("{}error:{} {:#}", markdown::RED, markdown::R, e),
                                    }
                                }
                                "review" if !arg.is_empty() => {
                                    match rt.block_on(async { agent.lock().await.memory().touch(&arg).await }) {
                                        Ok(true) => println!("reviewed {}", arg),
                                        Ok(false) => eprintln!("not found: {}", arg),
                                        Err(e) => eprintln!("{}error:{} {:#}", markdown::RED, markdown::R, e),
                                    }
                                }
                                "decay" => {
                                    match rt.block_on(async { agent.lock().await.memory().decay().await }) {
                                        Ok(r) => println!("promoted {}, evicted {}", r.promoted, r.evicted),
                                        Err(e) => eprintln!("{}error:{} {:#}", markdown::RED, markdown::R, e),
                                    }
                                }
                                "help" => println!("usage:\n  /memory              list index\n  /memory show <id>    show entry (auto-touch)\n  /memory <id>         shorthand for show\n  /memory search <q>   keyword search\n  /memory due          list entries past review window\n  /memory review <id>  touch entry (strength+=1)\n  /memory decay        sweep: promote / evict stale\n  /memory delete <id>  delete entry\n  /memory clear        wipe all"),
                                id => {
                                    match rt.block_on(async { agent.lock().await.memory().get_entry(id).await }) {
                                        Ok(Some(t)) => println!("\n{}\n", markdown::render(&t)),
                                        Ok(None) => eprintln!("not found: {}", id),
                                        Err(e) => eprintln!("{}error:{} {:#}", markdown::RED, markdown::R, e),
                                    }
                                }
                            }
                            continue;
                        }
                        "/model" => {
                            if parts.len() > 1 {
                                match rt.block_on(async {
                                    agent.lock().await.set_model(parts[1]).await
                                }) {
                                    Ok(()) => {
                                        println!("model set to {}", parts[1]);
                                    }
                                    Err(e) => {
                                        eprintln!("{}error:{} {:#}", markdown::RED, markdown::R, e)
                                    }
                                }
                            } else {
                                let (h, f) = rt.block_on(async { agent.lock().await.get_models() });
                                println!("heavy={} fast={}", h, f);
                            }
                            continue;
                        }
                        "/config" => {
                            let sub = parts.get(1).copied().unwrap_or("list");
                            match sub {
                                "" | "list" => {
                                    let env_path = config::env_file_path()?;
                                    let rows =
                                        rt.block_on(async { agent.lock().await.config().rows() });
                                    println!("\n.env: {}\n", env_path.display());
                                    for row in rows {
                                        println!(
                                            "{:<18} {:<24} {}",
                                            row.key.cli_name(),
                                            row.env_name,
                                            row.display_value
                                        );
                                    }
                                    println!();
                                }
                                "get" => {
                                    let Some(raw_key) = parts.get(2) else {
                                        eprintln!("usage: /config get <key>");
                                        continue;
                                    };
                                    let Some(key) = config::ConfigKey::parse(raw_key) else {
                                        eprintln!("unknown config key: {}", raw_key);
                                        continue;
                                    };
                                    let row = rt.block_on(async {
                                        agent
                                            .lock()
                                            .await
                                            .config()
                                            .rows()
                                            .into_iter()
                                            .find(|row| row.key == key)
                                    });
                                    if let Some(row) = row {
                                        println!(
                                            "{} ({}) = {}",
                                            row.key.cli_name(),
                                            row.env_name,
                                            row.display_value
                                        );
                                    }
                                }
                                "set" => {
                                    let Some(raw_key) = parts.get(2) else {
                                        eprintln!("usage: /config set <key> <value>");
                                        continue;
                                    };
                                    let Some(key) = config::ConfigKey::parse(raw_key) else {
                                        eprintln!("unknown config key: {}", raw_key);
                                        continue;
                                    };
                                    let value =
                                        parts.get(3..).map(|s| s.join(" ")).unwrap_or_default();
                                    if value.trim().is_empty() {
                                        eprintln!("usage: /config set <key> <value>");
                                        continue;
                                    }

                                    match config::save_config_value(key, &value) {
                                        Ok(env_path) => {
                                            match Config::load() {
                                                Ok(new_config) => {
                                                    match rt.block_on(async {
                                                        agent.lock().await.apply_config(new_config).await
                                                    }) {
                                                        Ok(reset) => {
                                                            let display = if key == config::ConfigKey::ApiKey {
                                                                "********".to_string()
                                                            } else {
                                                                value.trim().to_string()
                                                            };
                                                            println!(
                                                                "saved {}={} to {}",
                                                                key.cli_name(),
                                                                display,
                                                                env_path.display()
                                                            );
                                                            if reset {
                                                                println!("workspace changed; session conversation was reset");
                                                            } else {
                                                                println!("applied to current session");
                                                            }
                                                        }
                                                        Err(e) => eprintln!(
                                                            "{}error:{} failed to apply config: {:#}",
                                                            markdown::RED,
                                                            markdown::R,
                                                            e
                                                        ),
                                                    }
                                                }
                                                Err(e) => eprintln!(
                                                    "{}error:{} failed to reload config: {:#}",
                                                    markdown::RED,
                                                    markdown::R,
                                                    e
                                                ),
                                            }
                                        }
                                        Err(e) => eprintln!(
                                            "{}error:{} failed to save config: {:#}",
                                            markdown::RED,
                                            markdown::R,
                                            e
                                        ),
                                    }
                                }
                                "help" => {
                                    println!(
                                        "usage:\n  /config                     list effective config\n  /config get <key>           show one config value\n  /config set <key> <value>   save to .env and apply\n\nkeys: api_base_url, api_key, model, fast_model, tavily_api_key, workspace, max_retries, code_exec_timeout, sleep_interval, orkey"
                                    );
                                }
                                _ => {
                                    eprintln!("usage: /config [list|get|set|help] ...");
                                }
                            }
                            continue;
                        }
                        "/wechat" => {
                            let sub = parts.get(1).copied().unwrap_or("help");
                            match sub {
                                "setup" => {
                                    println!();
                                    println!(
                                        "┌─ 微信 iLink Bot 配置 ────────────────────────────┐"
                                    );
                                    println!(
                                        "│                                                  │"
                                    );
                                    println!(
                                        "│  即将生成二维码，请用微信扫码登录                │"
                                    );
                                    println!(
                                        "│  首次使用会创建微信机器人                         │"
                                    );
                                    println!(
                                        "│                                                  │"
                                    );
                                    println!(
                                        "│  Ctrl+C 可随时取消                                │"
                                    );
                                    println!(
                                        "│                                                  │"
                                    );
                                    println!(
                                        "└──────────────────────────────────────────────────┘"
                                    );
                                    println!();

                                    // Perform QR login
                                    let login_result = match rt
                                        .block_on(async { crate::channel::qr_login().await })
                                    {
                                        Ok(r) => r,
                                        Err(e) => {
                                            eprintln!("✗ Login failed: {:#}", e);
                                            continue;
                                        }
                                    };

                                    // Save to .env
                                    if let Err(e) = config::save_config_value(
                                        config::ConfigKey::WeChatBotToken,
                                        &login_result.bot_token,
                                    ) {
                                        eprintln!("failed to save bot_token: {:#}", e);
                                        continue;
                                    }
                                    if let Err(e) = config::save_config_value(
                                        config::ConfigKey::WeChatBaseUrl,
                                        &login_result.base_url,
                                    ) {
                                        eprintln!("failed to save base_url: {:#}", e);
                                        continue;
                                    }

                                    // Reload config and start channel immediately
                                    match Config::load() {
                                        Ok(new_config) => {
                                            let _ = rt.block_on(async {
                                                agent
                                                    .lock()
                                                    .await
                                                    .apply_config(new_config.clone())
                                                    .await
                                            });
                                            let ch_agent = agent.clone();
                                            tokio::spawn(async move {
                                                if let Err(e) =
                                                    channel::start(ch_agent, new_config).await
                                                {
                                                    tracing::error!("wechat bot exited: {:#}", e);
                                                }
                                            });
                                            println!("✓ WeChat Bot configured and started!");
                                        }
                                        Err(e) => {
                                            eprintln!("warning: failed to reload config: {:#}", e);
                                            println!("Login saved. Restart rubot to start the WeChat channel.");
                                        }
                                    }
                                    println!();
                                }
                                "status" => {
                                    let token = rt.block_on(async {
                                        agent.lock().await.config().wechat_bot_token.clone()
                                    });
                                    let base_url = rt.block_on(async {
                                        agent.lock().await.config().wechat_base_url.clone()
                                    });
                                    if token.is_empty() {
                                        println!("WeChat Bot: not configured");
                                        println!("  Run /wechat setup to scan QR code.");
                                    } else {
                                        println!("WeChat Bot: configured");
                                        println!(
                                            "  Token: {}...{}",
                                            &token[..8],
                                            &token[token.len().saturating_sub(4)..]
                                        );
                                        println!("  Base: {}", base_url);
                                        println!("  Status: active (if rubot was started with valid token)");
                                    }
                                }
                                _ => {
                                    println!("usage:");
                                    println!("  /wechat setup    Scan QR code to log in");
                                    println!("  /wechat status   Show current configuration");
                                }
                            }
                            continue;
                        }
                        "/usage" => {
                            let detail = rt.block_on(async {
                                agent.lock().await.usage_detail()
                            });
                            println!("\n{}\n", markdown::render(&detail));
                            continue;
                        }
                        _ => {
                            eprintln!("unknown command: {}", parts[0]);
                            continue;
                        }
                    }
                }

                let actual = if loop_mode && !last_input.is_empty() {
                    &last_input
                } else {
                    input
                };
                if !synthetic_input && should_store_repl_history(input) {
                    let _ = rl.add_history_entry(input);
                }
                let result = rt.block_on(async { agent.lock().await.process(actual).await });

                match result {
                    Ok(res) => {
                        println!("{}\n", markdown::render(&res));
                        let hud = rt.block_on(async {
                            agent.lock().await.usage_summary()
                        });
                        println!("{}\n", hud);
                        if loop_mode {
                            if res.contains("TASK COMPLETE") || res.contains(&stop_condition) {
                                loop_mode = false;
                                println!("{}[Loop ended]{}", markdown::DIM, markdown::R);
                            } else {
                                last_input = format!(
                                    "Continue. STOP: {}. End with 'TASK COMPLETE'.",
                                    stop_condition
                                );
                            }
                        }
                    }
                    Err(e) => eprintln!("{}error:{} {:#}", markdown::RED, markdown::R, e),
                }
            }
            Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => {
                rt.block_on(async { agent.lock().await.shutdown().await });
                let _ = rl.save_history(&history_path);
                println!("bye.");
                return Ok(());
            }
            Err(e) => {
                let _ = rl.save_history(&history_path);
                eprintln!("readline error: {}", e);
                return Err(e.into());
            }
        }
    }
}

fn repl_history_path(workspace: &Path) -> PathBuf {
    workspace.join(".rubot_repl_history")
}

fn should_store_repl_history(input: &str) -> bool {
    let trimmed = input.trim();
    !trimmed.is_empty() && !trimmed.starts_with('/')
}

#[cfg(test)]
mod tests {
    use super::{
        parse_startup_action, repl_history_path, should_store_repl_history, StartupAction,
    };
    use std::path::Path;

    #[test]
    fn parse_startup_action_defaults_to_repl() {
        assert_eq!(
            parse_startup_action(Vec::<String>::new()).unwrap(),
            StartupAction::Repl
        );
    }

    #[test]
    fn parse_startup_action_supports_help_and_version() {
        assert_eq!(
            parse_startup_action(vec!["--help".into()]).unwrap(),
            StartupAction::Help
        );
        assert_eq!(
            parse_startup_action(vec!["--version".into()]).unwrap(),
            StartupAction::Version
        );
    }

    #[test]
    fn parse_startup_action_rejects_unknown_flags() {
        assert!(parse_startup_action(vec!["--bogus".into()]).is_err());
    }

    #[test]
    fn repl_history_is_workspace_scoped() {
        let path = repl_history_path(Path::new("/tmp/rubot-workspace"));
        assert_eq!(path, Path::new("/tmp/rubot-workspace/.rubot_repl_history"));
    }

    #[test]
    fn repl_history_skips_commands() {
        assert!(should_store_repl_history("帮我分析这个项目"));
        assert!(!should_store_repl_history("/model"));
        assert!(!should_store_repl_history("   "));
    }
}
