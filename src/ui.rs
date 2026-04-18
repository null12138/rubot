use console::style;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

// ── Output scrollback buffer ──────────────────────────────────────

static SCROLLBACK: Mutex<Vec<String>> = Mutex::new(Vec::new()); // lazy init is fine

pub fn scrollback_push(line: &str) {
    if let Ok(mut buf) = SCROLLBACK.lock() {
        buf.push(line.to_string());
        // Keep buffer bounded
        if buf.len() > 10000 { buf.drain(..1000); }
    }
}

pub fn scrollback_get() -> Vec<String> {
    SCROLLBACK.lock().map(|b| b.clone()).unwrap_or_default()
}

/// Enter scrollback pager. Uses raw terminal for j/k/q navigation.
pub fn scrollback_pager() {
    let lines = scrollback_get();
    if lines.is_empty() { return; }

    let (rows, cols) = term_size();
    let content_height = (rows as usize).saturating_sub(HEADER_LINES as usize + 1); // -header -statusbar
    if content_height == 0 { return; }

    // Put terminal in raw mode
    unsafe {
        let fd = libc::STDIN_FILENO;
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut t) != 0 { return; }
        t.c_lflag &= !(libc::ECHO | libc::ICANON);
        t.c_cc[libc::VMIN] = 1;
        t.c_cc[libc::VTIME] = 0;
        libc::tcsetattr(fd, libc::TCSANOW, &t);
    }

    let total = lines.len();
    let mut offset = total.saturating_sub(content_height); // start at bottom

    let render = |off: usize| {
        let top = HEADER_LINES as usize + 1;
        for i in 0..content_height {
            let line_idx = off + i;
            let text = if line_idx < total {
                let l = &lines[line_idx];
                let visible: String = l.chars().take(cols as usize - 2).collect();
                visible
            } else {
                String::new()
            };
            print!("\x1b[{};1H\x1b[K {}", top + i, style(&text).dim());
        }
        // Footer hint
        print!("\x1b[{};1H\x1b[48;5;238m\x1b[K ↑/↓ j/k scroll · q quit · line {}/{}\x1b[0m", rows, off + 1, total);
        use std::io::Write;
        let _ = std::io::stdout().flush();
    };

    render(offset);

    // Read keys
    use std::io::Read;
    let mut buf = [0u8; 8];
    loop {
        if std::io::stdin().read(&mut buf).unwrap_or(0) == 0 { break; }
        let s = std::string::String::from_utf8_lossy(&buf[..]);
        match s.as_ref() {
            "q" | "\x1b" => break,                         // q or Esc
            "j" | "\x1b[B" => {                            // j or Down
                if offset + content_height < total {
                    offset += 1;
                    render(offset);
                }
            }
            "k" | "\x1b[A" => {                            // k or Up
                if offset > 0 {
                    offset -= 1;
                    render(offset);
                }
            }
            "\x1b[6~" => {                                  // PageDown
                offset = (offset + content_height).min(total.saturating_sub(content_height));
                render(offset);
            }
            "\x1b[5~" => {                                  // PageUp
                offset = offset.saturating_sub(content_height);
                render(offset);
            }
            "G" => {                                        // G = bottom
                offset = total.saturating_sub(content_height);
                render(offset);
            }
            "g" => {                                        // g = top
                offset = 0;
                render(offset);
            }
            _ => {}
        }
        buf = [0u8; 8];
    }

    // Restore terminal
    unsafe {
        let fd = libc::STDIN_FILENO;
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut t) == 0 {
            t.c_lflag |= libc::ECHO | libc::ICANON;
            libc::tcsetattr(fd, libc::TCSANOW, &t);
        }
    }

    // Redraw normal view
    clear_terminal();
    draw_header(&Mood::Idle);
    // Replay recent lines
    let recent_start = total.saturating_sub(content_height);
    for line in &lines[recent_start..] {
        println!("{}", style(line).dim());
    }
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mood { Idle, Thinking, Happy, Error }

pub struct Pet;
impl Pet {
    pub fn face(mood: &Mood) -> String {
        let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        match mood {
            Mood::Thinking => ["( •_•)", "( o_o)", "( •_•)", "( ._.)"][(t as usize / 200) % 4].to_string(),
            Mood::Happy => "( ^_^)b".to_string(),
            Mood::Error => "( >_<)!".to_string(),
            Mood::Idle => if (t / 4000 % 15) == 0 { "( -_-)".into() } else { "( •_•)".into() }
        }
    }
}

/// Header occupies this many lines: 5 ASCII art + 1 subtitle + 1 blank.
const HEADER_LINES: u16 = 7;

pub fn clear_terminal() {
    // Clear screen and move cursor home, then print a separator
    print!("\x1b[2J\x1b[H");
    use std::io::Write; let _ = std::io::stdout().flush();
}
pub fn term_size() -> (u16, u16) { console::Term::stdout().size() }

pub fn draw_header(_mood: &Mood) {
    let (_, cols) = term_size();
    let w = cols as usize;
    let art = ["____        __          __","/ __ \\__  __/ /_  ____  / /_","/ /_/ / / / __ \\/ __ \\/ __/","/ _, _/ /_/ / /_/ / /_/ / /_","/_/ |_|\\__,_/_.___/\\____/\\__/"];
    for line in art {
        let pad = w.saturating_sub(line.chars().count()) / 2;
        println!("{}{}", " ".repeat(pad), style(line).cyan().bold());
    }
    println!("{}\n", " ".repeat(w.saturating_sub(44) / 2) + "Atomic autonomous agent | Sandbox | High Speed");
}

pub fn prompt(mood: &Mood, _model: &str, _mem: usize) -> String {
    format!("{} {} ", style(Pet::face(mood)).yellow().bold(), style("›").cyan())
}

pub fn print_response(text: &str) {
    let cleaned = text.replace("TASK COMPLETE", "");
    if !cleaned.trim().is_empty() {
        println!("\n"); termimad::print_text(cleaned.trim()); println!();
        for line in cleaned.trim().lines() { scrollback_push(line); }
    }
}

pub fn stream_token(token: &str) {
    use std::io::{Write, stdout};
    let mut o = stdout().lock();
    let _ = write!(o, "{}", token);
    let _ = o.flush();
    // Accumulate into a pending buffer; flushed on stream_end
    if let Ok(mut pending) = STREAM_PENDING.lock() { pending.push_str(token); }
}

static STREAM_PENDING: Mutex<String> = Mutex::new(String::new());

pub fn stream_end() {
    use std::io::{Write, stdout};
    let mut o = stdout().lock();
    let _ = writeln!(o, "\n");
    let _ = o.flush();
    // Flush accumulated stream text to scrollback
    if let Ok(pending) = STREAM_PENDING.lock() {
        for line in pending.lines() { scrollback_push(line); }
    }
    if let Ok(mut pending) = STREAM_PENDING.lock() { pending.clear(); }
}

/// Suppress terminal echo + canonical mode so keypresses during streaming
/// don't show as garbage like `[[A`.
pub fn suppress_input() {
    unsafe {
        let fd = libc::STDIN_FILENO;
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut t) == 0 {
            t.c_lflag &= !(libc::ECHO | libc::ICANON);
            libc::tcsetattr(fd, libc::TCSANOW, &t);
        }
    }
}

/// Restore terminal to normal (echo + canonical) after streaming.
pub fn restore_input() {
    unsafe {
        let fd = libc::STDIN_FILENO;
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut t) == 0 {
            t.c_lflag |= libc::ECHO | libc::ICANON;
            libc::tcsetattr(fd, libc::TCSANOW, &t);
        }
    }
}

/// Drain any pending keypresses from stdin (clear buffered garbage).
pub fn drain_stdin() {
    use std::io::Read;
    let mut buf = [0u8; 64];
    unsafe {
        let fd = libc::STDIN_FILENO;
        let mut flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            flags |= libc::O_NONBLOCK;
            libc::fcntl(fd, libc::F_SETFL, flags);
            let _ = std::io::stdin().read(&mut buf);
            flags &= !libc::O_NONBLOCK;
            libc::fcntl(fd, libc::F_SETFL, flags);
        }
    }
}

pub fn print_error(err: &str) { let s = format!("  × {}", err); println!("{}", style(&s).red()); scrollback_push(&s); }
pub fn status(msg: &str) { let s = format!("  • {}", msg); println!("{}", style(&s).dim()); scrollback_push(&s); }
pub fn goodbye() { println!("\n  {}\n", style("Bye.").dim()); }
pub fn help_hint() { let s = "  • /quit  • /plan  • /memory  • /loop  • /scroll"; println!("{}", style(s).cyan()); scrollback_push(s); }

pub fn tool_call_start(name: &str, params: &str) {
    let s = format!("  ○ {} {}", name, truncate(params, 40));
    println!("{}", style(&s).dim()); scrollback_push(&s);
}
pub fn tool_call_result(ok: bool, out: &str) {
    let icon = if ok { "● ✓" } else { "● ✗" };
    let s = format!("    {} {}", icon, truncate(out, 60));
    let styled = if ok { style(&s).green() } else { style(&s).red() };
    println!("{}", styled); scrollback_push(&s);
}
pub fn llm_round(r: u32, m: &str) {
    let s = format!("  ◎ round {} {}", r, m);
    println!("{}", style(&s).dim()); scrollback_push(&s);
}
pub fn plan_step(id: usize, d: &str, s: &str) {
    let icon = match s { "OK" => "✓", "FAILED" => "×", _ => "→" };
    let line = format!("  {} Step {}: {}", icon, id, d);
    println!("{}", style(&line).dim()); scrollback_push(&line);
}
pub fn command_output(title: &str, content: &str) {
    let header = format!("┌ {}", title);
    println!("\n{}", style(&header).bold()); scrollback_push(&header);
    for line in content.lines() { scrollback_push(line); }
    termimad::print_text(content);
}
fn truncate(s: &str, max: usize) -> String {
    let l = s.replace('\n', " ").trim().to_string();
    if l.chars().count() > max { format!("{}…", l.chars().take(max).collect::<String>()) } else { l }
}
