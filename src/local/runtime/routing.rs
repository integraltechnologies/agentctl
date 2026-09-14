//! Deterministic, local policy. No model catalog, provider calls or storage access.
use super::*;

pub const BUILTINS: [&str; 5] = ["planner", "executor", "verifier", "recon", "reviewer"];
pub fn role_name(role: AgentRole) -> &'static str {
    match role {
        AgentRole::Planner => "planner",
        AgentRole::Executor => "executor",
        AgentRole::Verifier => "verifier",
    }
}
pub fn valid_role(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FailureClass {
    ProviderUnavailable,
    AuthUnavailable,
    CapabilityUnsupported,
    StartupFailure,
}

/// Optional fields are true patches; lists replace rather than concatenate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RolePatch {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub objective: Option<String>,
    pub instructions: Option<Vec<String>>,
    pub context_bytes: Option<usize>,
    pub advisory_tokens: Option<u64>,
    pub timeout_ms: Option<u64>,
    pub read_only: Option<bool>,
    pub network: Option<bool>,
    pub fallbacks: Option<Vec<RoleConfig>>,
    /// Number of alternatives, excluding the primary. Never more than four.
    pub max_fallback_attempts: Option<usize>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectRoles {
    pub profiles: BTreeMap<String, RolePatch>,
    /// Hard allowlist applies to primary, explicit overrides and every alternative.
    pub allowed_providers: Option<BTreeSet<String>>,
    pub read_only: bool,
    pub deny_network: bool,
    pub max_context_bytes: Option<usize>,
    /// Optional further restriction on the machine's runtime.concurrency.max_agents.
    /// Effective ceiling is always min(machine, project); a project can never raise it.
    pub max_agents: Option<usize>,
}
impl ProjectRoles {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
    pub fn validate(&self) -> Result<()> {
        validate_patches(&self.profiles)?;
        if let Some(n) = self.max_context_bytes {
            require(
                (1..=262144).contains(&n),
                "project routing.max_context_bytes must be 1–262144",
            )?;
        }
        if let Some(n) = self.max_agents {
            require(
                (1..=256).contains(&n),
                "project routing.max_agents must be between 1 and 256 inclusive",
            )?;
        }
        if let Some(ids) = &self.allowed_providers {
            require(
                !ids.is_empty() && ids.iter().all(|v| opaque(v)),
                "project routing.allowed_providers must be nonempty identifiers",
            )?;
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleProfile {
    pub role: String,
    pub objective: String,
    pub context_policy: String,
    pub reporting: String,
    pub verification: String,
    pub autonomy: String,
    pub instructions: Vec<String>,
    pub context_bytes: usize,
    pub advisory_tokens: Option<u64>,
    pub timeout_ms: u64,
    pub read_only: bool,
    pub network: bool,
    pub max_correction_rounds: u32,
    pub max_fallback_attempts: usize,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedRoute {
    pub configured_primary: RoleConfig,
    pub policy_skipped: Vec<RoleConfig>,
    pub profile: RoleProfile,
    pub primary: RoleConfig,
    pub fallbacks: Vec<RoleConfig>,
    pub sources: BTreeMap<String, String>,
    pub project_policy_hash: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedRoute {
    pub route: RoleConfig,
    pub reason: FailureClass,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteSnapshot {
    #[serde(default)]
    pub policy_skipped: Vec<RoleConfig>,
    pub requested_role: String,
    pub primary: RoleConfig,
    pub selected: RoleConfig,
    pub attempt: usize,
    pub failures: Vec<FailedRoute>,
    pub sources: BTreeMap<String, String>,
    pub profile_hash: String,
    pub project_policy_hash: String,
}
pub fn builtin(role: &str, runtime: &RuntimeConfig) -> RoleProfile {
    let (objective, context, reporting) = match role {
        "planner" => (
            "Decompose the bounded objective into meaningful TaskPackets, dependencies, invariants and checks. Request roles, not providers; do not implement or rediscover the repository.",
            "bounded PlannerPacket with canonical graph and trusted memory",
            "ExecutionPlan",
        ),
        "executor" => (
            "Implement the assigned TaskPacket completely within scope. Use necessary context, run required local checks, return compact evidence, and never self-verify or redesign unrelated code.",
            "TaskPacket, relevant source/graph/memory, invariants and checks",
            "ResultPacket",
        ),
        "verifier" => (
            "Independently evaluate objective, invariants, diff and captured evidence. Return PASS or REJECT; do not repair or use executor reasoning.",
            "TaskPacket or integration PlanPacket, invariants, diff and captured evidence only",
            "VerificationPacket",
        ),
        "recon" => (
            "Locate symbols and relationships using graph queries first, then bounded relevant source. Return compact findings; do not modify source.",
            "locator objective and bounded graph-first findings",
            "helper findings JSON (noncanonical)",
        ),
        _ => (
            "Review explicitly requested architecture/code using bounded canonical context. Do not substitute for mandatory verification.",
            "explicit objective and bounded relevant canonical context",
            "helper findings JSON (noncanonical)",
        ),
    };
    RoleProfile {
        role: role.into(),
        objective: objective.into(),
        context_policy: context.into(),
        reporting: reporting.into(),
        verification: "independent canonical verification remains mandatory".into(),
        autonomy:
            "issued job only; correction uses same role policy; no autonomous repair escalation"
                .into(),
        instructions: vec![],
        context_bytes: 262144,
        advisory_tokens: None,
        timeout_ms: runtime.timeout_ms,
        read_only: role != "executor",
        network: true,
        max_correction_rounds: runtime.max_correction_rounds,
        max_fallback_attempts: 4,
    }
}
fn opaque(s: &str) -> bool {
    !s.trim().is_empty() && s.len() <= 128 && !s.chars().any(char::is_control)
}
fn validate_route(route: &RoleConfig) -> Result<()> {
    require(
        opaque(&route.provider)
            && [&route.model, &route.effort]
                .into_iter()
                .flatten()
                .all(|s| opaque(s)),
        "invalid provider/model/effort identifier (model=\"\" is not a reset)",
    )
}
pub fn validate_patches(patches: &BTreeMap<String, RolePatch>) -> Result<()> {
    for (role, p) in patches {
        require(valid_role(role), format!("role {role}: invalid identifier"))?;
        for (field, value) in [
            ("provider", &p.provider),
            ("model", &p.model),
            ("effort", &p.effort),
        ] {
            if let Some(v) = value {
                require(opaque(v), format!("role {role}.{field}: invalid value"))?;
            }
        }
        if let Some(n) = p.context_bytes {
            require(
                (1..=262144).contains(&n),
                format!("role {role}.context_bytes must be 1–262144"),
            )?;
        }
        if let Some(n) = p.timeout_ms {
            require(
                (1..=3600000).contains(&n),
                format!("role {role}.timeout_ms must be 1–3600000"),
            )?;
        }
        require(
            p.advisory_tokens != Some(0),
            format!("role {role}.advisory_tokens must be positive"),
        )?;
        require(
            p.max_fallback_attempts.is_none_or(|n| n <= 4),
            format!("role {role}.max_fallback_attempts exceeds four"),
        )?;
        if let Some(list) = &p.fallbacks {
            require(
                list.len() <= 4,
                format!("role {role}.fallbacks exceeds four"),
            )?;
            for r in list {
                validate_route(r)?;
            }
        }
        if let Some(s) = &p.objective {
            require(
                !s.trim().is_empty() && s.len() <= 4096,
                format!("role {role}.objective must be 1–4096 bytes"),
            )?;
        }
        if let Some(list) = &p.instructions {
            require(
                list.len() <= 8 && list.iter().all(|s| !s.trim().is_empty() && s.len() <= 4096),
                format!("role {role}.instructions exceeds bounds"),
            )?;
        }
    }
    Ok(())
}

pub fn roles(runtime: &RuntimeConfig, project: &ProjectRoles) -> BTreeSet<String> {
    BUILTINS
        .into_iter()
        .map(str::to_owned)
        .chain(runtime.roles.keys().cloned())
        .chain(runtime.profiles.keys().cloned())
        .chain(project.profiles.keys().cloned())
        .collect()
}
pub fn resolve(
    runtime: &RuntimeConfig,
    project: &ProjectRoles,
    role: &str,
    explicit: Option<&RolePatch>,
) -> Result<ResolvedRoute> {
    runtime.validate()?;
    project.validate()?;
    require(
        roles(runtime, project).contains(role),
        format!("unknown role {role}"),
    )?;
    let mut profile = builtin(role, runtime);
    let mut primary = runtime.roles.get(role).cloned().unwrap_or(RoleConfig {
        provider: String::new(),
        model: None,
        effort: None,
    });
    let mut sources = BTreeMap::new();
    if runtime.roles.contains_key(role) {
        for f in ["provider", "model", "effort"] {
            sources.insert(f.into(), "machine runtime.roles (legacy)".into());
        }
    }
    let mut fallbacks = vec![];
    for (layer, patch) in [
        ("machine profiles", runtime.profiles.get(role)),
        ("project profiles", project.profiles.get(role)),
        ("explicit user", explicit),
    ] {
        let Some(p) = patch else { continue };
        validate_patches(&BTreeMap::from([(role.to_owned(), p.clone())]))
            .map_err(|e| Error::Invalid(format!("{layer}: {e}")))?;
        macro_rules! set {
            ($field:ident) => {
                if let Some(value) = &p.$field {
                    profile.$field = value.clone();
                    sources.insert(stringify!($field).into(), layer.into());
                }
            };
        }
        set!(objective);
        set!(instructions);
        set!(max_fallback_attempts);
        // Security-relevant fields: repository policy may only tighten what the
        // machine/user layers established; it can never grant itself authority.
        let repository = layer == "project profiles";
        let mut record = |field: &str| {
            sources.insert(field.into(), layer.into());
        };
        if let Some(value) = p.context_bytes {
            profile.context_bytes = if repository {
                profile.context_bytes.min(value)
            } else {
                value
            };
            record("context_bytes");
        }
        if let Some(value) = p.timeout_ms {
            profile.timeout_ms = if repository {
                profile.timeout_ms.min(value)
            } else {
                value
            };
            record("timeout_ms");
        }
        if let Some(value) = p.read_only {
            profile.read_only = if repository {
                profile.read_only || value
            } else {
                value
            };
            record("read_only");
        }
        if let Some(value) = p.network {
            profile.network = if repository {
                profile.network && value
            } else {
                value
            };
            record("network");
        }
        if let Some(value) = p.advisory_tokens {
            profile.advisory_tokens = Some(value);
            sources.insert("advisory_tokens".into(), layer.into());
        }
        if let Some(value) = &p.provider {
            primary.provider = value.clone();
            sources.insert("provider".into(), layer.into());
        }
        if let Some(value) = &p.model {
            primary.model = Some(value.clone());
            sources.insert("model".into(), layer.into());
        }
        if let Some(value) = &p.effort {
            primary.effort = Some(value.clone());
            sources.insert("effort".into(), layer.into());
        }
        if let Some(value) = &p.fallbacks {
            fallbacks = value.clone();
            sources.insert("fallbacks".into(), layer.into());
        }
    }
    require(
        !primary.provider.is_empty(),
        format!("role {role}: configure a provider in runtime.roles or runtime.profiles"),
    )?;
    require(
        role == "executor" || profile.read_only,
        format!("role {role}.read_only: non-executors cannot gain write permission"),
    )?;
    profile.read_only |= project.read_only;
    profile.network &= !project.deny_network;
    profile.context_bytes = profile
        .context_bytes
        .min(project.max_context_bytes.unwrap_or(262144));
    profile.timeout_ms = profile.timeout_ms.min(runtime.timeout_ms);
    let allowed = |r: &RoleConfig| {
        project
            .allowed_providers
            .as_ref()
            .is_none_or(|s| s.contains(&r.provider))
    };
    let explicit_provider = explicit.and_then(|p| p.provider.as_ref()).is_some();
    require(
        !explicit_provider || allowed(&primary),
        format!("role {role}: project forbids explicit provider override"),
    )?;
    // Explicitly choosing a configured alternative promotes it to primary,
    // rather than attempting the identical route twice.
    if explicit_provider {
        fallbacks.retain(|r| r != &primary);
    }
    let mut seen = BTreeSet::new();
    for route in std::iter::once(&primary).chain(&fallbacks) {
        validate_route(route)?;
        require(
            runtime.providers.contains_key(&route.provider),
            format!("role {role}: unconfigured provider {}", route.provider),
        )?;
        require(
            seen.insert(serde_json::to_string(route)?),
            format!("role {role}: duplicate/cyclic fallback route"),
        )?;
    }
    let configured_primary = primary.clone();
    let (mut candidates, policy_skipped): (Vec<_>, Vec<_>) =
        std::iter::once(primary).chain(fallbacks).partition(allowed);
    require(
        !candidates.is_empty(),
        format!("role {role}: all configured candidates forbidden by project policy"),
    )?;
    let primary = candidates.remove(0);
    let fallbacks = candidates;
    Ok(ResolvedRoute {
        configured_primary,
        policy_skipped,
        profile,
        primary,
        fallbacks,
        sources,
        project_policy_hash: planning::hash(project)?,
    })
}

pub fn validate_capabilities(c: &provider::Capabilities, route: &RoleConfig) -> Result<()> {
    if !c.fresh_session
        || !c.structured_output
        || (route.model.is_some() && !c.model)
        || (route.effort.is_some() && !c.effort)
    {
        return Err(Error::ProviderAvailability(
            FailureClass::CapabilityUnsupported,
        ));
    }
    Ok(())
}
