use std::io;

use agentctl::planner::{self, Planned};
use agentctl::project::Project;
use agentctl::state::PlanId;
use agentctl::{init, scheduler};
use anyhow::{Context, Result, bail};
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
    /// Work on existing plans.
    Plan {
        #[command(subcommand)]
        command: PlanCommand,
    },
}

#[derive(Subcommand)]
enum PlanCommand {
    /// Replan an existing ready, running or paused plan: its planner is
    /// given canonical feedback on how the plan's work went, and what it
    /// proposes is applied as one change, or not at all. Nothing runs:
    /// `agentctl run` runs whatever the replan made eligible.
    Update {
        /// The plan to replan.
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
        Command::Plan {
            command: PlanCommand::Update { plan },
        } => {
            let cwd = std::env::current_dir()?;
            let project = Project::discover(&cwd)?.context("no agentctl project here")?;
            let mut store = project.hydrate()?;
            let planning = planner::replan(&project, &mut store, plan, None)?;
            match planning.finish(&project, &mut store)? {
                Planned::Replanned {
                    replan,
                    explanation,
                    ..
                } => {
                    if let Some(explanation) = explanation {
                        println!("{explanation}");
                    }
                    for task in store.tasks(plan)? {
                        println!(
                            "task {} ({}): {:?}",
                            task.id,
                            task.key,
                            store.standing(task.id)?
                        );
                    }
                    println!("plan {plan}: replan {replan} applied");
                    Ok(())
                }
                Planned::Stale { invocation } => bail!(
                    "plan {plan} changed while its planner (invocation {invocation}) worked, \
                     so nothing was applied; update it again"
                ),
                Planned::Refused { invocation, reason } => bail!(
                    "the replan planner invocation {invocation} proposed was refused, \
                     so nothing was applied: {reason:#}"
                ),
                Planned::NoResult(outcome) => bail!(
                    "planner invocation {} ended {} without a result, so nothing was applied",
                    outcome.invocation,
                    outcome.end.state
                ),
                Planned::Applied { .. } => bail!("plan {plan} was planned, not replanned"),
            }
        }
    }
}
