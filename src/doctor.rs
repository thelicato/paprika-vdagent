use std::fs;
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rustix::fs::{Access, access};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

const CHECK_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, PartialEq, Eq)]
enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Default)]
struct Report {
    checks: Vec<(Level, String)>,
}

impl Report {
    fn add(&mut self, level: Level, message: impl Into<String>) {
        self.checks.push((level, message.into()));
    }

    fn print(&self) -> Result<()> {
        let mut failures = 0;
        let mut warnings = 0;
        for (level, message) in &self.checks {
            let label = match level {
                Level::Ok => "OK",
                Level::Warn => {
                    warnings += 1;
                    "WARN"
                }
                Level::Fail => {
                    failures += 1;
                    "FAIL"
                }
            };
            println!("[{label}] {message}");
        }
        println!(
            "\n{failures} failure(s), {warnings} warning(s). See the installation and session startup guide for fixes."
        );
        if failures > 0 {
            bail!("required checks failed");
        }
        Ok(())
    }
}

pub fn run(
    backend: crate::Backend,
    port: &Path,
    seat: Option<&str>,
    file_dir: Option<&Path>,
) -> Result<()> {
    let mut report = Report::default();
    match backend.resolve() {
        Err(error) => report.add(Level::Fail, format!("Backend selection failed: {error:#}")),
        Ok(crate::DisplayBackend::Wayland) => check_wayland(&mut report, seat),
        Ok(crate::DisplayBackend::X11) => {
            if seat.is_some() {
                if matches!(backend, crate::Backend::Auto) {
                    report.add(Level::Warn, "Ignoring the Wayland seat setting in an automatically detected X11 session.");
                } else {
                    report.add(
                        Level::Fail,
                        "--seat applies only to Wayland; remove it when selecting the X11 backend.",
                    );
                }
            }
            let (sender, receiver) = mpsc::sync_channel(1);
            thread::spawn(move || {
                let _ = sender.send(crate::x11::probe());
            });
            match receiver.recv_timeout(CHECK_TIMEOUT) {
                Ok(Ok(xfixes)) => {
                    report.add(Level::Ok, "Connected to the X11 server; CLIPBOARD and PRIMARY selections are available.");
                    if xfixes {
                        report.add(Level::Ok, "XFixes selection notifications are available.");
                    } else {
                        report.add(Level::Warn, "XFixes selection notifications are unavailable; the bridge will poll the clipboard.");
                    }
                }
                Ok(Err(error)) => report.add(Level::Fail, format!("X11 connection failed: {error:#}. Run doctor inside your X11 session without sudo.")),
                Err(error) => report.add(Level::Fail, format!("X11 diagnostics did not complete: {error}. Check that the X server is responding.")),
            }
        }
    }
    check_port(&mut report, port);
    check_agents(&mut report, Path::new("/proc"));
    let directory = file_dir
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Downloads")));
    check_directory(&mut report, directory.as_deref());
    check_service(&mut report);
    report.print()
}

fn check_port(report: &mut Report, port: &Path) {
    match fs::metadata(port) {
        Ok(metadata) => {
            if !metadata.file_type().is_char_device() {
                report.add(Level::Fail, format!("SPICE port {} is not a character device; select the VM's virtio channel with --virtio-port.", port.display()));
                return;
            }
            match access(port, Access::READ_OK | Access::WRITE_OK) {
                Ok(()) => report.add(Level::Ok, format!("SPICE port {} is readable and writable (not opened).", port.display())),
                Err(err) => report.add(Level::Fail, format!("Cannot read/write SPICE port {}: {err}. Configure persistent udev permissions for your desktop user; run Paprika without sudo.", port.display())),
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => report.add(Level::Fail, format!("SPICE port {} is missing. Configure the com.redhat.spice.0 virtio channel in the VM, or select it with --virtio-port.", port.display())),
        Err(err) => report.add(Level::Fail, format!("Cannot inspect SPICE port {}: {err}. Check device permissions.", port.display())),
    }
}

fn check_agents(report: &mut Report, proc_dir: &Path) {
    let entries = match fs::read_dir(proc_dir) {
        Ok(entries) => entries,
        Err(err) => {
            report.add(Level::Warn, format!("Cannot inspect competing agents: {err}. Check for spice-vdagent and spice-vdagentd manually."));
            return;
        }
    };
    let mut agents = Vec::new();
    let mut incomplete = false;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                incomplete = true;
                continue;
            }
        };
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        match fs::read_to_string(entry.path().join("comm")) {
            Ok(name) if matches!(name.trim(), "spice-vdagent" | "spice-vdagentd") => {
                agents.push(format!("{} (PID {pid})", name.trim()));
            }
            // Processes may exit between listing /proc and reading comm.
            Err(err) if err.kind() != io::ErrorKind::NotFound => incomplete = true,
            _ => {}
        }
    }
    agents.sort();
    if agents.is_empty() {
        report.add(Level::Ok, "No running stock SPICE agents found.");
    } else {
        report.add(Level::Fail, format!("Competing stock agents: {}. Stop spice-vdagent and mask spice-vdagentd.service and spice-vdagentd.socket before starting Paprika.", agents.join(", ")));
    }
    if incomplete {
        report.add(Level::Warn, "Some processes could not be inspected; check for competing agents with your system's process tools.");
    }
}

fn check_directory(report: &mut Report, directory: Option<&Path>) {
    let Some(directory) = directory else {
        report.add(
            Level::Warn,
            "No download directory: HOME is unset. Set --file-dir to receive files.",
        );
        return;
    };
    match fs::metadata(directory) {
        Ok(metadata) if !metadata.is_dir() => report.add(Level::Fail, format!("Download path {} is not a directory. Choose a directory with --file-dir.", directory.display())),
        Ok(_) => match access(directory, Access::WRITE_OK | Access::EXEC_OK) {
            Ok(()) => report.add(Level::Ok, format!("Download directory {} is writable.", directory.display())),
            Err(err) => report.add(Level::Fail, format!("Download directory {} is inaccessible: {err}. Fix permissions or choose --file-dir.", directory.display())),
        },
        Err(err) if err.kind() == io::ErrorKind::NotFound => report.add(Level::Warn, format!("Download directory {} does not exist. Paprika will try to create it on startup; check parent permissions or choose --file-dir.", directory.display())),
        Err(err) => report.add(Level::Fail, format!("Cannot inspect download directory {}: {err}. Check permissions or choose --file-dir.", directory.display())),
    }
}

fn systemctl_output(args: &[&str]) -> Result<Output> {
    let mut child = Command::new("systemctl")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("could not run systemctl")?;
    let deadline = Instant::now() + CHECK_TIMEOUT;
    loop {
        if child.try_wait()?.is_some() {
            return child
                .wait_with_output()
                .context("could not read systemctl output");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "systemctl did not respond within {} seconds",
                CHECK_TIMEOUT.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn check_service(report: &mut Report) {
    match systemctl_output(&[
        "--user", "show", "paprika-vdagent.service", "--property=LoadState,ActiveState",
    ]) {
        Ok(output) => {
            let properties = String::from_utf8_lossy(&output.stdout);
            if properties.lines().any(|line| line == "LoadState=loaded") {
                let active = properties.lines().find_map(|line| line.strip_prefix("ActiveState=")).unwrap_or("unknown");
                let level = if active == "active" { Level::Ok } else { Level::Warn };
                report.add(level, format!("User service installed, state={active}. Use `paprika-vdagent session-start` from the compositor's autostart to start it after login."));
            } else if properties.lines().any(|line| line == "LoadState=not-found") {
                report.add(Level::Warn, "User service is not installed. Install paprika-vdagent.service and run `systemctl --user daemon-reload`, or launch Paprika directly.");
            } else {
                report.add(Level::Warn, "Cannot inspect the user service. Check `systemctl --user status paprika-vdagent.service`; direct compositor startup also works without systemd.");
            }
        }
        Err(err) => report.add(Level::Warn, format!("User service check unavailable: {err}. Without systemd, launch Paprika directly from your compositor.")),
    }
    // A socket can activate the stock daemon even when no agent process is running.
    match systemctl_output(&[
        "show",
        "spice-vdagentd.socket",
        "--property=ActiveState,LoadState",
    ]) {
        Ok(output) => {
            let properties = String::from_utf8_lossy(&output.stdout);
            if output.status.success()
                && properties.lines().any(|line| line == "ActiveState=active")
            {
                report.add(Level::Fail, "spice-vdagentd.socket is active and can start a competing daemon. Mask both spice-vdagentd.service and spice-vdagentd.socket.");
            } else if !properties.lines().any(|line| matches!(line, "LoadState=loaded" | "LoadState=masked" | "LoadState=not-found")) {
                report.add(Level::Warn, "Cannot inspect spice-vdagentd.socket. If installed, ensure the stock socket cannot activate a competing daemon.");
            }
        }
        Err(err) => report.add(Level::Warn, format!("Stock socket check unavailable: {err}. If installed, ensure spice-vdagentd.socket cannot activate a competing daemon.")),
    }
}

fn check_wayland(report: &mut Report, seat: Option<&str>) {
    let seat = seat.map(str::to_owned);
    let (tx, rx) = mpsc::sync_channel(1);
    // Registry roundtrips can block on an unresponsive compositor. This one-off
    // worker owns the connection; the doctor command exits after the deadline.
    let worker = thread::Builder::new()
        .name("paprika-doctor-wayland".into())
        .spawn(move || {
            let _ = tx.send(inspect_wayland(seat.as_deref()));
        });
    if let Err(err) = worker {
        report.add(
            Level::Fail,
            format!("Could not start Wayland diagnostics: {err}."),
        );
        return;
    }
    match rx.recv_timeout(CHECK_TIMEOUT) {
        Ok(Ok(checks)) => report.checks.extend(checks.checks),
        Ok(Err(err)) => report.add(Level::Fail, format!("Wayland connection failed: {err:#}. Run doctor inside the guest's Wayland session without sudo; check WAYLAND_DISPLAY and XDG_RUNTIME_DIR.")),
        Err(err) => report.add(Level::Fail, format!("Wayland diagnostics did not complete: {err}. Check that the compositor is responding.")),
    }
}

#[derive(Default)]
struct RegistryState {
    seats: Vec<String>,
}

fn inspect_wayland(requested_seat: Option<&str>) -> Result<Report> {
    let connection = Connection::connect_to_env().context("could not connect to the compositor")?;
    let (globals, mut queue) = registry_queue_init::<RegistryState>(&connection)
        .context("could not read the Wayland registry")?;
    let mut report = Report::default();
    report.add(Level::Ok, "Connected to the Wayland compositor.");
    let mut protocols = Vec::new();
    let qh = queue.handle();
    // Keep seat proxies alive until their names have been received.
    let seats: Vec<WlSeat> = globals.contents().with_list(|list| {
        list.iter()
            .filter_map(|global| {
                match global.interface.as_str() {
                    "ext_data_control_manager_v1" | "zwlr_data_control_manager_v1"
                        if global.version >= 1 =>
                    {
                        protocols.push(format!("{} v{}", global.interface, global.version));
                    }
                    "wl_seat" if global.version >= 2 => {
                        return Some(globals.registry().bind(global.name, 2, &qh, ()));
                    }
                    _ => {}
                }
                None
            })
            .collect()
    });
    if protocols.is_empty() {
        report.add(Level::Fail, "No supported clipboard protocol advertised. Use a compositor exposing ext-data-control or wlr-data-control.");
    } else {
        report.add(
            Level::Ok,
            format!("Clipboard protocols: {}.", protocols.join(", ")),
        );
    }
    let mut state = RegistryState::default();
    queue
        .roundtrip(&mut state)
        .context("could not read Wayland seat names")?;
    if seats.is_empty() {
        report.add(
            Level::Fail,
            "No supported Wayland seats found (wl_seat v2 or newer required).",
        );
    } else if let Some(name) = requested_seat {
        if state.seats.iter().any(|seat| seat == name) {
            report.add(
                Level::Ok,
                format!("Requested Wayland seat {name} is available."),
            );
        } else {
            report.add(Level::Fail, format!("Requested Wayland seat {name} was not found. Check --seat; available seats: {}.", state.seats.join(", ")));
        }
    } else {
        report.add(
            Level::Ok,
            format!("Wayland seats: {}.", state.seats.join(", ")),
        );
    }
    Ok(report)
}

impl Dispatch<WlRegistry, GlobalListContents> for RegistryState {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for RegistryState {
    fn event(
        state: &mut Self,
        _: &WlSeat,
        event: <WlSeat as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Name { name } = event {
            state.seats.push(name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_both_stock_agents_without_matching_similar_names() {
        let directory =
            std::env::temp_dir().join(format!("paprika-doctor-proc-{}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        for (pid, name) in [
            (101, "spice-vdagent"),
            (102, "spice-vdagentd"),
            (103, "spice-vdagent-test"),
            (104, "paprika-vdagent"),
        ] {
            let process = directory.join(pid.to_string());
            fs::create_dir(&process).unwrap();
            fs::write(process.join("comm"), format!("{name}\n")).unwrap();
        }
        let mut report = Report::default();
        check_agents(&mut report, &directory);
        fs::remove_dir_all(directory).unwrap();
        assert_eq!(report.checks[0].0, Level::Fail);
        assert!(report.checks[0].1.contains("spice-vdagent (PID 101)"));
        assert!(report.checks[0].1.contains("spice-vdagentd (PID 102)"));
        assert!(!report.checks[0].1.contains("PID 103"));
        assert!(!report.checks[0].1.contains("PID 104"));
    }

    #[test]
    fn absent_proc_is_a_warning_not_a_false_success() {
        let mut report = Report::default();
        check_agents(&mut report, Path::new("/proc/paprika-nonexistent"));
        assert_eq!(report.checks[0].0, Level::Warn);
    }

    #[test]
    fn missing_port_is_a_failure_with_configuration_hint() {
        let mut report = Report::default();
        check_port(&mut report, Path::new("/dev/paprika-nonexistent"));
        assert_eq!(report.checks[0].0, Level::Fail);
        assert!(report.checks[0].1.contains("--virtio-port"));
    }

    #[test]
    fn missing_download_directory_does_not_get_created() {
        let directory =
            std::env::temp_dir().join(format!("paprika-doctor-absent-{}", std::process::id()));
        let mut report = Report::default();
        check_directory(&mut report, Some(&directory));
        assert_eq!(report.checks[0].0, Level::Warn);
        assert!(!directory.exists());
    }

    #[test]
    fn a_file_cannot_be_used_as_a_download_directory() {
        let mut report = Report::default();
        check_directory(&mut report, Some(Path::new("Cargo.toml")));
        assert_eq!(report.checks[0].0, Level::Fail);
    }
}
