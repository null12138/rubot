use std::io::IsTerminal;

pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const CYAN: &str = "\x1b[36m";
pub const GREEN: &str = "\x1b[32m";
pub const RED: &str = "\x1b[31m";
pub const YELLOW: &str = "\x1b[33m";
pub const R: &str = "\x1b[0m";

const H1: &str = "\x1b[1;4;33m";
const H2: &str = "\x1b[1;33m";

pub fn render(md: &str) -> String {
    if !std::io::stdout().is_terminal() {
        return md.to_string();
    }
    let lines: Vec<&str> = md.lines().collect();
    let mut out = String::new();
    let mut i = 0;
    let mut in_fence = false;
    while i < lines.len() {
        let line = lines[i];
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            i += 1;
            continue;
        }
        if in_fence {
            out.push_str(&format!("{}{}  │ {}{}\n", DIM, CYAN, line, R));
            i += 1;
            continue;
        }
        if is_table_row(line) && i + 1 < lines.len() && is_table_separator(lines[i + 1]) {
            let start = i;
            let mut end = i + 2;
            while end < lines.len() && is_table_row(lines[end]) {
                end += 1;
            }
            out.push_str(&render_table(&lines[start..end]));
            i = end;
            continue;
        }
        out.push_str(&render_line(line));
        out.push('\n');
        i += 1;
    }
    out
}

fn render_line(line: &str) -> String {
    for (prefix, style) in [
        ("###### ", BOLD),
        ("##### ", BOLD),
        ("#### ", BOLD),
        ("### ", BOLD),
        ("## ", H2),
        ("# ", H1),
    ] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return format!("{}{}{}", style, render_inline(rest), R);
        }
    }
    if let Some(rest) = line.strip_prefix("> ") {
        return format!("{}│ {}{}", DIM, render_inline(rest), R);
    }
    if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
        return format!("  • {}", render_inline(rest));
    }
    if let Some(rest) = line
        .strip_prefix("  - ")
        .or_else(|| line.strip_prefix("  * "))
    {
        return format!("    ◦ {}", render_inline(rest));
    }
    let t = line.trim();
    if t == "---" || t == "***" || t == "___" {
        return format!("{}{}{}", DIM, "─".repeat(40), R);
    }
    render_inline(line)
}

fn render_inline(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '`' {
            if let Some(end_off) = chars[i + 1..].iter().position(|&ch| ch == '`') {
                let code: String = chars[i + 1..i + 1 + end_off].iter().collect();
                out.push_str(&format!("{}{}{}", CYAN, code, R));
                i += end_off + 2;
                continue;
            }
        }
        if c == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            if let Some(close) = find_double(&chars, i + 2, '*') {
                let inner: String = chars[i + 2..close].iter().collect();
                out.push_str(&format!("{}{}{}", BOLD, render_inline(&inner), R));
                i = close + 2;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

fn find_double(chars: &[char], start: usize, ch: char) -> Option<usize> {
    let mut j = start;
    while j + 1 < chars.len() {
        if chars[j] == ch && chars[j + 1] == ch {
            return Some(j);
        }
        j += 1;
    }
    None
}

fn is_table_row(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.ends_with('|') && t.len() >= 3
}

fn is_table_separator(line: &str) -> bool {
    let t = line.trim();
    if !t.starts_with('|') || !t.ends_with('|') {
        return false;
    }
    t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t')) && t.contains('-')
}

fn split_row(row: &str) -> Vec<String> {
    let t = row.trim().trim_start_matches('|').trim_end_matches('|');
    t.split('|').map(|c| c.trim().to_string()).collect()
}

fn display_width(s: &str) -> usize {
    s.chars().map(|c| if is_wide(c) { 2 } else { 1 }).sum()
}

fn is_wide(c: char) -> bool {
    let u = c as u32;
    matches!(u,
        0x1100..=0x115F | 0x2E80..=0x303E | 0x3041..=0x33FF |
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xA000..=0xA4CF |
        0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE30..=0xFE4F |
        0xFF00..=0xFF60 | 0xFFE0..=0xFFE6
    )
}

fn render_table(rows: &[&str]) -> String {
    let header = split_row(rows[0]);
    let data: Vec<Vec<String>> = rows.iter().skip(2).map(|r| split_row(r)).collect();
    let n_cols = header
        .len()
        .max(data.iter().map(|r| r.len()).max().unwrap_or(0));

    let mut widths = vec![0usize; n_cols];
    for (i, h) in header.iter().enumerate() {
        widths[i] = widths[i].max(display_width(h));
    }
    for row in &data {
        for (i, c) in row.iter().enumerate() {
            if i < n_cols {
                widths[i] = widths[i].max(display_width(c));
            }
        }
    }

    let make_sep = |l: &str, m: &str, r: &str| -> String {
        let mut s = format!("{}{}", DIM, l);
        for (i, w) in widths.iter().enumerate() {
            s.push_str(&"─".repeat(w + 2));
            s.push_str(if i + 1 == widths.len() { r } else { m });
        }
        s.push_str(R);
        s
    };

    let mut s = String::new();
    s.push_str(&make_sep("┌", "┬", "┐"));
    s.push('\n');
    s.push_str(&render_table_row(&header, &widths, true));
    s.push_str(&make_sep("├", "┼", "┤"));
    s.push('\n');
    for row in &data {
        s.push_str(&render_table_row(row, &widths, false));
    }
    s.push_str(&make_sep("└", "┴", "┘"));
    s.push('\n');
    s
}

fn render_table_row(cells: &[String], widths: &[usize], bold: bool) -> String {
    let mut s = format!("{}│{}", DIM, R);
    for (i, w) in widths.iter().enumerate() {
        let cell = cells.get(i).map(|c| c.as_str()).unwrap_or("");
        let pad = w.saturating_sub(display_width(cell));
        if bold {
            s.push_str(&format!(" {}{}{}{} ", BOLD, cell, R, " ".repeat(pad)));
        } else {
            s.push_str(&format!(" {}{} ", cell, " ".repeat(pad)));
        }
        s.push_str(&format!("{}│{}", DIM, R));
    }
    s.push('\n');
    s
}
