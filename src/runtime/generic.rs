//! agentctl's external-agent protocol, version 1: any command that is an
//! engineering-agent runtime.
//!
//! The command is run directly, with the arguments configured for it. It
//! receives one JSON request on standard input, then end of input, and
//! answers on standard output in JSON Lines, one object per line with a
//! string `type`. It is never asked anything else.
//!
//! The request is `{"protocol": 1, "model", "reasoning_effort" (or null),
//! "instructions", "input", "output_schema", "workspace"}`, its workspace
//! `read_only`, `editable` or `disposable`. The runtime honors the
//! workspace within its working directory, or answers with an `error`; it
//! answers likewise for any protocol, model or effort it cannot honor.
//! agentctl adds no sandbox of its own and knows nothing of what the runtime
//! is.
//!
//! Events:
//!
//! - `{"type": "session", "id"}`: metadata only.
//! - `{"type": "usage", "input", "output", "cached_input", "cache_write",
//!   "reasoning", "estimated"}`: cumulative, the last one standing. The
//!   last three are optional; `estimated` (default false) says the counts
//!   are the runtime's own estimate rather than a provider's report.
//! - `{"type": "result", "value"}`: the one successful terminal event. The
//!   value is checked against the output schema by agentctl.
//! - `{"type": "error"}`: the terminal failure. Any message it carries is
//!   returned, never recorded.
//!
//! Other events are ignored. A second terminal event fails the invocation.

use std::ffi::OsString;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Launch, Prepared, Stream, TokenUsage, Workspace};

const PROTOCOL: u32 = 1;

pub(super) fn prepare(launch: &Launch) -> Result<Prepared> {
    let workspace = match launch.workspace {
        Workspace::ReadOnly => "read_only",
        Workspace::Editable => "editable",
        Workspace::Disposable => "disposable",
    };
    let request = json!({
        "protocol": PROTOCOL,
        "model": launch.model,
        "reasoning_effort": launch.effort.map(|e| e.to_string()),
        "instructions": launch.bootstrap,
        "input": launch.input,
        "output_schema": launch.output_schema,
        "workspace": workspace,
    });
    Ok(Prepared {
        args: Vec::<OsString>::new(),
        decoder: super::Decoder::Generic(Decoder::default()),
        input: Some(request.to_string()),
        files: Vec::new(),
    })
}

#[derive(Debug, Default)]
pub(super) struct Decoder {
    /// Whether a terminal event has been seen.
    ended: bool,
}

#[derive(Deserialize)]
struct Tokens {
    input: u64,
    output: u64,
    cached_input: Option<u64>,
    cache_write: Option<u64>,
    reasoning: Option<u64>,
    #[serde(default)]
    estimated: bool,
}

impl Decoder {
    pub(super) fn decode(&mut self, kind: &str, event: Value, stream: &mut Stream) {
        match kind {
            "session" => stream.session(event["id"].as_str()),
            "usage" => match serde_json::from_value::<Tokens>(event) {
                Ok(t) => {
                    stream.estimated = t.estimated;
                    stream.tokens = Some(TokenUsage {
                        input: t.input,
                        output: t.output,
                        cached_input: t.cached_input,
                        cache_write: t.cache_write,
                        reasoning: t.reasoning,
                    });
                }
                Err(_) => stream.malformed("invalid usage event"),
            },
            "result" | "error" => {
                if std::mem::replace(&mut self.ended, true) {
                    return stream.malformed("more than one terminal event");
                }
                if kind == "error" {
                    // What the runtime said is returned, never recorded.
                    if let Some(message) = event["message"].as_str() {
                        stream
                            .metadata
                            .insert("error_message".into(), message.into());
                    }
                    stream.error = Some("the runtime reported an error");
                } else {
                    match event.get("value") {
                        Some(value) => stream.result = Some(value.clone()),
                        None => stream.malformed("the result event carries no value"),
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Adapter, ReasoningEffort};
    use crate::runtime::{Progress, Provider, ROLE_LIFECYCLE};
    use crate::state::Usage;

    fn decode(lines: &[Value]) -> Stream {
        let mut progress = Progress::new(super::super::Decoder::Generic(Decoder::default()));
        for line in lines {
            progress.observe(line.to_string().as_bytes());
        }
        progress.stream
    }

    #[test]
    fn the_request_carries_the_launch() {
        let launch = Launch {
            agent: crate::state::tests::agent_id(1),
            provider: Provider::new("local", Adapter::Generic, "agent"),
            executable: None,
            model: "any-model".into(),
            effort: Some(ReasoningEffort::Low),
            bootstrap: "be careful".into(),
            input: "the task".into(),
            output_schema: json!({"type": "object"}),
            cwd: "/work".into(),
            workspace: Workspace::Editable,
            lifecycle: ROLE_LIFECYCLE,
        };
        let prepared = prepare(&launch).unwrap();
        assert!(prepared.args.is_empty());
        let request: Value = serde_json::from_str(&prepared.input.unwrap()).unwrap();
        assert_eq!(
            request,
            json!({
                "protocol": 1, "model": "any-model", "reasoning_effort": "low",
                "instructions": "be careful", "input": "the task",
                "output_schema": {"type": "object"}, "workspace": "editable",
            })
        );
    }

    #[test]
    fn events_decode_into_the_stream() {
        let stream = decode(&[
            json!({"type": "session", "id": "s-1"}),
            json!({"type": "progress", "text": "ignored"}),
            json!({"type": "usage", "input": 1, "output": 1}),
            json!({"type": "usage", "input": 10, "output": 4, "cached_input": 3, "estimated": true}),
            json!({"type": "result", "value": {"n": 7}}),
        ]);
        assert_eq!(stream.session.as_deref(), Some("s-1"));
        assert_eq!(stream.result, Some(json!({"n": 7})));
        assert_eq!(stream.malformed, None);
        let Usage::LocalEstimate(t) = stream.usage() else {
            panic!("an estimate is a local estimate");
        };
        assert_eq!((t.input, t.output, t.cached_input), (10, 4, Some(3)));
        let stream = decode(&[json!({"type": "usage", "input": 2, "output": 1})]);
        assert!(matches!(stream.usage(), Usage::ProviderReported(_)));
        assert!(matches!(decode(&[]).usage(), Usage::Unavailable));
    }

    #[test]
    fn inconsistent_streams_are_failures() {
        let result = json!({"type": "result", "value": 1});
        let error = json!({"type": "error", "message": "no"});
        for pair in [[&result, &result], [&error, &result], [&result, &error]] {
            let stream = decode(&[pair[0].clone(), pair[1].clone()]);
            assert!(stream.malformed.unwrap().contains("more than one"));
        }
        let stream = decode(&[json!({"type": "result"})]);
        assert!(stream.malformed.unwrap().contains("no value"));
        let stream = decode(&[json!({"type": "usage", "input": "many", "output": 1})]);
        assert!(stream.malformed.unwrap().contains("invalid usage"));
        let stream = decode(&[error]);
        assert!(stream.error.is_some() && stream.metadata["error_message"] == "no");
    }
}
