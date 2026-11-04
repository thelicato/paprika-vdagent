use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new(systemctl: &str) -> Self {
        let directory = std::env::temp_dir().join(format!(
            "paprika-doctor-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let executable = directory.join("systemctl");
        fs::write(
            &executable,
            format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$CALLS\"\n{systemctl}\n"),
        )
        .unwrap();
        fs::set_permissions(executable, fs::Permissions::from_mode(0o755)).unwrap();
        Self(directory)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_paprika-vdagent"));
        command
            .env_clear()
            .env("PATH", &self.0)
            .env("CALLS", self.0.join("calls"))
            .env("XDG_RUNTIME_DIR", &self.0)
            .env("WAYLAND_DISPLAY", "absent-compositor")
            .env("HOME", &self.0)
            .env("PAPRIKA_VDAGENT_PORT", self.0.join("absent-port"));
        command
    }

    fn run(&self, command: &mut Command) -> Output {
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(12);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("doctor did not finish within 12 seconds");
            }
            thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        // Doctor may inspect services, but must never change their state.
        for call in fs::read_to_string(self.0.join("calls")).unwrap().lines() {
            assert!(
                call.starts_with("--user show ") || call.starts_with("show "),
                "unexpected mutation: {call}"
            );
        }
        output
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn reports_independent_failures_and_missing_optional_service() {
    let fixture = Fixture::new("printf 'LoadState=not-found\\nActiveState=inactive\\n'");
    let output = fixture.run(fixture.command().arg("doctor"));
    assert!(!output.status.success());
    let report = String::from_utf8(output.stdout).unwrap();
    assert!(report.contains("[FAIL] Wayland connection failed"));
    assert!(report.contains("absent-port is missing"));
    assert!(report.contains("[WARN] User service is not installed"));
    assert!(report.contains("[WARN] Download directory"));
    assert!(!fixture.0.join("Downloads").exists());
    assert!(!fixture.0.join("absent-port").exists());
}

#[test]
fn honours_custom_paths_and_reports_active_service_and_socket() {
    let fixture = Fixture::new(
        "if [ \"$1\" = --user ]; then printf 'LoadState=loaded\\nActiveState=active\\n'; else printf 'LoadState=loaded\\nActiveState=active\\n'; fi",
    );
    let output = fixture.run(
        fixture
            .command()
            .args(["--virtio-port", "/dev/null", "--file-dir"])
            .arg(&fixture.0)
            .arg("doctor"),
    );
    let report = String::from_utf8(output.stdout).unwrap();
    assert!(report.contains("[OK] SPICE port /dev/null is readable and writable (not opened)"));
    assert!(report.contains("[OK] Download directory"));
    assert!(report.contains("[OK] User service installed, state=active"));
    assert!(report.contains("[FAIL] spice-vdagentd.socket is active"));
}

#[test]
fn rejects_regular_files_as_virtio_ports_without_changing_them() {
    let fixture = Fixture::new("exit 1");
    let port = fixture.0.join("regular-file");
    fs::write(&port, b"untouched").unwrap();
    let output = fixture.run(
        fixture
            .command()
            .arg("--virtio-port")
            .arg(&port)
            .arg("doctor"),
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("is not a character device"));
    assert_eq!(fs::read(&port).unwrap(), b"untouched");
}

#[test]
fn bounds_unresponsive_systemctl_checks() {
    let fixture = Fixture::new("if [ \"$1\" = --user ]; then exec /bin/sleep 30; fi");
    let started = Instant::now();
    let output = fixture.run(fixture.command().arg("doctor"));
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("systemctl did not respond within 3 seconds")
    );
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn bounds_unresponsive_wayland_registry_checks() {
    let fixture = Fixture::new("exit 1");
    let socket = fixture.0.join("stalled-compositor");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = thread::spawn(move || {
        let (mut client, _) = listener.accept().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut requests = Vec::new();
        // Accept requests without replying. The doctor must close the
        // connection when it exits, despite the stalled registry roundtrip.
        client.read_to_end(&mut requests).unwrap();
        assert!(!requests.is_empty());
    });
    let started = Instant::now();
    let output = fixture.run(
        fixture
            .command()
            .env("WAYLAND_DISPLAY", socket)
            .arg("doctor"),
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Wayland diagnostics did not complete")
    );
    assert!(started.elapsed() < Duration::from_secs(10));
    server.join().unwrap();
}
