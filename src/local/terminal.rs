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

/// Human presentation of a structured result: titled sections of aligned
/// `Label  value` fields, separated by blank lines.
///
/// ```text
/// Run
///   Plan      plan:demo
///   Status    BLOCKED
///
/// Next
///   agentctl run restore plan:demo
/// ```
///
/// This is presentation only. It never feeds JSON output or runtime decisions;
/// callers build it from the same value they would serialize.
#[derive(Debug, Default)]
pub struct Report {
    sections: Vec<(String, Vec<Row>)>,
}

#[derive(Debug)]
enum Row {
    Field(String, String),
    Text(String),
}

impl Report {
    pub fn new(title: impl Into<String>) -> Self {
        let mut report = Self::default();
        report.section(title);
        report
    }

    /// Starts a new titled section. An empty title renders rows without a
    /// heading, for a lead line such as "Plan cancelled".
    pub fn section(&mut self, title: impl Into<String>) -> &mut Self {
        self.sections.push((title.into(), vec![]));
        self
    }

    pub fn field(&mut self, label: &str, value: impl std::fmt::Display) -> &mut Self {
        self.push(Row::Field(label.into(), value.to_string()))
    }

    /// A field shown only when there is a value.
    pub fn field_opt<T: std::fmt::Display>(&mut self, label: &str, value: Option<T>) -> &mut Self {
        match value {
            Some(v) => self.field(label, v),
            None => self,
        }
    }

    /// A free-text line inside the current section.
    pub fn text(&mut self, text: impl Into<String>) -> &mut Self {
        self.push(Row::Text(text.into()))
    }

    /// A `CODE: explanation` reason, as runtime refusals record them, split
    /// into `Code` and `Reason` fields so the canonical code stays visible.
    pub fn reason(&mut self, reason: &str) -> &mut Self {
        match split_code(reason) {
            (Some(code), rest) => self.field("Code", code).field("Reason", rest),
            (None, rest) => self.field("Reason", rest),
        }
    }

    /// The operator's next step(s), as its own section.
    pub fn next<I: IntoIterator<Item = S>, S: Into<String>>(&mut self, steps: I) -> &mut Self {
        self.section("Next");
        for step in steps {
            self.text(step);
        }
        self
    }

    fn push(&mut self, row: Row) -> &mut Self {
        if self.sections.is_empty() {
            self.sections.push((String::new(), vec![]));
        }
        self.sections.last_mut().expect("section").1.push(row);
        self
    }
}

impl std::fmt::Display for Report {
    /// Lines are newline-separated with no trailing newline, so the report
    /// can be printed with `println!` like any other human text.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut lines: Vec<String> = vec![];
        for (title, rows) in &self.sections {
            if rows.is_empty() && title.is_empty() {
                continue;
            }
            if !lines.is_empty() {
                lines.push(String::new());
            }
            let indent = if title.is_empty() {
                ""
            } else {
                lines.push(title.clone());
                "  "
            };
            let width = rows
                .iter()
                .filter_map(|row| match row {
                    Row::Field(label, _) => Some(label.chars().count()),
                    Row::Text(_) => None,
                })
                .max()
                .unwrap_or(0);
            for row in rows {
                match row {
                    Row::Field(label, value) => {
                        let mut values = value.lines();
                        lines.push(format!(
                            "{indent}{label:<width$}  {}",
                            values.next().unwrap_or("")
                        ));
                        for line in values {
                            lines.push(format!("{indent}{:width$}  {line}", ""));
                        }
                    }
                    Row::Text(text) if text.is_empty() => lines.push(String::new()),
                    Row::Text(text) => {
                        for line in text.lines() {
                            lines.push(format!("{indent}{line}"));
                        }
                    }
                }
            }
        }
        let text = lines.join("\n");
        f.write_str(text.trim_end())
    }
}

/// Splits a leading canonical `SCREAMING_CODE: ` prefix from a reason.
pub fn split_code(reason: &str) -> (Option<&str>, &str) {
    match reason.split_once(": ") {
        Some((code, rest))
            if code.len() >= 3
                && code
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') =>
        {
            (Some(code), rest)
        }
        _ => (None, reason),
    }
}

/// The canonical serialized name of a unit enum (e.g. `RunState::Blocked` →
/// `BLOCKED`), so human output uses exactly the protocol's status vocabulary.
pub fn name(value: &impl Serialize) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        Ok(other) => other.to_string(),
        Err(_) => "UNKNOWN".into(),
    }
}

/// `yes`/`no` for booleans in human output.
pub fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
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
    fn reports_align_fields_per_section_and_split_reason_codes() {
        let mut report = Report::new("Run");
        report
            .field("Plan", "plan:a")
            .field("Status", "BLOCKED")
            .reason("SOURCE_DRIFT: policy changed")
            .next(["agentctl run restore plan:a"]);
        assert_eq!(
            report.to_string(),
            "Run\n  Plan    plan:a\n  Status  BLOCKED\n  Code    SOURCE_DRIFT\n  Reason  policy changed\n\nNext\n  agentctl run restore plan:a"
        );
        assert_eq!(split_code("plan: x"), (None, "plan: x"));
        let mut multi = Report::new("");
        multi.field("A", "one\ntwo");
        assert_eq!(multi.to_string(), "A  one\n   two");
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
