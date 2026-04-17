#![allow(dead_code)]

mod agent;
mod config;
mod context;
mod llm;
mod memory;
mod personality;
mod planner;
mod reflector;
mod state;
mod telegram;
mod tools;
mod ui;
mod workspace;

use config::Config;
use rustyline::error::ReadlineError;
use rustyline::history::FileHistory;
use rustyline::completion::{Completer, Pair};
use rustyline::hint::Hinter;
use rustyline::highlight::Highlighter;
use rustyline::validate::Validator;
use rustyline::Helper;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
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
            let matches: Vec<Pair> = self.commands
                .iter()
                .filter(|cmd| cmd.starts_with(line))
                .map(|cmd| Pair {
                    display: cmd.clone(),
                    replacement: cmd.clone(),
                })
                .collect();
            return Ok((0, matches));
        }
        Ok((0, Vec::new()))
    }
}

impl Hinter for RubotHelper {
    type Hint = String;
    fn hint(&self, line: &str, pos: usize, _ctx: &rustyline::Context<'_>) -> Option<String> {
        if line.starts_with('/') && line.len() > 1 {
            self.commands
                .iter()
                .find(|cmd| cmd.starts_with(line))
                .map(|cmd| {
                    if cmd.len() > pos {
                        cmd[pos..].to_string()
                    } else {
                        "".to_string()
                    }
                })
        } else {
            None
        }
    }
}

impl Highlighter for RubotHelper {
    fn highlight_hint<'h>(&self, hint: &'h str) -> std::borrow::Cow<'h, str> {
        std::borrow::Cow::Owned(format!("\x1b[2m{}\x1b[0m", hint))
    }
}

impl Validator for RubotHelper {}

impl RubotHelper {
    fn new() -> Self {
        Self {
            commands: vec![
                "/quit".to_string(),
                "/exit".to_string(),
                "/plan".to_string(),
                "/memory".to_string(),
                "/errors".to_string(),
                "/model".to_string(),
                "/fast_model".to_string(),
                "/clear".to_string(),
            ],
        }
    }
}

#[derive(Debug, Clone)]
struct UIState {
    model: String,
    workspace: String,
    memory_count: usize,
    mood: ui::Mood,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("rubot=warn")))
        .with_target(false)
        .compact()
        .init();

    let config = Config::load()?;
    config.ensure_workspace_dirs()?;

    let agent_raw = agent::Agent::new(config.clone()).await?;
    let (m1, _) = agent_raw.get_models();
    let initial_mem = agent_raw.memory().index().get_entries().await.unwrap_or_default().len();
    let ws_path = config.workspace_path.display().to_string();
    
    let ui_state = Arc::new(Mutex::new(UIState {
        model: m1.clone(),
        workspace: ws_path.clone(),
        memory_count: initial_mem,
        mood: ui::Mood::Idle,
    }));

    let agent = Arc::new(Mutex::new(agent_raw));
    let _tg_handle = telegram::start_bot(&config, agent.clone()).await?;

    ui::enter_alt_screen();
    ui::init_scrolling_region();
    ui::draw_header(&ui::Mood::Idle);
    ui::help_hint();

    let stop_flag = Arc::new(AtomicBool::new(false));
    let status_ui_state = ui_state.clone();
    let status_stop_flag = stop_flag.clone();
    
    tokio::spawn(async move {
        let mut last_size = (0, 0);
        while !status_stop_flag.load(Ordering::Relaxed) {
            let current_size = ui::term_size();
            if current_size != last_size {
                ui::init_scrolling_region();
                last_size = current_size;
            }

            let state = {
                if let Ok(s) = status_ui_state.try_lock() { s.clone() } 
                else { tokio::time::sleep(Duration::from_millis(50)).await; continue; }
            };
            ui::draw_header(&state.mood);
            ui::status_bar(&state.model, &state.workspace, state.memory_count, &state.mood);
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    });


    let repl_agent = agent.clone();
    let repl_ui_state = ui_state.clone();
    let repl_handle = tokio::task::spawn_blocking(move || run_repl(repl_agent, repl_ui_state));

    let _ = repl_handle.await?;
    
    stop_flag.store(true, Ordering::Relaxed);
    ui::reset_scrolling_region();
    ui::exit_alt_screen();

    Ok(())
}

fn run_repl(agent: Arc<Mutex<agent::Agent>>, ui_state: Arc<Mutex<UIState>>) -> anyhow::Result<()> {
    use rustyline::Config as RLConfig;
    use rustyline::Editor;
    use crate::ui::Mood;
    
    let mut rl: Editor<RubotHelper, FileHistory> = 
        Editor::with_config(RLConfig::builder().auto_add_history(true).build())?;
    rl.set_helper(Some(RubotHelper::new()));
    
    let history_path = ".rubot_history";
    let _ = rl.load_history(history_path);

    let rt = tokio::runtime::Handle::current();

    loop {
        match rl.readline(ui::prompt()) {
            Ok(line) => {
                let input = line.trim();
                
                {
                    let mut s = rt.block_on(ui_state.lock());
                    let ag = rt.block_on(agent.lock());
                    let (m1, _) = ag.get_models();
                    s.model = m1;
                    s.memory_count = rt.block_on(ag.memory().index().get_entries()).unwrap_or_default().len();
                }

                if input.is_empty() {
                    rt.block_on(ui_state.lock()).mood = Mood::Idle;
                    continue;
                }

                if input.starts_with('/') {
                    let parts: Vec<&str> = input.split_whitespace().collect();
                    let command = parts[0];

                    match command {
                        "/quit" | "/exit" => {
                            let mut ag = rt.block_on(agent.lock());
                            rt.block_on(ag.shutdown());
                            ui::goodbye();
                            return Ok(());
                        }
                        "/clear" => {
                            ui::clear_terminal();
                            ui::init_scrolling_region();
                            ui::draw_header(&Mood::Idle);
                            rt.block_on(ui_state.lock()).mood = Mood::Idle;
                            continue;
                        }
                        "/plan" => {
                            let ag = rt.block_on(agent.lock());
                            let plan = rt.block_on(ag.state().load_plan());
                            if let Ok(Some(p)) = plan { ui::command_output("Plan", &p); }
                            else { ui::status("No active plan"); }
                            continue;
                        }
                        "/memory" => {
                            let ag = rt.block_on(agent.lock());
                            let index = rt.block_on(ag.memory().get_index_for_context());
                            if let Ok(text) = index { ui::command_output("Memory Index", &text); }
                            continue;
                        }
                        "/errors" => {
                            let ag = rt.block_on(agent.lock());
                            ui::command_output("Error Book", &ag.error_book().to_text());
                            continue;
                        }
                        "/model" => {
                            if parts.len() > 1 {
                                let new_model = parts[1];
                                let mut ag = rt.block_on(agent.lock());
                                ag.set_model(new_model);
                                rt.block_on(ui_state.lock()).model = new_model.to_string();
                            }
                            continue;
                        }
                        _ => { if command.starts_with('/') { ui::print_error("Unknown command"); continue; } }
                    }
                }

                rt.block_on(ui_state.lock()).mood = Mood::Thinking;

                let mut ag = rt.block_on(agent.lock());
                match rt.block_on(ag.process(input)) {
                    Ok(response) => {
                        rt.block_on(ui_state.lock()).mood = Mood::Happy;
                        ui::print_response(&response);
                    }
                    Err(e) => {
                        rt.block_on(ui_state.lock()).mood = Mood::Error;
                        ui::print_error(&format!("{:#}", e));
                    }
                }
            }
            Err(ReadlineError::Interrupted) => { ui::status("Ctrl-C — type /quit to exit"); continue; }
            Err(ReadlineError::Eof) => {
                let mut ag = rt.block_on(agent.lock());
                rt.block_on(ag.shutdown());
                ui::goodbye();
                return Ok(());
            }
            Err(e) => { ui::print_error(&format!("Input error: {}", e)); return Err(e.into()); }
        }
    }
}
