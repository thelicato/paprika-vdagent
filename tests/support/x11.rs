use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::{DefaultStream, RustConnection};
use x11rb::{COPY_FROM_PARENT, CURRENT_TIME};

use super::common::Host;
use super::common::*;

pub struct Fixture {
    pub host: Host,
    pub bridge: Process,
    pub server: Process,
    clients: Vec<Process>,
    display: String,
    cookie: [u8; 16],
    directory: TestDir,
}

impl Fixture {
    pub fn new() -> Self {
        Self::with_options(true, 1024 * 1024, 2000)
    }

    pub fn with_options(notifications: bool, limit: usize, timeout: u64) -> Self {
        let directory = TestDir::new();
        fs::create_dir(directory.0.join("runtime")).unwrap();
        let mut cookie = [0; 16];
        File::open("/dev/urandom")
            .unwrap()
            .read_exact(&mut cookie)
            .unwrap();
        write_auth(directory.0.join("authority"), "", &cookie);
        let log = File::create(directory.0.join("server.log")).unwrap();
        let number = directory.0.join("display-number");
        let executable = std::env::var_os("PAPRIKA_TEST_XVFB").unwrap_or_else(|| "Xvfb".into());
        let mut command = isolated_command(&directory.0, executable);
        command
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "800x600x24",
                "-nolisten",
                "tcp",
                "-auth",
            ])
            .arg(directory.0.join("authority"))
            .stdout(File::create(&number).unwrap())
            .stderr(log);
        let mut server = Process::spawn(&mut command);
        let deadline = Instant::now() + WAIT;
        let display = loop {
            if let Some(status) = server.0.try_wait().unwrap() {
                panic!(
                    "Xvfb exited ({status}): {}",
                    fs::read_to_string(directory.0.join("server.log")).unwrap()
                );
            }
            let number = fs::read_to_string(&number).unwrap();
            if number.trim().parse::<u32>().is_ok() {
                break format!(":{}", number.trim());
            }
            assert!(Instant::now() < deadline, "Xvfb did not become ready");
            thread::sleep(Duration::from_millis(10));
        };
        write_auth(directory.0.join("authority"), &display[1..], &cookie);
        let host = Host::new();
        let log = File::create(directory.0.join("bridge.log")).unwrap();
        let mut command = isolated_command(&directory.0, env!("CARGO_BIN_EXE_paprika-vdagent"));
        command
            .env("DISPLAY", &display)
            .env("XAUTHORITY", directory.0.join("authority"))
            .env("RUST_LOG", "paprika_vdagent=debug")
            .args(["--backend", "x11", "--poll-ms", "20", "--max-text-bytes"])
            .arg(limit.to_string())
            .arg("--clipboard-timeout-ms")
            .arg(timeout.to_string())
            .arg("--virtio-port")
            .arg(&host.path)
            .arg("--file-dir")
            .arg(directory.0.join("Downloads"))
            .stdout(log.try_clone().unwrap())
            .stderr(log);
        if !notifications {
            command.arg("--poll-only");
        }
        let bridge = Process::spawn(&mut command);
        let mut fixture = Self {
            host,
            bridge,
            server,
            clients: Vec::new(),
            display,
            cookie,
            directory,
        };
        fixture.wait_for_log("starting bridge loop");
        fixture.expect(CAPABILITIES);
        fixture
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = isolated_command(&self.directory.0, program);
        command
            .env("DISPLAY", &self.display)
            .env("XAUTHORITY", self.directory.0.join("authority"))
            .env("LC_ALL", "C.UTF-8");
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
        assert_eq!(self.host.receive(QUIET), None, "unexpected clipboard echo");
    }

    pub fn negotiate(&mut self) {
        let mut payload = 0u32.to_le_bytes().to_vec();
        payload.extend(((1u32 << 5) | (1u32 << 6)).to_le_bytes());
        self.host.send(CAPABILITIES, &payload);
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
        let mut payload = selection.prefix().to_vec();
        payload.extend(1u32.to_le_bytes());
        self.host.send(GRAB, &payload);
        assert_eq!(self.expect(REQUEST), payload);
        payload.extend(text);
        self.host.send(CLIPBOARD, &payload);
        self.wait_for_clipboard(selection, text);
    }

    pub fn guest_copy(&mut self, selection: Selection, text: &[u8]) {
        let log = File::create(
            self.directory
                .0
                .join(format!("source-{}.log", self.clients.len())),
        )
        .unwrap();
        let mut command = self.command("xsel");
        command
            .args(["--input", "--nodetach", option(selection)])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(log);
        let mut client = Process::spawn(&mut command);
        client.0.stdin.take().unwrap().write_all(text).unwrap();
        self.clients.push(client);
        self.wait_for_clipboard(selection, text);
    }

    pub fn guest_clear(&self, selection: Selection) {
        assert!(
            self.capture(self.command("xsel").args(["--clear", option(selection)]))
                .0
                .success()
        );
        self.wait_for_clipboard(selection, b"");
    }

    pub fn wait_for_clipboard(&self, selection: Selection, expected: &[u8]) {
        let deadline = Instant::now() + WAIT;
        loop {
            let (status, bytes) = self.capture(self.command("xsel").args([
                "--output",
                "--selectionTimeout",
                "1000",
                option(selection),
            ]));
            if status.success() && bytes == expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "X11 clipboard mismatch: wanted {} bytes, got {} ({status})",
                expected.len(),
                bytes.len()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn doctor(&self) -> String {
        let bin = self.directory.0.join("bin");
        fs::create_dir(&bin).unwrap();
        let stub = bin.join("systemctl");
        fs::write(&stub, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        let (_, output) = self.capture(
            self.command(env!("CARGO_BIN_EXE_paprika-vdagent"))
                .env("PATH", bin)
                .args(["--backend", "x11", "--virtio-port"])
                .arg(&self.host.path)
                .arg("doctor"),
        );
        String::from_utf8(output).unwrap()
    }

    fn capture(&self, command: &mut Command) -> (ExitStatus, Vec<u8>) {
        let path = self.directory.0.join(format!("command-{}.out", next_id()));
        command
            .stdin(Stdio::null())
            .stdout(File::create(&path).unwrap())
            .stderr(Stdio::null());
        let status = Process::spawn(command).wait();
        let output = fs::read(&path).unwrap();
        fs::remove_file(path).unwrap();
        (status, output)
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
                "missing bridge message {message}: {logs}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn downloads(&self) -> PathBuf {
        self.directory.0.join("Downloads")
    }

    pub fn start_transfer(&mut self, id: u32) {
        let mut payload = id.to_le_bytes().to_vec();
        payload.extend(b"[vdagent-file-xfer]\nname=received.txt\nsize=7\n\0");
        self.host.send(FILE_START, &payload);
        let status = self.expect(FILE_STATUS);
        assert_eq!(status, [id.to_le_bytes(), 0u32.to_le_bytes()].concat());
    }

    pub fn partial_transfer(&mut self, id: u32) {
        let mut payload = id.to_le_bytes().to_vec();
        payload.extend(3u64.to_le_bytes());
        payload.extend(b"par");
        self.host.send(FILE_DATA, &payload);
    }

    pub fn assert_no_partial_files(&self) {
        assert_eq!(fs::read_dir(self.downloads()).unwrap().count(), 0);
    }

    pub fn client(&self) -> (RustConnection, Window) {
        let stream =
            UnixStream::connect(format!("/tmp/.X11-unix/X{}", &self.display[1..])).unwrap();
        let (stream, _) = DefaultStream::from_unix_stream(stream).unwrap();
        let connection = RustConnection::connect_to_stream_with_auth_info(
            stream,
            0,
            b"MIT-MAGIC-COOKIE-1".to_vec(),
            self.cookie.to_vec(),
        )
        .unwrap();
        let window = connection.generate_id().unwrap();
        connection
            .create_window(
                COPY_FROM_PARENT as u8,
                window,
                connection.setup().roots[0].root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_OUTPUT,
                COPY_FROM_PARENT,
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )
            .unwrap()
            .check()
            .unwrap();
        (connection, window)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for client in &mut self.clients {
            client.stop();
        }
        self.bridge.stop();
        self.server.stop();
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

pub fn atom(connection: &RustConnection, name: &[u8]) -> Atom {
    connection
        .intern_atom(false, name)
        .unwrap()
        .reply()
        .unwrap()
        .atom
}

pub fn next_event(connection: &RustConnection) -> Event {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(event) = connection.poll_for_event().unwrap() {
            return event;
        }
        assert!(Instant::now() < deadline, "X11 test event timed out");
        thread::sleep(Duration::from_millis(5));
    }
}

pub fn selection_property(
    connection: &RustConnection,
    window: Window,
    selection: Atom,
    target: Atom,
    property: Atom,
) -> GetPropertyReply {
    connection
        .convert_selection(window, selection, target, property, CURRENT_TIME)
        .unwrap();
    connection.flush().unwrap();
    loop {
        if let Event::SelectionNotify(event) = next_event(connection) {
            assert_eq!(event.property, property);
            return connection
                .get_property(false, window, property, AtomEnum::ANY, 0, 4096)
                .unwrap()
                .reply()
                .unwrap();
        }
    }
}

fn option(selection: Selection) -> &'static str {
    match selection {
        Selection::Clipboard => "--clipboard",
        Selection::Primary => "--primary",
    }
}

fn write_auth(path: PathBuf, display: &str, cookie: &[u8; 16]) {
    let mut bytes = u16::MAX.to_be_bytes().to_vec(); // FamilyWild, scoped by display and random cookie.
    for field in [
        b"".as_slice(),
        display.as_bytes(),
        b"MIT-MAGIC-COOKIE-1",
        cookie.as_slice(),
    ] {
        bytes.extend((field.len() as u16).to_be_bytes());
        bytes.extend(field);
    }
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .unwrap()
        .write_all(&bytes)
        .unwrap();
}
