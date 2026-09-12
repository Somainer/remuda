//! Bounded terminal cells; agent output must not inject terminal control codes.

pub(crate) fn cell(value: &str, width: usize) -> String {
    let mut result = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.next() {
                Some('[') => {
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(next) = chars.next() {
                        if next == '\u{7}' || (next == '\u{1b}' && chars.next() == Some('\\')) {
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else if !c.is_control() && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            result.push(c);
        } else if c == '\n' || c == '\r' || c == '\t' {
            result.push(' ');
        }
    }
    if result.chars().count() > width {
        format!(
            "{}…",
            result
                .chars()
                .take(width.saturating_sub(1))
                .collect::<String>()
        )
    } else {
        result
    }
}

pub(crate) fn render(headers: &[&str], widths: &[usize], rows: &[Vec<String>]) -> String {
    let mut out = String::new();
    for row in
        std::iter::once(headers.iter().map(|s| s.to_string()).collect()).chain(rows.iter().cloned())
    {
        for (value, width) in row.iter().zip(widths) {
            let value = cell(value, *width);
            out.push_str(&value);
            out.push_str(&" ".repeat(width.saturating_sub(value.chars().count()) + 2));
        }
        out.push('\n');
    }
    out
}
