//! Canonical discovery and loading of an agentctl project.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::Config;

pub const CONFIG_FILE: &str = "agentctl.toml";
pub const STATE_DIR: &str = ".agentctl";

#[derive(Debug)]
pub struct Project {
    pub root: PathBuf,
    pub config: Config,
}

impl Project {
    /// Loads the project owning `start`: the nearest ancestor directory
    /// (inclusive) containing `agentctl.toml`. `start` must be absolute.
    pub fn discover(start: &Path) -> Result<Option<Self>> {
        match start
            .ancestors()
            .find(|dir| dir.join(CONFIG_FILE).is_file())
        {
            Some(root) => Self::load(root).map(Some),
            None => Ok(None),
        }
    }

    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join(CONFIG_FILE);
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let config = Config::parse(&text).with_context(|| format!("invalid {}", path.display()))?;
        Ok(Self {
            root: root.to_path_buf(),
            config,
        })
    }

    /// Creates `agentctl.toml` for a new project. Never overwrites.
    pub fn create(root: &Path, config: Config) -> Result<Self> {
        let path = root.join(CONFIG_FILE);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        file.write_all(config.to_toml().as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(Self {
            root: root.to_path_buf(),
            config,
        })
    }

    /// Ensures the local, Git-ignored `.agentctl/` state directory exists.
    pub fn hydrate(&self) -> Result<()> {
        let state = self.root.join(STATE_DIR);
        fs::create_dir_all(&state).with_context(|| format!("creating {}", state.display()))?;
        ignore_state_dir(&self.root)
    }
}

/// Appends `/.agentctl/` to `.gitignore` unless an equivalent entry exists.
fn ignore_state_dir(root: &Path) -> Result<()> {
    let path = root.join(".gitignore");
    let existing = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let ignored = existing.lines().any(|line| {
        matches!(
            line.trim(),
            ".agentctl" | ".agentctl/" | "/.agentctl" | "/.agentctl/"
        )
    });
    if ignored {
        return Ok(());
    }
    let separator = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut file| writeln!(file, "{separator}/{STATE_DIR}/"))
        .with_context(|| format!("updating {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::sample;

    #[test]
    fn discovers_project_from_nested_directory() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("src/deep");
        fs::create_dir_all(&nested).unwrap();
        assert!(Project::discover(&nested).unwrap().is_none());

        Project::create(dir.path(), sample()).unwrap();
        let project = Project::discover(&nested).unwrap().unwrap();
        assert_eq!(project.root, dir.path());
        assert_eq!(project.config, sample());
    }

    #[test]
    fn discovery_reports_invalid_configuration() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(CONFIG_FILE), "[project]\n").unwrap();
        let err = format!("{:#}", Project::discover(dir.path()).unwrap_err());
        assert!(
            err.contains("invalid") && err.contains("missing field"),
            "{err}"
        );
    }

    #[test]
    fn create_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(CONFIG_FILE), "original").unwrap();
        assert!(Project::create(dir.path(), sample()).is_err());
        assert_eq!(
            fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap(),
            "original"
        );
    }

    #[test]
    fn hydrate_creates_state_dir_and_ignores_it() {
        let dir = tempfile::tempdir().unwrap();
        let project = Project::create(dir.path(), sample()).unwrap();
        project.hydrate().unwrap();
        assert!(dir.path().join(STATE_DIR).is_dir());
        assert_eq!(
            fs::read_to_string(dir.path().join(".gitignore")).unwrap(),
            "/.agentctl/\n"
        );
    }

    #[test]
    fn gitignore_update_is_append_only_and_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let gitignore = dir.path().join(".gitignore");
        fs::write(&gitignore, "# keep\n/target\n!important").unwrap();

        ignore_state_dir(dir.path()).unwrap();
        ignore_state_dir(dir.path()).unwrap();
        assert_eq!(
            fs::read_to_string(&gitignore).unwrap(),
            "# keep\n/target\n!important\n/.agentctl/\n"
        );

        fs::write(&gitignore, "target\r\n.agentctl\r\n").unwrap();
        ignore_state_dir(dir.path()).unwrap();
        assert_eq!(
            fs::read_to_string(&gitignore).unwrap(),
            "target\r\n.agentctl\r\n"
        );
    }
}
