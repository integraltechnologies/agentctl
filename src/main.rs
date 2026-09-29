use std::io::{self, Write as _};
use std::time::Duration;

use agentctl::planner::{self, Planned};
use agentctl::project::Project;
use agentctl::recovery::{self, Outcome};
use agentctl::state::EventQuery;
use agentctl::state::{
    AgentId, ConcernId, Decided, HumanDecision, IntegrationOutcome, IntegrationStatus, PlanId,
    PlanState, Store, TaskId,
};
use agentctl::{init, integration, observe, report, scheduler};
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
    /// Settle what agentctl processes that are no longer running left
    /// interrupted: establish what their actions did as far as can be
    /// proven, and preserve the rest, including any provider invocation
    /// whose lifecycle cannot be proven settled. Nothing is retried, judged or accepted anew. New work waits
    /// for this wherever interrupted work remains.
    Recover,
    /// Show where the project stands, from its recorded state alone: plans,
    /// their tasks, what awaits you, unresolved work and token usage.
    /// Changes nothing and never claims unfinished work is running or done.
    Status,
    /// Show the recorded event log, newest last. Chronology only: `status`
    /// says where things stand.
    Logs {
        /// Only this plan's events.
        #[arg(long)]
        plan: Option<PlanId>,
        /// Only this task's events.
        #[arg(long)]
        task: Option<TaskId>,
        /// Only this agent's events.
        #[arg(long)]
        agent: Option<AgentId>,
        /// Only events of this kind, or of a kind group when it ends in a
        /// dot (`invocation.`).
        #[arg(long)]
        kind: Option<String>,
        /// Show the first events after this sequence number, instead of the
        /// newest.
        #[arg(long)]
        after: Option<i64>,
        /// How many events to show.
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: u32,
        /// Keep waiting for new events until interrupted.
        #[arg(short, long)]
        follow: bool,
    },
}

#[derive(Subcommand)]
enum PlanCommand {
    /// Replan an existing ready, running or paused plan, or one whose
    /// planner awaits your decisions: its planner is given canonical
    /// feedback on how the plan's work went, and what it proposes is
    /// applied as one change, or not at all. Nothing runs: `agentctl run`
    /// runs whatever the replan made eligible. When the planner proposes
    /// that the plan's objective is met, its final integration verification
    /// runs at once.
    Update {
        /// The plan to replan.
        plan: PlanId,
    },
    /// Verify a settled plan whose planner proposed its completion: a fresh,
    /// independent verifier judges its accepted result as a whole, and only
    /// its pass completes the plan. A failure's blockers go to the planner
    /// (`plan update`); nothing is retried.
    Verify {
        /// The plan to verify.
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
        Command::Status => {
            let cwd = std::env::current_dir()?;
            let project = Project::discover(&cwd)?.context("no agentctl project here")?;
            let overview = match project.observe()? {
                Some(store) => Some(observe::overview(
                    &store,
                    project.config.agents.max_concurrency,
                )?),
                None => None,
            };
            print!("{}", report::status(&project.root, overview.as_ref()));
            Ok(())
        }
        Command::Logs {
            plan,
            task,
            agent,
            kind,
            after,
            limit,
            follow,
        } => {
            let cwd = std::env::current_dir()?;
            let project = Project::discover(&cwd)?.context("no agentctl project here")?;
            let query = EventQuery {
                after,
                plan,
                task,
                agent,
                kind,
                limit,
                newest: after.is_none(),
            };
            logs(&project, query, follow)
        }
        Command::Recover => {
            let cwd = std::env::current_dir()?;
            let project = Project::discover(&cwd)?.context("no agentctl project here")?;
            let report = recovery::recover(&project)?;
            if report.clean() {
                println!("nothing to recover");
                return Ok(());
            }
            for item in &report.items {
                let generation = item
                    .generation
                    .map(|g| format!(" generation {g}"))
                    .unwrap_or_default();
                println!(
                    "plan {}{generation}: {}: {}",
                    item.plan, item.subject, item.outcome
                );
            }
            let count =
                |f: fn(&Outcome) -> bool| report.items.iter().filter(|i| f(&i.outcome)).count();
            let recovered = count(|o| matches!(o, Outcome::Recovered(_)));
            let running = count(|o| matches!(o, Outcome::Running));
            let blocked = count(|o| matches!(o, Outcome::Blocked(_) | Outcome::Unsupported(_)));
            println!(
                "{recovered} recovered, {running} left to running processes, {blocked} blocked"
            );
            if !report.settled() {
                bail!("recovery is blocked: what it could not establish stays as it was found");
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
            command: PlanCommand::Verify { plan },
        } => {
            let cwd = std::env::current_dir()?;
            let project = Project::discover(&cwd)?.context("no agentctl project here")?;
            let mut store = project.hydrate()?;
            verify(&project, &mut store, plan)
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
                    if store.completion_proposal(plan)? == Some(replan) {
                        println!("plan {plan}: its planner proposed completion");
                        verify(&project, &mut store, plan)?;
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

/// Runs `plan`'s final integration verification and prints how it ended.
fn verify(project: &Project, store: &mut Store, plan: PlanId) -> Result<()> {
    let integrated = integration::verify(project, store, plan, None)?;
    let v = &integrated.integration;
    let IntegrationStatus::Finished(result) = &v.status else {
        bail!("integration verification {} did not finish", v.id);
    };
    println!(
        "integration verification {} of replan {}: {}",
        v.id, v.proposal, result.outcome
    );
    for blocker in result.report.iter().flat_map(|r| &r.blockers) {
        println!("  blocker {}: {}", blocker.id, blocker.summary);
    }
    if let Some(why) = &integrated.malformed {
        println!("  malformed result: {why}");
    }
    println!("plan {plan}: {}", store.plan(plan)?.state);
    match result.outcome {
        IntegrationOutcome::Passed => {}
        IntegrationOutcome::Failed => {
            println!("its planner decides what follows: `agentctl plan update {plan}`")
        }
        _ => println!("nothing was judged: `agentctl plan verify {plan}` verifies again"),
    }
    Ok(())
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

/// Prints the events `query` selects, then, when `follow`, whatever is
/// recorded after them, until interrupted. Reads only.
fn logs(project: &Project, mut query: EventQuery, follow: bool) -> Result<()> {
    const POLL: Duration = Duration::from_millis(500);
    const BATCH: u32 = 1000;
    let mut store = project.observe()?;
    match &store {
        Some(store) => {
            let events = store.query_events(&query)?;
            print_events(&events)?;
            if let Some(last) = events.last() {
                query.after = Some(last.seq);
            } else if query.newest {
                // Nothing matched: follow from whatever is recorded now.
                query.after = store
                    .query_events(&EventQuery {
                        after: None,
                        plan: None,
                        task: None,
                        agent: None,
                        kind: None,
                        limit: 1,
                        newest: true,
                    })?
                    .last()
                    .map(|e| e.seq);
            }
        }
        None => {
            println!("no state yet: nothing has been recorded here");
        }
    }
    if !follow {
        return Ok(());
    }
    query.newest = false;
    query.limit = BATCH;
    loop {
        std::thread::sleep(POLL);
        if store.is_none() {
            match project.observe() {
                Ok(opened) => store = opened,
                Err(e) => {
                    eprintln!("agentctl: reading state: {e:#}");
                    continue;
                }
            }
        }
        let Some(open) = &store else { continue };
        match open.query_events(&query) {
            Ok(events) => {
                print_events(&events)?;
                if let Some(last) = events.last() {
                    query.after = Some(last.seq);
                }
            }
            Err(e) => eprintln!("agentctl: reading events: {e:#}"),
        }
    }
}

fn print_events(events: &[agentctl::state::Event]) -> Result<()> {
    let mut out = io::stdout().lock();
    for event in events {
        writeln!(out, "{}", report::event_line(event))?;
    }
    out.flush()?;
    Ok(())
}
