//! OS-neutral worker security architecture.
//!
//! ```text
//! ProcessSpec ─compile─▶ SecurityPolicy ─check(capabilities)─▶ PlatformSecurityBackend::spawn
//!                                              │ a required capability is not met
//!                                              ▼
//!                                  refused before launch (never unsandboxed)
//! ```
//!
//! Planning, routing, lifecycle, verification, analytics, experiments and Stage 9
//! decisions only build `ProcessSpec`s; none of them sees a platform API. The
//! policy vocabulary here (read/write roots, denied control-plane paths, network,
//! environment allowlist, resource ceilings) is independent of Seatbelt, Landlock
//! or Windows primitives; each backend reports per-capability ENFORCED /
//! BEST_EFFORT / UNSUPPORTED and only enforces what it truthfully can.
//!
//! Trust boundary: the untrusted party is worker output and the processes it
//! causes to run. Same-user host compromise (another process of the operator's
//! account editing agentctl state or the workspace directly) is out of scope and
//! is not claimed to be solved.
pub mod json;
pub(crate) mod linux;
pub(crate) mod macos;
pub(crate) mod tree;
#[cfg(windows)]
pub(crate) mod windows;

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    process::Child,
};

use serde::{Deserialize, Serialize};

use super::runtime::process::ProcessSpec;
use super::{Error, Result, paths, require};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CapabilityStatus {
    Unsupported,
    BestEffort,
    Enforced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Capability {
    FilesystemRead,
    FilesystemWrite,
    FilesystemMetadataWrite,
    NetworkDeny,
    ProcessTree,
    EnvironmentIsolation,
    CredentialIsolation,
    MemoryLimit,
    CpuLimit,
    ProcessLimit,
    OpenFileLimit,
    FileSizeLimit,
    WallClock,
    OutputCapture,
}

#[derive(Debug, Clone, Serialize)]
pub struct CapabilityEntry {
    pub capability: Capability,
    pub status: CapabilityStatus,
    pub mechanism: String,
    pub detail: String,
}

/// Deterministic host capability discovery: no model call, no network request.
#[derive(Debug, Clone, Serialize)]
pub struct CapabilityReport {
    pub backend: &'static str,
    pub platform: &'static str,
    pub running_as_root: bool,
    pub capabilities: Vec<CapabilityEntry>,
}

impl CapabilityReport {
    pub fn status(&self, capability: Capability) -> CapabilityStatus {
        self.capabilities
            .iter()
            .find(|c| c.capability == capability)
            .map(|c| c.status)
            .unwrap_or(CapabilityStatus::Unsupported)
    }
    pub(crate) fn entry(
        capability: Capability,
        status: CapabilityStatus,
        mechanism: &str,
        detail: &str,
    ) -> CapabilityEntry {
        CapabilityEntry {
            capability,
            status,
            mechanism: mechanism.into(),
            detail: detail.into(),
        }
    }
    /// Controller-side guarantees that do not depend on the OS backend.
    pub(crate) fn controller_entries() -> [CapabilityEntry; 2] {
        [
            Self::entry(
                Capability::WallClock,
                CapabilityStatus::Enforced,
                "controller deadline + process-tree termination",
                "role/check/experiment timeouts are enforced by the polling controller",
            ),
            Self::entry(
                Capability::OutputCapture,
                CapabilityStatus::Enforced,
                "bounded capture",
                "stdout/stderr are each capped at 4 MiB; overflow fails the job",
            ),
        ]
    }
}

/// Who a launched process is. Provider frontends need network and their own
/// native authentication; tools (checks, experiments) never receive either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WorkerClass {
    ProviderFrontend,
    Tool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum NetworkPolicy {
    DenyAll,
    AllowAll,
}

// ---------------------------------------------------------------------------
// Machine-owned configuration ([runtime.security]) and project tightening.
// ---------------------------------------------------------------------------

const MIB: u64 = 1024 * 1024;
const MAX_EXTRA_READ_ROOTS: usize = 64;

fn default_processes() -> Option<u64> {
    Some(2048)
}
fn default_open_files() -> Option<u64> {
    Some(8192)
}
fn default_events() -> u64 {
    1_000_000
}
fn default_event_bytes() -> u64 {
    512 * MIB
}

/// Resource ceilings. Defaults are deliberately generous (ordinary compilation
/// and ML experiments must work); memory/CPU/file-size are unset unless the
/// machine operator chooses values. `strict` turns every configured limit into
/// a hard requirement: an UNSUPPORTED one then refuses launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_memory_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cpu_seconds: Option<u64>,
    /// Additional processes the job may create (on Unix, above the operator's
    /// current process count, because RLIMIT_NPROC is per user).
    #[serde(default = "default_processes", skip_serializing_if = "Option::is_none")]
    pub max_processes: Option<u64>,
    #[serde(
        default = "default_open_files",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_open_files: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_file_size_bytes: Option<u64>,
    #[serde(default)]
    pub strict: bool,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_memory_bytes: None,
            max_cpu_seconds: None,
            max_processes: default_processes(),
            max_open_files: default_open_files(),
            max_file_size_bytes: None,
            strict: false,
        }
    }
}

fn bounded(value: Option<u64>, range: std::ops::RangeInclusive<u64>, name: &str) -> Result<()> {
    if let Some(v) = value {
        require(
            range.contains(&v),
            format!(
                "runtime.security.resources.{name} must be {}..={}",
                range.start(),
                range.end()
            ),
        )?;
    }
    Ok(())
}

impl ResourceLimits {
    pub fn validate(&self) -> Result<()> {
        bounded(
            self.max_memory_bytes,
            64 * MIB..=1 << 44,
            "max_memory_bytes",
        )?;
        bounded(self.max_cpu_seconds, 1..=30 * 86_400, "max_cpu_seconds")?;
        bounded(self.max_processes, 16..=1_000_000, "max_processes")?;
        bounded(self.max_open_files, 64..=1_048_576, "max_open_files")?;
        bounded(
            self.max_file_size_bytes,
            MIB..=1 << 44,
            "max_file_size_bytes",
        )
    }
}

/// Stage 9 event-volume ceiling per experiment attempt. Machine-owned; a project
/// may only lower it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventVolumeLimits {
    #[serde(default = "default_events")]
    pub max_events_per_attempt: u64,
    #[serde(default = "default_event_bytes")]
    pub max_event_bytes_per_attempt: u64,
}

impl Default for EventVolumeLimits {
    fn default() -> Self {
        Self {
            max_events_per_attempt: default_events(),
            max_event_bytes_per_attempt: default_event_bytes(),
        }
    }
}

impl EventVolumeLimits {
    pub fn validate(&self) -> Result<()> {
        require(
            (1..=100_000_000).contains(&self.max_events_per_attempt),
            "runtime.security.experiment_events.max_events_per_attempt must be 1..=100000000",
        )?;
        require(
            (64 * 1024..=64 << 30).contains(&self.max_event_bytes_per_attempt),
            "runtime.security.experiment_events.max_event_bytes_per_attempt must be 65536..=68719476736",
        )
    }
}

/// `[runtime.security]`: machine/user security authority. Repository config can
/// never add read roots, environment, network or larger limits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityConfig {
    /// Extra absolute read-only roots for every worker (e.g. toolchains in HOME).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_roots: Vec<PathBuf>,
    /// Variable NAMES copied from the controller environment into workers.
    /// Credential-looking names are refused here; pass those explicitly per job.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inherit_env: Vec<String>,
    /// Explicit non-secret worker variables (e.g. RUSTUP_HOME).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub resources: ResourceLimits,
    #[serde(default)]
    pub experiment_events: EventVolumeLimits,
}

impl SecurityConfig {
    pub fn validate(&self) -> Result<()> {
        require(
            self.read_roots.len() <= MAX_EXTRA_READ_ROOTS,
            "runtime.security.read_roots exceeds 64 entries",
        )?;
        for root in &self.read_roots {
            paths::absolute_path(root)?;
            require(
                root.components().count() > 1,
                "runtime.security.read_roots cannot grant the filesystem root",
            )?;
        }
        require(
            self.inherit_env.len() <= 64 && self.env.len() <= 64,
            "runtime.security environment allowlists exceed 64 entries",
        )?;
        for name in self.inherit_env.iter().chain(self.env.keys()) {
            require(
                valid_env_name(name) && !loader_injection(name) && !reserved_env(name),
                format!(
                    "runtime.security: environment name {name:?} is invalid, reserved, or a loader-injection variable"
                ),
            )?;
            require(
                !credential_like(name),
                format!(
                    "runtime.security: {name} looks like a credential; ambient worker environment cannot carry credentials (pass it explicitly per experiment with --env)"
                ),
            )?;
        }
        for value in self.env.values() {
            require(
                value.len() <= 4096 && !value.contains('\0'),
                "runtime.security.env values must be at most 4096 bytes without NUL",
            )?;
        }
        self.resources.validate()?;
        self.experiment_events.validate()
    }

    /// Machine policy intersected with repository policy: every ceiling is the
    /// minimum of both, so a project can lower but never raise authority.
    pub fn tightened(&self, project: &ProjectSecurity) -> Self {
        fn min(machine: Option<u64>, project: Option<u64>) -> Option<u64> {
            match (machine, project) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            }
        }
        let mut effective = self.clone();
        let r = &mut effective.resources;
        r.max_memory_bytes = min(r.max_memory_bytes, project.max_memory_bytes);
        r.max_cpu_seconds = min(r.max_cpu_seconds, project.max_cpu_seconds);
        r.max_processes = min(r.max_processes, project.max_processes);
        r.max_open_files = min(r.max_open_files, project.max_open_files);
        r.max_file_size_bytes = min(r.max_file_size_bytes, project.max_file_size_bytes);
        let e = &mut effective.experiment_events;
        e.max_events_per_attempt = e
            .max_events_per_attempt
            .min(project.max_experiment_events.unwrap_or(u64::MAX));
        e.max_event_bytes_per_attempt = e
            .max_event_bytes_per_attempt
            .min(project.max_experiment_event_bytes.unwrap_or(u64::MAX));
        effective
    }
}

/// `[security]` in `.agentctl/project.toml`: tightening only. There is
/// deliberately no field that could grant paths, environment or network.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectSecurity {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_memory_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_cpu_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_processes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_open_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_file_size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_experiment_events: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_experiment_event_bytes: Option<u64>,
}

impl ProjectSecurity {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
    pub fn validate(&self) -> Result<()> {
        for value in [
            self.max_memory_bytes,
            self.max_cpu_seconds,
            self.max_processes,
            self.max_open_files,
            self.max_file_size_bytes,
            self.max_experiment_events,
            self.max_experiment_event_bytes,
        ]
        .into_iter()
        .flatten()
        {
            require(value >= 1, "project security limits must be positive")?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Environment vocabulary.
// ---------------------------------------------------------------------------

pub fn valid_env_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

/// Dynamic-loader injection; never passed to a worker under any configuration.
pub fn loader_injection(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.starts_with("LD_") || upper.starts_with("DYLD_")
}

const RESERVED_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "TMP",
    "TEMP",
    "USERPROFILE",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "XDG_DATA_HOME",
    "CODEX_HOME",
    "CLAUDE_CONFIG_DIR",
    "CARGO_TARGET_DIR",
    "PYTHONDONTWRITEBYTECODE",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_CONFIG_GLOBAL",
    "GIT_OPTIONAL_LOCKS",
    "GIT_TERMINAL_PROMPT",
    "LANG",
];

/// Names agentctl sets itself for every worker; nothing may override them.
pub fn reserved_env(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.starts_with("AGENTCTL_") || RESERVED_ENV.contains(&upper.as_str())
}

/// Conservative heuristic for names that usually carry credentials.
pub fn credential_like(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    const PARTS: &[&str] = &[
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "API_KEY",
        "APIKEY",
        "PRIVATE_KEY",
        "AUTH",
        "SESSION",
        "COOKIE",
    ];
    const PREFIXES: &[&str] = &[
        "AWS_",
        "AZURE_",
        "GOOGLE_",
        "GCP_",
        "GCLOUD_",
        "CLOUDSDK_",
        "GH_",
        "GITHUB_",
        "GITLAB_",
        "NPM_",
        "NUGET_",
        "PYPI_",
        "TWINE_",
        "CARGO_REGISTRY",
        "SSH_",
        "GPG_",
        "GNUPG",
        "GIT_",
        "ANTHROPIC_",
        "OPENAI_",
        "CODEX_",
        "CLAUDE_",
        "HF_",
        "HUGGING",
        "DOCKER_",
        "KUBE",
        "VAULT_",
        "OP_",
    ];
    PARTS.iter().any(|p| upper.contains(p)) || PREFIXES.iter().any(|p| upper.starts_with(p))
}

/// Explicit per-experiment passthrough (an operator authorization for that job).
pub fn validate_passthrough_names(names: &[String]) -> Result<()> {
    require(names.len() <= 32, "at most 32 --env passthrough names")?;
    let mut seen = BTreeSet::new();
    for name in names {
        require(
            valid_env_name(name),
            format!("--env {name:?}: expected an environment variable NAME"),
        )?;
        require(
            !loader_injection(name) && !reserved_env(name),
            format!(
                "--env {name}: loader-injection and agentctl-reserved variables cannot be passed to workers"
            ),
        )?;
        require(
            seen.insert(name.to_ascii_uppercase()),
            "duplicate --env name",
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Compiled per-launch policy.
// ---------------------------------------------------------------------------

/// A path workers may not touch even when it lies inside a granted root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeniedPath {
    pub path: PathBuf,
    pub read: bool,
    /// Also deny metadata (stat) — used for agentctl's own state directories.
    pub metadata: bool,
    pub write: bool,
    /// Controller-issued subpaths (scratch, provider auth files) re-allowed inside.
    pub except: Vec<PathBuf>,
    pub reason: &'static str,
}

#[derive(Debug, Clone, Default)]
pub struct FilesystemPolicy {
    pub workspace: PathBuf,
    pub scratch: PathBuf,
    /// Subtrees readable (and executable).
    pub read_roots: Vec<PathBuf>,
    /// Individual files readable.
    pub read_files: Vec<PathBuf>,
    /// Subtrees writable (also readable).
    pub write_roots: Vec<PathBuf>,
    /// Individual files writable (device nodes, provider auth files).
    pub write_files: Vec<PathBuf>,
    /// Always win over every grant.
    pub denied: Vec<DeniedPath>,
}

pub struct SecurityPolicy {
    pub class: WorkerClass,
    pub network: NetworkPolicy,
    pub filesystem: FilesystemPolicy,
    /// The complete worker environment (allowlist; nothing else is inherited).
    pub environment: BTreeMap<String, OsString>,
    pub resources: ResourceLimits,
    /// Values used only to redact captured output; never persisted.
    pub secrets: Vec<String>,
    /// Resolved program path (not canonicalized: multicall proxies use argv[0]).
    pub program: PathBuf,
    pub args: Vec<String>,
    pub marker: String,
}

impl SecurityPolicy {
    /// Every capability this launch needs, with the minimum acceptable status.
    pub fn required(&self) -> Vec<(Capability, CapabilityStatus)> {
        use Capability::*;
        use CapabilityStatus::*;
        let mut required = vec![
            (FilesystemRead, Enforced),
            (FilesystemWrite, Enforced),
            (EnvironmentIsolation, Enforced),
            (ProcessTree, BestEffort),
            (WallClock, Enforced),
            (OutputCapture, Enforced),
        ];
        if self.network == NetworkPolicy::DenyAll {
            required.push((NetworkDeny, Enforced));
        }
        if self.class == WorkerClass::Tool {
            required.push((CredentialIsolation, Enforced));
        }
        if self.resources.strict {
            let r = &self.resources;
            for (set, capability) in [
                (r.max_memory_bytes.is_some(), MemoryLimit),
                (r.max_cpu_seconds.is_some(), CpuLimit),
                (r.max_processes.is_some(), ProcessLimit),
                (r.max_open_files.is_some(), OpenFileLimit),
                (r.max_file_size_bytes.is_some(), FileSizeLimit),
            ] {
                if set {
                    required.push((capability, BestEffort));
                }
            }
        }
        required
    }
}

/// Fail closed: every required capability must meet its minimum status.
pub fn check(report: &CapabilityReport, policy: &SecurityPolicy) -> Result<()> {
    require(
        !report.running_as_root,
        "SECURITY_CAPABILITY_UNSUPPORTED: refusing to launch workers while running as root/administrator",
    )?;
    for (capability, minimum) in policy.required() {
        let status = report.status(capability);
        require(
            status >= minimum,
            format!(
                "SECURITY_CAPABILITY_UNSUPPORTED: backend {} reports {capability:?} as {status:?}; this job requires {minimum:?}; refusing to launch (no unsandboxed fallback)",
                report.backend
            ),
        )?;
    }
    Ok(())
}

fn canonical_or_self(path: &Path) -> PathBuf {
    if let Ok(real) = fs::canonicalize(path) {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if parent != path => canonical_or_self(parent).join(name),
        _ => path.to_path_buf(),
    }
}

fn push_unique(list: &mut Vec<PathBuf>, path: PathBuf) {
    if !list.contains(&path) {
        list.push(path);
    }
}

/// OS toolchain/runtime roots every worker needs to execute ordinary programs.
/// Nothing under a user's HOME is included.
pub fn platform_read_roots() -> Vec<PathBuf> {
    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &[
            "/usr",
            "/bin",
            "/sbin",
            "/System",
            "/Library",
            "/private/etc",
            "/private/var/db/timezone",
            "/private/var/select",
            "/dev",
            "/opt",
            "/nix",
            "/Applications/Xcode.app",
            "/Applications/Xcode-beta.app",
        ]
    } else if cfg!(target_os = "linux") {
        &[
            "/usr",
            "/bin",
            "/sbin",
            "/lib",
            "/lib32",
            "/lib64",
            "/libx32",
            "/etc",
            "/opt",
            "/proc",
            "/sys",
            "/dev",
            "/nix",
            "/run/current-system",
        ]
    } else {
        &[]
    };
    let mut roots = vec![];
    for candidate in candidates {
        if let Ok(real) = fs::canonicalize(candidate) {
            push_unique(&mut roots, real);
        }
    }
    roots
}

/// Individual system files whose symlink targets live outside the default roots
/// (e.g. /etc/resolv.conf -> /run/systemd/resolve/stub-resolv.conf).
fn platform_read_files() -> Vec<PathBuf> {
    let mut files = vec![];
    for candidate in [
        "/etc/resolv.conf",
        "/etc/hosts",
        "/etc/localtime",
        "/etc/nsswitch.conf",
    ] {
        if let Ok(real) = fs::canonicalize(candidate) {
            if real.is_file() {
                push_unique(&mut files, real);
            }
        }
    }
    files
}

fn platform_write_files() -> (Vec<PathBuf>, Vec<PathBuf>) {
    let files: &[&str] = if cfg!(target_os = "linux") {
        &[
            "/dev/null",
            "/dev/zero",
            "/dev/full",
            "/dev/random",
            "/dev/urandom",
        ]
    } else if cfg!(unix) {
        &["/dev/null", "/dev/zero"]
    } else {
        &[]
    };
    // Shared memory is required by Python multiprocessing / PyTorch DataLoader.
    let roots: &[&str] = if cfg!(target_os = "linux") {
        &["/dev/shm"]
    } else {
        &[]
    };
    let existing = |list: &[&str]| {
        list.iter()
            .map(PathBuf::from)
            .filter(|p| p.exists())
            .collect::<Vec<_>>()
    };
    (existing(files), existing(roots))
}

/// Well-known credential stores under the operator's HOME. Denied even when an
/// operator-granted read root happens to contain them.
const CREDENTIAL_HOME_PATHS: &[&str] = &[
    ".ssh",
    ".aws",
    ".gnupg",
    ".azure",
    ".kube",
    ".docker",
    ".config/gcloud",
    ".config/gh",
    ".config/git/credentials",
    ".git-credentials",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".password-store",
    ".local/share/keyrings",
    "Library/Keychains",
];

/// Controller PATH without relative/empty entries and without anything under
/// the workspace or agentctl state (so a repository cannot shadow `cargo`).
pub(crate) fn sanitized_path(ambient: Option<&OsStr>, forbidden: &[&Path]) -> OsString {
    let mut kept: Vec<PathBuf> = vec![];
    if let Some(value) = ambient {
        for entry in std::env::split_paths(value) {
            if paths::absolute_path(&entry).is_err() {
                continue;
            }
            let real = canonical_or_self(&entry);
            if forbidden
                .iter()
                .any(|f| real.starts_with(f) || entry.starts_with(f))
            {
                continue;
            }
            push_unique(&mut kept, entry);
        }
    }
    std::env::join_paths(kept).unwrap_or_default()
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

fn resolve_program(program: &Path, cwd: &Path, path_var: &OsStr) -> Result<PathBuf> {
    let not_found = || {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("executable {} not found", program.display()),
        ))
    };
    if program.components().count() > 1 || program.is_absolute() {
        let candidate = if program.is_absolute() {
            program.to_path_buf()
        } else {
            cwd.join(program)
        };
        return if candidate.is_file() {
            Ok(candidate)
        } else {
            Err(not_found())
        };
    }
    std::env::split_paths(path_var)
        .map(|dir| dir.join(program))
        .find(|c| is_executable_file(c))
        .ok_or_else(not_found)
}

/// The executable's own install directory plus one level of `#!` interpreter.
fn executable_roots(
    program: &Path,
    path_var: &OsStr,
    home: Option<&Path>,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let (mut roots, mut files) = (vec![], vec![]);
    let mut grant = |file: PathBuf| {
        match file.parent() {
            Some(parent)
                if parent.components().count() > 1
                    && !home.is_some_and(|h| h.starts_with(parent)) =>
            {
                push_unique(&mut roots, parent.to_path_buf())
            }
            // Never widen to "/" or to an ancestor of HOME: grant the file only.
            _ => push_unique(&mut files, file),
        }
    };
    let Ok(real) = fs::canonicalize(program) else {
        return (roots, files);
    };
    grant(real.clone());
    if let Some((interpreter, argument)) = shebang(&real) {
        let target = if interpreter.file_name() == Some(OsStr::new("env")) {
            argument
                .and_then(|name| resolve_program(Path::new(&name), Path::new("/"), path_var).ok())
        } else {
            Some(interpreter)
        };
        if let Some(real) = target.and_then(|t| fs::canonicalize(t).ok()) {
            grant(real);
        }
    }
    (roots, files)
}

fn shebang(path: &Path) -> Option<(PathBuf, Option<String>)> {
    use std::io::Read;
    let mut head = [0_u8; 256];
    let n = fs::File::open(path).ok()?.read(&mut head).ok()?;
    let line = head[..n].strip_prefix(b"#!")?;
    let line = &line[..line.iter().position(|b| *b == b'\n').unwrap_or(line.len())];
    let text = std::str::from_utf8(line).ok()?;
    let mut parts = text.split_whitespace();
    let interpreter = PathBuf::from(parts.next()?);
    interpreter
        .is_absolute()
        .then(|| (interpreter, parts.next().map(str::to_owned)))
}

/// Builds the complete, allowlisted worker environment.
fn environment(
    spec: &ProcessSpec,
    home: &Path,
    path_var: &OsStr,
    marker: &str,
) -> Result<(BTreeMap<String, OsString>, Vec<String>)> {
    let mut env: BTreeMap<String, OsString> = BTreeMap::new();
    let mut secrets = vec![];
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let scratch_home = spec.scratch.join("home");
    for (name, value) in [
        ("PATH", path_var.to_os_string()),
        ("HOME", scratch_home.clone().into_os_string()),
        ("TMPDIR", spec.scratch.clone().into_os_string()),
        ("TMP", spec.scratch.clone().into_os_string()),
        ("TEMP", spec.scratch.clone().into_os_string()),
        ("CODEX_HOME", scratch_home.join(".codex").into_os_string()),
        (
            "CLAUDE_CONFIG_DIR",
            scratch_home.join(".claude").into_os_string(),
        ),
        ("XDG_CONFIG_HOME", scratch_home.clone().into_os_string()),
        ("XDG_CACHE_HOME", scratch_home.clone().into_os_string()),
        ("XDG_DATA_HOME", scratch_home.clone().into_os_string()),
        (
            "CARGO_TARGET_DIR",
            spec.scratch.join("target").into_os_string(),
        ),
        ("PYTHONDONTWRITEBYTECODE", "1".into()),
        ("GIT_CONFIG_NOSYSTEM", "1".into()),
        ("GIT_CONFIG_GLOBAL", null.into()),
        ("GIT_OPTIONAL_LOCKS", "0".into()),
        ("GIT_TERMINAL_PROMPT", "0".into()),
        ("LANG", "en_US.UTF-8".into()),
        (tree::MARKER_ENV, marker.into()),
    ] {
        env.insert(name.into(), value);
    }
    if cfg!(windows) {
        env.insert("USERPROFILE".into(), scratch_home.clone().into_os_string());
        for name in [
            "SystemRoot",
            "windir",
            "PATHEXT",
            "COMSPEC",
            "NUMBER_OF_PROCESSORS",
        ] {
            if let Some(value) = std::env::var_os(name) {
                env.insert(name.into(), value);
            }
        }
    }
    let _ = home;
    for (name, value) in &spec.security.env {
        env.insert(name.clone(), value.into());
    }
    for name in &spec.security.inherit_env {
        if let Some(value) = std::env::var_os(name) {
            env.insert(name.clone(), value);
        }
    }
    if let Some(path) = &spec.experiment_event_file {
        env.insert(
            super::runtime::EVENT_FILE_ENV.into(),
            path.clone().into_os_string(),
        );
    }
    if spec.class == WorkerClass::ProviderFrontend {
        // Asks the provider CLI to scrub provider/cloud credentials from the
        // environment of its own tool subprocesses. Provider-side behavior; not
        // relied upon for any agentctl guarantee.
        env.insert("CLAUDE_CODE_SUBPROCESS_ENV_SCRUB".into(), "1".into());
        if let Some(native) = &spec.native_auth {
            for (name, value) in native.variables() {
                match value {
                    Some(value) => env.insert(name.into(), value),
                    None => env.remove(name),
                };
            }
        }
        if let Some((source, target)) = &spec.api_key {
            let value = std::env::var(source)
                .map_err(|_| Error::Invalid("configured API-key environment disappeared".into()))?;
            env.insert(target.clone(), value.clone().into());
            secrets.push(value);
        }
    } else {
        require(
            spec.native_auth.is_none() && spec.api_key.is_none(),
            "tool workers never receive provider authentication",
        )?;
    }
    validate_passthrough_names(&spec.credential_env)?;
    for name in &spec.credential_env {
        if let Ok(value) = std::env::var(name) {
            env.insert(name.clone(), value.clone().into());
            if !value.is_empty() {
                secrets.push(value);
            }
        }
    }
    Ok((env, secrets))
}

/// Compiles a launch request into the OS-neutral policy. Pure policy logic plus
/// read-only filesystem resolution; nothing is spawned here.
pub fn compile(spec: &ProcessSpec) -> Result<SecurityPolicy> {
    spec.security.validate()?;
    for path in [&spec.workspace, &spec.scratch, &spec.cwd] {
        paths::absolute_path(path)?;
    }
    let workspace = canonical_or_self(&spec.workspace);
    let scratch = canonical_or_self(&spec.scratch);
    let data_root = canonical_or_self(&spec.data_root);
    let config_root = canonical_or_self(&spec.config_root);
    let cache_root = canonical_or_self(&spec.cache_root);
    for state in [&data_root, &config_root, &cache_root] {
        require(
            !workspace.starts_with(state) && !state.starts_with(&workspace),
            "runtime machine state must be outside the workspace",
        )?;
    }
    let ambient_home = std::env::var_os("HOME").map(PathBuf::from);
    let real_home = ambient_home.as_deref().map(canonical_or_self);
    let path_var = sanitized_path(
        std::env::var_os("PATH").as_deref(),
        &[&workspace, &scratch, &data_root, &config_root, &cache_root],
    );
    let program = resolve_program(&spec.executable, &spec.cwd, &path_var)?;

    let mut fs_policy = FilesystemPolicy {
        workspace: workspace.clone(),
        scratch: scratch.clone(),
        ..Default::default()
    };
    fs_policy.read_roots = platform_read_roots();
    fs_policy.read_files = platform_read_files();
    for root in &spec.security.read_roots {
        if let Ok(real) = fs::canonicalize(root) {
            push_unique(&mut fs_policy.read_roots, real);
        }
    }
    push_unique(&mut fs_policy.read_roots, workspace.clone());
    for git in &spec.git_directories {
        push_unique(&mut fs_policy.read_roots, canonical_or_self(git));
    }
    let (exe_roots, exe_files) = executable_roots(&program, &path_var, real_home.as_deref());
    for root in exe_roots {
        push_unique(&mut fs_policy.read_roots, root);
    }
    for file in exe_files {
        push_unique(&mut fs_policy.read_files, file);
    }
    let (device_files, device_roots) = platform_write_files();
    fs_policy.write_files = device_files;
    fs_policy.write_roots = device_roots;
    push_unique(&mut fs_policy.write_roots, scratch.clone());
    if spec.writable {
        push_unique(&mut fs_policy.write_roots, workspace.clone());
    }

    let auth_files: Vec<PathBuf> = spec
        .native_auth
        .as_ref()
        .map(|a| a.readable_files())
        .unwrap_or_default()
        .iter()
        .map(|p| canonical_or_self(p))
        .collect();
    for file in &auth_files {
        require(
            !file.starts_with(&workspace)
                && !file.starts_with(&data_root)
                && !file.starts_with(&config_root)
                && !file.starts_with(&cache_root),
            "provider-native authentication must remain outside workspace/agentctl state",
        )?;
        push_unique(&mut fs_policy.read_files, file.clone());
        push_unique(&mut fs_policy.write_files, file.clone());
    }

    let config_files: Vec<PathBuf> = spec
        .native_auth
        .as_ref()
        .map(|a| a.config_files())
        .unwrap_or_default()
        .iter()
        .map(|p| canonical_or_self(p))
        .collect();
    for file in &config_files {
        push_unique(&mut fs_policy.read_files, file.clone());
    }
    // Native login on macOS goes through the login Keychain, and securityd checks
    // the CLIENT's sandbox for the keychain file. Provider frontends therefore
    // read it (read-only); tool workers keep the denial and lose Keychain mach
    // services entirely.
    let keychain_client = cfg!(target_os = "macos")
        && spec.class == WorkerClass::ProviderFrontend
        && spec.native_auth.is_some();
    if keychain_client {
        if let Some(home) = &real_home {
            let keychains = home.join("Library/Keychains");
            if keychains.is_dir() {
                push_unique(&mut fs_policy.read_roots, keychains);
            }
        }
    }
    let denied = &mut fs_policy.denied;
    denied.push(DeniedPath {
        path: data_root,
        read: true,
        metadata: true,
        write: true,
        except: vec![scratch.clone()],
        reason: "agentctl canonical state",
    });
    for (path, reason) in [
        (config_root, "agentctl machine configuration"),
        (cache_root, "agentctl cache"),
    ] {
        denied.push(DeniedPath {
            path,
            read: true,
            metadata: true,
            write: true,
            except: vec![],
            reason,
        });
    }
    let mut control: Vec<PathBuf> = spec
        .git_directories
        .iter()
        .map(|p| canonical_or_self(p))
        .collect();
    for name in [".git", ".agentctl", ".codex", ".claude"] {
        control.push(canonical_or_self(&workspace.join(name)));
    }
    for path in control {
        if !denied.iter().any(|d| d.path == path) {
            denied.push(DeniedPath {
                path,
                read: false,
                metadata: false,
                write: true,
                except: vec![],
                reason: "Git/agentctl/provider control plane",
            });
        }
    }
    let mut private_roots = vec![];
    if let Some(home) = &real_home {
        private_roots.push(home.join(".codex"));
        private_roots.push(home.join(".claude"));
    }
    if let Some(native) = &spec.native_auth {
        private_roots.push(canonical_or_self(&native.provider_home));
    }
    for root in private_roots {
        let root = canonical_or_self(&root);
        if !denied.iter().any(|d| d.path == root) {
            let mut except: Vec<PathBuf> =
                auth_files.iter().chain(&config_files).cloned().collect();
            except.push(scratch.clone());
            denied.push(DeniedPath {
                path: root,
                read: true,
                metadata: false,
                write: true,
                except,
                reason: "provider-private home",
            });
        }
    }
    if let Some(home) = &real_home {
        for relative in CREDENTIAL_HOME_PATHS
            .iter()
            .filter(|r| !(keychain_client && **r == "Library/Keychains"))
        {
            denied.push(DeniedPath {
                path: home.join(relative),
                read: true,
                metadata: false,
                write: true,
                except: vec![],
                reason: "credential store",
            });
        }
    }
    for rule in &spec.protected {
        paths::safe_relative(&rule.path)?;
        denied.push(DeniedPath {
            path: canonical_or_self(&workspace.join(&rule.path)),
            read: rule.deny_read,
            metadata: false,
            write: true,
            except: vec![],
            reason: "project protected path",
        });
    }
    // A grant that sits inside a (non-excepted) denial is not a grant.
    for grant in [&workspace, &scratch] {
        require(
            !fs_policy.denied.iter().any(|d| {
                d.write
                    && grant.starts_with(&d.path)
                    && !d.except.iter().any(|e| grant.starts_with(e))
            }),
            "workspace/scratch lies inside a denied control-plane or credential path",
        )?;
    }

    let marker = tree::new_marker();
    let (environment, secrets) = environment(spec, &scratch.join("home"), &path_var, &marker)?;
    Ok(SecurityPolicy {
        class: spec.class,
        network: if spec.network {
            NetworkPolicy::AllowAll
        } else {
            NetworkPolicy::DenyAll
        },
        filesystem: fs_policy,
        environment,
        resources: spec.security.resources,
        secrets,
        program,
        args: spec.args.clone(),
        marker,
    })
}

// ---------------------------------------------------------------------------
// Backend dispatch.
// ---------------------------------------------------------------------------

pub(crate) struct Launch<'a> {
    pub cwd: &'a Path,
    /// Workspace lease, inherited by the whole child family.
    #[cfg_attr(windows, allow(dead_code))]
    pub lock_fd: Option<i32>,
}

pub(crate) struct Spawned {
    pub child: Child,
    pub tree: tree::ProcessTree,
}

/// Platform-specific enforcement and spawn mechanics only. Canonical
/// orchestration never lives behind this boundary.
pub(crate) trait PlatformSecurityBackend: Sync {
    fn capabilities(&self) -> &CapabilityReport;
    fn spawn(&self, policy: &SecurityPolicy, launch: &Launch<'_>) -> Result<Spawned>;
}

#[cfg_attr(
    any(target_os = "macos", target_os = "linux", windows),
    allow(dead_code)
)]
struct UnsupportedPlatform;
impl PlatformSecurityBackend for UnsupportedPlatform {
    fn capabilities(&self) -> &CapabilityReport {
        static REPORT: std::sync::OnceLock<CapabilityReport> = std::sync::OnceLock::new();
        REPORT.get_or_init(|| CapabilityReport {
            backend: "unsupported",
            platform: std::env::consts::OS,
            running_as_root: false,
            capabilities: CapabilityReport::controller_entries().into(),
        })
    }
    fn spawn(&self, _: &SecurityPolicy, _: &Launch<'_>) -> Result<Spawned> {
        Err(Error::Invalid(
            "SECURITY_CAPABILITY_UNSUPPORTED: no security backend exists for this platform".into(),
        ))
    }
}

/// Deterministic capability report of the active backend (no model/network).
pub fn capabilities() -> &'static CapabilityReport {
    backend().capabilities()
}

pub(crate) fn backend() -> &'static dyn PlatformSecurityBackend {
    #[cfg(target_os = "macos")]
    {
        &macos::Seatbelt
    }
    #[cfg(target_os = "linux")]
    {
        &linux::LandlockSeccomp
    }
    #[cfg(windows)]
    {
        &windows::JobObjects
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        &UnsupportedPlatform
    }
}

/// True when the host enforces the baseline every worker requires.
pub fn baseline_enforced() -> bool {
    let report = backend().capabilities();
    !report.running_as_root
        && [
            Capability::FilesystemRead,
            Capability::FilesystemWrite,
            Capability::EnvironmentIsolation,
        ]
        .iter()
        .all(|c| report.status(*c) == CapabilityStatus::Enforced)
}

/// The single spawn entry point: capability check first, then the backend.
pub(crate) fn launch(policy: &SecurityPolicy, launch: &Launch<'_>) -> Result<Spawned> {
    let backend = backend();
    check(backend.capabilities(), policy)?;
    backend.spawn(policy, launch)
}

pub(crate) fn running_privileged() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(windows)]
    {
        windows::elevated()
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

// ---------------------------------------------------------------------------
// Unix spawn helpers shared by the macOS and Linux backends.
// ---------------------------------------------------------------------------

#[cfg(unix)]
pub(crate) mod unix {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    pub(crate) type Resource = libc::__rlimit_resource_t;
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    pub(crate) type Resource = libc::c_int;

    /// (resource, soft, hard), computed in the parent so the child only issues
    /// async-signal-safe setrlimit calls.
    pub(crate) fn rlimits(
        limits: &ResourceLimits,
        memory: bool,
    ) -> Vec<(Resource, libc::rlim_t, libc::rlim_t)> {
        fn clamp(
            resource: Resource,
            soft: u64,
            hard: u64,
        ) -> (Resource, libc::rlim_t, libc::rlim_t) {
            let mut current = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: valid out-pointer.
            let ceiling = if unsafe { libc::getrlimit(resource, &mut current) } == 0 {
                current.rlim_max
            } else {
                libc::RLIM_INFINITY
            };
            let fit = |v: u64| (v as libc::rlim_t).min(ceiling);
            (resource, fit(soft).min(fit(hard)), fit(hard))
        }
        // Core dumps of provider/tool processes could contain credentials and
        // would land in the workspace.
        let mut out = vec![(libc::RLIMIT_CORE as Resource, 0, 0)];
        if let Some(n) = limits.max_open_files {
            out.push(clamp(libc::RLIMIT_NOFILE as Resource, n, n));
        }
        if let Some(s) = limits.max_cpu_seconds {
            // SIGXCPU at the soft limit, SIGKILL five seconds later.
            out.push(clamp(libc::RLIMIT_CPU as Resource, s, s + 5));
        }
        if let Some(b) = limits.max_file_size_bytes {
            out.push(clamp(libc::RLIMIT_FSIZE as Resource, b, b));
        }
        if memory {
            if let Some(b) = limits.max_memory_bytes {
                out.push(clamp(libc::RLIMIT_AS as Resource, b, b));
            }
        }
        if let (Some(n), Some(current)) = (limits.max_processes, tree::count_user_processes()) {
            let total = current as u64 + n;
            out.push(clamp(libc::RLIMIT_NPROC as Resource, total, total));
        }
        out
    }

    /// Environment, stdio, cwd, own process group, inherited lease/sentinel fds
    /// and rlimits. `extra` runs last in the child (Landlock/seccomp on Linux)
    /// and must be async-signal-safe.
    pub(crate) fn command(
        program: &Path,
        args: &[OsString],
        policy: &SecurityPolicy,
        launch: &Launch<'_>,
        sentinel_fd: i32,
        memory_rlimit: bool,
        extra: impl Fn() -> std::io::Result<()> + Send + Sync + 'static,
    ) -> Command {
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(launch.cwd)
            .env_clear()
            .envs(&policy.environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let limits = rlimits(&policy.resources, memory_rlimit);
        let inherit: Vec<i32> = launch.lock_fd.into_iter().chain([sentinel_fd]).collect();
        // SAFETY: only async-signal-safe syscalls (fcntl, setrlimit, and the
        // backend's prctl/syscall hooks) run between fork and exec; all data was
        // prepared in the parent and is not allocated here.
        unsafe {
            command.pre_exec(move || {
                for fd in &inherit {
                    if libc::fcntl(*fd, libc::F_SETFD, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                for (resource, soft, hard) in &limits {
                    let value = libc::rlimit {
                        rlim_cur: *soft,
                        rlim_max: *hard,
                    };
                    if libc::setrlimit(*resource, &value) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                extra()
            });
        }
        command
    }

    pub(crate) fn spawn(
        mut command: Command,
        policy: &SecurityPolicy,
        sentinel: tree::Sentinel,
    ) -> Result<Spawned> {
        let child = command.spawn()?;
        drop(command);
        let tree = tree::ProcessTree::unix(child.id(), policy.marker.clone(), sentinel);
        Ok(Spawned { child, tree })
    }
}

// ---------------------------------------------------------------------------
// Operator diagnostics (`agentctl security doctor`).
// ---------------------------------------------------------------------------

/// Local-only self-test: launches `/bin/sh` through the real backend and
/// checks that an allowed workspace file is readable while a planted secret,
/// a write outside the workspace and `.git` stay denied. No model/network.
pub fn self_test(config: &SecurityConfig) -> std::result::Result<(), String> {
    use super::runtime::process::{NativeProcess, RunningProcess};
    let base = std::env::temp_dir().join(format!(
        "agentctl-security-self-test-{}-{}",
        std::process::id(),
        super::now_ms().unwrap_or_default()
    ));
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(base.clone());
    let setup = || -> std::io::Result<PathBuf> {
        for dir in [
            "repo/.git",
            "state/data/scratch",
            "state/config",
            "state/cache",
            "outside",
        ] {
            fs::create_dir_all(base.join(dir))?;
        }
        fs::write(base.join("repo/allowed"), "ok")?;
        fs::write(base.join("outside/secret"), "planted secret")?;
        fs::canonicalize(&base)
    };
    let base = setup().map_err(|e| format!("self-test setup failed: {e}"))?;
    let spec = ProcessSpec {
        project_policy_hash: None,
        native_auth: None,
        api_key: None,
        executable: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            "cat \"$1\" >/dev/null || exit 10; if cat \"$2\" >/dev/null 2>&1; then exit 11; fi; if (printf x > \"$3\") 2>/dev/null; then exit 12; fi; if (printf x > \"$4\") 2>/dev/null; then exit 13; fi; exit 0".into(),
            "self-test".into(),
            base.join("repo/allowed").display().to_string(),
            base.join("outside/secret").display().to_string(),
            base.join("outside/written").display().to_string(),
            base.join("repo/.git/written").display().to_string(),
        ],
        input: vec![],
        cwd: base.join("repo"),
        workspace: base.join("repo"),
        scratch: base.join("state/data/scratch"),
        data_root: base.join("state/data"),
        config_root: base.join("state/config"),
        cache_root: base.join("state/cache"),
        writable: true,
        network: false,
        timeout_ms: 10_000,
        git_directories: vec![base.join("repo/.git")],
        protected: vec![],
        credential_env: vec![],
        experiment_event_file: None,
        class: WorkerClass::Tool,
        security: config.clone(),
        lock_fd: None,
    };
    let mut process = NativeProcess::launch(&spec).map_err(|e| e.to_string())?;
    let output = loop {
        match process.poll() {
            Ok(Some(output)) => break output,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(e) => return Err(e.to_string()),
        }
    };
    let outside_written = base.join("outside/written").exists();
    match (output.exit, output.failure, outside_written) {
        (Some(0), None, false) => Ok(()),
        (exit, failure, written) => Err(format!(
            "self-test violated confinement: exit {exit:?} (10 allowed read failed, 11 secret readable, 12 outside write, 13 .git write), failure {failure:?}, outside file created: {written}"
        )),
    }
}

#[cfg(all(test, unix))]
mod native_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn report(status: CapabilityStatus) -> CapabilityReport {
        use Capability::*;
        CapabilityReport {
            backend: "fixture",
            platform: "fixture",
            running_as_root: false,
            capabilities: [
                FilesystemRead,
                FilesystemWrite,
                EnvironmentIsolation,
                ProcessTree,
                NetworkDeny,
                CredentialIsolation,
                WallClock,
                OutputCapture,
            ]
            .into_iter()
            .map(|c| CapabilityReport::entry(c, status, "fixture", ""))
            .collect(),
        }
    }

    fn policy(network: NetworkPolicy, resources: ResourceLimits) -> SecurityPolicy {
        SecurityPolicy {
            class: WorkerClass::Tool,
            network,
            filesystem: FilesystemPolicy::default(),
            environment: BTreeMap::new(),
            resources,
            secrets: vec![],
            program: "/bin/true".into(),
            args: vec![],
            marker: "m".into(),
        }
    }

    #[test]
    fn unsupported_or_best_effort_hard_requirements_refuse_before_launch() {
        let deny = policy(NetworkPolicy::DenyAll, ResourceLimits::default());
        check(&report(CapabilityStatus::Enforced), &deny).unwrap();
        let error = check(&report(CapabilityStatus::BestEffort), &deny).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("SECURITY_CAPABILITY_UNSUPPORTED")
        );
        let mut missing_network = report(CapabilityStatus::Enforced);
        missing_network
            .capabilities
            .retain(|c| c.capability != Capability::NetworkDeny);
        assert!(check(&missing_network, &deny).is_err());
        // Network allowed: missing network denial is not required.
        check(
            &missing_network,
            &policy(NetworkPolicy::AllowAll, ResourceLimits::default()),
        )
        .unwrap();
        // strict resources: an UNSUPPORTED configured limit refuses launch.
        let strict = ResourceLimits {
            max_memory_bytes: Some(1 << 30),
            strict: true,
            ..ResourceLimits::default()
        };
        assert!(
            check(
                &report(CapabilityStatus::Enforced),
                &policy(NetworkPolicy::DenyAll, strict)
            )
            .is_err()
        );
        let mut root = report(CapabilityStatus::Enforced);
        root.running_as_root = true;
        assert!(check(&root, &deny).is_err());
    }

    #[test]
    fn project_policy_only_tightens_machine_authority() {
        let machine = SecurityConfig::default();
        let project = ProjectSecurity {
            max_processes: Some(10_000_000),
            max_open_files: Some(128),
            max_memory_bytes: Some(1 << 30),
            max_experiment_events: Some(u64::MAX),
            max_experiment_event_bytes: Some(1024 * 1024),
            ..Default::default()
        };
        let effective = machine.tightened(&project);
        assert_eq!(
            effective.resources.max_processes,
            machine.resources.max_processes
        );
        assert_eq!(effective.resources.max_open_files, Some(128));
        assert_eq!(effective.resources.max_memory_bytes, Some(1 << 30));
        assert_eq!(
            effective.experiment_events.max_events_per_attempt,
            machine.experiment_events.max_events_per_attempt
        );
        assert_eq!(
            effective.experiment_events.max_event_bytes_per_attempt,
            1024 * 1024
        );
        assert_eq!(effective.read_roots, machine.read_roots);
        // Repository policy has no vocabulary for grants.
        assert!(toml::from_str::<ProjectSecurity>("read_roots = [\"/\"]").is_err());
        assert!(
            toml::from_str::<ProjectSecurity>("inherit_env = [\"AWS_SECRET_ACCESS_KEY\"]").is_err()
        );
    }

    #[test]
    fn machine_environment_allowlists_refuse_credentials_and_loader_injection() {
        for name in [
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "ANTHROPIC_API_KEY",
            "SSH_AUTH_SOCK",
            "NPM_TOKEN",
        ] {
            let config = SecurityConfig {
                inherit_env: vec![name.into()],
                ..Default::default()
            };
            assert!(config.validate().is_err(), "{name}");
        }
        for name in [
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "PATH",
            "HOME",
            "AGENTCTL_JOB_MARKER",
        ] {
            assert!(
                validate_passthrough_names(&[name.into()]).is_err(),
                "{name}"
            );
        }
        let ok = SecurityConfig {
            inherit_env: vec!["RUSTUP_HOME".into()],
            env: BTreeMap::from([("CARGO_HOME".into(), "/opt/cargo".into())]),
            read_roots: vec!["/opt/toolchains".into()],
            ..Default::default()
        };
        ok.validate().unwrap();
        assert!(
            SecurityConfig {
                read_roots: vec!["/".into()],
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            SecurityConfig {
                read_roots: vec!["relative".into()],
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        validate_passthrough_names(&["WANDB_API_KEY".into()]).unwrap();
    }

    #[test]
    fn sanitized_path_drops_relative_and_workspace_entries() {
        let joined = std::env::join_paths([
            "/usr/bin",
            "",
            ".",
            "bin",
            "/repo/node_modules/.bin",
            "/usr/bin",
        ])
        .unwrap();
        let kept = sanitized_path(Some(&joined), &[Path::new("/repo")]);
        assert_eq!(
            std::env::split_paths(&kept).collect::<Vec<_>>(),
            vec![PathBuf::from("/usr/bin")]
        );
    }

    #[test]
    fn keychain_access_is_granted_only_to_native_provider_frontends() {
        let base = std::env::temp_dir().join(format!(
            "agentctl-keychain-policy-{}-{}",
            std::process::id(),
            crate::local::now_ms().unwrap()
        ));
        fs::create_dir_all(base.join("repo/.git")).unwrap();
        fs::create_dir_all(base.join("state/data/scratch")).unwrap();
        let base = fs::canonicalize(&base).unwrap();
        let mut spec = ProcessSpec {
            project_policy_hash: None,
            native_auth: None,
            api_key: None,
            executable: "/bin/sh".into(),
            args: vec![],
            input: vec![],
            cwd: base.join("repo"),
            workspace: base.join("repo"),
            scratch: base.join("state/data/scratch"),
            data_root: base.join("state/data"),
            config_root: base.join("state/config"),
            cache_root: base.join("state/cache"),
            writable: false,
            network: false,
            timeout_ms: 1000,
            git_directories: vec![base.join("repo/.git")],
            protected: vec![],
            credential_env: vec![],
            experiment_event_file: None,
            class: WorkerClass::Tool,
            security: SecurityConfig::default(),
            lock_fd: None,
        };
        let home = std::env::var_os("HOME").map(|h| canonical_or_self(Path::new(&h)));
        let keychains = home.as_ref().map(|h| h.join("Library/Keychains"));
        let grants = |p: &SecurityPolicy| {
            keychains
                .as_ref()
                .is_some_and(|k| p.filesystem.read_roots.contains(k))
        };
        let denies = |p: &SecurityPolicy| {
            keychains
                .as_ref()
                .is_some_and(|k| p.filesystem.denied.iter().any(|d| &d.path == k && d.read))
        };
        let never_writable = |p: &SecurityPolicy| {
            !p.filesystem
                .write_roots
                .iter()
                .any(|w| keychains.as_ref().is_some_and(|k| w.starts_with(k)))
        };
        let tool = compile(&spec).unwrap();
        assert!(!grants(&tool) && never_writable(&tool));
        if home.is_some() {
            assert!(denies(&tool), "tool workers keep the Keychain denial");
        }
        spec.class = WorkerClass::ProviderFrontend;
        let no_login = compile(&spec).unwrap();
        assert!(!grants(&no_login), "only native login needs the Keychain");
        if let Some(home) = &home {
            spec.native_auth = Some(crate::local::runtime::credentials::NativeAuth {
                provider: "claude".into(),
                home: home.clone(),
                provider_home: home.join(".claude"),
                config_override: false,
            });
            let frontend = compile(&spec).unwrap();
            let exists = keychains.as_ref().is_some_and(|k| k.is_dir());
            assert_eq!(grants(&frontend), cfg!(target_os = "macos") && exists);
            assert_eq!(denies(&frontend), !cfg!(target_os = "macos"));
            assert!(never_writable(&frontend), "the Keychain is never writable");
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn canonical_store_runs_in_defensive_mode_without_extension_loading() {
        let dir = std::env::temp_dir().join(format!(
            "agentctl-defensive-{}-{}",
            std::process::id(),
            crate::local::now_ms().unwrap()
        ));
        let store = crate::local::store::Store::open(&dir.join("state.sqlite3"), 5000).unwrap();
        assert!(
            store
                .connection
                .db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE)
                .unwrap()
        );
        assert!(
            store
                .connection
                .query_row("SELECT load_extension('/nonexistent')", [], |r| r
                    .get::<_, i64>(0))
                .is_err()
        );
        let trusted: i64 = store
            .connection
            .query_row("PRAGMA trusted_schema", [], |r| r.get(0))
            .unwrap();
        assert_eq!(trusted, 0);
        drop(store);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn windows_path_aliases_and_case_folded_control_plane_components() {
        for alias in ["CON", "nul.txt", "Com1.log", "LPT9", "file.", "file "] {
            assert!(paths::windows_alias(alias), "{alias}");
        }
        for normal in ["console", "com10", "src", "aux_data"] {
            assert!(!paths::windows_alias(normal), "{normal}");
        }
        for control in [".git", ".GIT", ".Agentctl", "GIT~1", "agentc~1"] {
            assert!(paths::is_control_plane_component(control), "{control}");
        }
        assert!(!paths::is_control_plane_component(".github"));
        for bad in ["C:x", "a\\b", "file:stream", "../x", "/abs", "a//b"] {
            assert!(paths::safe_relative(bad).is_err(), "{bad}");
        }
    }
}
