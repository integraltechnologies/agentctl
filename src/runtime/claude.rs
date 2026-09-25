//! Claude Code, run as `claude --print` with streamed JSON events.
//!
//! The input arrives on standard input. The bootstrap is appended to Claude
//! Code's own system prompt, which carries its tool instructions. The CLI
//! is asked to enforce the output schema and reports the value as the result
//! event's `structured_output`, which agentctl checks again; its prose
//! `result` is never the result.
//! Nothing is persisted for resumption, and nobody answers permission
//! prompts, so any tool use that would ask is denied. An editable workspace
//! runs in Claude's own `acceptEdits` mode, which accepts file edits in the
//! working directory without asking.

use std::ffi::OsString;

use anyhow::{Result, ensure};
use serde::Deserialize;
use serde_json::Value;

use super::{Launch, Passthrough, Prepared, Stream, TokenUsage, Workspace};
use crate::config::ReasoningEffort;

pub(super) const ENV: Passthrough = Passthrough {
    names: &["CLAUDE_CONFIG_DIR", "CLAUDE_CODE_OAUTH_TOKEN"],
    prefixes: &["ANTHROPIC_", "CLAUDE_CODE_USE_"],
};

pub(super) fn prepare(launch: &Launch) -> Result<Prepared> {
    // Values are attached with `=` so none can be read as an option.
    let mut args: Vec<String> = [
        "--print",
        "--output-format=stream-json",
        "--verbose",
        "--no-session-persistence",
        match launch.workspace {
            Workspace::ReadOnly => "--permission-mode=default",
            Workspace::Editable => "--permission-mode=acceptEdits",
        },
        "--permission-prompts=none",
    ]
    .map(String::from)
    .into();
    args.push(format!("--model={}", launch.model));
    if let Some(effort) = launch.effort {
        ensure!(
            effort != ReasoningEffort::Minimal,
            "claude has no `{effort}` reasoning effort"
        );
        args.push(format!("--effort={effort}"));
    }
    if !launch.bootstrap.is_empty() {
        args.push(format!("--append-system-prompt={}", launch.bootstrap));
    }
    args.push(format!("--json-schema={}", launch.output_schema));
    Ok(Prepared {
        args: args.into_iter().map(OsString::from).collect(),
        env: ENV,
        decoder: super::Decoder::Claude(Decoder::default()),
        files: Vec::new(),
    })
}

#[derive(Debug, Default)]
pub(super) struct Decoder {
    ended: bool,
}

/// The final event of a `--print` run.
#[derive(Deserialize)]
struct Ending {
    is_error: bool,
    subtype: String,
    session_id: Option<String>,
    /// Prose: the answer, or the error message.
    result: Option<String>,
    structured_output: Option<Value>,
    usage: Option<Tokens>,
    total_cost_usd: Option<f64>,
    terminal_reason: Option<String>,
    api_error_status: Option<Value>,
}

/// Claude's counts are disjoint: input excludes cache reads and writes, and
/// output includes thinking.
#[derive(Deserialize)]
struct Tokens {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    output_tokens_details: Option<OutputDetails>,
}

#[derive(Deserialize)]
struct OutputDetails {
    thinking_tokens: Option<u64>,
}

impl Decoder {
    pub(super) fn decode(&mut self, kind: &str, event: Value, stream: &mut Stream) {
        match kind {
            "system" if event["subtype"] == "init" => {
                stream.session(event["session_id"].as_str());
            }
            "result" => self.end(event, stream),
            _ => {}
        }
    }

    fn end(&mut self, event: Value, stream: &mut Stream) {
        if std::mem::replace(&mut self.ended, true) {
            return stream.malformed("more than one result event");
        }
        let ending: Ending = match serde_json::from_value(event) {
            Ok(ending) => ending,
            Err(_) => return stream.malformed("invalid claude result event"),
        };
        stream.session(ending.session_id.as_deref());
        if let Some(cost) = ending.total_cost_usd {
            stream.metadata.insert("total_cost_usd".into(), cost.into());
        }
        if let Some(reason) = ending.terminal_reason {
            stream
                .metadata
                .insert("terminal_reason".into(), reason.into());
        }
        if let Some(t) = ending.usage {
            let cached = t.cache_read_input_tokens;
            let written = t.cache_creation_input_tokens;
            let Some(input) = t
                .input_tokens
                .checked_add(cached.unwrap_or(0))
                .and_then(|n| n.checked_add(written.unwrap_or(0)))
            else {
                return stream.malformed("token counts overflow");
            };
            stream.tokens = Some(TokenUsage {
                input,
                output: t.output_tokens,
                cached_input: cached,
                cache_write: written,
                reasoning: t.output_tokens_details.and_then(|d| d.thinking_tokens),
            });
        }
        if ending.is_error || ending.subtype != "success" {
            // What Claude said is returned, never recorded.
            let metadata = &mut stream.metadata;
            metadata.insert("error_subtype".into(), ending.subtype.into());
            if let Some(status) = ending.api_error_status.filter(|s| !s.is_null()) {
                metadata.insert("api_error_status".into(), status);
            }
            if let Some(message) = ending.result {
                metadata.insert("error_message".into(), message.into());
            }
            stream.error = Some("claude reported an error");
        } else {
            match ending.structured_output {
                Some(value) => stream.result = Some(value),
                None => stream.malformed("the result carries no structured output"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::Progress;
    use serde_json::json;

    fn decode(lines: &[Value]) -> Stream {
        let mut progress = Progress::new(super::super::Decoder::Claude(Decoder::default()));
        for line in lines {
            progress.observe(line.to_string().as_bytes());
        }
        progress.stream
    }

    fn init() -> Value {
        json!({"type": "system", "subtype": "init", "session_id": "s-1", "model": "m"})
    }

    /// A result as the installed CLI (2.1.267) reports it, abridged.
    fn result() -> Value {
        json!({
            "type": "result", "subtype": "success", "is_error": false,
            "session_id": "s-1", "result": "{\"n\":7}", "structured_output": {"n": 7},
            "total_cost_usd": 0.015771, "terminal_reason": "completed",
            "api_error_status": null,
            "usage": {
                "input_tokens": 9, "cache_creation_input_tokens": 7461,
                "cache_read_input_tokens": 0, "output_tokens": 168,
                "output_tokens_details": {"thinking_tokens": 111}
            }
        })
    }

    #[test]
    fn a_structured_result_with_usage() {
        let stream = decode(&[
            init(),
            json!({"type": "assistant", "message": {}}),
            result(),
        ]);
        assert_eq!(stream.result, Some(json!({"n": 7})));
        assert_eq!(stream.session.as_deref(), Some("s-1"));
        assert_eq!(
            stream.tokens,
            Some(TokenUsage {
                input: 7470,
                output: 168,
                cached_input: Some(0),
                cache_write: Some(7461),
                reasoning: Some(111),
            })
        );
        assert_eq!(stream.metadata["total_cost_usd"], json!(0.015771));
        assert_eq!((stream.error, stream.malformed), (None, None));
    }

    #[test]
    fn prose_is_never_a_result() {
        let mut prose = result();
        prose.as_object_mut().unwrap().remove("structured_output");
        let stream = decode(&[prose]);
        assert_eq!(stream.result, None);
        assert!(stream.malformed.unwrap().contains("no structured output"));
    }

    #[test]
    fn reported_errors_win_over_their_subtype() {
        // The CLI reports an unknown model as subtype `success`.
        let mut error = result();
        let fields = error.as_object_mut().unwrap();
        fields.insert("is_error".into(), json!(true));
        fields.insert("api_error_status".into(), json!(404));
        fields.insert(
            "result".into(),
            json!("There's an issue with the selected model"),
        );
        let stream = decode(&[error]);
        assert_eq!(stream.result, None);
        assert_eq!(stream.error, Some("claude reported an error"));
        assert_eq!(stream.metadata["api_error_status"], json!(404));
        let message = stream.metadata["error_message"].as_str().unwrap();
        assert!(message.contains("selected model"), "{message}");
        assert!(stream.tokens.is_some(), "usage is kept for failures");
    }

    #[test]
    fn inconsistent_endings_are_malformed() {
        let stream = decode(&[result(), result()]);
        assert!(stream.malformed.unwrap().contains("more than one"));
        let stream = decode(&[json!({"type": "result", "subtype": "success"})]);
        assert!(stream.malformed.unwrap().contains("invalid claude result"));
        let mut unreported = result();
        unreported.as_object_mut().unwrap().remove("usage");
        let stream = decode(&[unreported]);
        assert_eq!((stream.tokens, stream.malformed), (None, None));
    }

    #[test]
    fn values_cannot_become_options() {
        let launch = |effort| Launch {
            agent: crate::state::tests::agent_id(1),
            provider: super::super::Provider::Claude,
            executable: None,
            model: "--dangerously-skip-permissions".into(),
            effort,
            bootstrap: "-p".into(),
            input: "task".into(),
            output_schema: json!({"type": "object"}),
            cwd: "/".into(),
            workspace: super::super::Workspace::ReadOnly,
        };
        let args = prepare(&launch(Some(ReasoningEffort::High))).unwrap().args;
        assert!(args.contains(&"--model=--dangerously-skip-permissions".into()));
        assert!(args.contains(&"--append-system-prompt=-p".into()));
        assert!(args.contains(&"--effort=high".into()));
        assert!(!args.iter().any(|a| a == "task"), "input goes to stdin");
        assert!(prepare(&launch(Some(ReasoningEffort::Minimal))).is_err());
    }

    /// An editable workspace uses Claude's own edit-accepting mode; nothing
    /// skips its permission checks.
    #[test]
    fn workspaces_use_claude_permission_modes() {
        let launch = |workspace| Launch {
            agent: crate::state::tests::agent_id(1),
            provider: super::super::Provider::Claude,
            executable: None,
            model: "m".into(),
            effort: None,
            bootstrap: String::new(),
            input: "task".into(),
            output_schema: json!({"type": "object"}),
            cwd: "/work".into(),
            workspace,
        };
        for (workspace, mode) in [
            (Workspace::ReadOnly, "--permission-mode=default"),
            (Workspace::Editable, "--permission-mode=acceptEdits"),
        ] {
            let args: Vec<String> = prepare(&launch(workspace))
                .unwrap()
                .args
                .iter()
                .map(|a| a.to_str().unwrap().to_owned())
                .collect();
            let modes: Vec<&String> = args
                .iter()
                .filter(|a| a.starts_with("--permission-mode"))
                .collect();
            assert_eq!(modes, [mode]);
            assert!(args.contains(&"--permission-prompts=none".to_owned()));
            assert!(
                !args
                    .iter()
                    .any(|a| a.contains("dangerously") || a.contains("bypass"))
            );
        }
    }
}
