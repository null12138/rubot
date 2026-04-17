/// Format Agent output for Telegram.
/// Escapes MarkdownV2 special characters so the message renders as plain text.
pub fn format_for_telegram(input: &str) -> String {
    escape_markdown_v2(input)
}

fn escape_markdown_v2(text: &str) -> String {
    let mut result = String::with_capacity(text.len() * 2);
    for ch in text.chars() {
        match ch {
            '_' | '*' | '[' | ']' | '(' | ')' | '~' | '`' | '>' | '#' | '+'
            | '-' | '=' | '|' | '{' | '}' | '.' | '!' | '\\' => {
                result.push('\\');
                result.push(ch);
            }
            _ => result.push(ch),
        }
    }
    result
}
