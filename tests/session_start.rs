use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct Fixture(std::path::PathBuf);

impl Fixture {
    fn new(script: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "paprika-session-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let executable = path.join("systemctl");
        fs::write(&executable, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(executable, fs::Permissions::from_mode(0o755)).unwrap();
        Self(path)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_paprika-vdagent"));
        command
            .arg("session-start")
            .env_clear()
            .env("PATH", &self.0)
            .env("CALLS", self.0.join("calls"))
            .env("WAYLAND_DISPLAY", "wayland-test")
            .env("XDG_RUNTIME_DIR", &self.0);
        command
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.0.join("calls")).unwrap_or_default()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn imports_environment_before_restarting_service() {
    let fixture = Fixture::new("printf '%s\\n' \"$*\" >> \"$CALLS\"");
    let output = fixture.command().output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        fixture.calls(),
        "--user import-environment WAYLAND_DISPLAY XDG_RUNTIME_DIR\n--user restart paprika-vdagent.service\n"
    );
}

#[test]
fn includes_hyprland_environment_when_available() {
    let fixture = Fixture::new("printf '%s\\n' \"$*\" >> \"$CALLS\"");
    assert!(
        fixture
            .command()
            .env("HYPRLAND_INSTANCE_SIGNATURE", "test-instance")
            .status()
            .unwrap()
            .success()
    );
    assert!(
        fixture
            .calls()
            .contains("XDG_RUNTIME_DIR HYPRLAND_INSTANCE_SIGNATURE\n")
    );
}

#[test]
fn does_not_restart_after_environment_import_failure() {
    let fixture = Fixture::new("printf '%s\\n' \"$*\" >> \"$CALLS\"; exit 1");
    let output = fixture.command().output().unwrap();
    assert!(!output.status.success());
    assert!(!fixture.calls().contains("restart"));
}

#[test]
fn rejects_missing_wayland_environment_without_calling_systemctl() {
    let fixture = Fixture::new("printf '%s\\n' \"$*\" >> \"$CALLS\"");
    let output = fixture
        .command()
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("WAYLAND_DISPLAY is not set"));
    assert!(fixture.calls().is_empty());
}

#[test]
fn reports_service_start_failure() {
    let fixture = Fixture::new("[ \"$2\" != restart ]");
    let output = fixture.command().output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("install the user service"));
}
