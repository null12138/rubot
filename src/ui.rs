use console::style;
use std::time::{SystemTime, UNIX_EPOCH};
use chrono::Local;

pub const HEADER_HEIGHT: u16 = 7;

pub fn enter_alt_screen() { print!("\x1b[?1049h\x1b[H"); let _ = std::io::Write::flush(&mut std::io::stdout()); }
pub fn exit_alt_screen() { print!("\x1b[?1049l"); let _ = std::io::Write::flush(&mut std::io::stdout()); }
pub fn clear_terminal() { print!("\x1b[2J\x1b[H"); let _ = std::io::Write::flush(&mut std::io::stdout()); }

pub fn term_size() -> (u16, u16) { console::Term::stdout().size() }

pub fn init_scrolling_region() {
    let (rows, _) = term_size();
    if rows > (HEADER_HEIGHT + 3) {
        print!("\x1b[2J");
        print!("\x1b[{};{}r", HEADER_HEIGHT + 1, rows - 1);
        print!("\x1b[{};1H", HEADER_HEIGHT + 1);
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }
}

pub fn reset_scrolling_region() {
    print!("\x1b[r");
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mood { Idle, Thinking, Happy, Error }

pub struct Pet;
impl Pet {
    fn get_tick() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
    }
    pub fn get_animated_face(mood: &Mood) -> String {
        let t = Self::get_tick();
        match mood {
            Mood::Thinking => {
                let frames = ["( •_•)", "( o_o)", "( •_•)", "( ._.)"];
                frames[(t as usize / 200) % frames.len()].to_string()
            }
            Mood::Happy => "( ^_^)b".to_string(),
            Mood::Error => "( >_<)!".to_string(),
            Mood::Idle => {
                let blink = (t / 4000 % 15) == 0;
                if blink { "( -_-)".to_string() } else { "( •_•)".to_string() }
            }
        }
    }
}

pub fn draw_header(_mood: &Mood) {
    let (rows, cols) = term_size();
    if rows < HEADER_HEIGHT { return; }
    let w = cols as usize;
    let art = [
        "____        __          __",
        "/ __ \\__  __/ /_  ____  / /_",
        "/ /_/ / / / / __ \\/ __ \\/ __/",
        "/ _, _/ /_/ / /_/ / /_/ / /_",
        "/_/ |_|\\__,_/_.___/\\____/\\__/"
    ];
    let mut out = String::new();
    out.push_str("\x1b[s\x1b[1;1H"); 
    for i in 1..=HEADER_HEIGHT { out.push_str(&format!("\x1b[{};1H\x1b[K", i)); }
    
    out.push_str("\x1b[2;1H");
    for line in art {
        let pad = w.saturating_sub(line.chars().count()) / 2;
        out.push_str(&format!("{}{}\n", " ".repeat(pad), style(line).cyan().bold()));
    }
    
    let tagline = "Atomic autonomous agent with hierarchical memory";
    let tag_pad = w.saturating_sub(tagline.chars().count()) / 2;
    out.push_str(&format!("{}{}\n", " ".repeat(tag_pad), style(tagline).dim().italic()));
    
    out.push_str("\x1b[u"); 
    print!("{}", out);
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

pub fn status_bar(model: &str, workspace: &str, memory_count: usize, mood: &Mood) {
    let (rows, cols) = term_size();
    if rows < 2 { return; }
    let w = cols as usize;
    let time_str = Local::now().format("%H:%M:%S").to_string();
    let face = Pet::get_animated_face(mood);
    let ws_display = if workspace.len() > 20 { format!("...{}", &workspace[workspace.len().saturating_sub(17)..]) } else { workspace.to_string() };
    
    let pet_part = format!(" {} ", style(&face).yellow());
    let model_part = format!(" {} ", style(model).bold());
    let mem_part = format!(" [MEM: {}] ", style(memory_count).cyan());
    let ws_part = format!(" {} ", style(&ws_display).dim());
    let time_part = format!(" {} ", style(&time_str).white());

    let left_w = face.chars().count() + model.chars().count() + 4;
    let mid_w = memory_count.to_string().len() + 9;
    let right_w = ws_display.chars().count() + time_str.chars().count() + 3;
    
    let pad_total = w.saturating_sub(left_w + mid_w + right_w + 1);
    let pad_side = pad_total / 2;

    let theme_bg = "\x1b[48;5;236m";
    let reset = "\x1b[0m";

    // Fixed mapping: 9 placeholders, 9 arguments.
    print!(
        "\x1b[s\x1b[{};1H\x1b[K{}{}{}{}{}{}{}{}{}\x1b[u",
        rows, theme_bg,
        " ".repeat(pad_side),
        pet_part, model_part, mem_part, ws_part, time_part,
        " ".repeat(pad_total - pad_side),
        reset
    );
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

pub fn prompt() -> &'static str { "\x1b[1;36m›\x1b[0m " }
pub fn print_response(text: &str) {
    println!();
    termimad::print_text(text);
    println!();
}
pub fn print_error(err: &str) { println!("\n  {} {}\n", style("×").red().bold(), style(err).red()); }
pub fn status(msg: &str) { println!("  {} {}", style("•").dim(), style(msg).dim()); }
pub fn command_output(title: &str, content: &str) { 
    println!("\n{} {}\n", style("┌").dim(), style(title).bold());
    termimad::print_text(content);
    println!();
}
pub fn goodbye() { println!("\n  {}\n", style("Bye.").dim()); }
pub fn help_hint() {
    print!("  ");
    for (cmd, desc) in [("/quit", "exit"), ("/plan", "plan"), ("/memory", "mem"), ("/errors", "err")] {
        print!("{} {}  ", style(cmd).cyan(), style(desc).dim());
    }
    println!("\n");
}
pub fn tool_call_start(name: &str, params: &str) { println!("  {} {} {}", style("○").yellow(), style(name).white(), style(truncate(params, 50)).dim()); }
pub fn tool_call_result(ok: bool, out: &str) {
    let icon = if ok { style("●").green() } else { style("●").red() };
    println!("    {} {}", icon, style(truncate(out, 70)).dim());
}
pub fn llm_round(r: u32, m: &str) { println!("  {} {} {}", style("◎").dim(), style(format!("round {}", r)).dim(), style(m).dim()); }
pub fn plan_step(id: usize, d: &str, s: &str) {
    let icon = match s { "OK" => style("✓").green(), "FAILED" => style("×").red(), _ => style("→").dim() };
    println!("  {} Step {}: {}", icon, id, d);
}
fn truncate(s: &str, max: usize) -> String {
    let l = s.lines().next().unwrap_or("");
    if l.chars().count() > max { format!("{}…", l.chars().take(max).collect::<String>()) } else { l.to_string() }
}
