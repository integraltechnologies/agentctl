//! `agentctl init`: bootstrap a new project or hydrate an existing one.

use std::fmt::Display;
use std::io::{BufRead, Write};
use std::num::NonZeroU32;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Result, bail};

use crate::config::{Agents, CodeGraph, Config, ProjectInfo, ReasoningEffort, Role, Text};
use crate::project::{CONFIG_FILE, Project};

/// Runs `init` from `cwd`. `provider_available` is the lightweight machine
/// readiness check; it never affects whether a configuration is valid.
pub fn run<R: BufRead, W: Write>(
    cwd: &Path,
    prompt: &mut Prompter<R, W>,
    provider_available: impl Fn(&str) -> bool,
) -> Result<()> {
    let project = match Project::discover(cwd)? {
        Some(project) => {
            project.hydrate()?;
            prompt.say(format_args!(
                "Found existing project at {}; local state is ready.",
                project.root.display()
            ))?;
            for provider in unavailable_providers(&project.config, &provider_available) {
                prompt.say(format_args!("warning: {}", unavailable_message(provider)))?;
            }
            project
        }
        None => {
            // A new project belongs at the repository root when there is one.
            let root = cwd
                .ancestors()
                .find(|dir| dir.join(".git").exists())
                .unwrap_or(cwd);
            let config = ask_config(prompt, root)?;
            let missing = unavailable_providers(&config, &provider_available);
            for provider in &missing {
                prompt.say(format_args!("warning: {}", unavailable_message(provider)))?;
            }
            if !missing.is_empty() && !prompt.confirm("Continue initialization anyway?", false)? {
                bail!("initialization cancelled; nothing was written");
            }
            let project = Project::create(root, config)?;
            project.hydrate()?;
            prompt.say(format_args!(
                "Wrote {}.",
                project.root.join(CONFIG_FILE).display()
            ))?;
            project
        }
    };

    if prompt.confirm("Build the initial repository index now?", true)? {
        build_initial_index(&project, prompt)?;
    }
    Ok(())
}

/// Attachment point for CodeGraph's initial index build.
fn build_initial_index<R: BufRead, W: Write>(
    _project: &Project,
    prompt: &mut Prompter<R, W>,
) -> Result<()> {
    prompt.say("Repository indexing is not available in this version; no index was built.")
}

fn ask_config<R: BufRead, W: Write>(prompt: &mut Prompter<R, W>, root: &Path) -> Result<Config> {
    let dir_name = root
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.parse().ok());
    let name = prompt.ask("Project name", dir_name)?;
    let version = prompt.ask("Project version", "0.1.0".parse().ok())?;
    let roots = prompt.ask(
        "CodeGraph source roots (comma-separated)",
        "src".parse().ok(),
    )?;
    let codegraph = CodeGraph { roots };
    for root_dir in codegraph.roots.iter() {
        if !root.join(root_dir.as_str()).is_dir() {
            prompt.say(format_args!(
                "warning: source root `{}` does not exist yet",
                root_dir.as_str()
            ))?;
        }
    }

    let planner = ask_role(
        prompt,
        "planner",
        "claude".parse().ok(),
        "claude-opus-5-5".parse().ok(),
        ReasoningEffort::High,
    )?;
    let executor = ask_role(
        prompt,
        "executor",
        Some(planner.provider.clone()),
        Some(planner.model.clone()),
        ReasoningEffort::Medium,
    )?;
    let verifier = ask_role(
        prompt,
        "verifier",
        Some(planner.provider.clone()),
        Some(planner.model.clone()),
        ReasoningEffort::High,
    )?;
    let max_concurrency = prompt.ask("Maximum concurrent agents", NonZeroU32::new(4))?;

    Ok(Config {
        project: ProjectInfo { name, version },
        codegraph,
        agents: Agents {
            max_concurrency,
            planner,
            executor,
            verifier,
        },
    })
}

fn ask_role<R: BufRead, W: Write>(
    prompt: &mut Prompter<R, W>,
    role: &str,
    provider: Option<Text>,
    model: Option<Text>,
    effort: ReasoningEffort,
) -> Result<Role> {
    Ok(Role {
        provider: prompt.ask(&format!("{role} provider"), provider)?,
        model: prompt.ask(&format!("{role} model"), model)?,
        reasoning_effort: prompt.ask(
            &format!("{role} reasoning effort (minimal|low|medium|high|xhigh|max)"),
            Some(effort),
        )?,
    })
}

fn unavailable_providers(config: &Config, provider_available: impl Fn(&str) -> bool) -> Vec<&str> {
    let mut missing: Vec<&str> = Vec::new();
    for role in config.agents.roles() {
        let provider = role.provider.as_str();
        if !missing.contains(&provider) && !provider_available(provider) {
            missing.push(provider);
        }
    }
    missing
}

fn unavailable_message(provider: &str) -> String {
    format!("provider `{provider}` appears unavailable (no `{provider}` executable on PATH)")
}

/// Line-oriented interactive prompts.
pub struct Prompter<R, W> {
    input: R,
    output: W,
}

impl<R: BufRead, W: Write> Prompter<R, W> {
    pub fn new(input: R, output: W) -> Self {
        Self { input, output }
    }

    fn say(&mut self, message: impl Display) -> Result<()> {
        writeln!(self.output, "{message}")?;
        Ok(())
    }

    fn read_answer(&mut self, question: &str) -> Result<String> {
        write!(self.output, "{question} ")?;
        self.output.flush()?;
        let mut line = String::new();
        if self.input.read_line(&mut line)? == 0 {
            bail!("input ended before initialization finished");
        }
        Ok(line.trim().to_owned())
    }

    /// Asks until the answer parses; an empty answer accepts the default.
    fn ask<T>(&mut self, label: &str, default: Option<T>) -> Result<T>
    where
        T: FromStr + Display,
        T::Err: Display,
    {
        let question = match &default {
            Some(d) => format!("{label} [{d}]:"),
            None => format!("{label}:"),
        };
        let mut default = default;
        loop {
            let answer = self.read_answer(&question)?;
            if answer.is_empty() {
                if let Some(d) = default.take() {
                    return Ok(d);
                }
                continue;
            }
            match answer.parse() {
                Ok(value) => return Ok(value),
                Err(e) => self.say(format_args!("  invalid value: {e}"))?,
            }
        }
    }

    fn confirm(&mut self, question: &str, default: bool) -> Result<bool> {
        let question = format!("{question} {}", if default { "[Y/n]" } else { "[y/N]" });
        loop {
            match self.read_answer(&question)?.to_lowercase().as_str() {
                "" => return Ok(default),
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => self.say("  please answer y or n")?,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::sample;
    use crate::project::STATE_DIR;
    use std::fs;

    fn init(cwd: &Path, input: &str, available: bool) -> (Result<()>, String) {
        let mut output = Vec::new();
        let mut prompt = Prompter::new(input.as_bytes(), &mut output);
        let result = run(cwd, &mut prompt, |_| available);
        (result, String::from_utf8(output).unwrap())
    }

    #[test]
    fn new_project_accepts_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("my-repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join("src/sub")).unwrap();

        // 13 accepted defaults, then decline indexing.
        let (result, output) = init(&repo.join("src/sub"), &("\n".repeat(13) + "n\n"), true);
        result.unwrap();

        let project = Project::load(&repo).unwrap();
        assert_eq!(project.config.project.name.as_str(), "my-repo");
        assert_eq!(project.config.codegraph.roots.to_string(), "src");
        assert_eq!(
            project.config.agents.executor.reasoning_effort,
            ReasoningEffort::Medium
        );
        assert!(repo.join(STATE_DIR).is_dir());
        assert!(!output.contains("warning"), "{output}");
    }

    #[test]
    fn invalid_answers_are_asked_again() {
        let dir = tempfile::tempdir().unwrap();
        let input =
            "demo\n\n../x\nsrc, lib\n\n\nturbo\n\n".to_owned() + &"\n".repeat(6) + "0\n2\nn\n";
        let (result, output) = init(dir.path(), &input, true);
        result.unwrap();

        let config = Project::load(dir.path()).unwrap().config;
        assert_eq!(config.codegraph.roots.to_string(), "src, lib");
        assert_eq!(config.agents.max_concurrency.get(), 2);
        assert_eq!(output.matches("invalid value").count(), 3, "{output}");
    }

    #[test]
    fn unavailable_provider_can_cancel_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let (result, output) = init(dir.path(), &("\n".repeat(13) + "n\n"), false);
        assert!(result.is_err());
        assert_eq!(output.matches("appears unavailable").count(), 1, "{output}");
        assert!(!dir.path().join(CONFIG_FILE).exists());
        assert!(!dir.path().join(STATE_DIR).exists());
    }

    #[test]
    fn existing_project_is_hydrated_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let original = sample().to_toml();
        fs::write(dir.path().join(CONFIG_FILE), &original).unwrap();

        // Only the indexing question is asked; providers being unavailable
        // warns but does not invalidate the configuration.
        let (result, output) = init(dir.path(), "y\n", false);
        result.unwrap();

        assert_eq!(
            fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap(),
            original
        );
        assert!(dir.path().join(STATE_DIR).is_dir());
        assert!(output.contains("appears unavailable"), "{output}");
        assert!(output.contains("no index was built"), "{output}");
    }

    #[test]
    fn existing_invalid_project_is_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(CONFIG_FILE), "bogus = 1\n").unwrap();
        let (result, _) = init(dir.path(), "", true);
        assert!(result.is_err());
        assert!(!dir.path().join(STATE_DIR).exists());
    }
}
