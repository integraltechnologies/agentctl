use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleConfig {
    pub provider: String,
    pub model: Option<String>,
    pub effort: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub adapter: String,
    pub executable: PathBuf,
    #[serde(default)]
    pub authentication: super::credentials::Authentication,
}
/// Machine-owned hard ceiling on simultaneously active runtime agent jobs
/// (planner/executor/verifier/integration-verifier alike). Never raised by
/// provider output, routing profiles, or explicit user role overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConcurrencyConfig {
    #[serde(default = "default_max_agents")]
    pub max_agents: usize,
}
fn default_max_agents() -> usize {
    4
}
impl Default for ConcurrencyConfig {
    fn default() -> Self {
        Self {
            max_agents: default_max_agents(),
        }
    }
}
impl ConcurrencyConfig {
    pub fn validate(&self) -> Result<()> {
        require(
            (1..=256).contains(&self.max_agents),
            "runtime.concurrency.max_agents must be between 1 and 256 inclusive",
        )
    }
}
/// Relaunches agentctl will spend on one task whose launch provably mutated
/// nothing (controller crash, provider timeout, unusable reply). Machine-owned
/// and deliberately not configurable: it exists so a transient failure does not
/// end a plan, not as a retry policy.
pub const MAX_EXECUTOR_RELAUNCHES: u32 = 2;

/// Fresh provider jobs agentctl may spend, within one invocation, on a
/// mechanically unusable exchange (timeout, transient exit, a reply that is
/// not the canonical document, or a verifier/executor contract violation).
/// Each retry is its own durable job. Executors are retried only when the
/// workspace is provably unchanged. Machine-owned and not configurable: it
/// absorbs provider noise; engineering corrections remain replacement plans.
pub const MAX_PROVIDER_RETRIES: u32 = 2;

/// Fresh planner jobs agentctl issues after a planner decision is refused for
/// a planner-contract rule, each told the exact refusal. Distinct from
/// runtime correction rounds, which replace an executed plan.
pub const MAX_PLAN_CORRECTIONS: u32 = 1;

/// Hard maxima of the context relay. Configuration may choose values
/// inside them; worker output, planner output and project policy cannot.
pub const HARD_MAX_CONTEXT_ROUNDS: u32 = 4;
pub const HARD_MAX_VERIFIER_CONTEXT_ROUNDS: u32 = 2;
pub const HARD_MAX_CONTEXT_ROUND_BYTES: u32 = protocol_max_request_bytes();
pub const HARD_MAX_CONTEXT_TASK_BYTES: u32 = 96 * 1024;
pub const HARD_MAX_CONTEXT_ESCALATIONS: u32 = 2;
const fn protocol_max_request_bytes() -> u32 {
    crate::protocol::MAX_CONTEXT_REQUEST_BYTES
}

/// What repository content a provider job can read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContextVisibility {
    /// The whole workspace is readable (the default until the relay is proven
    /// with real providers; see docs/security.md).
    #[default]
    Workspace,
    /// Executor and verifier jobs read only the repository files actually
    /// issued to them (opt-in).
    Issued,
}

/// Machine-owned budgets of the planner-mediated context relay
/// (`[runtime.context]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextConfig {
    /// Granted context rounds per executor task (fresh re-issues).
    #[serde(default = "default_context_rounds")]
    pub max_rounds: u32,
    /// Granted context rounds per verification (packet or integration).
    #[serde(default = "default_verifier_context_rounds")]
    pub verifier_max_rounds: u32,
    /// Bytes one round's ContextDelta may carry.
    #[serde(default = "default_context_round_bytes")]
    pub max_round_bytes: u32,
    /// Cumulative ContextDelta bytes per task or verification.
    #[serde(default = "default_context_task_bytes")]
    pub max_task_bytes: u32,
    /// Planner escalations per executor task.
    #[serde(default = "default_context_escalations")]
    pub max_escalations: u32,
    #[serde(default)]
    pub visibility: ContextVisibility,
}
fn default_context_rounds() -> u32 {
    2
}
fn default_verifier_context_rounds() -> u32 {
    1
}
fn default_context_round_bytes() -> u32 {
    16 * 1024
}
fn default_context_task_bytes() -> u32 {
    48 * 1024
}
fn default_context_escalations() -> u32 {
    1
}
impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_rounds: default_context_rounds(),
            verifier_max_rounds: default_verifier_context_rounds(),
            max_round_bytes: default_context_round_bytes(),
            max_task_bytes: default_context_task_bytes(),
            max_escalations: default_context_escalations(),
            visibility: ContextVisibility::default(),
        }
    }
}
impl ContextConfig {
    pub fn validate(&self) -> Result<()> {
        require(
            self.max_rounds <= HARD_MAX_CONTEXT_ROUNDS
                && self.verifier_max_rounds <= HARD_MAX_VERIFIER_CONTEXT_ROUNDS
                && self.max_escalations <= HARD_MAX_CONTEXT_ESCALATIONS,
            "runtime.context: max_rounds 0–4, verifier_max_rounds 0–2, max_escalations 0–2",
        )?;
        require(
            (1024..=HARD_MAX_CONTEXT_ROUND_BYTES).contains(&self.max_round_bytes)
                && (1024..=HARD_MAX_CONTEXT_TASK_BYTES).contains(&self.max_task_bytes)
                && self.max_round_bytes <= self.max_task_bytes,
            "runtime.context: max_round_bytes 1024–32768, max_task_bytes 1024–98304, round ≤ task",
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub profiles: BTreeMap<String, routing::RolePatch>,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub roles: BTreeMap<String, RoleConfig>,
    #[serde(default = "timeout")]
    pub timeout_ms: u64,
    #[serde(default = "rounds")]
    pub max_correction_rounds: u32,
    #[serde(default)]
    pub concurrency: ConcurrencyConfig,
    /// Machine-owned worker security authority (`[runtime.security]`).
    #[serde(default)]
    pub security: crate::local::security::SecurityConfig,
    /// Machine-owned context-relay budgets and visibility (`[runtime.context]`).
    #[serde(default)]
    pub context: ContextConfig,
}
fn timeout() -> u64 {
    600_000
}
fn rounds() -> u32 {
    2
}
impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            profiles: BTreeMap::new(),
            providers: BTreeMap::new(),
            roles: BTreeMap::new(),
            timeout_ms: timeout(),
            max_correction_rounds: rounds(),
            concurrency: ConcurrencyConfig::default(),
            security: Default::default(),
            context: ContextConfig::default(),
        }
    }
}
impl RuntimeConfig {
    pub fn validate(&self) -> Result<()> {
        routing::validate_patches(&self.profiles)?;
        self.concurrency.validate()?;
        self.security.validate()?;
        self.context.validate()?;
        require(
            (1..=3_600_000).contains(&self.timeout_ms),
            "runtime timeout must be 1–3600000 ms",
        )?;
        require(
            self.max_correction_rounds <= 2,
            "runtime correction limit must be 0–2; escalation is mandatory thereafter",
        )?;
        for (id, provider) in &self.providers {
            provider.authentication.validate()?;
            JobId::new(id.clone()).map_err(Error::Invalid)?;
            paths::absolute_path(&provider.executable)?;
            require(
                ["codex", "claude"].contains(&provider.adapter.as_str()),
                "unknown provider adapter",
            )?;
        }
        for (role, config) in &self.roles {
            require(
                routing::valid_role(role) && self.providers.contains_key(&config.provider),
                "invalid runtime role/provider mapping",
            )?;
            for value in [&config.model, &config.effort].into_iter().flatten() {
                require(
                    !value.is_empty() && value.len() <= 128 && !value.contains(['\0', '\n', '\r']),
                    "invalid opaque model/effort",
                )?;
            }
        }
        Ok(())
    }
    /// The hard concurrent-agent ceiling in effect for a project: the machine
    /// value, optionally further lowered by project policy. A project can
    /// never raise the machine-configured ceiling.
    pub fn effective_max_agents(&self, project: &routing::ProjectRoles) -> usize {
        self.concurrency
            .max_agents
            .min(project.max_agents.unwrap_or(usize::MAX))
    }
    pub fn role(&self, role: AgentRole) -> Result<&RoleConfig> {
        self.roles
            .get(match role {
                AgentRole::Planner => "planner",
                AgentRole::Executor => "executor",
                AgentRole::Verifier => "verifier",
            })
            .ok_or_else(|| {
                Error::Invalid(format!("configure runtime role {role:?} before launching"))
            })
    }
}
