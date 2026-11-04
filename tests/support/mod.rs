mod common;
pub mod stalled_source;
pub use common::*;

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

impl Selection {
    fn add_option(self, command: &mut Command) {
        if matches!(self, Self::Primary) {
            command.arg("--primary");
        }
    }
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
        let id = next_id();
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
