use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new(script: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "paprika-session-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let executable = path.join("systemctl");
        fs::write(
            &executable,
            format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$CALLS\"\n{script}\n"),
        )
        .unwrap();
        fs::set_permissions(executable, fs::Permissions::from_mode(0o755)).unwrap();
        Self(path)
    }

    fn base_command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_paprika-vdagent"));
        command
            .env_clear()
            .env("PATH", &self.0)
            .env("CALLS", self.0.join("calls"))
            .env("WAYLAND_DISPLAY", "wayland-test")
            .env("XDG_RUNTIME_DIR", &self.0);
        command
    }

    fn command(&self) -> Command {
        let mut command = self.base_command();
        command.arg("session-start");
        command
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.0.join("calls")).unwrap_or_default()
    }

    fn config_path(&self) -> PathBuf {
        self.0
            .join("systemd/user/paprika-vdagent.service.d/90-paprika-session.conf")
    }

    fn config(&self) -> String {
        fs::read_to_string(self.config_path()).unwrap()
    }

    fn run(&self, command: &mut Command) {
        let output = command.output().unwrap();
        assert!(output.status.success(), "{output:?}");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn scopes_wayland_settings_to_the_service_before_reload_and_restart() {
    let fixture = Fixture::new("");
    fixture.run(&mut fixture.command());
    assert_eq!(
        fixture.calls(),
        "--user daemon-reload\n--user restart paprika-vdagent.service\n"
    );
    let config = fixture.config();
    assert!(config.contains("Environment=\"PAPRIKA_VDAGENT_BACKEND=wayland\""));
    assert!(config.contains("Environment=\"WAYLAND_DISPLAY=wayland-test\""));
    assert!(config.contains(
        "UnsetEnvironment=WAYLAND_SOCKET DISPLAY XAUTHORITY HYPRLAND_INSTANCE_SIGNATURE"
    ));
    assert_eq!(
        fs::metadata(fixture.config_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn wayland_with_xwayland_still_uses_the_native_wayland_backend() {
    let fixture = Fixture::new("");
    fixture.run(
        fixture
            .command()
            .env("XDG_SESSION_TYPE", "wayland")
            .env("DISPLAY", ":9")
            .env("XAUTHORITY", "/old/authority")
            .env("HYPRLAND_INSTANCE_SIGNATURE", "current-hyprland"),
    );
    let config = fixture.config();
    assert!(config.contains("PAPRIKA_VDAGENT_BACKEND=wayland"));
    assert!(config.contains("Environment=\"HYPRLAND_INSTANCE_SIGNATURE=current-hyprland\""));
    assert!(!config.contains(":9"));
    assert!(!config.contains("/old/authority"));
}

#[test]
fn x11_session_replaces_stale_wayland_settings_and_retains_authentication() {
    let fixture = Fixture::new("");
    fixture.run(
        fixture
            .command()
            .env("XDG_SESSION_TYPE", "x11")
            .env("DISPLAY", ":3")
            .env("WAYLAND_SOCKET", "42")
            .env("HYPRLAND_INSTANCE_SIGNATURE", "old-hyprland")
            .env("XAUTHORITY", "/run/user/1000/x11-authority"),
    );
    let config = fixture.config();
    assert!(config.contains("Environment=\"PAPRIKA_VDAGENT_BACKEND=x11\""));
    assert!(config.contains("Environment=\"DISPLAY=:3\""));
    assert!(config.contains("Environment=\"XAUTHORITY=/run/user/1000/x11-authority\""));
    assert!(config.contains("UnsetEnvironment=WAYLAND_DISPLAY WAYLAND_SOCKET HYPRLAND_INSTANCE_SIGNATURE PAPRIKA_VDAGENT_SEAT"));
    assert!(!config.contains("old-hyprland"));
}

#[test]
fn display_alone_selects_x11_and_missing_authority_uses_normal_authentication_fallback() {
    let fixture = Fixture::new("");
    fixture.run(
        fixture
            .command()
            .env_remove("WAYLAND_DISPLAY")
            .env("DISPLAY", ":3"),
    );
    let config = fixture.config();
    assert!(config.contains("PAPRIKA_VDAGENT_BACKEND=x11"));
    assert!(!config.contains("Environment=\"XAUTHORITY="));
    assert!(config.contains("WAYLAND_SOCKET XAUTHORITY HYPRLAND_INSTANCE_SIGNATURE"));
}

#[test]
fn switching_sessions_replaces_the_override_instead_of_accumulating_stale_values() {
    let fixture = Fixture::new("");
    fixture.run(
        fixture
            .command()
            .env("HYPRLAND_INSTANCE_SIGNATURE", "first-session"),
    );
    assert!(fixture.config().contains("first-session"));
    fixture.run(
        fixture
            .command()
            .env("XDG_SESSION_TYPE", "x11")
            .env("DISPLAY", ":3")
            .env("XAUTHORITY", "/first-authority"),
    );
    assert!(!fixture.config().contains("first-session"));
    fixture.run(
        fixture
            .command()
            .env("XDG_SESSION_TYPE", "wayland")
            .env("WAYLAND_DISPLAY", "wayland-new")
            .env("DISPLAY", ":3")
            .env("XAUTHORITY", "/first-authority"),
    );
    let config = fixture.config();
    assert!(config.contains("WAYLAND_DISPLAY=wayland-new"));
    assert!(!config.contains("first-authority"));
    assert!(!config.contains("DISPLAY=:3"));
    assert!(config.contains(
        "UnsetEnvironment=WAYLAND_SOCKET DISPLAY XAUTHORITY HYPRLAND_INSTANCE_SIGNATURE"
    ));
    let directory = fixture.config_path().parent().unwrap().to_path_buf();
    assert_eq!(
        fs::read_dir(directory).unwrap().count(),
        1,
        "atomic writes must not leave temporary files"
    );
}

#[test]
fn explicit_backend_and_environment_override_auto_detection() {
    let fixture = Fixture::new("");
    fixture.run(
        fixture
            .base_command()
            .args(["--backend", "x11", "session-start"])
            .env("XDG_SESSION_TYPE", "wayland")
            .env("DISPLAY", ":4"),
    );
    assert!(fixture.config().contains("PAPRIKA_VDAGENT_BACKEND=x11"));
    fixture.run(
        fixture
            .command()
            .env("PAPRIKA_VDAGENT_BACKEND", "wayland")
            .env("XDG_SESSION_TYPE", "x11"),
    );
    assert!(fixture.config().contains("PAPRIKA_VDAGENT_BACKEND=wayland"));
}

#[test]
fn resolves_relative_authority_paths_and_escapes_systemd_specifiers() {
    let fixture = Fixture::new("");
    fixture.run(
        fixture
            .command()
            .env("XDG_SESSION_TYPE", "x11")
            .env("DISPLAY", ":3")
            .env("XAUTHORITY", "auth file%name")
            .current_dir(&fixture.0),
    );
    let expected = fixture.0.join("auth file%%name");
    assert!(fixture.config().contains(&format!(
        "Environment=\"XAUTHORITY={}\"",
        expected.display()
    )));
}

#[test]
fn rejects_missing_session_variables_before_writing_or_calling_systemctl() {
    for missing in ["WAYLAND_DISPLAY", "XDG_RUNTIME_DIR"] {
        let fixture = Fixture::new("");
        let output = fixture
            .command()
            .env("XDG_SESSION_TYPE", "wayland")
            .env_remove(missing)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains(&format!("{missing} is not set")));
        assert!(fixture.calls().is_empty());
        assert!(!fixture.config_path().exists());
    }
    let fixture = Fixture::new("");
    let output = fixture
        .command()
        .env("XDG_SESSION_TYPE", "x11")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("DISPLAY is not set"));
    assert!(fixture.calls().is_empty());
}

#[test]
fn rejects_an_undetected_session_or_a_socket_only_wayland_service_environment() {
    let fixture = Fixture::new("");
    let output = fixture
        .command()
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot detect a graphical session"));
    let output = fixture
        .command()
        .env_remove("WAYLAND_DISPLAY")
        .env("WAYLAND_SOCKET", "42")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("file descriptor cannot be forwarded")
    );
    assert!(fixture.calls().is_empty());
}

#[test]
fn rejects_relative_runtime_directories_and_unwritable_override_locations() {
    let fixture = Fixture::new("");
    let output = fixture
        .command()
        .env("XDG_RUNTIME_DIR", ".")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(fixture.calls().is_empty());
    fs::write(fixture.0.join("systemd"), b"existing file").unwrap();
    let output = fixture.command().output().unwrap();
    assert!(!output.status.success());
    assert!(fixture.calls().is_empty());
    assert_eq!(
        fs::read(fixture.0.join("systemd")).unwrap(),
        b"existing file"
    );
}

#[test]
fn does_not_restart_when_reloading_the_manager_fails() {
    let fixture = Fixture::new("[ \"$2\" != daemon-reload ]");
    let output = fixture.command().output().unwrap();
    assert!(!output.status.success());
    assert_eq!(fixture.calls(), "--user daemon-reload\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("could not reload"));
}

#[test]
fn reports_service_start_failure() {
    let fixture = Fixture::new("[ \"$2\" != restart ]");
    let output = fixture.command().output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("install the user service"));
}
