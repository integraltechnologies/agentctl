//! Owned process-tree termination and escape detection.
//!
//! A job's processes are identified three ways:
//! 1. its own POSIX process group (ordinary descendants; killed with killpg);
//! 2. a random per-launch `AGENTCTL_JOB_MARKER` in the environment, which
//!    `setsid`/double-fork daemons normally keep;
//! 3. an inherited "sentinel" pipe write end: the read end reaches EOF only when
//!    every process holding it has exited.
//!
//! After the direct child exits (or is cancelled/timed out) the group is killed,
//! marker/sentinel holders outside the group are found (Linux: `/proc` environ
//! and fd links; macOS: libproc pipe-descriptor scan, plus the marker where the
//! OS still exposes environments) and killed, and the sentinel is checked. A
//! holder that cannot be identified yields `Unproven`: an exited direct child is
//! never taken as proof the tree is dead. Residual (not detectable without
//! cgroups/PID namespaces/Job Objects): a daemon that closes every inherited
//! descriptor — and, on Linux, also clears its environment — leaves no trace.
//! On Windows the Job Object owns the entire tree.
use std::io;

pub(crate) const MARKER_ENV: &str = "AGENTCTL_JOB_MARKER";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TreeOutcome {
    Clean,
    /// Descendants had left the job's process group; they were found and killed.
    #[cfg_attr(not(unix), allow(dead_code))]
    EscapedTerminated(usize),
    Unproven(String),
}

pub(crate) fn new_marker() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut random = [0_u8; 16];
    #[cfg(unix)]
    {
        use std::io::Read;
        let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut random));
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(&random);
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(
        &std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
            .to_le_bytes(),
    );
    hasher.update(&NEXT.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    hasher.finalize().to_hex()[..32].to_string()
}

#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(unix)]
mod unix {
    use super::*;
    use std::collections::BTreeSet;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    pub(crate) struct Sentinel {
        read: OwnedFd,
        write: Option<OwnedFd>,
        /// Identity of the WRITE endpoint, captured while both ends are open.
        /// Never contains 0 (closed/unknown endpoints read back as 0 and would
        /// match unrelated processes). Stable for the tree's lifetime because we
        /// keep the read end open, so the pipe cannot be freed and reused.
        ids: Vec<u64>,
    }

    impl Sentinel {
        pub(crate) fn new() -> io::Result<Self> {
            let mut fds = [0; 2];
            #[cfg(target_os = "linux")]
            // SAFETY: valid two-element out array.
            let result = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
            #[cfg(not(target_os = "linux"))]
            // SAFETY: valid two-element out array.
            let result = unsafe { libc::pipe(fds.as_mut_ptr()) };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fresh descriptors from pipe(2).
            let (read, write) =
                unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
            for fd in fds {
                // SAFETY: owned descriptors; the child clears CLOEXEC on its copy.
                if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            // SAFETY: owned descriptor.
            if unsafe { libc::fcntl(read.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } == -1 {
                return Err(io::Error::last_os_error());
            }
            let mut ids = identities(&read);
            ids.retain(|id| *id != 0);
            Ok(Self {
                read,
                write: Some(write),
                ids,
            })
        }
        pub(crate) fn child_fd(&self) -> i32 {
            self.write.as_ref().map(AsRawFd::as_raw_fd).unwrap_or(-1)
        }
        /// True once every process holding the write end has exited.
        fn released(&self) -> bool {
            let mut buffer = [0_u8; 64];
            loop {
                // SAFETY: valid buffer; nonblocking descriptor.
                let n = unsafe {
                    libc::read(
                        self.read.as_raw_fd(),
                        buffer.as_mut_ptr().cast(),
                        buffer.len(),
                    )
                };
                if n == 0 {
                    return true;
                }
                if n > 0 {
                    continue;
                }
                if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    return false;
                }
            }
        }
        fn ids(&self) -> Vec<u64> {
            self.ids.clone()
        }
    }

    /// Kernel identity recognising write-end holders in other processes: the
    /// pipe inode on Linux (/proc/PID/fd -> "pipe:[inode]"); on macOS the write
    /// endpoint's handle, i.e. the read end's peer while the write end is open.
    fn identities(read: &OwnedFd) -> Vec<u64> {
        #[cfg(target_os = "linux")]
        {
            // SAFETY: zeroed stat is a valid out-parameter.
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::fstat(read.as_raw_fd(), &mut stat) } == 0 {
                return vec![stat.st_ino];
            }
            vec![]
        }
        #[cfg(target_os = "macos")]
        {
            // SAFETY: no preconditions.
            let me = unsafe { libc::getpid() };
            pipe_handles(me, read.as_raw_fd())
                .map(|(_, peer)| vec![peer])
                .unwrap_or_default()
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = read;
            vec![]
        }
    }

    pub(crate) struct ProcessTree {
        pgid: i32,
        marker: String,
        sentinel: Sentinel,
    }

    impl ProcessTree {
        pub(crate) fn unix(pid: u32, marker: String, mut sentinel: Sentinel) -> Self {
            // Only the job's processes may hold the write end from now on.
            sentinel.write = None;
            Self {
                pgid: pid as i32,
                marker,
                sentinel,
            }
        }
        pub(crate) fn terminate(&mut self) -> io::Result<()> {
            // SAFETY: negative-PID semantics via killpg address only our group.
            if unsafe { libc::killpg(self.pgid, libc::SIGKILL) } == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        pub(crate) fn reap(&mut self) -> TreeOutcome {
            // SAFETY: see terminate; ESRCH (group already gone) is expected.
            unsafe { libc::killpg(self.pgid, libc::SIGKILL) };
            let mut killed = BTreeSet::new();
            let ids = self.sentinel.ids();
            for _ in 0..50 {
                let before = killed.len();
                for pid in escaped(self.pgid, &self.marker, &ids) {
                    // SAFETY: pid carries this launch's unique marker/sentinel.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                    killed.insert(pid);
                }
                if killed.len() == before && self.sentinel.released() {
                    return if killed.is_empty() {
                        TreeOutcome::Clean
                    } else {
                        TreeOutcome::EscapedTerminated(killed.len())
                    };
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            TreeOutcome::Unproven(format!(
                "a descendant still holds the job sentinel after termination{} and could not be identified",
                if killed.is_empty() {
                    String::new()
                } else {
                    format!(" ({} escaped descendant(s) were killed)", killed.len())
                }
            ))
        }
    }

    fn marker_in(environment: &[u8], marker: &str) -> bool {
        let needle = format!("{MARKER_ENV}={marker}");
        environment
            .split(|b| *b == 0)
            .any(|entry| entry == needle.as_bytes())
    }

    /// Same-user processes outside the job's own group that carry the job's
    /// marker or (Linux) hold its sentinel pipe. Zombies are ignored.
    #[cfg(target_os = "linux")]
    fn escaped(pgid: i32, marker: &str, sentinel: &[u64]) -> Vec<i32> {
        use std::io::Read;
        use std::os::unix::fs::MetadataExt;
        // SAFETY: no preconditions.
        let (me, uid) = unsafe { (libc::getpid(), libc::geteuid()) };
        let pipes: Vec<String> = sentinel
            .iter()
            .map(|inode| format!("pipe:[{inode}]"))
            .collect();
        let mut found = vec![];
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return found;
        };
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<i32>().ok())
            else {
                continue;
            };
            if pid == me || entry.metadata().map(|m| m.uid()).ok() != Some(uid) {
                continue;
            }
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            let fields: Vec<&str> = stat
                .rsplit_once(')')
                .map(|(_, rest)| rest.split_whitespace().collect())
                .unwrap_or_default();
            if fields.first() == Some(&"Z")
                || fields.get(2).and_then(|g| g.parse().ok()) == Some(pgid)
            {
                continue;
            }
            let mut environment = vec![];
            let carries_marker = std::fs::File::open(entry.path().join("environ"))
                .and_then(|f| f.take(1 << 20).read_to_end(&mut environment))
                .is_ok()
                && marker_in(&environment, marker);
            let holds_sentinel = || {
                !pipes.is_empty()
                    && std::fs::read_dir(entry.path().join("fd"))
                        .map(|fds| {
                            fds.flatten().any(|fd| {
                                std::fs::read_link(fd.path()).is_ok_and(|target| {
                                    pipes.iter().any(|p| target.as_os_str() == p.as_str())
                                })
                            })
                        })
                        .unwrap_or(false)
            };
            if carries_marker || holds_sentinel() {
                found.push(pid);
            }
        }
        found
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn count_user_processes() -> Option<usize> {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: no preconditions.
        let uid = unsafe { libc::geteuid() };
        Some(
            std::fs::read_dir("/proc")
                .ok()?
                .flatten()
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|s| s.parse::<u32>().is_ok())
                })
                .filter(|e| e.metadata().map(|m| m.uid()).ok() == Some(uid))
                .count(),
        )
    }

    /// (pid, process group, zombie) for every process of the effective user.
    #[cfg(target_os = "macos")]
    fn user_processes() -> Option<Vec<(i32, i32, bool)>> {
        const SZOMB: u32 = 5;
        // SAFETY: no preconditions.
        let uid = unsafe { libc::geteuid() };
        // SAFETY: a null buffer asks libproc for the current process count.
        let estimate = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        let mut pids = vec![0 as libc::pid_t; estimate.max(4096) as usize + 256];
        let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        // SAFETY: buffer holds `bytes` bytes.
        let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        if count <= 0 {
            return None;
        }
        pids.truncate(count as usize);
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let mut processes = vec![];
        for pid in pids.into_iter().filter(|pid| *pid > 0) {
            // SAFETY: plain-old-data out-parameter of the stated size.
            let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
            let written = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDTBSDINFO,
                    0,
                    (&mut info as *mut libc::proc_bsdinfo).cast(),
                    size,
                )
            };
            if written == size && info.pbi_uid == uid {
                processes.push((pid, info.pbi_pgid as i32, info.pbi_status == SZOMB));
            }
        }
        Some(processes)
    }

    /// The environment block of another same-user process (KERN_PROCARGS2).
    #[cfg(target_os = "macos")]
    fn environment_of(pid: i32) -> Option<Vec<u8>> {
        let mut argmax: libc::c_int = 0;
        let mut size = std::mem::size_of::<libc::c_int>();
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        // SAFETY: valid out-pointer of the stated size.
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                (&mut argmax as *mut libc::c_int).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return None;
        }
        let mut buffer = vec![0_u8; usize::try_from(argmax).ok()?];
        let mut size = buffer.len();
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        // SAFETY: buffer holds `size` bytes.
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buffer.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return None;
        }
        buffer.truncate(size);
        let argc = i32::from_ne_bytes(buffer.get(..4)?.try_into().ok()?);
        let mut rest = &buffer[4..];
        // Executable path, then NUL padding, then argc arguments.
        let skip = |rest: &mut &[u8]| {
            let end = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
            *rest = &rest[end..];
        };
        skip(&mut rest);
        while rest.first() == Some(&0) {
            rest = &rest[1..];
        }
        for _ in 0..argc.max(0) {
            skip(&mut rest);
            rest = rest.get(1..).unwrap_or(&[]);
        }
        Some(rest.to_vec())
    }

    /// `<sys/proc_info.h>` layouts (stable ABI); a size mismatch simply yields
    /// no match, which degrades to "unproven", never to a false "clean".
    #[cfg(target_os = "macos")]
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct PipeFdinfo {
        fi_openflags: u32,
        fi_status: u32,
        fi_offset: i64,
        fi_type: i32,
        fi_guardflags: u32,
        pipe_stat: [u64; 17],
        pipe_handle: u64,
        pipe_peerhandle: u64,
        pipe_status: i32,
        rfu_1: i32,
    }

    #[cfg(target_os = "macos")]
    const PROC_PIDFDPIPEINFO: libc::c_int = 6;
    #[cfg(target_os = "macos")]
    const PROX_FDTYPE_PIPE: u32 = 6;

    /// (endpoint handle, peer handle) of a pipe descriptor in another process.
    #[cfg(target_os = "macos")]
    fn pipe_handles(pid: i32, fd: i32) -> Option<(u64, u64)> {
        // SAFETY: plain-old-data out-parameter of the stated size.
        let mut info: PipeFdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<PipeFdinfo>() as libc::c_int;
        let written = unsafe {
            libc::proc_pidfdinfo(
                pid,
                fd,
                PROC_PIDFDPIPEINFO,
                (&mut info as *mut PipeFdinfo).cast(),
                size,
            )
        };
        (written == size).then_some((info.pipe_handle, info.pipe_peerhandle))
    }

    /// True when `pid` holds the job sentinel's write endpoint: one of its pipe
    /// descriptors IS that endpoint (its own handle equals the identity). Peer
    /// handles are never compared, and 0 never matches.
    #[cfg(target_os = "macos")]
    fn holds_pipe(pid: i32, ids: &[u64]) -> bool {
        if ids.is_empty() || ids.contains(&0) {
            return false;
        }
        // SAFETY: size query with a null buffer.
        let bytes =
            unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        if bytes <= 0 {
            return false;
        }
        let item = std::mem::size_of::<libc::proc_fdinfo>();
        // SAFETY: plain-old-data element type.
        let mut fds: Vec<libc::proc_fdinfo> =
            vec![unsafe { std::mem::zeroed() }; bytes as usize / item + 16];
        let capacity = (fds.len() * item) as libc::c_int;
        // SAFETY: buffer holds `capacity` bytes.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr().cast(),
                capacity,
            )
        };
        if written <= 0 {
            return false;
        }
        fds.truncate(written as usize / item);
        fds.iter()
            .filter(|fd| fd.proc_fdtype == PROX_FDTYPE_PIPE)
            .any(|fd| {
                pipe_handles(pid, fd.proc_fd).is_some_and(|(handle, _)| ids.contains(&handle))
            })
    }

    /// Recent macOS hides other processes' environments, so the inherited
    /// sentinel descriptor is the primary identity; the marker is used when the
    /// environment is visible.
    #[cfg(target_os = "macos")]
    fn escaped(pgid: i32, marker: &str, sentinel: &[u64]) -> Vec<i32> {
        // SAFETY: no preconditions.
        let me = unsafe { libc::getpid() };
        user_processes()
            .unwrap_or_default()
            .into_iter()
            .filter(|(pid, group, zombie)| *pid != me && *group != pgid && !zombie)
            .filter(|(pid, _, _)| {
                holds_pipe(*pid, sentinel)
                    || environment_of(*pid).is_some_and(|env| marker_in(&env, marker))
            })
            .map(|(pid, _, _)| pid)
            .collect()
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn count_user_processes() -> Option<usize> {
        user_processes().map(|p| p.len())
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn escaped(_: i32, _: &str, _: &[u64]) -> Vec<i32> {
        vec![]
    }

    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    mod tests {
        use super::*;
        use std::os::unix::process::CommandExt;

        fn find(pid: i32, pgid: i32, marker: &str, ids: &[u64]) -> bool {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                if escaped(pgid, marker, ids).contains(&pid) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            false
        }

        #[test]
        fn sentinel_holders_outside_the_job_group_are_identified() {
            let sentinel = Sentinel::new().unwrap();
            let fd = sentinel.child_fd();
            let ids = sentinel.ids();
            assert!(!ids.is_empty(), "sentinel identity unavailable");
            let mut command = std::process::Command::new("/bin/sleep");
            command.arg("30").process_group(0);
            // SAFETY: async-signal-safe fcntl only.
            unsafe {
                command.pre_exec(move || {
                    if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let mut child = command.spawn().unwrap();
            let pid = child.id() as i32;
            let found = find(pid, -1, &new_marker(), &ids);
            // A process in the job's own group is never reported as escaped.
            let own_group = escaped(pid, &new_marker(), &ids).contains(&pid);
            let unrelated = escaped(-1, &new_marker(), &[]).contains(&pid);
            let _ = child.kill();
            let _ = child.wait();
            assert!(found, "sentinel holder was not identified");
            assert!(
                !ids.contains(&0),
                "a zero identity would match unrelated pipes"
            );
            // With the only holder gone, NOTHING else on the system may match:
            // a false positive here would mean killing unrelated processes.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            let mut strays = escaped(-1, &new_marker(), &ids);
            while !strays.is_empty() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
                strays = escaped(-1, &new_marker(), &ids);
            }
            assert!(
                strays.is_empty(),
                "false-positive sentinel matches: {strays:?}"
            );
            assert!(!own_group && !unrelated);
            assert!(count_user_processes().unwrap() > 0);
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn marker_bearing_processes_are_identified_on_linux() {
            let marker = new_marker();
            let mut child = std::process::Command::new("/bin/sleep")
                .arg("30")
                .env(MARKER_ENV, &marker)
                .process_group(0)
                .spawn()
                .unwrap();
            let pid = child.id() as i32;
            let found = find(pid, -1, &marker, &[]);
            let _ = child.kill();
            let _ = child.wait();
            assert!(found, "marker-bearing process was not identified");
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub(crate) fn count_user_processes() -> Option<usize> {
        None
    }
}

#[cfg(windows)]
pub(crate) struct ProcessTree {
    job: super::windows::Job,
}

#[cfg(windows)]
impl ProcessTree {
    pub(crate) fn windows(job: super::windows::Job) -> Self {
        Self { job }
    }
    pub(crate) fn terminate(&mut self) -> io::Result<()> {
        self.job.terminate()
    }
    pub(crate) fn reap(&mut self) -> TreeOutcome {
        let _ = self.job.terminate();
        for _ in 0..50 {
            if self.job.active() == Some(0) {
                return TreeOutcome::Clean;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        TreeOutcome::Unproven("Job Object still reports active processes after termination".into())
    }
}
