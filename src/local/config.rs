use std::{collections::BTreeMap, path::Path};

use serde::{Deserialize, Serialize};

use super::{Error, Result, paths, require};
use crate::{Validate, protocol::CommandSpec, validation::repo_path};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineConfig {
    pub version: u32,
    pub busy_timeout_ms: u64,
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self {
            version: 1,
            busy_timeout_ms: 5000,
        }
    }
}

impl MachineConfig {
    pub fn validate(&self) -> Result<()> {
        require(
            self.version == 1,
            format!(
                "unsupported machine config version {}; expected 1",
                self.version
            ),
        )?;
        require(
            (1..=60_000).contains(&self.busy_timeout_ms),
            "busy_timeout_ms must be between 1 and 60000",
        )
    }

    pub fn load(path: &Path) -> Result<Self> {
        let value: Self = parse(path)?;
        value
            .validate()
            .map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))?;
        Ok(value)
    }

    pub fn initialize(path: &Path) -> Result<Self> {
        paths::create_default(
            path,
            &toml::to_string_pretty(&Self::default()).map_err(|e| Error::Invalid(e.to_string()))?,
        )?;
        Self::load(path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Declaration {
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtectedRule {
    pub path: String,
    pub deny_read: bool,
    pub deny_write: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationDefinition {
    pub description: String,
    /// IDs in the project's commands table; declarations do not execute anything.
    pub command_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    pub version: u32,
    pub display_name: Option<String>,
    #[serde(default)]
    pub invariants: BTreeMap<String, Declaration>,
    #[serde(default)]
    pub architecture: BTreeMap<String, Declaration>,
    #[serde(default)]
    pub commands: BTreeMap<String, CommandSpec>,
    #[serde(default)]
    pub protected: Vec<ProtectedRule>,
    #[serde(default)]
    pub verification: BTreeMap<String, VerificationDefinition>,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            version: 1,
            display_name: None,
            invariants: BTreeMap::new(),
            architecture: BTreeMap::new(),
            commands: BTreeMap::new(),
            protected: vec![],
            verification: BTreeMap::new(),
        }
    }
}

impl ProjectConfig {
    pub fn validate(&self) -> Result<()> {
        require(
            self.version == 1,
            format!(
                "unsupported project config version {}; expected 1",
                self.version
            ),
        )?;
        if let Some(name) = &self.display_name {
            require(!name.trim().is_empty(), "display_name cannot be blank")?;
        }
        for declarations in [&self.invariants, &self.architecture] {
            for (id, declaration) in declarations {
                valid_key(id)?;
                require(
                    !declaration.description.trim().is_empty(),
                    format!("{id}: description cannot be blank"),
                )?;
            }
        }
        for (id, command) in &self.commands {
            valid_key(id)?;
            command.validate()?;
            if command.cwd != "." {
                repo_path(&command.cwd)?;
            }
        }
        for rule in &self.protected {
            repo_path(&rule.path)?;
            require(
                rule.deny_read || rule.deny_write,
                "protected rules must deny reading or writing",
            )?;
            require(
                !rule.reason.trim().is_empty(),
                "protected rule requires a reason",
            )?;
        }
        for (id, check) in &self.verification {
            valid_key(id)?;
            require(
                !check.description.trim().is_empty(),
                format!("{id}: verification description cannot be blank"),
            )?;
            for command in &check.command_refs {
                require(
                    self.commands.contains_key(command),
                    format!("verification {id} references unknown command {command}"),
                )?;
            }
        }
        Ok(())
    }

    pub fn load(root: &Path) -> Result<Self> {
        let directory = root.join(".agentctl");
        let metadata = std::fs::symlink_metadata(&directory).map_err(|e| {
            Error::Invalid(format!(
                "{}: {e}; run agentctl repo init",
                directory.display()
            ))
        })?;
        require(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            format!("{}: refusing symlink/non-directory", directory.display()),
        )?;
        let path = paths::project_config(root);
        let value: Self = parse(&path)?;
        value
            .validate()
            .map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))?;
        Ok(value)
    }

    pub fn initialize(root: &Path) -> Result<Self> {
        paths::ensure_directory(&root.join(".agentctl"))?;
        let default =
            toml::to_string_pretty(&Self::default()).map_err(|e| Error::Invalid(e.to_string()))?;
        paths::create_default(&paths::project_config(root), &default)?;
        Self::load(root)
    }
}

fn valid_key(id: &str) -> Result<()> {
    crate::protocol::TaskId::new(id)
        .map(|_| ())
        .map_err(Error::Invalid)
}

fn parse<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    toml::from_str(&paths::read_text(path)?)
        .map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))
}
