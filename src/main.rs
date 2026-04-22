mod agent;
mod config;
mod llm;
mod markdown;
mod memory;
mod personality;
mod planner;
mod tools;

use config::Config;
use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::FileHistory;
use rustyline::validate::Validator;
use rustyline::Helper;
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
            ]
            .into_iter()
            .map(Into::into)
            .collect(),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("rubot=warn")),
        )
        .with_target(false)
        .compact()
        .init();

    let config = Config::load()?;
    config.ensure_workspace_dirs()?;
    let agent = Arc::new(Mutex::new(agent::Agent::new(config).await?));

    println!(
        "{}rubot{} {}— /quit to exit · /loop <task>|<stop> to auto-loop{}",
        markdown::BOLD,
        markdown::R,
        markdown::DIM,
        markdown::R
    );
    let a = agent.clone();
    tokio::task::spawn_blocking(move || run_repl(a)).await??;
    Ok(())
}

fn run_repl(agent: Arc<Mutex<agent::Agent>>) -> anyhow::Result<()> {
    use rustyline::{error::ReadlineError, Config as RLConfig, Editor};
    let mut rl: Editor<RubotHelper, FileHistory> =
        Editor::with_config(RLConfig::builder().auto_add_history(true).build())?;
    rl.set_helper(Some(RubotHelper::new()));
    let _ = rl.load_history(".rubot_history");
    let rt = tokio::runtime::Handle::current();

    let mut loop_mode = false;
    let mut last_input = String::new();
    let mut stop_condition = String::new();

    loop {
        let line = if loop_mode && !last_input.is_empty() {
            Ok(format!(
                "Continue. STOP: {}. End with 'TASK COMPLETE'.",
                stop_condition
            ))
        } else {
            rl.readline("\x1b[1;36mrubot\x1b[0m \x1b[2m›\x1b[0m ")
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
                                rt.block_on(async { agent.lock().await.set_model(parts[1]) });
                                println!("model set to {}", parts[1]);
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
                                        "usage:\n  /config                     list effective config\n  /config get <key>           show one config value\n  /config set <key> <value>   save to .env and apply\n\nkeys: api_base_url, api_key, model, fast_model, workspace, max_retries, code_exec_timeout"
                                    );
                                }
                                _ => {
                                    eprintln!("usage: /config [list|get|set|help] ...");
                                }
                            }
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
                let result = rt.block_on(async { agent.lock().await.process(actual).await });

                match result {
                    Ok(res) => {
                        println!("\n{}\n", markdown::render(&res));
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
                println!("bye.");
                return Ok(());
            }
            Err(e) => {
                eprintln!("readline error: {}", e);
                return Err(e.into());
            }
        }
    }
}
