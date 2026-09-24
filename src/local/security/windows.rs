//! Windows backend.
//!
//! Implemented: Job Objects own the whole process tree (KILL_ON_JOB_CLOSE, no
//! breakaway, processes start suspended and are assigned before any code runs),
//! active-process and job-memory limits, the shared allowlisted environment with
//! an isolated TEMP/TMP/USERPROFILE, and administrator refusal.
//!
//! Not implemented, and reported UNSUPPORTED rather than faked: Job Objects give
//! no filesystem or network confinement. Restricted tokens / low integrity only
//! stop writes "up" and never reads, and AppContainer (the viable path, which
//! also denies network by default) needs per-root ACL grants that cannot be
//! verified here. Because filesystem confinement is a hard requirement for every
//! worker, all worker launches are refused before spawn on Windows.
use super::*;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_JOB_MEMORY,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, GetCurrentProcess, OpenProcessToken, OpenThread,
    ResumeThread, THREAD_SUSPEND_RESUME,
};

pub(crate) struct JobObjects;

fn report() -> CapabilityReport {
    use Capability::*;
    use CapabilityStatus::*;
    let none =
        "Job Objects provide no filesystem/network confinement; AppContainer is not implemented";
    let mut capabilities = vec![
        CapabilityReport::entry(FilesystemRead, Unsupported, "none", none),
        CapabilityReport::entry(FilesystemWrite, Unsupported, "none", none),
        CapabilityReport::entry(FilesystemMetadataWrite, Unsupported, "none", none),
        CapabilityReport::entry(NetworkDeny, Unsupported, "none", none),
        CapabilityReport::entry(
            ServiceBrokerDeny,
            Unsupported,
            "none",
            "COM/ShellExecute can have a service start a process outside the Job Object; not mediated without AppContainer",
        ),
        CapabilityReport::entry(
            ProcessTree,
            Enforced,
            "Job Object (KILL_ON_JOB_CLOSE, suspended start, no breakaway)",
            "whole tree terminates with the job",
        ),
        CapabilityReport::entry(
            EnvironmentIsolation,
            Enforced,
            "allowlisted environment builder",
            "TEMP/TMP/USERPROFILE isolated per job",
        ),
        CapabilityReport::entry(
            CredentialIsolation,
            Unsupported,
            "environment allowlist only",
            "credential files remain readable without filesystem confinement",
        ),
        CapabilityReport::entry(
            MemoryLimit,
            Enforced,
            "JOB_OBJECT_LIMIT_JOB_MEMORY",
            "aggregate committed memory of the job",
        ),
        CapabilityReport::entry(
            CpuLimit,
            Unsupported,
            "none",
            "CPU rate control not configured",
        ),
        CapabilityReport::entry(
            ProcessLimit,
            Enforced,
            "JOB_OBJECT_LIMIT_ACTIVE_PROCESS",
            "aggregate across the job",
        ),
        CapabilityReport::entry(
            OpenFileLimit,
            Unsupported,
            "none",
            "no per-job handle quota",
        ),
        CapabilityReport::entry(FileSizeLimit, Unsupported, "none", "no per-file size quota"),
    ];
    capabilities.extend(CapabilityReport::controller_entries());
    CapabilityReport {
        backend: "windows-job-objects",
        platform: "windows",
        running_as_root: elevated(),
        capabilities,
    }
}

pub(crate) fn elevated() -> bool {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: pseudo-handle for the current process; valid out-pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut returned = 0u32;
    // SAFETY: buffer of the exact struct size.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    };
    // SAFETY: token was opened above.
    unsafe { CloseHandle(token) };
    ok != 0 && elevation.TokenIsElevated != 0
}

pub(crate) struct Job(HANDLE);
// SAFETY: kernel handles may be used from any thread.
unsafe impl Send for Job {}

impl Job {
    fn new(limits: &ResourceLimits) -> std::io::Result<Self> {
        // SAFETY: anonymous job with default security.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let job = Self(handle);
        // SAFETY: plain-old-data structure.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        let basic = &mut info.BasicLimitInformation;
        basic.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        if let Some(n) = limits.max_processes {
            basic.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            basic.ActiveProcessLimit = u32::try_from(n).unwrap_or(u32::MAX);
        }
        if let Some(bytes) = limits.max_memory_bytes {
            info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
            info.JobMemoryLimit = usize::try_from(bytes).unwrap_or(usize::MAX);
        }
        // SAFETY: valid job handle and exact structure size.
        let ok = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(job)
    }
    fn assign(&self, process: HANDLE) -> std::io::Result<()> {
        // SAFETY: both handles are valid.
        if unsafe { AssignProcessToJobObject(self.0, process) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    pub(crate) fn terminate(&self) -> std::io::Result<()> {
        // SAFETY: valid job handle.
        if unsafe { TerminateJobObject(self.0, 1) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    pub(crate) fn active(&self) -> Option<u32> {
        // SAFETY: plain-old-data structure.
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: exact structure size.
        let ok = unsafe {
            QueryInformationJobObject(
                self.0,
                JobObjectBasicAccountingInformation,
                (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(info.ActiveProcesses)
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: owned handle; KILL_ON_JOB_CLOSE ends any remaining process.
        unsafe { CloseHandle(self.0) };
    }
}

/// Resumes the suspended primary thread only after job assignment.
fn resume(pid: u32) -> std::io::Result<()> {
    // SAFETY: thread snapshot of the whole system.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: plain-old-data structure.
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
    let mut resumed = false;
    // SAFETY: valid snapshot and entry.
    let mut more = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: thread id from the snapshot.
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if !thread.is_null() {
                // SAFETY: valid thread handle.
                unsafe {
                    ResumeThread(thread);
                    CloseHandle(thread);
                }
                resumed = true;
            }
        }
        // SAFETY: valid snapshot and entry.
        more = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    // SAFETY: owned snapshot handle.
    unsafe { CloseHandle(snapshot) };
    if resumed {
        Ok(())
    } else {
        Err(std::io::Error::other("suspended worker thread not found"))
    }
}

impl PlatformSecurityBackend for JobObjects {
    fn capabilities(&self) -> &CapabilityReport {
        static REPORT: std::sync::OnceLock<CapabilityReport> = std::sync::OnceLock::new();
        REPORT.get_or_init(report)
    }
    /// Reached only if a future backend reports filesystem confinement; the
    /// capability check refuses every worker today.
    fn spawn(&self, policy: &SecurityPolicy, launch: &Launch<'_>) -> Result<Spawned> {
        let job = Job::new(&policy.resources)?;
        let mut command = Command::new(&policy.program);
        command
            .args(&policy.args)
            .current_dir(launch.cwd)
            .env_clear()
            .envs(&policy.environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP);
        let mut child = command.spawn()?;
        if let Err(error) = job
            .assign(child.as_raw_handle() as HANDLE)
            .and_then(|()| resume(child.id()))
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error.into());
        }
        Ok(Spawned {
            child,
            tree: tree::ProcessTree::windows(job),
        })
    }
}
