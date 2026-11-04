pub mod stalled_source;

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
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
const QUIET: Duration = Duration::from_millis(250);
static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

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

    fn add_option(self, command: &mut Command) {
        if matches!(self, Self::Primary) {
            command.arg("--primary");
        }
    }
}

pub struct Process(Child);

impl Process {
    fn spawn(command: &mut Command) -> Self {
        Self(
            command
                .spawn()
                .unwrap_or_else(|err| panic!("could not start {command:?}: {err}")),
        )
    }

    pub fn stop(&mut self) {
        let _ = self.0.kill();
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

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
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
    path: PathBuf,
    wire: Vec<u8>,
    message: Vec<u8>,
}

impl Host {
    fn new() -> Self {
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

pub struct Fixture {
    pub host: Host,
    pub bridge: Process,
    pub compositor: Process,
    clients: Vec<Process>,
    socket: PathBuf,
    // Declared last so processes and file descriptors are dropped first.
    directory: TestDir,
}

impl Fixture {
    pub fn new() -> Self {
        let directory = TestDir::new();
        let runtime = directory.0.join("runtime");
        fs::create_dir(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        let config = directory.0.join("sway.conf");
        fs::write(
            &config,
            "xwayland disable\nswaybg_command -\nswaynag_command -\nseat seat0 fallback true\n",
        )
        .unwrap();
        let sway = std::env::var_os("PAPRIKA_TEST_SWAY").unwrap_or_else(|| "sway".into());
        let log = File::create(directory.0.join("compositor.log")).unwrap();
        let mut command = isolated_command(&directory.0, sway);
        command
            .args(["--config"])
            .arg(config)
            .env("WLR_BACKENDS", "headless")
            .env("WLR_RENDERER", "pixman")
            .env("WLR_LIBINPUT_NO_DEVICES", "1")
            .stdout(log.try_clone().unwrap())
            .stderr(log);
        let mut compositor = Process::spawn(&mut command);
        let deadline = Instant::now() + WAIT;
        let socket = loop {
            if let Some(status) = compositor.0.try_wait().unwrap() {
                panic!(
                    "headless Sway exited ({status}): {}",
                    fs::read_to_string(directory.0.join("compositor.log")).unwrap()
                );
            }
            if let Some(socket) =
                fs::read_dir(&runtime)
                    .unwrap()
                    .filter_map(Result::ok)
                    .find(|entry| {
                        entry.file_type().unwrap().is_socket()
                            && entry.file_name().to_string_lossy().starts_with("wayland-")
                    })
            {
                break socket.path();
            }
            assert!(
                Instant::now() < deadline,
                "headless Sway did not create its socket: {}",
                fs::read_to_string(directory.0.join("compositor.log")).unwrap()
            );
            thread::sleep(Duration::from_millis(10));
        };
        let host = Host::new();
        let bridge = Self::spawn_bridge(&directory.0, &socket, &host.path);
        let mut fixture = Self {
            host,
            bridge,
            compositor,
            clients: Vec::new(),
            socket,
            directory,
        };
        fixture.wait_for_log("starting bridge loop");
        let payload = fixture.expect(CAPABILITIES);
        assert_eq!(
            word(&payload[..4]),
            1,
            "agent should request peer capabilities"
        );
        fixture
    }

    fn spawn_bridge(directory: &Path, socket: &Path, port: &Path) -> Process {
        let log = File::create(directory.join("bridge.log")).unwrap();
        Process::spawn(
            isolated_command(directory, env!("CARGO_BIN_EXE_paprika-vdagent"))
                .env("WAYLAND_DISPLAY", socket)
                .env("RUST_LOG", "paprika_vdagent=debug")
                .arg("--virtio-port")
                .arg(port)
                .args(["--poll-ms", "20", "--clipboard-timeout-ms", "100"])
                .arg("--file-dir")
                .arg(directory.join("Downloads"))
                .stdout(log.try_clone().unwrap())
                .stderr(log),
        )
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = isolated_command(&self.directory.0, program);
        command.env("WAYLAND_DISPLAY", &self.socket);
        command
    }

    pub fn expect(&mut self, kind: u32) -> Vec<u8> {
        let message = self
            .host
            .receive(WAIT)
            .unwrap_or_else(|| panic!("timed out waiting for SPICE message {kind}"));
        assert_eq!(message.0, kind, "unexpected SPICE message: {message:?}");
        message.1
    }

    pub fn quiet(&mut self) {
        assert_eq!(
            self.host.receive(QUIET),
            None,
            "unexpected clipboard echo or stale SPICE traffic"
        );
    }

    pub fn negotiate(&mut self) {
        // Clipboard by demand and selection prefixes. Serial negotiation is
        // covered separately by the transport tests.
        let mut capabilities = 0u32.to_le_bytes().to_vec();
        capabilities.extend(((1u32 << 5) | (1u32 << 6)).to_le_bytes());
        self.host.send(CAPABILITIES, &capabilities);
    }

    pub fn expect_grab(&mut self, selection: Selection) {
        let mut expected = selection.prefix().to_vec();
        expected.extend(1u32.to_le_bytes());
        assert_eq!(self.expect(GRAB), expected);
    }

    pub fn request_text(&mut self, selection: Selection) -> Vec<u8> {
        let mut request = selection.prefix().to_vec();
        request.extend(1u32.to_le_bytes());
        self.host.send(REQUEST, &request);
        let data = self.expect(CLIPBOARD);
        assert_eq!(&data[..8], request);
        data[8..].to_vec()
    }

    pub fn host_copy(&mut self, selection: Selection, text: &[u8]) {
        let mut types = selection.prefix().to_vec();
        types.extend(1u32.to_le_bytes());
        self.host.send(GRAB, &types);
        assert_eq!(self.expect(REQUEST), types);
        types.extend(text);
        self.host.send(CLIPBOARD, &types);
        self.wait_for_clipboard(selection, Some(text));
    }

    pub fn guest_copy(&mut self, selection: Selection, text: &[u8]) {
        let log = File::create(
            self.directory
                .0
                .join(format!("copy-{}.log", self.clients.len())),
        )
        .unwrap();
        let mut command = self.command("wl-copy");
        command.args(["--foreground", "--type", "text/plain;charset=utf-8"]);
        selection.add_option(&mut command);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(log);
        let mut client = Process::spawn(&mut command);
        client.0.stdin.take().unwrap().write_all(text).unwrap();
        self.clients.push(client);
        self.wait_for_clipboard(selection, Some(text));
    }

    pub fn stall_first_clipboard_read(&mut self) {
        let ready = self.directory.0.join("source-ready");
        let log = File::create(self.directory.0.join("stalled-source.log")).unwrap();
        let mut command = self.command(std::env::current_exe().unwrap());
        command
            .args(["--exact", "clipboard_source_helper", "--nocapture"])
            .env("PAPRIKA_TEST_SOURCE", "stall")
            .env("PAPRIKA_TEST_SOURCE_READY", &ready)
            .stdout(log.try_clone().unwrap())
            .stderr(log);
        self.clients.push(Process::spawn(&mut command));
        let deadline = Instant::now() + WAIT;
        while !ready.exists() {
            assert!(
                Instant::now() < deadline,
                "stalled clipboard source did not become ready"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn guest_clear(&self, selection: Selection) {
        let mut command = self.command("wl-copy");
        command.arg("--clear");
        selection.add_option(&mut command);
        assert!(self.capture(&mut command).0.success());
        self.wait_for_clipboard(selection, None);
    }

    fn capture(&self, command: &mut Command) -> (ExitStatus, Vec<u8>) {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path = self.directory.0.join(format!("command-{id}.out"));
        let output = File::create(&path).unwrap();
        // File capture avoids deadlocking on a full stdout pipe.
        command
            .stdin(Stdio::null())
            .stdout(output)
            .stderr(Stdio::null());
        let status = Process::spawn(command).wait();
        let bytes = fs::read(&path).unwrap();
        fs::remove_file(path).unwrap();
        (status, bytes)
    }

    pub fn wait_for_clipboard(&self, selection: Selection, expected: Option<&[u8]>) {
        let deadline = Instant::now() + WAIT;
        loop {
            let mut command = self.command("wl-paste");
            command.args(["--no-newline", "--type", "text/plain;charset=utf-8"]);
            selection.add_option(&mut command);
            let (status, bytes) = self.capture(&mut command);
            if (status.success() && expected == Some(bytes.as_slice()))
                || (!status.success() && expected.is_none())
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Wayland {selection:?} did not become {expected:?}; got {bytes:?} ({status})"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn downloads(&self) -> PathBuf {
        self.directory.0.join("Downloads")
    }

    pub fn start_transfer(&mut self, id: u32, name: &str, size: usize) {
        let mut metadata = id.to_le_bytes().to_vec();
        metadata.extend(format!("[vdagent-file-xfer]\nname={name}\nsize={size}\n\0").as_bytes());
        self.host.send(FILE_START, &metadata);
        self.expect_status(id, 0);
    }

    pub fn transfer_data(&mut self, id: u32, bytes: &[u8]) {
        let mut payload = id.to_le_bytes().to_vec();
        payload.extend((bytes.len() as u64).to_le_bytes());
        payload.extend(bytes);
        self.host.send(FILE_DATA, &payload);
    }

    pub fn expect_status(&mut self, id: u32, status: u32) {
        let mut expected = id.to_le_bytes().to_vec();
        expected.extend(status.to_le_bytes());
        assert_eq!(self.expect(FILE_STATUS), expected);
    }

    pub fn wait_for_no_parts(&self) {
        let deadline = Instant::now() + WAIT;
        loop {
            let partial = fs::read_dir(self.downloads()).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|ext| ext == "part")
            });
            if !partial {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "partial downloads were not cleaned up"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn wait_for_log(&mut self, message: &str) {
        let deadline = Instant::now() + WAIT;
        loop {
            let logs = fs::read_to_string(self.directory.0.join("bridge.log")).unwrap();
            if logs.contains(message) {
                return;
            }
            assert!(
                self.bridge.0.try_wait().unwrap().is_none(),
                "bridge exited before {message}: {logs}"
            );
            assert!(
                Instant::now() < deadline,
                "bridge did not log {message}: {logs}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn restart_bridge(&mut self) {
        assert!(
            self.bridge.0.try_wait().unwrap().is_some(),
            "previous bridge must have exited"
        );
        self.host = Host::new();
        self.bridge = Self::spawn_bridge(&self.directory.0, &self.socket, &self.host.path);
        self.wait_for_log("starting bridge loop");
        self.expect(CAPABILITIES);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for client in &mut self.clients {
            client.stop();
        }
        self.bridge.stop();
        self.compositor.stop();
        if thread::panicking() {
            for entry in fs::read_dir(&self.directory.0)
                .unwrap()
                .filter_map(Result::ok)
            {
                if entry.path().extension().is_some_and(|ext| ext == "log") {
                    eprintln!(
                        "{}:\n{}",
                        entry.file_name().to_string_lossy(),
                        fs::read_to_string(entry.path()).unwrap_or_default()
                    );
                }
            }
        }
    }
}

fn isolated_command(directory: &Path, program: impl AsRef<std::ffi::OsStr>) -> Command {
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
        .stdin(Stdio::null());
    command
}
