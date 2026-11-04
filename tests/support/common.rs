use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::pty::{OpenptFlags, grantpt, ioctl_tiocgptpeer, openpt, ptsname, unlockpt};
use rustix::termios::{OptionalActions, tcgetattr, tcsetattr};

pub const CLIPBOARD: u32 = 4;
pub const CAPABILITIES: u32 = 6;
pub const GRAB: u32 = 7;
pub const REQUEST: u32 = 8;
pub const RELEASE: u32 = 9;
pub const FILE_START: u32 = 10;
pub const FILE_STATUS: u32 = 11;
pub const FILE_DATA: u32 = 12;
pub const CLIENT_DISCONNECTED: u32 = 13;
pub const WAIT: Duration = Duration::from_secs(5);
pub const QUIET: Duration = Duration::from_millis(250);
static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

pub fn next_id() -> usize {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, Copy, Debug)]
pub enum Selection {
    Clipboard,
    Primary,
}

impl Selection {
    pub fn prefix(self) -> [u8; 4] {
        [
            match self {
                Self::Clipboard => 0,
                Self::Primary => 1,
            },
            0,
            0,
            0,
        ]
    }
}

pub struct Process(pub Child);

impl Process {
    pub fn spawn(command: &mut Command) -> Self {
        Self(
            command
                .spawn()
                .unwrap_or_else(|err| panic!("could not start {command:?}: {err}")),
        )
    }

    pub fn stop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            // Xvfb must remove its global display socket and lock on exit.
            let _ = rustix::process::kill_process(
                rustix::process::Pid::from_child(&self.0),
                rustix::process::Signal::TERM,
            );
            let deadline = Instant::now() + Duration::from_millis(500);
            while Instant::now() < deadline {
                if self.0.try_wait().ok().flatten().is_some() {
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }

    pub fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "process did not exit within {WAIT:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.stop();
    }
}

pub struct TestDir(pub PathBuf);

impl TestDir {
    pub fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "paprika-bridge-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Independent SPICE framing over a raw PTY. Keep partially read chunks across
/// receive deadlines, so a quiet-period assertion cannot discard wire data.
pub struct Host {
    master: Option<File>,
    _slave: File,
    pub path: PathBuf,
    wire: Vec<u8>,
    message: Vec<u8>,
}

impl Host {
    pub fn new() -> Self {
        let master =
            openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC).unwrap();
        grantpt(&master).unwrap();
        unlockpt(&master).unwrap();
        let path = PathBuf::from(ptsname(&master, Vec::new()).unwrap().to_str().unwrap());
        let slave = ioctl_tiocgptpeer(
            &master,
            OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC,
        )
        .unwrap();
        let mut settings = tcgetattr(&slave).unwrap();
        settings.make_raw();
        tcsetattr(&slave, OptionalActions::Now, &settings).unwrap();
        fcntl_setfl(&master, fcntl_getfl(&master).unwrap() | OFlags::NONBLOCK).unwrap();
        Self {
            master: Some(File::from(master)),
            _slave: File::from(slave),
            path,
            wire: Vec::new(),
            message: Vec::new(),
        }
    }

    pub fn close(&mut self) {
        self.master = None;
    }

    pub fn send(&mut self, kind: u32, payload: &[u8]) {
        let mut message = Vec::new();
        message.extend(1u32.to_le_bytes()); // VD_AGENT_PROTOCOL
        message.extend(kind.to_le_bytes());
        message.extend(0u64.to_le_bytes());
        message.extend((payload.len() as u32).to_le_bytes());
        message.extend(payload);
        let deadline = Instant::now() + WAIT;
        let master = self.master.as_mut().expect("SPICE connection is closed");
        for chunk in message.chunks(1024) {
            let mut frame = 1u32.to_le_bytes().to_vec(); // VDP_CLIENT_PORT
            frame.extend((chunk.len() as u32).to_le_bytes());
            frame.extend(chunk);
            let mut remaining = frame.as_slice();
            while !remaining.is_empty() {
                let timeout =
                    Timespec::try_from(deadline.saturating_duration_since(Instant::now())).unwrap();
                assert_ne!(
                    poll(&mut [PollFd::new(&*master, PollFlags::OUT)], Some(&timeout)).unwrap(),
                    0,
                    "SPICE write timed out"
                );
                match master.write(remaining) {
                    Ok(0) => panic!("SPICE connection closed while writing"),
                    Ok(count) => remaining = &remaining[count..],
                    Err(err)
                        if matches!(
                            err.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(err) => panic!("SPICE write failed: {err}"),
                }
            }
        }
    }

    pub fn receive(&mut self, timeout: Duration) -> Option<(u32, Vec<u8>)> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.wire.len() >= 8 {
                assert_eq!(word(&self.wire[..4]), 1, "unexpected SPICE chunk port");
                let count = word(&self.wire[4..8]) as usize;
                assert!((1..=1024).contains(&count), "invalid SPICE chunk length");
                if self.wire.len() >= 8 + count {
                    self.message.extend_from_slice(&self.wire[8..8 + count]);
                    self.wire.drain(..8 + count);
                    if self.message.len() >= 20 {
                        assert_eq!(word(&self.message[..4]), 1, "unexpected agent protocol");
                        let expected = 20 + word(&self.message[16..20]) as usize;
                        assert!(
                            expected <= 2 * 1024 * 1024,
                            "unexpectedly large agent message"
                        );
                        assert!(
                            self.message.len() <= expected,
                            "agent message exceeds its header size"
                        );
                        if self.message.len() == expected {
                            let kind = word(&self.message[4..8]);
                            let payload = self.message[20..].to_vec();
                            self.message.clear();
                            return Some((kind, payload));
                        }
                    }
                    continue;
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let timeout = Timespec::try_from(remaining).unwrap();
            let master = self.master.as_mut().expect("SPICE connection is closed");
            if poll(&mut [PollFd::new(&*master, PollFlags::IN)], Some(&timeout)).unwrap() == 0 {
                return None;
            }
            let mut buffer = [0u8; 8192];
            match master.read(&mut buffer) {
                Ok(0) => panic!("agent closed the SPICE connection"),
                Ok(count) => self.wire.extend_from_slice(&buffer[..count]),
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(err) => panic!("SPICE read failed: {err}"),
            }
        }
    }
}

pub fn word(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().unwrap())
}

pub fn isolated_command(directory: &Path, program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    // Never inherit a live display, compositor IPC socket, D-Bus address, user
    // configuration, or Paprika options from the invoking desktop session.
    command
        .env_clear()
        .env(
            "PATH",
            std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
        )
        .env("HOME", directory)
        .env("XDG_RUNTIME_DIR", directory.join("runtime"))
        .env("XDG_CONFIG_HOME", directory.join("config"))
        .env("XDG_DATA_HOME", directory.join("data"))
        .env("XDG_CACHE_HOME", directory.join("cache"))
        .stdin(std::process::Stdio::null());
    command
}
