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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub roles: BTreeMap<String, RoleConfig>,
    #[serde(default = "timeout")]
    pub timeout_ms: u64,
    #[serde(default = "rounds")]
    pub max_correction_rounds: u32,
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
            providers: BTreeMap::new(),
            roles: BTreeMap::new(),
            timeout_ms: timeout(),
            max_correction_rounds: rounds(),
        }
    }
}
impl RuntimeConfig {
    pub fn validate(&self) -> Result<()> {
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
                ["planner", "executor", "verifier"].contains(&role.as_str())
                    && self.providers.contains_key(&config.provider),
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
