use std::io;

use agentctl::planner::{self, Planned};
use agentctl::project::Project;
use agentctl::state::{ConcernId, Decided, HumanDecision, PlanId, PlanState, Store};
use agentctl::{init, scheduler};
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};

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
    /// Replan an existing ready, running or paused plan, or one whose
    /// planner awaits your decisions: its planner is given canonical
    /// feedback on how the plan's work went, and what it proposes is
    /// applied as one change, or not at all. Nothing runs: `agentctl run`
    /// runs whatever the replan made eligible.
    Update {
        /// The plan to replan.
        plan: PlanId,
    },
    /// Show a plan's state and every concern its planner raised for your
    /// decision, with its reason, evidence and affected tasks.
    Attention {
        /// The plan to show.
        plan: PlanId,
    },
    /// Decide a concern of a plan that needs attention, once: accept it and
    /// let the plan continue unchanged, instruct its planner, or stop the
    /// plan for good.
    Decide {
        /// The plan the concern is of.
        plan: PlanId,
        /// The concern, by the number `plan attention` shows.
        concern: ConcernId,
        decision: Decision,
        /// What the planner is to do, for `instruct`, exactly as given.
        #[arg(long, required_if_eq("decision", "instruct"))]
        instruction: Option<String>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Decision {
    Accept,
    Instruct,
    Stop,
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
            command: PlanCommand::Attention { plan },
        } => {
            let cwd = std::env::current_dir()?;
            let project = Project::discover(&cwd)?.context("no agentctl project here")?;
            show_attention(&project.hydrate()?, plan)
        }
        Command::Plan {
            command:
                PlanCommand::Decide {
                    plan,
                    concern,
                    decision,
                    instruction,
                },
        } => {
            let decision = match (decision, instruction) {
                (Decision::Accept, None) => HumanDecision::Accept,
                (Decision::Instruct, Some(text)) => HumanDecision::Instruct(text),
                (Decision::Stop, None) => HumanDecision::Stop,
                _ => bail!("only `instruct` takes an instruction"),
            };
            let cwd = std::env::current_dir()?;
            let project = Project::discover(&cwd)?.context("no agentctl project here")?;
            let mut store = project.hydrate()?;
            if store.decide(plan, concern, &decision)? == Decided::AlreadyRecorded {
                println!("concern {concern} was already decided so; nothing changed");
            }
            show_attention(&store, plan)
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
                    let state = store.plan(plan)?.state;
                    println!("plan {plan}: replan {replan} applied; {state}");
                    if state == PlanState::NeedsAttention {
                        show_attention(&store, plan)?;
                    }
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

/// Prints `plan`'s state, its concerns and what continues it.
fn show_attention(store: &Store, plan: PlanId) -> Result<()> {
    let state = store.plan(plan)?.state;
    let concerns = store.attention(plan)?;
    for c in &concerns {
        let decision = match &c.decision {
            None => "undecided".to_owned(),
            Some((HumanDecision::Accept, _)) => "accepted".to_owned(),
            Some((HumanDecision::Instruct(text), _)) => format!("instructed: {text}"),
            Some((HumanDecision::Stop, _)) => "stopped".to_owned(),
        };
        println!(
            "concern {} `{}` (replan {}): {decision}",
            c.id, c.key, c.replan
        );
        println!("  reason: {}", c.reason);
        for item in &c.evidence {
            println!("  evidence: {item}");
        }
        if !c.tasks.is_empty() {
            println!("  tasks: {}", c.tasks.join(", "));
        }
    }
    println!("plan {plan}: {state}");
    if state == PlanState::NeedsAttention {
        let blocking = concerns.iter().filter(|c| c.blocks()).count();
        if concerns
            .iter()
            .any(|c| matches!(c.decision, Some((HumanDecision::Stop, _))))
        {
            println!("stopped by its human: it never continues autonomously");
        } else if blocking > 0 {
            println!("{blocking} concerns await `agentctl plan decide {plan} <concern> ...`");
        } else {
            println!("an instruction awaits its planner: `agentctl plan update {plan}`");
        }
    }
    Ok(())
}
