//! Codex, run as `codex exec --json` with JSON Lines events.
//!
//! The input arrives on standard input (`-`). The bootstrap becomes Codex's
//! developer instructions. The output schema is handed over in a file, and
//! the turn's final agent message must be JSON, which agentctl checks against
//! the schema again; earlier messages are progress prose. A failed turn is
//! fatal and final: nothing Codex reports afterwards can undo it. Sessions are
//! ephemeral. Model-generated commands run in Codex's own sandbox: read-only,
//! or in an editable workspace `workspace-write`, whose only writable root is
//! then the working directory, not the temporary directories Codex would
//! otherwise add. A disposable workspace keeps those, for the artifacts of
//! builds and tests. That is Codex's confinement, not agentctl's.

use std::ffi::OsString;
use std::io::Write;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use super::{Launch, Passthrough, Prepared, Stream, TokenUsage, Workspace};

pub(super) const ENV: Passthrough = Passthrough {
    names: &["CODEX_HOME"],
    prefixes: &["OPENAI_"],
};

pub(super) fn prepare(launch: &Launch) -> Result<Prepared> {
    let mut schema = tempfile::Builder::new()
        .prefix("agentctl-schema-")
        .suffix(".json")
        .tempfile()
        .context("staging the output schema")?;
    serde_json::to_writer(&mut schema, &launch.output_schema)?;
    schema.flush()?;
    // Values are attached with `=` so none can be read as an option.
    let mut args: Vec<OsString> = [
        "exec",
        "--json",
        "--ephemeral",
        // agentctl, not Codex, decides where an agent works.
        "--skip-git-repo-check",
        match launch.workspace {
            Workspace::ReadOnly => "--sandbox=read-only",
            Workspace::Editable | Workspace::Disposable => "--sandbox=workspace-write",
        },
    ]
    .map(OsString::from)
    .into();
    if launch.workspace == Workspace::Editable {
        for key in ["exclude_tmpdir_env_var", "exclude_slash_tmp"] {
            args.push(format!("--config=sandbox_workspace_write.{key}=true").into());
        }
    }
    args.push(joined("--cd=", launch.cwd.as_os_str()));
    args.push(format!("--model={}", launch.model).into());
    // `--config` values are TOML; a quoted string is taken literally.
    if let Some(effort) = launch.effort {
        let effort = toml_string(&effort.to_string());
        args.push(format!("--config=model_reasoning_effort={effort}").into());
    }
    if !launch.bootstrap.is_empty() {
        let bootstrap = toml_string(&launch.bootstrap);
        args.push(format!("--config=developer_instructions={bootstrap}").into());
    }
    args.push(joined("--output-schema=", schema.path().as_os_str()));
    args.push("-".into());
    Ok(Prepared {
        args,
        env: ENV,
        decoder: super::Decoder::Codex(Decoder::default()),
        files: vec![schema],
    })
}

fn joined(option: &str, value: &std::ffi::OsStr) -> OsString {
    let mut arg = OsString::from(option);
    arg.push(value);
    arg
}

fn toml_string(text: &str) -> String {
    toml::Value::String(text.to_owned()).to_string()
}

#[derive(Debug, Default)]
pub(super) struct Decoder {
    /// The latest agent message of the current turn.
    message: Option<String>,
    /// Whether the current turn has already ended.
    turn_ended: bool,
    /// Whether any turn failed, which fails the invocation.
    failed: bool,
}

/// Usage of one turn. Codex reports cached input within `input_tokens` and
/// reasoning within `output_tokens`.
#[derive(Deserialize)]
struct Tokens {
    input_tokens: u64,
    output_tokens: u64,
    cached_input_tokens: Option<u64>,
    cache_write_input_tokens: Option<u64>,
    reasoning_output_tokens: Option<u64>,
}

impl Decoder {
    pub(super) fn decode(&mut self, kind: &str, event: Value, stream: &mut Stream) {
        match kind {
            "thread.started" => stream.session(event["thread_id"].as_str()),
            "item.completed" if event["item"]["type"] == "agent_message" => {
                match event["item"]["text"].as_str() {
                    Some(text) => self.message = Some(text.to_owned()),
                    None => stream.malformed("an agent message has no text"),
                }
            }
            "turn.started" => {
                self.message = None;
                self.turn_ended = false;
            }
            "turn.completed" => self.complete(&event["usage"], stream),
            "turn.failed" => {
                if std::mem::replace(&mut self.turn_ended, true) {
                    stream.malformed("more than one end of a codex turn");
                }
                self.failed = true;
                // What Codex said is returned, never recorded.
                if let Some(message) = event["error"]["message"].as_str() {
                    stream
                        .metadata
                        .insert("error_message".into(), message.into());
                }
                stream.error = Some("codex reported that the turn failed");
            }
            // Codex also reports errors it recovers from, such as a retried
            // connection, so an error stands only if the turn never completes.
            "error" if !self.failed => {
                stream.error = Some("codex reported an error and the turn did not complete");
            }
            _ => {}
        }
    }

    fn complete(&mut self, usage: &Value, stream: &mut Stream) {
        if std::mem::replace(&mut self.turn_ended, true) {
            return stream.malformed("more than one end of a codex turn");
        }
        if self.failed {
            // A failed turn stays failed; a completion afterwards contradicts it.
            return stream.malformed("codex reported completion after a failed turn");
        }
        stream.error = None;
        if !usage.is_null() {
            match Tokens::deserialize(usage).map(|turn| sum(stream.tokens, turn)) {
                Ok(Some(total)) => stream.tokens = Some(total),
                Ok(None) => stream.malformed("token counts overflow"),
                Err(_) => stream.malformed("invalid codex turn usage"),
            }
        }
        match self.message.take().map(|text| serde_json::from_str(&text)) {
            Some(Ok(value)) => stream.result = Some(value),
            Some(Err(_)) => stream.malformed("the final codex message is not JSON"),
            None => {}
        }
    }
}

/// Usage of every turn so far. A subset only some turns report is unknown.
fn sum(total: Option<TokenUsage>, turn: Tokens) -> Option<TokenUsage> {
    let turn = TokenUsage {
        input: turn.input_tokens,
        output: turn.output_tokens,
        cached_input: turn.cached_input_tokens,
        cache_write: turn.cache_write_input_tokens,
        reasoning: turn.reasoning_output_tokens,
    };
    let Some(total) = total else {
        return Some(turn);
    };
    let subset = |a: Option<u64>, b: Option<u64>| match (a, b) {
        (Some(a), Some(b)) => a.checked_add(b).map(Some),
        _ => Some(None),
    };
    Some(TokenUsage {
        input: total.input.checked_add(turn.input)?,
        output: total.output.checked_add(turn.output)?,
        cached_input: subset(total.cached_input, turn.cached_input)?,
        cache_write: subset(total.cache_write, turn.cache_write)?,
        reasoning: subset(total.reasoning, turn.reasoning)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::Progress;
    use serde_json::json;

    fn decode(lines: &[Value]) -> Stream {
        let mut progress = Progress::new(super::super::Decoder::Codex(Decoder::default()));
        for line in lines {
            progress.observe(line.to_string().as_bytes());
        }
        progress.stream
    }

    fn message(text: &str) -> Value {
        json!({"type": "item.completed", "item": {"id": "item_0", "type": "agent_message", "text": text}})
    }

    /// Usage as the installed CLI (0.146.1) reports it.
    fn completed() -> Value {
        json!({"type": "turn.completed", "usage": {
            "input_tokens": 12552, "cached_input_tokens": 0, "cache_write_input_tokens": 0,
            "output_tokens": 15, "reasoning_output_tokens": 0
        }})
    }

    #[test]
    fn the_final_message_is_the_result() {
        let stream = decode(&[
            json!({"type": "thread.started", "thread_id": "t-1"}),
            json!({"type": "turn.started"}),
            message("Looking at the files first."),
            json!({"type": "item.completed", "item": {"type": "command_execution"}}),
            message(r#"{"n":7}"#),
            completed(),
        ]);
        assert_eq!(stream.result, Some(json!({"n": 7})));
        assert_eq!(stream.session.as_deref(), Some("t-1"));
        assert_eq!(
            stream.tokens,
            Some(TokenUsage {
                input: 12552,
                output: 15,
                cached_input: Some(0),
                cache_write: Some(0),
                reasoning: Some(0),
            })
        );
        assert_eq!((stream.error, stream.malformed), (None, None));
    }

    #[test]
    fn a_prose_final_message_is_malformed() {
        let stream = decode(&[message(r#"{"n":7}"#), message("Done: n is 7."), completed()]);
        assert_eq!(stream.result, None);
        assert!(stream.malformed.unwrap().contains("not JSON"));
    }

    #[test]
    fn only_unrecovered_errors_stand() {
        // As the installed CLI reports a model it cannot use.
        let stream = decode(&[
            json!({"type": "error", "message": "status 400: model is not supported"}),
            json!({"type": "turn.failed", "error": {"message": "status 400: model is not supported"}}),
        ]);
        assert_eq!(stream.error, Some("codex reported that the turn failed"));
        assert_eq!(
            stream.metadata["error_message"],
            json!("status 400: model is not supported")
        );
        let stream = decode(&[
            json!({"type": "error", "message": "Reconnecting... 1/5"}),
            message("{}"),
            completed(),
        ]);
        assert_eq!((stream.error, stream.result), (None, Some(json!({}))));
    }

    #[test]
    fn a_failed_turn_stays_failed() {
        let failed = json!({"type": "turn.failed", "error": {"message": "fatal"}});
        let started = json!({"type": "turn.started"});
        let stream = decode(&[
            started.clone(),
            failed.clone(),
            message(r#"{"n":7}"#),
            completed(),
        ]);
        assert_eq!(stream.error, Some("codex reported that the turn failed"));
        assert_eq!(stream.result, None);
        assert!(stream.malformed.is_some());
        // Nor can a later turn undo it.
        let stream = decode(&[
            started.clone(),
            failed.clone(),
            started.clone(),
            message(r#"{"n":7}"#),
            completed(),
        ]);
        assert!(stream.error.is_some() && stream.malformed.is_some());
        assert_eq!(stream.result, None);
        // Nor a recoverable error's recovery.
        let stream = decode(&[
            started.clone(),
            failed.clone(),
            json!({"type": "error", "message": "Reconnecting... 1/5"}),
            completed(),
        ]);
        assert_eq!(stream.error, Some("codex reported that the turn failed"));
        assert_eq!(stream.result, None);
    }

    #[test]
    fn a_turn_ends_once() {
        let failed = json!({"type": "turn.failed", "error": {"message": "fatal"}});
        let started = json!({"type": "turn.started"});
        for ends in [[completed(), completed()], [completed(), failed.clone()]] {
            let mut lines = vec![started.clone(), message(r#"{"n":7}"#)];
            lines.extend(ends);
            let stream = decode(&lines);
            assert_eq!(
                stream.malformed,
                Some("more than one end of a codex turn"),
                "{lines:?}"
            );
        }
    }

    #[test]
    fn turns_add_up_and_unreported_subsets_stay_unknown() {
        let partial =
            json!({"type": "turn.completed", "usage": {"input_tokens": 5, "output_tokens": 1}});
        let started = || json!({"type": "turn.started"});
        let stream = decode(&[
            started(),
            message("1"),
            completed(),
            started(),
            message("2"),
            partial,
        ]);
        assert_eq!(stream.result, Some(json!(2)));
        assert_eq!(
            stream.tokens,
            Some(TokenUsage {
                input: 12557,
                output: 16,
                cached_input: None,
                cache_write: None,
                reasoning: None,
            })
        );
        let stream = decode(&[json!({"type": "turn.completed", "usage": {"input_tokens": -1}})]);
        assert!(
            stream
                .malformed
                .unwrap()
                .contains("invalid codex turn usage")
        );
        let stream = decode(&[message("{}"), json!({"type": "turn.completed"})]);
        assert_eq!((stream.tokens, stream.result), (None, Some(json!({}))));
    }

    #[test]
    fn configuration_values_stay_literal_toml_strings() {
        let bootstrap = "Say \"hi\"\n= [not] a table\\ ''' -c x=1";
        let launch = Launch {
            agent: crate::state::tests::agent_id(1),
            provider: super::super::Provider::Codex,
            executable: None,
            model: "m".into(),
            effort: Some(crate::config::ReasoningEffort::Minimal),
            bootstrap: bootstrap.into(),
            input: "task".into(),
            output_schema: json!({"type": "object"}),
            cwd: "/work dir".into(),
            workspace: Workspace::ReadOnly,
        };
        let prepared = prepare(&launch).unwrap();
        let args: Vec<String> = prepared
            .args
            .iter()
            .map(|a| a.to_str().unwrap().to_owned())
            .collect();
        let config = |key: &str| {
            let arg = args
                .iter()
                .find_map(|a| a.strip_prefix(&format!("--config={key}=")))
                .unwrap();
            let table: toml::Table = toml::from_str(&format!("v = {arg}")).unwrap();
            table["v"].as_str().unwrap().to_owned()
        };
        assert_eq!(config("developer_instructions"), bootstrap);
        assert_eq!(config("model_reasoning_effort"), "minimal");
        assert!(args.contains(&"--cd=/work dir".to_owned()));
        assert_eq!(args.last().map(String::as_str), Some("-"));
        let schema = std::fs::read_to_string(prepared.files[0].path()).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&schema).unwrap(),
            launch.output_schema
        );
    }

    /// An editable workspace keeps Codex's own sandbox, writable only in the
    /// working directory; nothing ever runs outside it.
    #[test]
    fn workspaces_keep_codex_sandboxed() {
        let launch = |workspace| Launch {
            agent: crate::state::tests::agent_id(1),
            provider: super::super::Provider::Codex,
            executable: None,
            model: "m".into(),
            effort: None,
            bootstrap: String::new(),
            input: "task".into(),
            output_schema: json!({"type": "object"}),
            cwd: "/work".into(),
            workspace,
        };
        let args = |workspace| -> Vec<String> {
            let prepared = prepare(&launch(workspace)).unwrap();
            prepared
                .args
                .iter()
                .map(|a| a.to_str().unwrap().to_owned())
                .collect()
        };
        let editable = args(Workspace::Editable);
        let sandboxes: Vec<&String> = editable
            .iter()
            .filter(|a| a.starts_with("--sandbox"))
            .collect();
        assert_eq!(sandboxes, ["--sandbox=workspace-write"]);
        for key in ["exclude_tmpdir_env_var", "exclude_slash_tmp"] {
            let arg = format!("--config=sandbox_workspace_write.{key}=true");
            assert!(editable.contains(&arg), "{editable:?}");
        }
        let disposable = args(Workspace::Disposable);
        assert!(disposable.contains(&"--sandbox=workspace-write".to_owned()));
        assert!(
            !disposable
                .iter()
                .any(|a| a.contains("sandbox_workspace_write")),
            "{disposable:?}"
        );
        let read_only = args(Workspace::ReadOnly);
        assert!(read_only.contains(&"--sandbox=read-only".to_owned()));
        assert!(
            !read_only
                .iter()
                .any(|a| a.contains("sandbox_workspace_write"))
        );
        for args in [editable, disposable, read_only] {
            assert!(!args.iter().any(|a| a.contains("danger")), "{args:?}");
        }
    }
}
