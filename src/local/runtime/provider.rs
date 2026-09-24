use super::process::{NativeProcess, ProcessOutput, ProcessSpec, RunningProcess};
use super::*;
use crate::local::security::json as strict_json;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
pub struct Capabilities {
    pub model: bool,
    pub effort: bool,
    pub fresh_session: bool,
    pub structured_output: bool,
    pub token_usage: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobInput {
    #[serde(skip)]
    pub compiled: Option<super::prompt::CompiledPrompt>,
    pub ownership: AgentOwnership,
    pub job_id: JobId,
    pub session_id: String,
    pub role: AgentRole,
    pub plan_id: Option<PlanId>,
    pub task_id: Option<TaskId>,
    pub repository_id: RepositoryId,
    pub workspace_id: WorkspaceId,
    pub source: SourceStateRef,
    pub artifact: Value,
}
pub trait ProviderAdapter: Send {
    /// A fresh adapter instance for an isolated concurrent invocation. Adapters
    /// with process-local mutable state may decline; the scheduler then falls
    /// back to serial execution rather than sharing that state unsafely.
    fn fork(&self) -> Option<Box<dyn ProviderAdapter>> {
        None
    }
    /// Token-free mechanical availability check. Never parse model prose.
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn capabilities(&self) -> Capabilities;
    fn launch(
        &mut self,
        input: &JobInput,
        process: ProcessSpec,
        config: &RoleConfig,
    ) -> Result<Box<dyn RunningProcess>>;
    fn collect(&self, output: &ProcessOutput) -> Result<Value>;
    /// Provider-specific recognition of an infrastructure condition in a
    /// failed exchange: `Some((retryable, summary))` when the raw output proves
    /// one (for example an authentication or quota refusal). Only mechanical
    /// signals the provider itself emits; never a judgement of model prose.
    fn fault(&self, _output: &ProcessOutput) -> Option<(bool, String)> {
        None
    }
    /// A single final observation, never inferred from text length or cost.
    fn usage(&self, _output: &ProcessOutput) -> Result<Usage> {
        Ok(Usage::default())
    }
}
#[derive(Debug, Clone)]
pub struct Usage {
    pub provenance: TokenUsageProvenance,
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cached: Option<u64>,
}
impl Default for Usage {
    fn default() -> Self {
        Self {
            provenance: TokenUsageProvenance::Unknown,
            input: None,
            output: None,
            cached: None,
        }
    }
}
pub struct CodexAdapter {
    pub executable: PathBuf,
    pub authentication: super::credentials::Authentication,
}
pub struct ClaudeAdapter {
    pub executable: PathBuf,
    pub authentication: super::credentials::Authentication,
}
fn prompt(input: &JobInput) -> Result<Vec<u8>> {
    input
        .compiled
        .as_ref()
        .map(|c| c.bytes.clone())
        .ok_or_else(|| Error::Invalid("adapter requires compiled role instructions".into()))
}
impl ProviderAdapter for CodexAdapter {
    fn fork(&self) -> Option<Box<dyn ProviderAdapter>> {
        Some(Box::new(Self {
            executable: self.executable.clone(),
            authentication: self.authentication.clone(),
        }))
    }
    fn preflight(&self) -> Result<()> {
        executable_available(&self.executable)
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            model: true,
            effort: true,
            fresh_session: true,
            structured_output: true,
            token_usage: false,
        }
    }
    fn launch(
        &mut self,
        input: &JobInput,
        mut process: ProcessSpec,
        config: &RoleConfig,
    ) -> Result<Box<dyn RunningProcess>> {
        process.executable = self.executable.clone();
        process.input = prompt(input)?;
        process.class = crate::local::security::WorkerClass::ProviderFrontend;
        self.authentication
            .configure(&self.executable, "codex", &mut process)?;
        process.args = vec![
            "exec".into(),
            "--ephemeral".into(),
            "--ignore-user-config".into(),
            "--ignore-rules".into(),
            "--color".into(),
            "never".into(),
            // The mandatory outer OS sandbox provides role-specific isolation.
            // Nested Seatbelt application fails on macOS; never launch directly.
            "--dangerously-bypass-approvals-and-sandbox".into(),
            "-c".into(),
            "project_doc_max_bytes=0".into(),
            "-c".into(),
            "shell_environment_policy.inherit=\"none\"".into(),
            "-c".into(),
            "cli_auth_credentials_store=\"auto\"".into(),
            "-c".into(),
            "history.persistence=\"none\"".into(),
            "-c".into(),
            "features.multi_agent=false".into(),
            "-c".into(),
            "features.memories=false".into(),
        ];
        process.args.extend([
            "-c".into(),
            format!(
                "sqlite_home={}",
                serde_json::to_string(&process.scratch.join("codex-state"))?
            ),
        ]);
        process.args.extend(codex_contract_args(input)?);
        if process.api_key.is_some() {
            process.args.extend([
                "-c".into(),
                "cli_auth_credentials_store=\"ephemeral\"".into(),
            ]);
        }
        if let Some(model) = &config.model {
            process.args.extend(["--model".into(), model.clone()]);
        }
        if let Some(effort) = &config.effort {
            process.args.extend([
                "-c".into(),
                format!("model_reasoning_effort={}", serde_json::to_string(effort)?),
            ]);
        }
        process.args.push("-".into());
        Ok(Box::new(NativeProcess::launch(&process)?))
    }
    fn collect(&self, output: &ProcessOutput) -> Result<Value> {
        Ok(strict_json::from_slice(&output.stdout)?)
    }
    fn fault(&self, output: &ProcessOutput) -> Option<(bool, String)> {
        let text = String::from_utf8_lossy(&output.stderr);
        let line = |needle: &str| {
            text.lines()
                .find(|l| l.contains(needle))
                .map(|l| l.trim().chars().take(300).collect::<String>())
        };
        line("usage limit")
            .or_else(|| line("rate limit"))
            .or_else(|| line("401 Unauthorized"))
            .or_else(|| line("Not logged in"))
            .map(|summary| (false, summary))
    }
}
/// The canonical role contract as Codex developer instructions: a
/// configuration value Codex sends in its own developer channel, separate from
/// the user turn. Codex's native `--output-schema` is not used: it requests
/// strict structured output, which requires every property to be required,
/// while canonical schemas legitimately have optional fields. The schema
/// therefore travels in the job input and the reply is parsed strictly.
pub fn codex_contract_args(input: &JobInput) -> Result<Vec<String>> {
    let compiled = input
        .compiled
        .as_ref()
        .ok_or_else(|| Error::Invalid("adapter requires compiled role instructions".into()))?;
    Ok(vec![
        "-c".into(),
        // A JSON string is a valid TOML basic string.
        format!(
            "developer_instructions={}",
            serde_json::to_string(&compiled.system)?
        ),
    ])
}
/// The canonical role contract through Claude Code's native channels: the
/// system prompt (appended, so the CLI keeps its own tool instructions) and
/// validated structured output, which arrives in the envelope's
/// `structured_output` field. The CLI's validator does not accept the
/// draft-2020-12 `$schema` identifier, so only that key is dropped; the schema
/// itself is unchanged.
pub fn claude_contract_args(input: &JobInput) -> Result<Vec<String>> {
    let compiled = input
        .compiled
        .as_ref()
        .ok_or_else(|| Error::Invalid("adapter requires compiled role instructions".into()))?;
    let mut schema = compiled.schema.clone();
    if let Some(object) = schema.as_object_mut() {
        object.remove("$schema");
    }
    Ok(vec![
        "--append-system-prompt".into(),
        compiled.system.clone(),
        "--json-schema".into(),
        serde_json::to_string(&schema)?,
    ])
}
impl ProviderAdapter for ClaudeAdapter {
    fn fork(&self) -> Option<Box<dyn ProviderAdapter>> {
        Some(Box::new(Self {
            executable: self.executable.clone(),
            authentication: self.authentication.clone(),
        }))
    }
    fn preflight(&self) -> Result<()> {
        executable_available(&self.executable)
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            model: true,
            effort: true,
            fresh_session: true,
            structured_output: true,
            token_usage: true,
        }
    }
    fn launch(
        &mut self,
        input: &JobInput,
        mut process: ProcessSpec,
        config: &RoleConfig,
    ) -> Result<Box<dyn RunningProcess>> {
        process.executable = self.executable.clone();
        process.input = prompt(input)?;
        process.class = crate::local::security::WorkerClass::ProviderFrontend;
        self.authentication
            .configure(&self.executable, "claude", &mut process)?;
        process.args = vec![
            "--print".into(),
            "--safe-mode".into(),
            "--restricted".into(),
            "--no-session-persistence".into(),
            "--session-id".into(),
            input.session_id.clone(),
            "--output-format".into(),
            "json".into(),
            "--setting-sources".into(),
            "".into(),
            "--strict-mcp-config".into(),
            "--mcp-config".into(),
            "{\"mcpServers\":{}}".into(),
            "--disable-slash-commands".into(),
            "--permission-mode".into(),
            "dontAsk".into(),
            "--tools".into(),
            if input.role == AgentRole::Executor && process.writable {
                "Read,Edit,Write"
            } else {
                "Read"
            }
            .into(),
            "--allowedTools".into(),
            if input.role == AgentRole::Executor && process.writable {
                "Read,Edit,Write"
            } else {
                "Read"
            }
            .into(),
        ];
        process.args.extend(claude_contract_args(input)?);
        if let Some(model) = &config.model {
            process.args.extend(["--model".into(), model.clone()]);
        }
        if let Some(effort) = &config.effort {
            process.args.extend(["--effort".into(), effort.clone()]);
        }
        Ok(Box::new(NativeProcess::launch(&process)?))
    }
    fn collect(&self, output: &ProcessOutput) -> Result<Value> {
        let response: Value = strict_json::from_slice(&output.stdout)?;
        require(
            response.get("is_error").and_then(Value::as_bool) != Some(true),
            "Claude reported a failed result",
        )?;
        // Native structured output: the CLI validated it against the schema.
        if let Some(value) = response.get("structured_output").filter(|v| !v.is_null()) {
            return Ok(value.clone());
        }
        Ok(strict_json::from_str(
            response
                .get("result")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Invalid("Claude output is missing its JSON result".into()))?,
        )?)
    }
    fn fault(&self, output: &ProcessOutput) -> Option<(bool, String)> {
        let response: Value = serde_json::from_slice(&output.stdout).ok()?;
        if response.get("is_error").and_then(Value::as_bool) != Some(true) {
            return None;
        }
        let status = response.get("api_error_status").and_then(Value::as_u64);
        let summary = response
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or("Claude reported an error")
            .chars()
            .take(300)
            .collect::<String>();
        match status {
            Some(401 | 403) => Some((false, format!("authentication refused ({summary})"))),
            Some(429) => Some((false, format!("rate or usage limit ({summary})"))),
            Some(500..=599) => Some((
                true,
                format!("provider server error {status:?} ({summary})"),
            )),
            _ => Some((true, summary)),
        }
    }
    fn usage(&self, output: &ProcessOutput) -> Result<Usage> {
        let response: Value = serde_json::from_slice(&output.stdout)?;
        let Some(usage) = response.get("usage") else {
            return Ok(Usage::default());
        };
        let input = usage.get("input_tokens").and_then(Value::as_u64);
        let output = usage.get("output_tokens").and_then(Value::as_u64);
        let cached = usage.get("cache_read_input_tokens").and_then(Value::as_u64);
        Ok(Usage {
            provenance: if input.is_some() || output.is_some() || cached.is_some() {
                TokenUsageProvenance::Exact
            } else {
                TokenUsageProvenance::Unknown
            },
            input,
            output,
            cached,
        })
    }
}
fn executable_available(path: &Path) -> Result<()> {
    if !path.is_file() {
        return Err(Error::ProviderAvailability(
            routing::FailureClass::ProviderUnavailable,
        ));
    }
    Ok(())
}
