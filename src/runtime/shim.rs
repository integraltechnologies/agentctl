//! Provider stdio and environment, across a lifecycle domain.
//!
//! procd starts a domain's first process from an argument vector alone: it
//! inherits agentctl's own standard streams, working directory and
//! environment, none of which can be set per launch. A provider needs its
//! own stdin, stdout and stderr, working directory and scrubbed environment.
//! So procd starts this shim (the `agentctl-shim` binary) as the domain's
//! first process, and the shim starts the provider itself, inside the
//! domain, as agentctl's `Command` would have.
//!
//! The two meet over loopback: agentctl listens, and the shim connects four
//! times, once each for the provider's stdin, stdout and stderr and for
//! control messages, proving itself with a token from a private directory
//! whose path is the shim's only argument (an argument is visible to other
//! users; the directory's contents are not). The shim only relays: what the
//! provider said, and how it ended, are classified by agentctl alone.
//!
//! Control messages, one per line, from the shim:
//!
//! - `started`, or `failed <why>` if the provider could not be started;
//! - `input ok`, or `input failed <why>`: whether all input reached the
//!   provider before its stdin was closed;
//! - `exit <raw status> <held>`: how the provider ended, and whether an
//!   output stream stayed open after, held by a process it started.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use tempfile::TempDir;

use super::{POLL, Passthrough, Provider, claude, codex};

const SPEC: &str = "spec";
/// How long the shim's connections may take to arrive.
const CONNECT_WAIT: Duration = Duration::from_secs(20);
/// How long output and input may keep flowing once the provider exited.
const DRAIN: Duration = Duration::from_secs(2);
const TOKEN_BYTES: usize = 16;

/// The channels a shim opens, each named by its first byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Channel {
    Input,
    Output,
    Error,
    Control,
}

impl Channel {
    fn byte(self) -> u8 {
        match self {
            Self::Input => b'i',
            Self::Output => b'o',
            Self::Error => b'e',
            Self::Control => b'c',
        }
    }

    fn from_byte(byte: u8) -> Option<Self> {
        [Self::Input, Self::Output, Self::Error, Self::Control]
            .into_iter()
            .find(|c| c.byte() == byte)
    }
}

/// What the shim is to start.
pub(super) struct Spec {
    pub provider: Provider,
    pub exe: PathBuf,
    pub cwd: PathBuf,
    pub args: Vec<OsString>,
}

/// One launch's meeting place: a private directory holding the spec and a
/// token, and the listener the shim connects to.
pub(super) struct Rendezvous {
    dir: TempDir,
    listener: TcpListener,
    token: String,
}

/// The four connections of a shim.
pub(super) struct Streams {
    pub input: TcpStream,
    pub output: TcpStream,
    pub error: TcpStream,
    pub control: BufReader<TcpStream>,
}

/// What the shim first reported of starting the provider.
pub(super) enum Started {
    Yes,
    Failed(String),
}

/// A message from the shim on the control channel, after the first.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Message {
    Input(Result<(), String>),
    Exit { raw: i64, held: bool },
}

impl Rendezvous {
    pub(super) fn create(spec: &Spec) -> Result<Self> {
        let mut private = tempfile::Builder::new();
        private.prefix("agentctl-shim-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            private.permissions(fs::Permissions::from_mode(0o700));
        }
        let dir = private
            .tempdir()
            .context("creating the shim's private directory")?;
        let listener =
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).context("listening on loopback")?;
        let port = listener.local_addr()?.port();
        let token = random_token()?;
        let mut file = format!(
            "port {port}\ntoken {token}\nprovider {}\n",
            spec.provider.name()
        );
        file += &format!(
            "exe {}\ncwd {}\n",
            encode(spec.exe.as_os_str()),
            encode(spec.cwd.as_os_str())
        );
        for arg in &spec.args {
            file += &format!("arg {}\n", encode(arg));
        }
        fs::write(dir.path().join(SPEC), file).context("writing the shim's spec")?;
        Ok(Self {
            dir,
            listener,
            token,
        })
    }

    /// The private directory: the shim's one argument.
    pub(super) fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// Accepts the shim's four connections and its first report, within
    /// `CONNECT_WAIT`, and returns the private directory with them: it must
    /// outlive the shim's reading of it, which it has done by then.
    pub(super) fn accept(self) -> Result<(Streams, Started, TempDir)> {
        let deadline = Instant::now() + CONNECT_WAIT;
        self.listener.set_nonblocking(true)?;
        let mut found: [Option<TcpStream>; 4] = [None, None, None, None];
        let slot = |c: Channel| match c {
            Channel::Input => 0,
            Channel::Output => 1,
            Channel::Error => 2,
            Channel::Control => 3,
        };
        while found.iter().any(Option::is_none) {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    // Anything that cannot prove itself is dropped.
                    if let Some(channel) = self.hello(&stream) {
                        found[slot(channel)].get_or_insert(stream);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        bail!("the provider shim did not connect");
                    }
                    thread::sleep(POLL);
                }
                Err(e) => return Err(e).context("accepting the provider shim"),
            }
        }
        let [Some(input), Some(output), Some(error), Some(control)] = found else {
            unreachable!("every channel was found");
        };
        control.set_read_timeout(Some(
            deadline.saturating_duration_since(Instant::now()).max(POLL),
        ))?;
        let mut control = BufReader::new(control);
        let mut line = String::new();
        control
            .read_line(&mut line)
            .map_err(|e| anyhow!("the provider shim did not report starting: {e}"))?;
        control.get_ref().set_read_timeout(None)?;
        let started = match line.trim_end() {
            "started" => Started::Yes,
            other => match other.strip_prefix("failed ") {
                Some(why) => Started::Failed(why.to_owned()),
                None => bail!("the provider shim's first report is not understood"),
            },
        };
        Ok((
            Streams {
                input,
                output,
                error,
                control,
            },
            started,
            self.dir,
        ))
    }

    /// Which channel `stream` opened, if it presented this launch's token.
    fn hello(&self, stream: &TcpStream) -> Option<Channel> {
        stream.set_nonblocking(false).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
        let mut hello = vec![0; self.token.len() + 1];
        (&*stream).read_exact(&mut hello).ok()?;
        stream.set_read_timeout(None).ok()?;
        let (token, channel) = hello.split_at(self.token.len());
        (token == self.token.as_bytes())
            .then(|| Channel::from_byte(channel[0]))
            .flatten()
    }
}

impl Message {
    /// Understands one control line; `None` for anything else, which is
    /// never acted on.
    pub(super) fn parse(line: &str) -> Option<Self> {
        let line = line.trim_end();
        if line == "input ok" {
            return Some(Self::Input(Ok(())));
        }
        if let Some(why) = line.strip_prefix("input failed ") {
            return Some(Self::Input(Err(why.to_owned())));
        }
        let rest = line.strip_prefix("exit ")?;
        let (raw, held) = rest.split_once(' ')?;
        let held = match held {
            "0" => false,
            "1" => true,
            _ => return None,
        };
        Some(Self::Exit {
            raw: raw.parse().ok()?,
            held,
        })
    }
}

/// The exit status a shim's `raw` names, if it is one on this host.
pub(super) fn status_from_raw(raw: i64) -> Option<ExitStatus> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        i32::try_from(raw).ok().map(ExitStatus::from_raw)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        u32::try_from(raw).ok().map(ExitStatus::from_raw)
    }
}

fn raw_status(status: ExitStatus) -> i64 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        i64::from(status.into_raw())
    }
    #[cfg(windows)]
    {
        i64::from(status.code().unwrap_or(1) as u32)
    }
}

/// `text` as hexadecimal, exactly, whatever it holds.
fn encode(text: &std::ffi::OsStr) -> String {
    let bytes: Vec<u8> = {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            text.as_bytes().to_vec()
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            text.encode_wide().flat_map(u16::to_le_bytes).collect()
        }
    };
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn decode(hex: &str) -> Result<OsString> {
    ensure_hex(hex)?;
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("checked hexadecimal"))
        .collect();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(OsString::from_vec(bytes))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        if !bytes.len().is_multiple_of(2) {
            bail!("odd length");
        }
        let wide: Vec<u16> = bytes
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        Ok(OsString::from_wide(&wide))
    }
}

fn ensure_hex(hex: &str) -> Result<()> {
    if !hex.len().is_multiple_of(2) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("not hexadecimal");
    }
    Ok(())
}

/// An unguessable token, from the operating system.
fn random_token() -> Result<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    #[cfg(unix)]
    {
        // SAFETY: `bytes` is writable for the length given.
        let status = unsafe { libc::getentropy(bytes.as_mut_ptr().cast(), bytes.len()) };
        if status != 0 {
            return Err(io::Error::last_os_error()).context("reading random bytes");
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Security::Cryptography::{
            BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
        };
        // SAFETY: `bytes` is writable for the length given.
        let status = unsafe {
            BCryptGenRandom(
                std::ptr::null_mut(),
                bytes.as_mut_ptr(),
                bytes.len() as u32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        if status != 0 {
            bail!("reading random bytes failed ({status})");
        }
    }
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Where the shim binary is: beside the running executable, or one directory
/// up from it (where `cargo` puts the binaries its test executables sit
/// under).
pub(super) fn locate() -> Result<PathBuf> {
    let name = format!("agentctl-shim{}", env::consts::EXE_SUFFIX);
    let exe = env::current_exe().context("finding the running executable")?;
    let dir = exe
        .parent()
        .context("the running executable has no directory")?;
    [Some(dir), dir.parent()]
        .into_iter()
        .flatten()
        .map(|d| d.join(&name))
        .find(|p| p.is_file())
        .with_context(|| {
            format!(
                "the provider shim `{name}` is not beside {}; build and install every agentctl binary",
                exe.display()
            )
        })
}

// ---- The shim itself ------------------------------------------------------

/// The entry point of the `agentctl-shim` binary.
pub fn main() -> ExitCode {
    let Some(dir) = env::args_os().nth(1) else {
        eprintln!("agentctl-shim: started by agentctl only");
        return ExitCode::from(2);
    };
    match run(Path::new(&dir)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("agentctl-shim: {e:#}");
            ExitCode::FAILURE
        }
    }
}

struct Loaded {
    port: u16,
    token: String,
    spec: Spec,
}

fn load(dir: &Path) -> Result<Loaded> {
    let text = fs::read_to_string(dir.join(SPEC)).context("reading the spec")?;
    let (mut port, mut token, mut provider, mut exe, mut cwd) = (None, None, None, None, None);
    let mut args = Vec::new();
    for line in text.lines() {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "port" => port = Some(value.parse().context("the port")?),
            "token" => token = Some(value.to_owned()),
            "provider" => provider = Some(value.parse::<Provider>()?),
            "exe" => exe = Some(decode(value)?),
            "cwd" => cwd = Some(decode(value)?),
            "arg" => args.push(decode(value)?),
            other => bail!("the spec holds an unknown entry `{other}`"),
        }
    }
    let missing = || anyhow!("the spec is incomplete");
    Ok(Loaded {
        port: port.ok_or_else(missing)?,
        token: token.ok_or_else(missing)?,
        spec: Spec {
            provider: provider.ok_or_else(missing)?,
            exe: exe.ok_or_else(missing)?.into(),
            cwd: cwd.ok_or_else(missing)?.into(),
            args,
        },
    })
}

fn open(loaded: &Loaded, channel: Channel) -> io::Result<TcpStream> {
    let mut stream = TcpStream::connect(SocketAddr::from((Ipv4Addr::LOCALHOST, loaded.port)))?;
    stream.write_all(loaded.token.as_bytes())?;
    stream.write_all(&[channel.byte()])?;
    Ok(stream)
}

/// Writes control lines from any thread.
#[derive(Clone)]
struct Control(Arc<Mutex<TcpStream>>);

impl Control {
    fn say(&self, line: &str) {
        let line: String = line
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(500)
            .collect();
        let mut stream = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(stream, "{line}");
    }
}

fn run(dir: &Path) -> Result<()> {
    let loaded = load(dir)?;
    let control = Control(Arc::new(Mutex::new(open(&loaded, Channel::Control)?)));
    let input = open(&loaded, Channel::Input)?;
    let output = open(&loaded, Channel::Output)?;
    let error = open(&loaded, Channel::Error)?;
    // Held apart from the relays, so the shim can end their channels itself.
    let ends = [output.try_clone()?, error.try_clone()?];

    let passthrough: &Passthrough = match loaded.spec.provider {
        Provider::Claude => &claude::ENV,
        Provider::Codex => &codex::ENV,
    };
    let spawned = Command::new(&loaded.spec.exe)
        .args(&loaded.spec.args)
        .current_dir(&loaded.spec.cwd)
        .env_clear()
        .envs(
            env::vars_os().filter(|(name, _)| name.to_str().is_some_and(|n| passthrough.passes(n))),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            control.say(&format!(
                "failed cannot start {} in {}: {e}",
                loaded.spec.exe.display(),
                loaded.spec.cwd.display()
            ));
            return Ok(());
        }
    };
    control.say("started");

    let mut stdin = child.stdin.take().context("stdin is piped")?;
    let stdout = child.stdout.take().context("stdout is piped")?;
    let stderr = child.stderr.take().context("stderr is piped")?;
    let reported = Arc::new(AtomicBool::new(false));
    let feed = {
        let (control, reported) = (control.clone(), Arc::clone(&reported));
        let mut input = input;
        thread::spawn(move || {
            // Closing the provider's stdin once its input is all in is what
            // says the input is complete.
            let sent = io::copy(&mut input, &mut stdin).and_then(|_| stdin.flush());
            drop(stdin);
            if !reported.swap(true, Ordering::SeqCst) {
                match sent {
                    Ok(()) => control.say("input ok"),
                    Err(e) => control.say(&format!("input failed {e}")),
                }
            }
        })
    };
    let relay = |mut from: Box<dyn Read + Send>, mut to: TcpStream| -> JoinHandle<()> {
        thread::spawn(move || {
            let _ = io::copy(&mut from, &mut to);
            let _ = to.shutdown(Shutdown::Write);
        })
    };
    let streams = [
        relay(Box::new(stdout), output),
        relay(Box::new(stderr), error),
    ];

    let status = child.wait().context("waiting for the provider")?;
    // What the provider left holding its streams, or its input undelivered,
    // is reported, never waited for.
    let deadline = Instant::now() + DRAIN;
    while Instant::now() < deadline
        && !(streams.iter().all(JoinHandle::is_finished) && feed.is_finished())
    {
        thread::sleep(POLL);
    }
    let held = !streams.iter().all(JoinHandle::is_finished);
    if held {
        finish_channels(&ends);
    }
    if !feed.is_finished() && !reported.swap(true, Ordering::SeqCst) {
        control.say("input failed the provider ended before all input was delivered");
    }
    control.say(&format!("exit {} {}", raw_status(status), u8::from(held)));
    Ok(())
}

/// Ends the output channels on purpose, when a process the provider started
/// still holds its output open after the provider exited.
///
/// Everything the relays had read is already delivered; the stream is over
/// as far as the provider goes. Left to itself, the shim's exit (or the
/// domain's termination) would tear the sockets down while the relays are
/// still blocked, which some hosts (Windows) report to the reader as a
/// connection reset instead of the end of the stream. Shutting down the
/// write side sends a clean end of stream, and a relay that later wakes can
/// no longer write through the channel. The reader still learns of the
/// condition from the `held` in the `exit` report, and the descendants are
/// still ended only by terminating the domain.
fn finish_channels(ends: &[TcpStream]) {
    for end in ends {
        let _ = end.shutdown(Shutdown::Write);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finishing_a_channel_is_a_clean_end_and_stops_later_writes() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let shim_side = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut reader, _) = listener.accept().unwrap();
        // What a relay owns, and what the shim kept.
        let mut relay = shim_side.try_clone().unwrap();
        relay.write_all(b"result\n").unwrap();
        finish_channels(&[shim_side]);
        // A relay woken later by the descendant cannot add to the stream.
        assert!(relay.write_all(b"late\n").is_err());
        let mut got = Vec::new();
        reader
            .read_to_end(&mut got)
            .expect("a clean end, not a reset");
        assert_eq!(got, b"result\n");
    }

    #[test]
    fn text_survives_the_spec_exactly() {
        for text in ["", "plain", "with space and é", "--json-schema={\"a\":1}"] {
            let os = OsString::from(text);
            assert_eq!(decode(&encode(&os)).unwrap(), os);
        }
        assert!(decode("abc").is_err() && decode("zz").is_err());
    }

    #[test]
    fn only_understood_reports_are_acted_on() {
        use Message::*;
        assert_eq!(Message::parse("input ok\n"), Some(Input(Ok(()))));
        assert_eq!(
            Message::parse("input failed pipe closed"),
            Some(Input(Err("pipe closed".into())))
        );
        assert_eq!(
            Message::parse("exit 256 1\n"),
            Some(Exit {
                raw: 256,
                held: true
            })
        );
        for other in [
            "", "exit", "exit 1", "exit x 0", "exit 1 2", "started", "EXIT 0 0",
        ] {
            assert_eq!(Message::parse(other), None, "{other}");
        }
    }

    #[test]
    fn only_the_launchs_token_opens_a_channel() {
        let spec = Spec {
            provider: Provider::Claude,
            exe: "provider".into(),
            cwd: ".".into(),
            args: vec!["a".into()],
        };
        let rendezvous = Rendezvous::create(&spec).unwrap();
        let port = rendezvous.listener.local_addr().unwrap().port();
        let dial = |token: &str, channel: u8| {
            let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            client.write_all(token.as_bytes()).unwrap();
            client.write_all(&[channel]).unwrap();
            rendezvous.listener.accept().unwrap().0
        };
        let wrong = "0".repeat(rendezvous.token.len());
        assert_eq!(rendezvous.hello(&dial(&wrong, b'c')), None);
        assert_eq!(rendezvous.hello(&dial(&rendezvous.token, b'x')), None);
        assert_eq!(
            rendezvous.hello(&dial(&rendezvous.token, b'o')),
            Some(Channel::Output)
        );
        // The spec is exactly what the shim will read, in a directory only
        // its owner can use.
        let loaded = load(rendezvous.dir()).unwrap();
        assert_eq!(
            (loaded.port, loaded.token),
            (port, rendezvous.token.clone())
        );
        assert_eq!(loaded.spec.args, [OsString::from("a")]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(rendezvous.dir()).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "{mode:o}");
        }
    }
}
