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
        }
    }
}
impl RuntimeConfig {
    pub fn validate(&self) -> Result<()> {
        routing::validate_patches(&self.profiles)?;
        self.concurrency.validate()?;
        self.security.validate()?;
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
