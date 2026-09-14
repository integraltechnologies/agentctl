//! Terminal-safe rendering of untrusted text (filenames, Git refs, provider text,
//! metric/tag names, error messages, command output summaries).
//!
//! Human output neutralizes anything a terminal could interpret as control
//! (C0 except newline/tab, DEL, C1) or that can visually reorder text (bidi
//! controls), rewriting it as a visible `\u{..}` escape. JSON output keeps the
//! exact structured value: serde_json already escapes C0, and the remaining
//! dangerous characters are emitted as `\uXXXX` escapes that decode identically.
use std::borrow::Cow;

use serde::Serialize;

fn dangerous(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
        )
}

fn escape(text: &str, keep: impl Fn(char) -> bool) -> Cow<'_, str> {
    if !text.chars().any(|c| dangerous(c) && !keep(c)) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if dangerous(c) && !keep(c) {
            out.push_str(&format!("\\u{{{:x}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// A complete multi-line human rendering: newlines and tabs are layout.
pub fn human(text: &str) -> Cow<'_, str> {
    escape(text, |c| c == '\n' || c == '\t')
}

/// One untrusted field embedded inside a line: newlines are escaped too, so a
/// value cannot forge additional output lines.
pub fn field(text: &str) -> Cow<'_, str> {
    escape(text, |_| false)
}

/// Pretty JSON whose string contents cannot carry raw DEL/C1/bidi characters.
pub fn json(value: &impl Serialize) -> serde_json::Result<String> {
    Ok(escape_json(serde_json::to_string_pretty(value)?))
}

/// Compact variant of [`json`].
pub fn json_compact(value: &impl Serialize) -> serde_json::Result<String> {
    Ok(escape_json(serde_json::to_string(value)?))
}

fn escape_json(text: String) -> String {
    if !text.chars().any(dangerous_in_json) {
        return text;
    }
    // Outside strings serde emits only ASCII structure/whitespace, so every
    // remaining dangerous character sits inside a string literal.
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        if dangerous_in_json(c) {
            out.push_str(&format!("\\u{:04x}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

fn dangerous_in_json(c: char) -> bool {
    dangerous(c) && !matches!(c, '\n' | ' ' | '\t')
}

/// Shared CLI printer: exact JSON or neutralized human text.
pub fn print(json_mode: bool, value: &impl Serialize, human_text: &str) -> serde_json::Result<()> {
    if json_mode {
        println!("{}", json(value)?);
    } else {
        println!("{}", human(human_text));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_rendering_neutralizes_escape_sequences_but_keeps_layout() {
        let hostile = "ok\x1b]0;pwned\x07\x1b[2J\r\u{9b}31m\u{202e}txt.exe\nnext\tcol";
        let shown = human(hostile);
        assert!(
            !shown
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t')
        );
        assert!(!shown.contains('\u{202e}'));
        assert!(shown.contains("\\u{1b}]0;pwned\\u{7}"));
        assert!(shown.contains("\\u{9b}31m"));
        assert!(shown.contains("\nnext\tcol"));
        assert!(matches!(human("plain"), Cow::Borrowed(_)));
        assert_eq!(field("a\nb"), "a\\u{a}b");
    }

    #[test]
    fn json_output_preserves_exact_values_without_raw_controls() {
        let value = serde_json::json!({"name": "m\u{1b}[31m\u{9b}\u{7f}\u{2066}x", "n": 1});
        let text = json(&value).unwrap();
        assert!(!text.chars().any(|c| dangerous(c) && c != '\n'));
        let back: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(back, value);
    }
}
