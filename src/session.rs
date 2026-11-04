use std::process::Command;

use anyhow::{Context, Result, bail};

/// Import only variables belonging to this session, then restart the user unit.
pub fn start() -> Result<()> {
    for name in ["WAYLAND_DISPLAY", "XDG_RUNTIME_DIR"] {
        if std::env::var_os(name).is_none_or(|value| value.is_empty()) {
            bail!(
                "{name} is not set; run `paprika-vdagent session-start` inside your Wayland session"
            );
        }
    }

    let mut variables = vec!["WAYLAND_DISPLAY", "XDG_RUNTIME_DIR"];
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some_and(|value| !value.is_empty()) {
        variables.push("HYPRLAND_INSTANCE_SIGNATURE");
    }
    let status = Command::new("systemctl")
        .args(["--user", "import-environment"])
        .args(variables)
        .status()
        .context("could not run systemctl; without systemd, launch paprika-vdagent directly from your compositor")?;
    if !status.success() {
        bail!("could not import the Wayland environment into the systemd user manager ({status})");
    }

    let status = Command::new("systemctl")
        .args(["--user", "restart", "paprika-vdagent.service"])
        .status()
        .context("could not restart the Paprika user service")?;
    if !status.success() {
        bail!(
            "could not restart paprika-vdagent.service ({status}); install the user service and run `systemctl --user daemon-reload`"
        );
    }
    println!("Started paprika-vdagent.service with this Wayland session's environment.");
    Ok(())
}
