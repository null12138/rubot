#![allow(dead_code)]

mod agent;
mod config;
mod context { pub mod cleaner; }
mod llm;
mod memory;
mod personality;
mod planner;
mod reflector;
mod state { pub mod manager; }
mod telegram;
mod tools;
mod ui;
mod workspace { pub mod git; }

use config::Config;
use rustyline::history::FileHistory;
use rustyline::completion::{Completer, Pair};
use rustyline::hint::Hinter;
use rustyline::highlight::Highlighter;
use rustyline::validate::Validator;
use rustyline::Helper;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

struct RubotHelper { commands: Vec<String> }
impl Helper for RubotHelper {}
impl Completer for RubotHelper {
    type Candidate = Pair;
    fn complete(&self, line: &str, _pos: usize, _ctx: &rustyline::Context<'_>) -> rustyline::Result<(usize, Vec<Pair>)> {
        if line.starts_with('/') {
            let m: Vec<_> = self.commands.iter().filter(|cmd| cmd.starts_with(line)).map(|cmd| Pair { display: cmd.clone(), replacement: cmd.clone() }).collect();
            return Ok((0, m));
        }
        Ok((0, Vec::new()))
    }
}
impl Hinter for RubotHelper {
    type Hint = String;
    fn hint(&self, line: &str, pos: usize, _ctx: &rustyline::Context<'_>) -> Option<String> {
        if line.starts_with('/') && line.len() > 1 {
            self.commands.iter().find(|cmd| cmd.starts_with(line)).map(|cmd| if cmd.len() > pos { cmd[pos..].to_string() } else { "".to_string() })
        } else { None }
    }
}
impl Highlighter for RubotHelper {
    fn highlight_hint<'h>(&self, h: &'h str) -> std::borrow::Cow<'h, str> { std::borrow::Cow::Owned(format!("\x1b[2m{}\x1b[0m", h)) }
}
impl Validator for RubotHelper {}
impl RubotHelper {
    fn new() -> Self { Self { commands: vec!["/quit","/exit","/plan","/memory","/errors","/model","/fast_model","/clear","/loop","/scroll"].into_iter().map(Into::into).collect() } }
}

#[derive(Clone)]
struct UIState { mood: ui::Mood, model: String, mem: usize }

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("rubot=info,teloxide=warn"))).with_target(false).compact().init();
    let config = Config::load()?;
    config.ensure_workspace_dirs()?;
    let agent_raw = agent::Agent::new(config.clone()).await?;
    let (m1, _) = agent_raw.get_models();
    let initial_mem = agent_raw.memory().get_index().await.unwrap_or_default().len();

    let ui_state = Arc::new(std::sync::Mutex::new(UIState { mood: ui::Mood::Idle, model: m1, mem: initial_mem }));
    let agent = Arc::new(Mutex::new(agent_raw));
    let _tg = telegram::start_bot(&config, agent.clone()).await?;

    ui::clear_terminal();
    ui::draw_header(&ui::Mood::Idle);
    ui::help_hint();

    let stop = Arc::new(AtomicBool::new(false));

    let r_agent = agent.clone();
    let r_ui = ui_state;
    let _ = tokio::task::spawn_blocking(move || run_repl(r_agent, r_ui)).await?;
    stop.store(true, Ordering::Relaxed);
    Ok(())
}

fn run_repl(agent: Arc<Mutex<agent::Agent>>, ui_state: Arc<std::sync::Mutex<UIState>>) -> anyhow::Result<()> {
    use rustyline::{Config as RLConfig, Editor, error::ReadlineError};
    let mut rl: Editor<RubotHelper, FileHistory> = Editor::with_config(RLConfig::builder().auto_add_history(true).build())?;
    rl.set_helper(Some(RubotHelper::new()));
    let _ = rl.load_history(".rubot_history");
    let rt = tokio::runtime::Handle::current();

    let mut loop_mode = false;
    let mut last_input = String::new();
    let mut stop_condition = String::new();

    loop {
        {
            let ag = rt.block_on(agent.lock());
            let mut s = ui_state.lock().unwrap();
            let (m1, _) = ag.get_models();
            s.model = m1;
            s.mem = rt.block_on(async { ag.memory().get_index().await.unwrap_or_default().len() });
        }

        let current_mood = { ui_state.lock().unwrap().mood };
        let prompt_str = if loop_mode {
            format!("{} [LOOP] › ", ui::Pet::face(&current_mood))
        } else {
            ui::prompt(&current_mood, "", 0)
        };

        let line = if loop_mode && !last_input.is_empty() {
            Ok(format!("Continue. STOP: {}. End with 'TASK COMPLETE'.", stop_condition))
        } else {
            rl.readline(&prompt_str)
        };

        match line {
            Ok(line) => {
                let input = line.trim();
                if input.is_empty() { { ui_state.lock().unwrap().mood = ui::Mood::Idle; } continue; }
                if input.starts_with('/') {
                    let parts: Vec<&str> = input.split_whitespace().collect();
                    match parts[0] {
                        "/quit" | "/exit" => { rt.block_on(async { agent.lock().await.shutdown().await }); ui::goodbye(); return Ok(()); }
                        "/loop" => {
                            loop_mode = !loop_mode;
                            if loop_mode {
                                let full = parts[1..].join(" ");
                                if let Some((t, s)) = full.split_once('|') { last_input = t.trim().into(); stop_condition = s.trim().into(); }
                                else { last_input = full; stop_condition = "done".into(); }
                                ui::status(&format!("Loop ON | Stop: {}", stop_condition));
                            } else { ui::status("Loop OFF"); }
                            continue;
                        }
                        "/clear" => { ui::clear_terminal(); ui::draw_header(&ui::Mood::Idle); continue; }
                        "/plan" => { if let Ok(Some(p)) = rt.block_on(async { agent.lock().await.state().load_plan().await }) { ui::command_output("Plan", &p); } else { ui::status("No plan"); } continue; }
                        "/memory" => { if let Ok(t) = rt.block_on(async { agent.lock().await.memory().get_index_text().await }) { ui::command_output("Memory", &t); } continue; }
                        "/errors" => { let t = rt.block_on(async { agent.lock().await.error_book().to_text() }); ui::command_output("Errors", &t); continue; }
                        "/scroll" => { ui::scrollback_pager(); continue; }
                        "/model" => { if parts.len() > 1 { rt.block_on(async { agent.lock().await.set_model(parts[1]) }); } continue; }
                        _ => { if parts[0].starts_with('/') { ui::print_error("Unknown cmd"); continue; } }
                    }
                }

                let actual = if loop_mode && !last_input.is_empty() { &last_input } else { input };
                { ui_state.lock().unwrap().mood = ui::Mood::Thinking; }

                // Suppress stdin echo during processing to prevent [[A garbage from keypresses
                ui::suppress_input();
                let result = if loop_mode {
                    rt.block_on(async {
                        let mut ag = agent.lock().await;
                        ag.process(actual).await
                    })
                } else {
                    rt.block_on(async {
                        let mut ag = agent.lock().await;
                        ag.process_stream(actual, |token| crate::ui::stream_token(token)).await
                    })
                };
                ui::drain_stdin();
                ui::restore_input();

                match result {
                    Ok(res) => {
                        { ui_state.lock().unwrap().mood = ui::Mood::Happy; }
                        ui::print_response(&res);
                        if loop_mode {
                            if res.contains("TASK COMPLETE") || res.contains(&stop_condition) { loop_mode = false; ui::status("Loop ended."); { ui_state.lock().unwrap().mood = ui::Mood::Idle; } }
                            else { last_input = format!("Continue. STOP: {}. End with 'TASK COMPLETE'.", stop_condition); }
                        } else { { ui_state.lock().unwrap().mood = ui::Mood::Idle; } }
                    }
                    Err(e) => {
                        { ui_state.lock().unwrap().mood = ui::Mood::Error; }
                        ui::print_error(&format!("{:#}", e));
                    }
                }
            }
            Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => { rt.block_on(async { agent.lock().await.shutdown().await }); ui::goodbye(); return Ok(()); }
            Err(e) => { ui::print_error(&format!("Error: {}", e)); return Err(e.into()); }
        }
    }
}
