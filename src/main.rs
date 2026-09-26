use std::io;

use agentctl::project::Project;
use agentctl::state::PlanId;
use agentctl::{init, scheduler};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

/// Local, provider-agnostic engineering control plane.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a project here, or hydrate local state for an existing one.
    Init,
    /// Run an existing ready or running plan's tasks, as far as they can
    /// go now: each eligible task through its executor, an independent
    /// verifier and acceptance, within the configured concurrency.
    Run {
        /// The plan to run.
        plan: PlanId,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Init => {
            let mut prompt = init::Prompter::new(io::stdin().lock(), io::stdout().lock());
            init::run(&std::env::current_dir()?, &mut prompt, |provider| {
                which::which(provider).is_ok()
            })
        }
        Command::Run { plan } => {
            let cwd = std::env::current_dir()?;
            let project = Project::discover(&cwd)?.context("no agentctl project here")?;
            let report = scheduler::run(&project, plan, None)?;
            for end in &report.finished {
                let work = &end.work;
                println!(
                    "task {} generation {}: {:?}{}",
                    work.task,
                    work.generation,
                    end.release,
                    end.error
                        .as_deref()
                        .map(|e| format!(" ({e})"))
                        .unwrap_or_default()
                );
            }
            for (task, status) in &report.snapshot.tasks {
                println!("task {task}: {status:?}");
            }
            println!(
                "plan {plan}: {} ({:?})",
                report.snapshot.state,
                report.snapshot.condition()
            );
            if let Some(why) = report.stopped {
                anyhow::bail!("scheduling stopped early: {why}");
            }
            Ok(())
        }
    }
}
