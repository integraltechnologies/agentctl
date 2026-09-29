//! The portable, version-controlled project configuration (`agentctl.toml`).
//!
//! Every field is validated by its type during deserialization, so a parsed
//! `Config` is always valid. Validity never depends on the current machine.

use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroU32;
use std::str::FromStr;
use std::time::Duration;

use serde::de::IntoDeserializer;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub project: ProjectInfo,
    pub codegraph: CodeGraph,
    pub agents: Agents,
    /// Provider declarations, by the name roles refer to. `claude` and
    /// `codex` are implicit unless declared here.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<Text, ProviderDef>,
}

/// A declared provider: which adapter speaks to which command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderDef {
    pub adapter: Adapter,
    /// The executable: a path, or a name found on `PATH`.
    pub command: Text,
    /// Arguments placed before the adapter's own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Exact names of environment variables the provider may see, beyond
    /// what every provider gets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<Text>,
}

/// The wire protocol a provider's command is run with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Adapter {
    Claude,
    Codex,
    /// agentctl's external-agent protocol, version 1.
    Generic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectInfo {
    pub name: Text,
    pub version: Text,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeGraph {
    /// The only directories CodeGraph may index.
    pub roots: SourceRoots,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Agents {
    pub max_concurrency: NonZeroU32,
    /// How many minutes one invocation of any role may run before agentctl
    /// ends it; two hours when not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation_timeout_minutes: Option<NonZeroU32>,
    pub planner: Role,
    pub executor: Role,
    pub verifier: Role,
}

impl Agents {
    pub fn roles(&self) -> [&Role; 3] {
        [&self.planner, &self.executor, &self.verifier]
    }

    /// How long one invocation of any role may run.
    pub fn invocation_timeout(&self) -> Duration {
        match self.invocation_timeout_minutes {
            Some(minutes) => Duration::from_secs(u64::from(minutes.get()) * 60),
            None => crate::runtime::ROLE_TIMEOUT,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Role {
    /// Provider-neutral identifier; interpreted only by provider adapters.
    pub provider: Text,
    pub model: Text,
    pub reasoning_effort: ReasoningEffort,
}

impl Config {
    /// Parses and validates `text`: beyond its shape, every role's
    /// provider must be declared or implicit.
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        let config: Self = toml::from_str(text)?;
        config
            .validate()
            .map_err(<toml::de::Error as serde::de::Error>::custom)?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        for (name, def) in &self.providers {
            if let Some(env) = def.env.iter().find(|n| n.as_str().contains('=')) {
                return Err(format!(
                    "provider `{name}` names environment variable `{env}`, which contains `=`"
                ));
            }
        }
        for (role, config) in ["planner", "executor", "verifier"]
            .into_iter()
            .zip(self.agents.roles())
        {
            let name = config.provider.as_str();
            if self.provider(name).is_none() {
                return Err(format!(
                    "the {role} role uses provider `{name}`, which is neither claude, codex \
                     nor declared under [providers.{name}]"
                ));
            }
        }
        Ok(())
    }

    /// The provider called `name`: as declared, or else the implicit
    /// built-in of that name, whose command is its own name.
    pub fn provider(&self, name: &str) -> Option<ProviderDef> {
        let implicit = |adapter| ProviderDef {
            adapter,
            command: name.parse().expect("a provider name is text"),
            args: Vec::new(),
            env: Vec::new(),
        };
        self.providers.get(name).cloned().or(match name {
            "claude" => Some(implicit(Adapter::Claude)),
            "codex" => Some(implicit(Adapter::Codex)),
            _ => None,
        })
    }

    pub fn to_toml(&self) -> String {
        toml::to_string(self).expect("configuration always serializes to TOML")
    }
}

/// Canonical reasoning effort. Provider adapters translate these levels or
/// reject the ones their provider cannot honor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl FromStr for ReasoningEffort {
    type Err = serde::de::value::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::deserialize(s.into_deserializer())
    }
}

impl fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        })
    }
}

/// Non-empty text without surrounding whitespace.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Text(String);

impl Text {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for Text {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Text {
    type Error = String;

    fn try_from(s: String) -> Result<Self, String> {
        if s.trim().is_empty() {
            Err("must not be empty".into())
        } else if s.trim() != s {
            Err(format!(
                "`{s}` must not have leading or trailing whitespace"
            ))
        } else {
            Ok(Self(s))
        }
    }
}

impl FromStr for Text {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        s.to_owned().try_into()
    }
}

impl From<Text> for String {
    fn from(t: Text) -> Self {
        t.0
    }
}

impl fmt::Display for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A portable project-relative directory, normalized to `/`-separated
/// components (`.` is the project root itself).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SourceRoot(String);

impl SourceRoot {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the canonical project-relative `path` lies within this root.
    pub fn contains_path(&self, path: &str) -> bool {
        self.0 == "." || self.0 == path || path.starts_with(&format!("{}/", self.0))
    }

    fn contains(&self, other: &Self) -> bool {
        self.contains_path(&other.0)
    }
}

impl FromStr for SourceRoot {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        if s.trim().is_empty() {
            return Err("source root must not be empty".into());
        }
        if s.contains('\\') {
            return Err(format!("source root `{s}` must use `/` as the separator"));
        }
        if s.starts_with('/') || s.contains(':') {
            return Err(format!(
                "source root `{s}` must be relative to the project root"
            ));
        }
        let parts: Vec<&str> = s
            .split('/')
            .filter(|p| !p.is_empty() && *p != ".")
            .collect();
        if parts.contains(&"..") {
            return Err(format!("source root `{s}` must stay inside the project"));
        }
        Ok(Self(if parts.is_empty() {
            ".".into()
        } else {
            parts.join("/")
        }))
    }
}

impl TryFrom<String> for SourceRoot {
    type Error = String;

    fn try_from(s: String) -> Result<Self, String> {
        s.parse()
    }
}

impl From<SourceRoot> for String {
    fn from(r: SourceRoot) -> Self {
        r.0
    }
}

/// One or more non-overlapping source roots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<SourceRoot>", into = "Vec<SourceRoot>")]
pub struct SourceRoots(Vec<SourceRoot>);

impl SourceRoots {
    pub fn iter(&self) -> impl Iterator<Item = &SourceRoot> {
        self.0.iter()
    }
}

impl TryFrom<Vec<SourceRoot>> for SourceRoots {
    type Error = String;

    fn try_from(roots: Vec<SourceRoot>) -> Result<Self, String> {
        if roots.is_empty() {
            return Err("at least one source root is required".into());
        }
        for (i, a) in roots.iter().enumerate() {
            for b in &roots[i + 1..] {
                if a.contains(b) || b.contains(a) {
                    return Err(format!("source roots `{}` and `{}` overlap", a.0, b.0));
                }
            }
        }
        Ok(Self(roots))
    }
}

impl From<SourceRoots> for Vec<SourceRoot> {
    fn from(r: SourceRoots) -> Self {
        r.0
    }
}

/// Comma-separated list, as entered interactively.
impl FromStr for SourceRoots {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        s.split(',')
            .map(|r| r.trim().parse())
            .collect::<Result<Vec<_>, _>>()?
            .try_into()
    }
}

impl fmt::Display for SourceRoots {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let roots: Vec<&str> = self.iter().map(SourceRoot::as_str).collect();
        f.write_str(&roots.join(", "))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sample() -> Config {
        let role = |effort| Role {
            provider: "claude".parse().unwrap(),
            model: "claude-opus-5-5".parse().unwrap(),
            reasoning_effort: effort,
        };
        Config {
            project: ProjectInfo {
                name: "demo".parse().unwrap(),
                version: "0.1.0".parse().unwrap(),
            },
            codegraph: CodeGraph {
                roots: "src, crates/core".parse().unwrap(),
            },
            agents: Agents {
                max_concurrency: NonZeroU32::new(4).unwrap(),
                invocation_timeout_minutes: None,
                planner: role(ReasoningEffort::High),
                executor: role(ReasoningEffort::Medium),
                verifier: role(ReasoningEffort::Xhigh),
            },
            providers: BTreeMap::new(),
        }
    }

    fn with_provider(name: &str) -> Config {
        let mut config = sample();
        config.agents.executor.provider = name.parse().unwrap();
        config
    }

    #[test]
    fn round_trips_through_toml() {
        let config = sample();
        assert_eq!(Config::parse(&config.to_toml()).unwrap(), config);
    }

    fn rejects(replace: &str, with: &str, expected: &str) {
        let text = sample().to_toml();
        assert!(text.contains(replace), "fixture lacks {replace:?}");
        let err = Config::parse(&text.replacen(replace, with, 1))
            .unwrap_err()
            .to_string();
        assert!(err.contains(expected), "{err}");
    }

    #[test]
    fn rejects_invalid_configuration() {
        rejects("name = \"demo\"", "name = \" \"", "must not be empty");
        rejects("\"src\"", "\"../src\"", "must stay inside the project");
        rejects("\"src\"", "\"/abs\"", "must be relative");
        rejects("\"src\"", "\"crates\"", "overlap");
        rejects(
            "[\"src\", \"crates/core\"]",
            "[]",
            "at least one source root",
        );
        rejects("max_concurrency = 4", "max_concurrency = 0", "nonzero");
        rejects("\"high\"", "\"extreme\"", "unknown variant `extreme`");
        rejects("model = ", "modle = ", "unknown field `modle`");
        rejects(
            "[agents.verifier]",
            "[agents.reviewer]",
            "unknown field `reviewer`",
        );
    }

    #[test]
    fn built_in_providers_are_implicit() {
        let config = sample();
        let claude = config.provider("claude").unwrap();
        assert_eq!(
            (claude.adapter, claude.command.as_str()),
            (Adapter::Claude, "claude")
        );
        let codex = config.provider("codex").unwrap();
        assert_eq!(
            (codex.adapter, codex.command.as_str()),
            (Adapter::Codex, "codex")
        );
        assert!(config.provider("local").is_none());
    }

    #[test]
    fn declared_providers_resolve_and_round_trip() {
        let mut config = with_provider("local");
        let def = |adapter, command: &str| ProviderDef {
            adapter,
            command: command.parse().unwrap(),
            args: vec!["run".into()],
            env: vec!["MY_RUNTIME_HOME".parse().unwrap()],
        };
        config
            .providers
            .insert("local".parse().unwrap(), def(Adapter::Generic, "my-agent"));
        config
            .providers
            .insert("work".parse().unwrap(), def(Adapter::Claude, "claude"));
        let text = config.to_toml();
        assert!(text.contains("[providers.local]"), "{text}");
        let parsed = Config::parse(&text).unwrap();
        assert_eq!(parsed, config);
        assert_eq!(
            parsed.provider("local"),
            config.providers.get("local").cloned()
        );
        assert_eq!(parsed.provider("work").unwrap().adapter, Adapter::Claude);
    }

    #[test]
    fn a_role_needs_a_known_provider() {
        let text = with_provider("local").to_toml();
        let err = Config::parse(&text).unwrap_err().to_string();
        assert!(err.contains("executor role uses provider `local`"), "{err}");
        let declared =
            format!("{text}\n[providers.local]\nadapter = \"generic\"\ncommand = \"my-agent\"\n");
        let config = Config::parse(&declared).unwrap();
        assert!(config.providers.get("local").unwrap().args.is_empty());
    }

    #[test]
    fn provider_declarations_are_strict() {
        let declared = |body: &str| {
            let mut config = with_provider("local").to_toml();
            config.push_str(&format!("\n[providers.local]\n{body}"));
            Config::parse(&config).unwrap_err().to_string()
        };
        let base = "adapter = \"generic\"\ncommand = \"x\"\n";
        assert!(declared(&format!("{base}model = \"m\"")).contains("unknown field `model`"));
        assert!(declared("adapter = \"gemini\"\ncommand = \"x\"").contains("unknown variant"));
        assert!(declared("adapter = \"generic\"").contains("missing field `command`"));
        assert!(declared(&format!("{base}env = [\"A=B\"]")).contains("contains `=`"));
    }

    #[test]
    fn normalizes_source_roots() {
        let roots: SourceRoots = "./src/, lib//x".parse().unwrap();
        assert_eq!(roots.to_string(), "src, lib/x");
        assert_eq!(".".parse::<SourceRoot>().unwrap().as_str(), ".");
    }
}
