//! The agent runtime: runs one provider invocation on behalf of a logical
//! agent, through a provider-neutral contract.
//!
//! Providers supply intelligence; agentctl owns the rest. An invocation's
//! identity, lifecycle, process, input, structured result, usage, liveness,
//! cancellation and failure classification are agentctl's, and how it ended
//! is recorded through [`Store`]. Every invocation starts fresh: a provider
//! session is observed as metadata, never resumed and never identity.
//! Provider command lines and output formats live only in the adapters.
//!
//! A launch delivers its whole input on the provider's standard input, which
//! is then closed: neither adapter's non-interactive mode accepts input
//! mid-run. The provider must answer with one structured result satisfying
//! the launch's output schema, which agentctl itself checks; prose is never
//! taken as a result.
//!
//! What is recorded durably about how an invocation ended is agentctl's own
//! classification. Provider-controlled text (its output, standard error and
//! error messages) may carry secrets, so it is never recorded; standard error
//! is returned in the [`Outcome`] only.

mod claude;
mod codex;

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use std::{env, fmt};

use anyhow::{Context, Result, anyhow, bail, ensure};
use jsonschema::Validator;
use serde_json::{Map, Value};
use tempfile::NamedTempFile;

use crate::config::ReasoningEffort;
use crate::platform::{self, Capability, Level};
use crate::state::{AgentId, InvocationId, Store};

pub use crate::state::{FailureKind, InvocationEnd, InvocationState, TokenUsage, Usage};

const POLL: Duration = Duration::from_millis(20);
/// How long a provider asked to terminate may take before it is killed.
const TERMINATION_GRACE: Duration = Duration::from_secs(3);
/// How long a killed provider may take to be observed exiting.
const KILL_WAIT: Duration = Duration::from_secs(5);
/// How long output may keep flowing once the provider has exited. A
/// descendant that inherited its pipes can hold them open indefinitely.
const DRAIN: Duration = Duration::from_secs(2);
const STDERR_TAIL: usize = 16 * 1024;
const DIAGNOSTIC_LIMIT: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    /// The provider's configuration name, which is also its CLI's name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

impl FromStr for Provider {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            _ => bail!("unknown provider `{s}`; agentctl runs claude and codex"),
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What an invocation may do to files in its working directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workspace {
    ReadOnly,
    /// It may edit files within its working directory, in the provider's
    /// own editing mode. The provider decides what that confines; agentctl
    /// neither relies on it nor adds a sandbox of its own, so an editable
    /// working directory must hold nothing agentctl needs protected.
    Editable,
}

/// Everything needed to launch one invocation.
#[derive(Debug, Clone)]
pub struct Launch {
    /// The logical agent the invocation embodies.
    pub agent: AgentId,
    pub provider: Provider,
    /// The provider CLI; `None` finds the provider's executable on `PATH`.
    pub executable: Option<PathBuf>,
    pub model: String,
    pub effort: Option<ReasoningEffort>,
    /// Instructions added to the provider's own system instructions.
    pub bootstrap: String,
    /// The task and its context.
    pub input: String,
    /// The JSON Schema the structured result must satisfy.
    pub output_schema: Value,
    /// The absolute working directory.
    pub cwd: PathBuf,
    pub workspace: Workspace,
}

/// How an invocation ended, with what agentctl received from the provider.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub invocation: InvocationId,
    /// Exactly what was recorded.
    pub end: InvocationEnd,
    /// The structured result, present exactly when the invocation succeeded.
    /// Its meaning is defined by whoever chose the output schema.
    pub payload: Option<Value>,
    /// Provider facts with no neutral equivalent, such as Claude's cost.
    /// Provider-controlled, so never recorded.
    pub provider_metadata: Map<String, Value>,
    /// The end of the provider's standard error, for diagnosis only.
    /// Provider-controlled, so never recorded.
    pub stderr: String,
}

/// A snapshot of a live invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    pub state: InvocationState,
    /// The provider process, once launched: diagnostic, never identity.
    pub pid: Option<u32>,
    /// Whether agentctl has seen the process exit.
    pub exited: bool,
    pub cancel_requested: bool,
    /// How long since the provider last wrote any output.
    pub quiet_for: Option<Duration>,
    /// Structured events received, and the latest one's provider type.
    pub events: u64,
    pub last_event: Option<String>,
    pub provider_session: Option<String>,
    /// Usage the provider has reported so far.
    pub usage: Usage,
}

impl Observation {
    /// Whether the provider process is running as far as agentctl knows.
    pub fn alive(&self) -> bool {
        self.state == InvocationState::Running && !self.exited
    }
}

/// Observes and cancels an invocation from any thread.
#[derive(Clone)]
pub struct Control {
    shared: Arc<Shared>,
}

impl Control {
    /// Requests cancellation. The invocation is cancelled only once its
    /// process has been terminated and observed to exit.
    pub fn cancel(&self) {
        self.shared.cancel.store(true, Ordering::SeqCst);
    }

    pub fn observe(&self) -> Observation {
        let p = self.shared.progress();
        Observation {
            state: p.state,
            pid: p.pid,
            exited: p.exited,
            cancel_requested: self.shared.cancel.load(Ordering::SeqCst),
            quiet_for: p.last_output.map(|at| at.elapsed()),
            events: p.events,
            last_event: p.last_event.clone(),
            provider_session: p.stream.session.clone(),
            usage: p.stream.usage(),
        }
    }
}

/// One invocation, owned by agentctl from launch until it has ended.
/// Dropping a launched invocation before [`Invocation::wait`] kills its
/// process without recording an end, as if agentctl had disappeared.
pub struct Invocation {
    id: InvocationId,
    shared: Arc<Shared>,
    stage: Stage,
}

enum Stage {
    Launched(Process),
    /// It failed before a process existed, and that is already recorded.
    Ended(Box<Outcome>),
}

/// Records and launches an invocation. An invalid launch, including an
/// output schema that is not a valid JSON Schema, is refused before anything
/// is recorded; a provider that cannot be started is recorded as failed, and
/// [`Invocation::wait`] then returns that outcome.
pub fn spawn(store: &mut Store, launch: &Launch) -> Result<Invocation> {
    spawn_after(store, launch, |_, _| Ok(()))
}

/// [`spawn`], running `prepare` once the invocation is recorded and before
/// any provider process exists, so that whatever it records durably
/// precedes anything the provider does. Should `prepare` fail, nothing is
/// launched and the invocation is recorded as failed to spawn, if it can be.
pub fn spawn_after(
    store: &mut Store,
    launch: &Launch,
    prepare: impl FnOnce(&mut Store, InvocationId) -> Result<()>,
) -> Result<Invocation> {
    ensure!(
        launch.cwd.is_absolute(),
        "working directory {} is not absolute",
        launch.cwd.display()
    );
    ensure!(
        launch.output_schema.is_object(),
        "the output schema must be a JSON object"
    );
    // Only in-document references resolve: this build of the validator
    // cannot fetch files or URLs.
    let schema = jsonschema::validator_for(&launch.output_schema)
        .map_err(|e| anyhow!("the output schema is not a valid JSON Schema: {e}"))?;
    let prepared = match launch.provider {
        Provider::Claude => claude::prepare(launch),
        Provider::Codex => codex::prepare(launch),
    }?;
    let effort = launch.effort.map(|e| e.to_string());
    let id = store.start_invocation(
        launch.agent,
        launch.provider.name(),
        &launch.model,
        effort.as_deref(),
    )?;
    if let Err(e) = prepare(store, id) {
        let end = InvocationEnd {
            state: InvocationState::Failed,
            failure: Some(FailureKind::SpawnFailed),
            diagnostic: Some("agentctl abandoned the launch before starting the provider".into()),
            exit_code: None,
            provider_session: None,
            usage: Usage::Unavailable,
        };
        // The original error matters more than any failure to record this.
        let _ = store.finish_invocation(id, &end);
        return Err(e);
    }
    let shared = Arc::new(Shared {
        cancel: AtomicBool::new(false),
        progress: Mutex::new(Progress::new(prepared.decoder)),
        schema,
    });
    let child = match start(launch, &prepared.args, &prepared.env) {
        Ok(child) => child,
        Err((kind, why)) => {
            let end = InvocationEnd {
                state: InvocationState::Failed,
                failure: Some(kind),
                diagnostic: Some(clip(&why, DIAGNOSTIC_LIMIT)),
                exit_code: None,
                provider_session: None,
                usage: Usage::Unavailable,
            };
            store.finish_invocation(id, &end)?;
            shared.progress().state = end.state;
            let outcome = Outcome {
                invocation: id,
                end,
                payload: None,
                provider_metadata: Map::new(),
                stderr: String::new(),
            };
            return Ok(Invocation {
                id,
                shared,
                stage: Stage::Ended(Box::new(outcome)),
            });
        }
    };
    let process = Process::own(child, &shared, launch.input.clone(), prepared.files);
    let invocation = Invocation {
        id,
        shared,
        stage: Stage::Launched(process),
    };
    // Should this fail, dropping `invocation` kills the process, and the
    // record stays `starting`: a process may have existed.
    store.invocation_running(id)?;
    invocation.shared.progress().state = InvocationState::Running;
    Ok(invocation)
}

impl Invocation {
    pub fn id(&self) -> InvocationId {
        self.id
    }

    pub fn control(&self) -> Control {
        Control {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Waits for the invocation to end, cancelling it if requested, then
    /// records and returns how it ended.
    pub fn wait(self, store: &mut Store) -> Result<Outcome> {
        let process = match self.stage {
            Stage::Ended(outcome) => return Ok(*outcome),
            Stage::Launched(process) => process,
        };
        let outcome = process.supervise(self.id, &self.shared);
        store
            .finish_invocation(self.id, &outcome.end)
            .with_context(|| format!("recording how invocation {} ended", self.id))?;
        self.shared.progress().state = outcome.end.state;
        Ok(outcome)
    }
}

/// A provider command line, as an adapter prepares it.
struct Prepared {
    args: Vec<OsString>,
    env: Passthrough,
    decoder: Decoder,
    /// Files the arguments name, removed once the invocation ends.
    files: Vec<NamedTempFile>,
}

/// Environment variables a provider needs beyond [`COMMON_ENV`], typically
/// for its authentication and configuration.
struct Passthrough {
    names: &'static [&'static str],
    prefixes: &'static [&'static str],
}

/// What any provider may need from the environment: to find programs and
/// the user's home and configuration, temporary space, locale, proxies and
/// certificates, on Unix and on Windows. Nothing else is passed on.
const COMMON_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "TEMP",
    "TMP",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "XDG_RUNTIME_DIR",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "USERPROFILE",
    "USERNAME",
    "USERDOMAIN",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "COMMONPROGRAMFILES",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "OS",
];

impl Passthrough {
    /// Whether the variable `name` is passed on. Names compare ignoring
    /// ASCII case, as Windows compares them.
    fn passes(&self, name: &str) -> bool {
        COMMON_ENV
            .iter()
            .chain(self.names)
            .any(|n| n.eq_ignore_ascii_case(name))
            || self.prefixes.iter().any(|p| {
                name.get(..p.len())
                    .is_some_and(|start| start.eq_ignore_ascii_case(p))
            })
    }
}

/// Spawns the provider executable itself: never through a shell, with only
/// the environment it needs, and with every standard stream owned.
fn start(
    launch: &Launch,
    args: &[OsString],
    passthrough: &Passthrough,
) -> Result<Child, (FailureKind, String)> {
    let name = launch.provider.name();
    let exe = match &launch.executable {
        Some(path) => path.clone(),
        None => which::which(name).map_err(|e| {
            (
                FailureKind::ExecutableMissing,
                format!("no `{name}` executable on PATH ({e})"),
            )
        })?,
    };
    if !exe.is_file() {
        return Err((
            FailureKind::ExecutableMissing,
            format!("no {name} executable at {}", exe.display()),
        ));
    }
    if !platform::runs_directly(&exe) {
        return Err((
            FailureKind::SpawnFailed,
            format!(
                "{} would run through a command interpreter; agentctl runs providers directly",
                exe.display()
            ),
        ));
    }
    Command::new(&exe)
        .args(args)
        .current_dir(&launch.cwd)
        .env_clear()
        .envs(
            env::vars_os().filter(|(name, _)| name.to_str().is_some_and(|n| passthrough.passes(n))),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            (
                FailureKind::SpawnFailed,
                format!(
                    "cannot start {} in {}: {e}",
                    exe.display(),
                    launch.cwd.display()
                ),
            )
        })
}

struct Shared {
    cancel: AtomicBool,
    progress: Mutex<Progress>,
    /// The launch's output schema, which a result must satisfy.
    schema: Validator,
}

impl Shared {
    fn progress(&self) -> MutexGuard<'_, Progress> {
        self.progress.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// What agentctl has observed of a live invocation.
struct Progress {
    state: InvocationState,
    pid: Option<u32>,
    exited: bool,
    input_delivered: bool,
    input_error: Option<String>,
    last_output: Option<Instant>,
    events: u64,
    last_event: Option<String>,
    decoder: Decoder,
    stream: Stream,
    stderr: Vec<u8>,
}

impl Progress {
    fn new(decoder: Decoder) -> Self {
        Self {
            state: InvocationState::Starting,
            pid: None,
            exited: false,
            input_delivered: false,
            input_error: None,
            last_output: None,
            events: 0,
            last_event: None,
            decoder,
            stream: Stream::default(),
            stderr: Vec::new(),
        }
    }

    /// Takes one line of the provider's JSON Lines output.
    fn observe(&mut self, line: &[u8]) {
        if line.is_empty() {
            return;
        }
        self.events += 1;
        match parse_event(line) {
            Ok((kind, event)) => {
                match &mut self.decoder {
                    Decoder::Claude(d) => d.decode(&kind, event, &mut self.stream),
                    Decoder::Codex(d) => d.decode(&kind, event, &mut self.stream),
                }
                self.last_event = Some(kind);
            }
            Err(why) => self.stream.malformed(why),
        }
    }
}

enum Decoder {
    Claude(claude::Decoder),
    Codex(codex::Decoder),
}

/// What a provider's structured output has established, in neutral terms.
#[derive(Debug, Default)]
struct Stream {
    session: Option<String>,
    /// Token usage the provider reported.
    tokens: Option<TokenUsage>,
    result: Option<Value>,
    /// That the provider reported an error, in agentctl's words: what it
    /// said is provider-controlled, so it goes to `metadata` at most.
    error: Option<&'static str>,
    /// Why the output cannot be trusted, once it cannot. agentctl's words
    /// only: the offending output is never quoted.
    malformed: Option<&'static str>,
    metadata: Map<String, Value>,
}

impl Stream {
    fn malformed(&mut self, why: &'static str) {
        self.malformed.get_or_insert(why);
    }

    fn session(&mut self, id: Option<&str>) {
        if let Some(id) = id.filter(|id| !id.is_empty()) {
            self.session = Some(id.to_owned());
        }
    }

    fn usage(&self) -> Usage {
        self.tokens
            .map_or(Usage::Unavailable, Usage::ProviderReported)
    }
}

/// Parses one output line as a JSON object with a string `type`.
fn parse_event(line: &[u8]) -> Result<(String, Value), &'static str> {
    let event: Value = serde_json::from_slice(line).map_err(|_| "provider output is not JSON")?;
    match event.get("type").and_then(Value::as_str) {
        Some(kind) => Ok((kind.to_owned(), event)),
        None => Err("a provider output event has no type"),
    }
}

/// A launched provider process and the threads serving its streams.
struct Process {
    child: Child,
    reaped: bool,
    threads: Vec<JoinHandle<()>>,
    _files: Vec<NamedTempFile>,
}

impl Process {
    fn own(
        mut child: Child,
        shared: &Arc<Shared>,
        input: String,
        files: Vec<NamedTempFile>,
    ) -> Self {
        shared.progress().pid = Some(child.id());
        let mut stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let writer = {
            let shared = Arc::clone(shared);
            // Dropping `stdin` afterwards closes it: the input is complete.
            thread::spawn(move || match stdin.write_all(input.as_bytes()) {
                Ok(()) => shared.progress().input_delivered = true,
                Err(e) => shared.progress().input_error = Some(e.to_string()),
            })
        };
        let reader = {
            let shared = Arc::clone(shared);
            thread::spawn(move || read_events(stdout, &shared))
        };
        let diagnostics = {
            let shared = Arc::clone(shared);
            thread::spawn(move || read_stderr(stderr, &shared))
        };
        Self {
            child,
            reaped: false,
            threads: vec![writer, reader, diagnostics],
            _files: files,
        }
    }

    /// Waits for the process to end, terminating it once cancellation is
    /// requested, and classifies how the invocation ended.
    fn supervise(mut self, invocation: InvocationId, shared: &Shared) -> Outcome {
        let ended = self.wait_or_terminate(&shared.cancel);
        shared.progress().exited = ended.is_ok();
        let drained = ended.is_ok() && self.drain();
        let p = shared.progress();
        let (state, failure, mut diagnostic, status) = match ended {
            Ok((status, cancelled)) => {
                let (state, failure, diagnostic) = classify(status, cancelled, &p, &shared.schema);
                (state, failure, diagnostic, Some(status))
            }
            Err(why) => (InvocationState::Interrupted, None, Some(why), None),
        };
        if status.is_some() && !drained {
            let note = "its output stayed open after it exited, held by a process it started";
            diagnostic = Some(match diagnostic {
                Some(d) => format!("{d}; {note}"),
                None => note.to_owned(),
            });
        }
        Outcome {
            invocation,
            end: InvocationEnd {
                state,
                failure,
                diagnostic: diagnostic.map(|d| clip(&d, DIAGNOSTIC_LIMIT)),
                exit_code: status.and_then(|s| s.code()),
                provider_session: p.stream.session.clone(),
                usage: p.stream.usage(),
            },
            payload: (state == InvocationState::Succeeded)
                .then(|| p.stream.result.clone())
                .flatten(),
            provider_metadata: p.stream.metadata.clone(),
            stderr: String::from_utf8_lossy(&p.stderr).into_owned(),
        }
    }

    /// The process's exit status, and whether agentctl terminated it; an
    /// error when its end cannot be established.
    fn wait_or_terminate(&mut self, cancel: &AtomicBool) -> Result<(ExitStatus, bool), String> {
        let mut kill_at: Option<Instant> = None;
        let mut killed: Option<(Instant, Option<std::io::Error>)> = None;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.reaped = true;
                    return Ok((status, kill_at.is_some()));
                }
                Ok(None) => {}
                Err(e) => return Err(format!("cannot observe the provider process: {e}")),
            }
            let now = Instant::now();
            if kill_at.is_none() && cancel.load(Ordering::SeqCst) {
                // Without a graceful request, termination is the kill.
                kill_at = Some(match platform::request_termination(&self.child) {
                    Ok(true) => now + TERMINATION_GRACE,
                    Ok(false) | Err(_) => now,
                });
            }
            match &killed {
                None if kill_at.is_some_and(|at| now >= at) => {
                    killed = Some((now, self.child.kill().err()));
                }
                Some((at, error)) if now >= *at + KILL_WAIT => {
                    let error = error
                        .as_ref()
                        .map(|e| format!(" ({e})"))
                        .unwrap_or_default();
                    return Err(format!(
                        "termination not confirmed: provider process {} was killed{error} \
                         but not seen to exit",
                        self.child.id()
                    ));
                }
                _ => {}
            }
            thread::sleep(POLL);
        }
    }

    /// Waits a bounded time for the stream threads to finish; whether they
    /// did.
    fn drain(&self) -> bool {
        let deadline = Instant::now() + DRAIN;
        while !self.threads.iter().all(JoinHandle::is_finished) {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(POLL);
        }
        true
    }
}

impl Drop for Process {
    /// A process agentctl stops owning is killed and, if it exits in time,
    /// reaped, so that it neither lingers nor becomes a zombie.
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        let _ = self.child.kill();
        let deadline = Instant::now() + KILL_WAIT;
        while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
            thread::sleep(POLL);
        }
    }
}

/// How an invocation whose process exited with `status` ended. The
/// diagnostic is agentctl's own: it quotes nothing the provider wrote.
fn classify(
    status: ExitStatus,
    cancelled: bool,
    p: &Progress,
    schema: &Validator,
) -> (InvocationState, Option<FailureKind>, Option<String>) {
    use FailureKind::*;
    let failed = |kind, why: String| (InvocationState::Failed, Some(kind), Some(why));
    let stream = &p.stream;
    if cancelled {
        let untracked = match platform::capabilities()
            .get(Capability::ProcessTreeTermination)
            .level
        {
            Level::Enforced => "",
            _ => "; processes it started are not tracked and may outlive it",
        };
        let why = format!("cancelled by agentctl; the provider {status}{untracked}");
        return (InvocationState::Cancelled, None, Some(why));
    }
    if !p.input_delivered {
        let why = p
            .input_error
            .as_deref()
            .unwrap_or("delivery did not complete");
        return failed(InputFailed, format!("the input was not delivered: {why}"));
    }
    if let Some(error) = stream.error {
        return failed(ProviderError, error.to_owned());
    }
    if let Some(why) = stream.malformed {
        return failed(MalformedOutput, why.to_owned());
    }
    match (&stream.result, status.success()) {
        (Some(result), true) if schema.is_valid(result) => (InvocationState::Succeeded, None, None),
        (Some(_), true) => failed(
            MalformedOutput,
            "the structured result does not satisfy the output schema".to_owned(),
        ),
        (Some(_), false) => failed(
            ExitStatus,
            format!("the provider {status} after its result"),
        ),
        (None, true) => failed(NoResult, format!("the provider {status} without a result")),
        (None, false) => failed(ExitStatus, format!("the provider {status}")),
    }
}

fn read_events(stdout: ChildStdout, shared: &Shared) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line);
        let mut p = shared.progress();
        match read {
            Ok(0) => return,
            Ok(_) => {
                p.last_output = Some(Instant::now());
                p.observe(line.trim_ascii());
            }
            Err(_) => return p.stream.malformed("provider output could not be read"),
        }
    }
}

/// Keeps the end of the provider's standard error.
fn read_stderr(mut stderr: ChildStderr, shared: &Shared) {
    let mut buf = [0; 4096];
    while let Ok(n @ 1..) = stderr.read(&mut buf) {
        let mut p = shared.progress();
        p.last_output = Some(Instant::now());
        p.stderr.extend_from_slice(&buf[..n]);
        let excess = p.stderr.len().saturating_sub(STDERR_TAIL);
        p.stderr.drain(..excess);
    }
}

/// `text`, cut to at most `limit` bytes on a character boundary.
fn clip(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit.saturating_sub(3);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_needed_environment_passes() {
        let claude = &claude::ENV;
        for name in [
            "PATH",
            "Path",
            "HOME",
            "ANTHROPIC_API_KEY",
            "anthropic_base_url",
            "CLAUDE_CONFIG_DIR",
        ] {
            assert!(claude.passes(name), "{name}");
        }
        for name in [
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "OPENAI_API_KEY",
            "CODEX_HOME",
            "CLAUDECODE",
            "ANTHROPI",
            "É",
        ] {
            assert!(!claude.passes(name), "{name}");
        }
        let codex = &codex::ENV;
        for name in ["OPENAI_API_KEY", "CODEX_HOME", "SystemRoot"] {
            assert!(codex.passes(name), "{name}");
        }
        assert!(!codex.passes("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn events_are_typed_json_objects() {
        let (kind, event) = parse_event(br#"{"type":"x","n":1}"#).unwrap();
        assert_eq!((kind.as_str(), &event["n"]), ("x", &Value::from(1)));
        assert!(
            parse_event(b"plain prose")
                .unwrap_err()
                .contains("not JSON")
        );
        assert!(parse_event(br#"{"n":1}"#).unwrap_err().contains("no type"));
        assert!(parse_event(br#"["type"]"#).unwrap_err().contains("no type"));
    }

    #[test]
    fn clipping_respects_characters() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("ééééé", 8), "éé...");
    }

    #[test]
    fn providers_are_named_as_configured() {
        for provider in [Provider::Claude, Provider::Codex] {
            assert_eq!(provider.name().parse::<Provider>().unwrap(), provider);
        }
        assert!("gemini".parse::<Provider>().is_err());
    }
}
