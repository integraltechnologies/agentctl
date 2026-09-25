//! The planner protocol: the boundary between a planning agent and
//! agentctl.
//!
//! A planner supplies engineering judgment: how the human's intent is
//! decomposed into tasks, what each must achieve and may touch, and in what
//! order. It never acts on state. It answers one invocation with a
//! structured [`Response`] proposing [`Command`]s, which agentctl treats as
//! untrusted: it validates the whole response against the plan as it would
//! result, and applies it atomically or not at all. Human intent, accepted
//! source, runtime truth and history are beyond any command's reach.
//!
//! Every invocation starts fresh. Its input is the plan's canonical state
//! (intent and task DAG) and a map of the repository drawn from CodeGraph,
//! so planning continues from the store alone, never from a provider
//! session. A planner's prose explanation is returned, never recorded.

use std::fmt;
use std::path::PathBuf;

use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::graph::Freshness;
use crate::project::Project;
use crate::runtime::{self, Control, Launch, Outcome, Provider};
use crate::source;
use crate::state::{AgentId, InvocationId, PlanId, PlanState, Store};

/// Bounds on one response.
const COMMANDS_LIMIT: usize = 128;
const EXPLANATION_LIMIT: usize = 4096;
/// Roughly how much of the repository map one input carries, in bytes of
/// JSON, and how many entities it lists per source.
const REPOSITORY_BUDGET: usize = 64 * 1024;
const ENTITIES_LIMIT: usize = 200;

const INSTRUCTIONS: &str = "\
You are the planning agent of agentctl, an engineering control plane. You \
decide how the human's intent is decomposed into tasks: their objectives, \
the context each worker needs, the exact files each may change, and the \
dependencies that order them. You cannot change the intent, execute work or \
change files: agentctl validates what you propose and applies it only if \
all of it is valid.

Your input is JSON holding the human's intent, the plan's current tasks and \
a map of the repository's accepted source from its code graph. It is the \
complete planning state: nothing of any earlier session carries over. Read \
source files only when the map is not precise enough.

Answer only with the structured response. Its commands apply in order, as \
one change:
- add_task: a new task. `task` is its key: lowercase ASCII letters, digits, \
`_` and `-`, starting with a letter, at most 64 bytes, unique in the plan. \
`depends_on` lists keys of tasks that must complete first.
- update_task: changes a task's objective, context or paths; null leaves one \
unchanged.
- remove_task: removes a task no other task depends on.
- set_dependencies: replaces a task's dependencies.
- finalize: last, once the plan is complete; it becomes ready to execute.
`paths` are exact project-relative files within the source roots, separated \
by `/`: literal names, never patterns. An objective states what done means \
for the task; context is what its worker must know beyond that.";

/// A change a planner proposes to its plan, naming tasks by their keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    AddTask {
        task: String,
        objective: String,
        context: String,
        paths: Vec<String>,
        depends_on: Vec<String>,
    },
    /// Changes what is given; `None` leaves it unchanged.
    UpdateTask {
        task: String,
        objective: Option<String>,
        context: Option<String>,
        paths: Option<Vec<String>>,
    },
    /// Removes a task no other task depends on.
    RemoveTask { task: String },
    /// Replaces a task's dependencies.
    SetDependencies {
        task: String,
        depends_on: Vec<String>,
    },
    /// Completes planning: the plan becomes ready if it is executable. A
    /// struct variant, so that unknown fields are refused here too.
    Finalize {},
}

impl Command {
    pub fn op(&self) -> &'static str {
        match self {
            Self::AddTask { .. } => "add_task",
            Self::UpdateTask { .. } => "update_task",
            Self::RemoveTask { .. } => "remove_task",
            Self::SetDependencies { .. } => "set_dependencies",
            Self::Finalize {} => "finalize",
        }
    }
}

/// A planner's structured answer to one invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub commands: Vec<Command>,
    /// Why, for whoever reads it now. Never recorded.
    pub explanation: Option<String>,
}

/// Why a proposed revision was refused. Nothing of it was applied.
#[derive(Debug)]
pub struct Rejection {
    /// The refused command's 1-based position and operation, or `None` when
    /// the response as a whole was refused.
    pub command: Option<(usize, &'static str)>,
    pub reason: anyhow::Error,
}

impl Rejection {
    pub(crate) fn response(reason: anyhow::Error) -> Self {
        Self {
            command: None,
            reason,
        }
    }

    pub(crate) fn command(position: usize, op: &'static str, reason: anyhow::Error) -> Self {
        Self {
            command: Some((position, op)),
            reason,
        }
    }

    /// What was refused, in agentctl's words only.
    fn subject(&self) -> String {
        match self.command {
            Some((position, op)) => format!("command {position} ({op})"),
            None => "the response".into(),
        }
    }
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} was refused: {:#}", self.subject(), self.reason)
    }
}

impl std::error::Error for Rejection {}

/// The JSON Schema of a [`Response`], in the subset that providers enforce
/// strictly: every property required, `null` standing for absence.
/// agentctl enforces the protocol's bounds itself.
pub fn response_schema() -> Value {
    let text = json!({"type": "string"});
    let keys = json!({"type": "array", "items": {"type": "string"}});
    let command = |op: &str, properties: Value| {
        let mut properties = properties.as_object().cloned().unwrap_or_default();
        properties.insert("op".into(), json!({"type": "string", "enum": [op]}));
        let required: Vec<&String> = properties.keys().collect();
        json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        })
    };
    let nullable = |schema: &Value| json!({"anyOf": [schema, {"type": "null"}]});
    json!({
        "type": "object",
        "properties": {
            "commands": {"type": "array", "items": {"anyOf": [
                command("add_task", json!({"task": text, "objective": text, "context": text,
                    "paths": keys, "depends_on": keys})),
                command("update_task", json!({"task": text, "objective": nullable(&text),
                    "context": nullable(&text), "paths": nullable(&keys)})),
                command("remove_task", json!({"task": text})),
                command("set_dependencies", json!({"task": text, "depends_on": keys})),
                command("finalize", json!({})),
            ]}},
            "explanation": nullable(&text),
        },
        "required": ["commands", "explanation"],
        "additionalProperties": false,
    })
}

/// Validates and applies a planner's commands to a planning plan as one
/// transition, returning whether they finalized it. Requested paths must be
/// literal source paths of the project. Refused commands change nothing,
/// and the error carries the [`Rejection`].
pub fn apply(
    project: &Project,
    store: &mut Store,
    plan: PlanId,
    commands: &[Command],
) -> Result<bool> {
    let count = commands.len();
    if !(1..=COMMANDS_LIMIT).contains(&count) {
        let why = anyhow!("a response proposes 1 to {COMMANDS_LIMIT} commands, not {count}");
        return Err(Rejection::response(why).into());
    }
    store.revise_plan(plan, commands, &|path| source::check_source(project, path))
}

/// Everything a fresh planner invocation is given: the human's intent, the
/// plan's task DAG and the repository map, from canonical state alone.
pub fn input(project: &Project, store: &Store, plan: PlanId) -> Result<Value> {
    let current = store.plan(plan)?;
    let tasks = store.tasks(plan)?;
    let key = |id| tasks.iter().find(|t| t.id == id).map(|t| t.key.as_str());
    let tasks: Vec<Value> = tasks
        .iter()
        .map(|t| {
            json!({
                "task": t.key,
                "objective": t.objective,
                "context": t.context,
                "paths": t.scope,
                "depends_on": t.depends_on.iter().map(|&d| key(d)).collect::<Vec<_>>(),
            })
        })
        .collect();
    let roots: Vec<&str> = project
        .config
        .codegraph
        .roots
        .iter()
        .map(|r| r.as_str())
        .collect();
    Ok(json!({
        "intent": {
            "objective": current.intent.objective,
            "constraints": current.intent.constraints,
            "completion_criteria": current.intent.completion_criteria,
        },
        "plan": {"state": current.state.to_string(), "tasks": tasks},
        "source_roots": roots,
        "repository": repository(project, store)?,
    }))
}

/// A map of the project's accepted source: each source in the configured
/// roots with the entities its graph defines while that graph is current.
/// Stale or missing graph facts are marked, never given. Only accepted
/// source appears, so ignored, generated and unaccepted files never do.
fn repository(project: &Project, store: &Store) -> Result<Value> {
    let roots = &project.config.codegraph.roots;
    let mut sources = Vec::new();
    let mut used = 0;
    let mut omitted = 0;
    for path in store.accepted_paths()? {
        if !roots.iter().any(|root| root.contains_path(&path)) {
            continue;
        }
        let source = match store.entities(&path)? {
            Freshness::Current(entities) => {
                let listed: Vec<Value> = entities
                    .iter()
                    .take(ENTITIES_LIMIT)
                    .map(|e| json!({"kind": e.id.kind, "symbol": e.id.symbol}))
                    .collect();
                json!({
                    "path": path,
                    "graph": "current",
                    "entities": listed,
                    "entities_omitted": entities.len() - listed.len(),
                })
            }
            Freshness::Stale => json!({"path": path, "graph": "stale"}),
            Freshness::Unindexed => json!({"path": path, "graph": "unindexed"}),
            Freshness::Absent => continue,
        };
        let size = source.to_string().len();
        if used + size > REPOSITORY_BUDGET {
            omitted += 1;
            continue;
        }
        used += size;
        sources.push(source);
    }
    Ok(json!({"sources": sources, "sources_omitted": omitted}))
}

/// How one planner invocation ended for the plan.
#[derive(Debug)]
pub enum Planned {
    /// The invocation produced no result: it failed or was cancelled, as
    /// recorded. The plan is unchanged.
    NoResult(Box<Outcome>),
    /// The invocation succeeded, but what it proposed was refused. The plan
    /// is unchanged.
    Refused {
        invocation: InvocationId,
        reason: anyhow::Error,
    },
    /// Every proposed command was applied; `ready` when they finalized the
    /// plan.
    Applied {
        invocation: InvocationId,
        ready: bool,
        explanation: Option<String>,
    },
}

/// A live planner invocation.
pub struct Planner {
    agent: AgentId,
    plan: PlanId,
    invocation: runtime::Invocation,
}

/// Invokes the configured planner role afresh on a planning plan, through
/// `executable` or else the provider's CLI on `PATH`.
pub fn start(
    project: &Project,
    store: &mut Store,
    plan: PlanId,
    executable: Option<PathBuf>,
) -> Result<Planner> {
    let state = store.plan(plan)?.state;
    ensure!(
        state == PlanState::Planning,
        "plan {plan} is {state}; only a planning plan is planned"
    );
    let role = &project.config.agents.planner;
    let input = serde_json::to_string_pretty(&input(project, store, plan)?)?;
    let agent = store.planner(plan)?;
    let launch = Launch {
        agent,
        provider: role.provider.as_str().parse::<Provider>()?,
        executable,
        model: role.model.to_string(),
        effort: Some(role.reasoning_effort),
        bootstrap: INSTRUCTIONS.into(),
        input,
        output_schema: response_schema(),
        cwd: project.root.clone(),
    };
    let invocation = runtime::spawn(store, &launch)?;
    Ok(Planner {
        agent,
        plan,
        invocation,
    })
}

impl Planner {
    pub fn control(&self) -> Control {
        self.invocation.control()
    }

    /// Waits for the invocation to end, then validates and applies what it
    /// proposed. A refusal is recorded without the planner's words.
    pub fn finish(self, project: &Project, store: &mut Store) -> Result<Planned> {
        let outcome = self.invocation.wait(store)?;
        let invocation = outcome.invocation;
        let Some(payload) = outcome.payload.clone() else {
            return Ok(Planned::NoResult(Box::new(outcome)));
        };
        let proposal = serde_json::from_value::<Response>(payload)
            .map_err(|e| anyhow::Error::from(Rejection::response(e.into())))
            .and_then(|response| {
                let explanation = response.explanation.unwrap_or_default();
                if explanation.len() > EXPLANATION_LIMIT {
                    let why = anyhow!("explanations are at most {EXPLANATION_LIMIT} bytes");
                    return Err(Rejection::response(why).into());
                }
                let ready = apply(project, store, self.plan, &response.commands)?;
                Ok((ready, (!explanation.is_empty()).then_some(explanation)))
            });
        match proposal {
            Ok((ready, explanation)) => Ok(Planned::Applied {
                invocation,
                ready,
                explanation,
            }),
            Err(reason) => {
                let Some(rejection) = reason.downcast_ref::<Rejection>() else {
                    return Err(reason);
                };
                let detail = format!("invocation {invocation}: {} refused", rejection.subject());
                store.planner_refused(self.agent, &detail)?;
                Ok(Planned::Refused { invocation, reason })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::tests::Fixture;
    use crate::state::{Event, HumanIntent, Task};

    fn intent() -> HumanIntent {
        HumanIntent {
            objective: "Parse configuration once".into(),
            constraints: vec!["Keep the public API".into()],
            completion_criteria: vec!["cargo test passes".into(), "no new warnings".into()],
        }
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn add(task: &str, paths: &[&str], depends_on: &[&str]) -> Command {
        Command::AddTask {
            task: task.into(),
            objective: format!("Do {task}"),
            context: "Only what the objective needs.".into(),
            paths: strings(paths),
            depends_on: strings(depends_on),
        }
    }

    fn on(task: &str, depends_on: &[&str]) -> Command {
        Command::SetDependencies {
            task: task.into(),
            depends_on: strings(depends_on),
        }
    }

    fn update(task: &str, objective: Option<&str>, context: Option<&str>) -> Command {
        Command::UpdateTask {
            task: task.into(),
            objective: objective.map(Into::into),
            context: context.map(Into::into),
            paths: None,
        }
    }

    fn remove(task: &str) -> Command {
        Command::RemoveTask { task: task.into() }
    }

    /// Everything canonical about a plan: its record, its tasks and all
    /// events.
    fn snapshot(fx: &Fixture, plan: PlanId) -> (crate::state::Plan, Vec<Task>, Vec<Event>) {
        (
            fx.store.plan(plan).unwrap(),
            fx.store.tasks(plan).unwrap(),
            fx.store.events_after(0, 10_000).unwrap(),
        )
    }

    fn keys(tasks: &[Task]) -> Vec<&str> {
        tasks.iter().map(|t| t.key.as_str()).collect()
    }

    /// Applies `commands`, which must be refused as `expected` without
    /// changing anything.
    fn refused(fx: &mut Fixture, plan: PlanId, commands: &[Command], expected: &str) -> Rejection {
        let before = snapshot(fx, plan);
        let error = apply(&fx.project, &mut fx.store, plan, commands).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(expected), "{message}");
        assert_eq!(snapshot(fx, plan), before, "{message}");
        error.downcast::<Rejection>().unwrap()
    }

    #[test]
    fn a_response_applies_as_one_transition() {
        let mut fx = Fixture::new("src, crates/core");
        let plan = fx.store.create_plan(&intent()).unwrap();
        let commands = [
            add("parse", &["src/config.rs"], &[]),
            add(
                "wire",
                &["src/main.rs", "crates/core/src/lib.rs"],
                &["parse"],
            ),
            add("scratch", &[], &[]),
            add("test", &["src/tests.rs"], &["parse"]),
            on("test", &["wire", "parse"]),
            Command::UpdateTask {
                task: "wire".into(),
                objective: Some("Wire the parsed configuration in".into()),
                context: None,
                paths: Some(strings(&["src/main.rs"])),
            },
            remove("scratch"),
        ];
        assert!(!apply(&fx.project, &mut fx.store, plan, &commands).unwrap());

        let fx = fx.reopen();
        let tasks = fx.store.tasks(plan).unwrap();
        assert_eq!(keys(&tasks), ["parse", "wire", "test"]);
        let [parse, wire, test] = [&tasks[0], &tasks[1], &tasks[2]];
        assert_eq!(parse.scope, ["src/config.rs"]);
        assert_eq!(wire.objective, "Wire the parsed configuration in");
        assert_eq!(wire.context, "Only what the objective needs.");
        assert_eq!(
            (&wire.scope, &wire.depends_on),
            (&strings(&["src/main.rs"]), &vec![parse.id])
        );
        assert_eq!(test.depends_on, [parse.id, wire.id]);
        let current = fx.store.plan(plan).unwrap();
        assert_eq!(
            (current.intent, current.state),
            (intent(), PlanState::Planning)
        );
        let events = fx.store.events_after(0, 100).unwrap();
        let kinds: Vec<_> = events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, ["plan.created", "plan.revised"]);
        assert_eq!(events[1].detail, "7 commands applied");
    }

    #[test]
    fn invalid_commands_are_refused_and_change_nothing() {
        let mut fx = Fixture::new("src");
        let plan = fx.store.create_plan(&intent()).unwrap();
        let base = [
            add("base", &["src/base.rs"], &[]),
            add("next", &[], &["base"]),
        ];
        apply(&fx.project, &mut fx.store, plan, &base).unwrap();

        let long = "x".repeat(65);
        let cases: Vec<(Vec<Command>, &str)> = vec![
            (vec![], "1 to 128 commands"),
            (vec![remove("next"); 129], "1 to 128 commands"),
            (vec![add("a", &[], &["missing"])], "no task `missing`"),
            (vec![update("missing", None, None)], "no task `missing`"),
            (vec![remove("missing")], "no task `missing`"),
            (vec![on("missing", &[])], "no task `missing`"),
            (vec![add("base", &[], &[])], "task `base` already exists"),
            (
                vec![add("a", &[], &[]), add("a", &[], &[])],
                "task `a` already exists",
            ),
            (vec![add("a", &[], &["a"])], "cannot depend on itself"),
            (vec![on("base", &["base"])], "cannot depend on itself"),
            (vec![on("base", &["next"])], "would form a cycle"),
            (
                vec![add("a", &[], &["next"]), on("base", &["a"])],
                "would form a cycle",
            ),
            (vec![on("next", &["base", "base"])], "more than once"),
            (vec![remove("base")], "while `next` depends on it"),
            (vec![add("Upper", &[], &[])], "not a task key"),
            (vec![add("1st", &[], &[])], "not a task key"),
            (vec![add("a b", &[], &[])], "not a task key"),
            (vec![add("", &[], &[])], "not a task key"),
            (vec![add(&long, &[], &[])], "not a task key"),
            (
                vec![update("base", Some(" \n"), None)],
                "objective must not be blank",
            ),
            (
                vec![update("base", None, Some("a\u{1b}[2J"))],
                "must not contain control characters",
            ),
            (
                vec![update("base", None, Some(&"x".repeat(16 * 1024 + 1)))],
                "context must be at most",
            ),
            (
                vec![add("a", &["src/a.rs", "src/a.rs"], &[])],
                "requested more than once",
            ),
            (
                vec![add("a", &[&"s".repeat(4097)], &[])],
                "at most 4096 bytes",
            ),
        ];
        for (commands, expected) in cases {
            refused(&mut fx, plan, &commands, expected);
        }

        // A command late in a response is refused, and every earlier one
        // with it.
        let late = [
            add("a", &["src/a.rs"], &["base"]),
            add("b", &[], &["a"]),
            update("base", Some("Changed"), None),
            remove("next"),
            add("c", &[], &["gone"]),
        ];
        let rejection = refused(&mut fx, plan, &late, "no task `gone`");
        assert_eq!(rejection.command, Some((5, "add_task")));
        assert_eq!(keys(&fx.store.tasks(plan).unwrap()), ["base", "next"]);
    }

    #[test]
    fn requested_paths_are_literal_source_paths() {
        let mut fx = Fixture::new("src");
        let plan = fx.store.create_plan(&intent()).unwrap();
        for path in [
            "",
            "/src/a.rs",
            "src/../a.rs",
            "../src/a.rs",
            "src//a.rs",
            "src/./a.rs",
            "src/a.rs/",
            "src/a\0.rs",
        ] {
            refused(&mut fx, plan, &[add("a", &[path], &[])], "not a canonical");
        }
        for path in ["lib/a.rs", "README.md", "srcs/a.rs", ".agentctl/state.db"] {
            let message = "outside the configured source roots";
            refused(&mut fx, plan, &[add("a", &[path], &[])], message);
        }
        refused(
            &mut fx,
            plan,
            &[add("a", &["src/.git/config"], &[])],
            "Git state",
        );

        // Names that are patterns elsewhere are only names here: each is
        // kept exactly, as one path.
        let mut literal = vec![
            "src/[slug].rs",
            "src/(group).rs",
            "src/a+b.rs",
            "src/日本語.rs",
            "src/file name.rs",
            "src/{a,b}.rs",
            "src/[!a].rs",
        ];
        if cfg!(unix) {
            literal.extend(["src/a*b.rs", "src/?.rs", "src/**"]);
        }
        let commands = [add("a", &literal, &[]), Command::Finalize {}];
        assert!(apply(&fx.project, &mut fx.store, plan, &commands).unwrap());
        let mut scope = fx.store.tasks(plan).unwrap().remove(0).scope;
        scope.sort();
        literal.sort();
        assert_eq!(scope, literal);
    }

    #[test]
    fn finalizing_needs_an_executable_dag_and_is_final() {
        let mut fx = Fixture::new("src");
        let plan = fx.store.create_plan(&intent()).unwrap();
        refused(&mut fx, plan, &[Command::Finalize {}], "without tasks");
        refused(
            &mut fx,
            plan,
            &[add("a", &[], &[]), Command::Finalize {}, add("b", &[], &[])],
            "already finalized",
        );

        // Finalizing checks every task's scope again, as the project now
        // stands.
        apply(
            &fx.project,
            &mut fx.store,
            plan,
            &[add("a", &["src/a.rs"], &[])],
        )
        .unwrap();
        let roots = fx.project.config.codegraph.roots.clone();
        fx.project.config.codegraph.roots = "lib".parse().unwrap();
        let rejection = refused(
            &mut fx,
            plan,
            &[Command::Finalize {}],
            "task `a` is not executable",
        );
        assert_eq!(rejection.command, Some((1, "finalize")));
        fx.project.config.codegraph.roots = roots;

        let commands = [add("b", &["src/b.rs"], &["a"]), Command::Finalize {}];
        assert!(apply(&fx.project, &mut fx.store, plan, &commands).unwrap());
        let (current, tasks, events) = snapshot(&fx, plan);
        assert_eq!(current.state, PlanState::Ready);
        assert_eq!(keys(&tasks), ["a", "b"]);
        let last: Vec<_> = events[events.len() - 2..]
            .iter()
            .map(|e| (e.kind.as_str(), e.detail.as_str()))
            .collect();
        assert_eq!(
            last,
            [
                ("plan.revised", "2 commands applied"),
                ("plan.state", "planning -> ready")
            ]
        );

        // A ready plan is no longer revised, by any command.
        for commands in [
            vec![add("c", &[], &[])],
            vec![update("a", None, None)],
            vec![remove("b")],
            vec![on("b", &[])],
            vec![Command::Finalize {}],
        ] {
            let rejection = refused(&mut fx, plan, &commands, "only a planning plan is revised");
            assert_eq!(rejection.command, None);
        }
        let message = format!(
            "{:#}",
            fx.store.set_plan_state(plan, PlanState::Ready).unwrap_err()
        );
        assert!(
            message.contains("cannot go from ready to ready"),
            "{message}"
        );
    }

    #[test]
    fn responses_are_strict_and_cannot_touch_intent() {
        let validator = jsonschema::validator_for(&response_schema()).unwrap();
        let valid = json!({
            "commands": [
                {"op": "add_task", "task": "a", "objective": "Do a", "context": "",
                 "paths": ["src/[slug].rs"], "depends_on": []},
                {"op": "update_task", "task": "a", "objective": null, "context": "More",
                 "paths": null},
                {"op": "set_dependencies", "task": "a", "depends_on": []},
                {"op": "remove_task", "task": "a"},
                {"op": "finalize"},
            ],
            "explanation": null,
        });
        assert!(validator.is_valid(&valid));
        let response: Response = serde_json::from_value(valid).unwrap();
        assert_eq!(response.commands.len(), 5);
        assert_eq!(response.commands[4], Command::Finalize {});

        // Neither the schema nor agentctl accepts anything else, including
        // any way of naming the human's intent.
        for invalid in [
            json!({"commands": [{"op": "set_objective", "objective": "Other"}], "explanation": null}),
            json!({"commands": [{"op": "finalize", "intent": "Other"}], "explanation": null}),
            json!({"commands": [{"op": "add_task", "task": "a", "objective": "x", "context": "",
                   "paths": [], "depends_on": [], "constraints": []}], "explanation": null}),
            json!({"commands": [], "explanation": null, "intent": {"objective": "Other"}}),
            json!({"commands": [{"op": "remove_task"}], "explanation": null}),
            json!({"commands": [{"op": "remove_task", "task": 7}], "explanation": null}),
            json!({"commands": "finalize", "explanation": null}),
        ] {
            assert!(!validator.is_valid(&invalid), "{invalid}");
            assert!(serde_json::from_value::<Response>(invalid).is_err());
        }

        // Whatever is applied, the intent stays as the human stated it, and
        // the store itself refuses to rewrite it.
        let mut fx = Fixture::new("src");
        let plan = fx.store.create_plan(&intent()).unwrap();
        let commands = [add("a", &[], &[]), Command::Finalize {}];
        apply(&fx.project, &mut fx.store, plan, &commands).unwrap();
        assert_eq!(fx.store.plan(plan).unwrap().intent, intent());
        for column in ["objective", "constraints", "completion_criteria"] {
            let sql = format!("UPDATE plans SET {column} = '[]'");
            let message = fx.store.raw().execute(&sql, []).unwrap_err().to_string();
            assert!(message.contains("human intent is immutable"), "{message}");
        }
    }

    #[test]
    fn human_intent_is_bounded_structure() {
        let mut fx = Fixture::new("src");
        let with = |f: fn(&mut HumanIntent)| {
            let mut intent = intent();
            f(&mut intent);
            intent
        };
        for (bad, expected) in [
            (
                with(|i| i.objective = "  ".into()),
                "objective must not be blank",
            ),
            (
                with(|i| i.objective = "x".repeat(4097)),
                "at most 4096 bytes",
            ),
            (
                with(|i| i.constraints.push(String::new())),
                "constraints must not be blank",
            ),
            (
                with(|i| i.completion_criteria = vec!["x".into(); 33]),
                "at most 32 completion criteria",
            ),
            (
                with(|i| i.constraints = vec!["\0".into()]),
                "control characters",
            ),
        ] {
            let message = format!("{:#}", fx.store.create_plan(&bad).unwrap_err());
            assert!(message.contains(expected), "{message}");
        }
        assert!(fx.store.events_after(0, 10).unwrap().is_empty());
    }

    #[test]
    fn input_is_canonical_state_and_a_current_graph_map() {
        let mut fx = Fixture::new("src");
        fx.accept(
            "src/lib.rs",
            Some("pub fn parse() {}\npub struct Config;\n"),
        );
        fx.accept("src/stale.rs", Some("pub fn old() {}\n"));
        fx.accept("src/raw.rs", Some("pub fn raw() {}\n"));
        fx.accept("src/gone.rs", Some("pub fn gone() {}\n"));
        for path in ["src/lib.rs", "src/stale.rs"] {
            crate::graph::rust::index(&fx.project, &mut fx.store, path).unwrap();
        }
        fx.accept("src/stale.rs", Some("pub fn new() {}\n"));
        fx.accept("src/gone.rs", None);
        // Present in the working tree, but never accepted: not source yet.
        std::fs::write(fx.project.root.join("src/draft.rs"), "pub fn draft() {}").unwrap();

        let plan = fx.store.create_plan(&intent()).unwrap();
        let commands = [
            add("parse", &["src/lib.rs"], &[]),
            add("use", &[], &["parse"]),
        ];
        apply(&fx.project, &mut fx.store, plan, &commands).unwrap();

        let input = input(&fx.project, &fx.store, plan).unwrap();
        assert_eq!(
            input["intent"],
            json!({
                "objective": "Parse configuration once",
                "constraints": ["Keep the public API"],
                "completion_criteria": ["cargo test passes", "no new warnings"],
            })
        );
        assert_eq!(input["plan"]["state"], "planning");
        assert_eq!(
            input["plan"]["tasks"][1],
            json!({"task": "use", "objective": "Do use",
                   "context": "Only what the objective needs.", "paths": [],
                   "depends_on": ["parse"]})
        );
        assert_eq!(input["source_roots"], json!(["src"]));
        assert_eq!(
            input["repository"],
            json!({
                "sources": [
                    {"path": "src/lib.rs", "graph": "current", "entities_omitted": 0,
                     "entities": [{"kind": "function", "symbol": "parse"},
                                  {"kind": "module", "symbol": "self"},
                                  {"kind": "struct", "symbol": "Config"}]},
                    {"path": "src/raw.rs", "graph": "unindexed"},
                    {"path": "src/stale.rs", "graph": "stale"},
                ],
                "sources_omitted": 0,
            })
        );
    }
}
