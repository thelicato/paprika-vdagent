# Installation and session startup

Paprika runs **inside the guest's graphical session**, after login. It needs access to the SPICE virtio port and either a Wayland compositor exposing `ext-data-control` or `wlr-data-control`, or an X11 server. The default `--backend auto` selects the native session; `--backend wayland` and `--backend x11` provide explicit overrides. A distro's package format does not determine display compatibility. The steps below assume one desktop user per guest.

Follow steps 1–3, choose a graphical-session startup method in step 4, then reboot and run the checks in step 5. Run guest commands as your desktop user, using sudo only where shown.

## 1. Install

### Distribution packages

Download the release package for your architecture, then use your distro's tool (replace the example filename with the downloaded file):

| Distribution | Command |
| --- | --- |
| Arch Linux, CachyOS, other pacman systems | `sudo pacman -U ./paprika-vdagent-<version>-x86_64.pkg.tar.zst` |
| Debian, Ubuntu | `sudo apt install ./paprika-vdagent_<version>_<architecture>.deb` |
| Fedora | `sudo dnf install ./paprika-vdagent-<version>.<architecture>.rpm` |
| openSUSE | `sudo zypper install ./paprika-vdagent-<version>.<architecture>.rpm` |

Packages include the binary and a systemd user service. Continue with step 2.

### Build from source or use a tarball

On apt-based distributions, install the build dependencies and clone the repository:

```sh
sudo apt update
sudo apt install -y git build-essential pkg-config libwayland-dev cargo rustc
git clone https://github.com/thelicato/paprika-vdagent.git
cd paprika-vdagent
```

If you already have a checkout, use that repository instead of cloning it again.

Build with a Rust toolchain supporting edition 2024 and your distro's Wayland development packages:

```sh
cargo build --locked --release
```

For Arch Linux and CachyOS, the build dependencies can be installed with:

```sh
sudo pacman -S --needed rust base-devel wayland pkgconf
```

The local pacman recipe is another option:

```sh
cd packaging/pacman
makepkg -si
```

For a manual source install, run these from the repository root:

```sh
sudo install -Dm755 target/release/paprika-vdagent /usr/bin/paprika-vdagent
sudo install -Dm644 packaging/systemd/user/paprika-vdagent.service \
  /usr/lib/systemd/user/paprika-vdagent.service
systemctl --user daemon-reload
```

After rebuilding an existing installation, repeat the binary installation command and run `paprika-vdagent session-start` from your graphical session to restart with the updated binary.

Tarballs include this guide in `docs/` and the service in `packaging/systemd/user/`. For tarballs, install the extracted binary at `/usr/bin/paprika-vdagent` and the [service file](../packaging/systemd/user/paprika-vdagent.service) at `/usr/lib/systemd/user/paprika-vdagent.service`, or run the binary directly using your compositor's autostart mechanism.

## 2. Release the SPICE port

Paprika owns `/dev/virtio-ports/com.redhat.spice.0` directly. The stock agents must not use it at the same time. On systemd systems:

```sh
sudo systemctl mask --now spice-vdagentd.service spice-vdagentd.socket
pkill -x spice-vdagent || true
```

If your distribution uses different unit names, inspect `systemctl list-unit-files 'spice*'` and stop/mask the corresponding daemon and socket. If no stock agent is installed, skip this step. Remove any explicit `spice-vdagent` launch from your session startup configuration as well.

### Disable the stock desktop autostart entry

Desktops supporting XDG autostart may launch the stock session agent independently of systemd. Masking the daemon and socket does not disable desktop autostart entries. Check for a stock entry such as `/etc/xdg/autostart/spice-vdagent.desktop`, then create a per-user override with the same filename:

```sh
mkdir -p ~/.config/autostart
cat > ~/.config/autostart/spice-vdagent.desktop <<'EOF'
[Desktop Entry]
Type=Application
Name=Spice vdagent
Hidden=true
EOF
pkill -x spice-vdagent || true
```

If an override already exists, preserve any settings you need and set `Hidden=true`. On other desktops, check the system XDG autostart directories and override the stock entry's actual filename.

Without this override, `doctor` can report a competing `spice-vdagent` immediately after login, then pass a minute or two later when that process exits. Disable its startup rather than waiting for it to disappear each login.

Masking persists across reboots and prevents socket activation. Paprika replaces the stock clipboard and incoming file-transfer path; other stock-agent features are outside its scope.

## 3. Persist port permissions

Check that the SPICE channel exists:

```sh
ls -l /dev/virtio-ports/com.redhat.spice.0
```

If absent, configure a SPICE guest-agent channel in the VM settings on the host. For libvirt/QEMU this is a virtio channel named `com.redhat.spice.0`. The QEMU guest-agent channel `org.qemu.guest_agent.0` serves a different purpose.

If your desktop user already has read/write access, no rule is needed. Otherwise, run the following **as the desktop user**, using sudo only as shown. This grants that user access to this specific port, including after reboot:

```sh
sudo groupadd --system --force paprika-vdagent
sudo usermod -aG paprika-vdagent "$(id -un)"
printf 'SUBSYSTEM=="virtio-ports", ATTR{name}=="com.redhat.spice.0", GROUP="paprika-vdagent", MODE="0660"\n' |
  sudo tee /etc/udev/rules.d/99-paprika-vdagent.rules
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=virtio-ports
sudo udevadm settle
```

Reboot after completing the startup configuration in step 4 so both the graphical session and the systemd user manager receive the new group membership. Opening another terminal alone does not refresh those processes' groups.

Verify access without sudo:

```sh
id -nG
ls -lL /dev/virtio-ports/com.redhat.spice.0
test -r /dev/virtio-ports/com.redhat.spice.0 &&
  test -w /dev/virtio-ports/com.redhat.spice.0 && echo 'Port access OK'
```

Your groups should include `paprika-vdagent`, and the device should show `root paprika-vdagent` with permissions `crw-rw----`.

See the [udev rule reference](https://www.freedesktop.org/software/systemd/man/latest/udev.html) for the matching and permission fields. A dedicated system group avoids udev versions that reject desktop users in `OWNER`, including numeric IDs, with “Not a system user.” Only members of this group receive access through this rule. Running Paprika with sudo is unnecessary and can prevent it from connecting to your graphical session.

For temporary testing only, the following grants access until the device is recreated. The `setfacl` command is provided by the `acl` package on Arch/CachyOS:

```sh
sudo setfacl -m "u:$(id -un):rw" /dev/virtio-ports/com.redhat.spice.0
```

## 4. Start with your graphical session

The installed binary provides `paprika-vdagent session-start` for both X11 and Wayland. It detects the session, saves settings in a private runtime override for `paprika-vdagent.service`, reloads the user manager, and restarts the service. It does not change permissions, stop competing agents, or change other user services' environments.

Choose **one** startup method below and run it at each graphical login. The existing Hyprland and Sway commands remain the same. You do not need to enable the user service separately when using these hooks.

### Automatic backend selection

`paprika-vdagent`, `paprika-vdagent doctor`, and `paprika-vdagent session-start` use the same rules:

1. An explicit `--backend wayland` or `--backend x11`, including `PAPRIKA_VDAGENT_BACKEND`, overrides detection.
2. `XDG_SESSION_TYPE=wayland` selects Wayland, and `XDG_SESSION_TYPE=x11` selects X11, even if variables from an older session remain.
3. Without a recognised session type, a non-empty `WAYLAND_DISPLAY` or `WAYLAND_SOCKET` selects Wayland. Otherwise a non-empty `DISPLAY` selects X11.
4. Without a display hint, detection fails with instructions to run inside a graphical session or select a backend explicitly.

A Wayland session keeps using native data-control access when XWayland also sets `DISPLAY`. A failed Wayland connection or missing data-control protocol does not trigger an X11 fallback. When session metadata is missing or misleading, use an explicit backend override.

For example, force X11 for a manual run or service startup with:

```sh
paprika-vdagent --backend x11
paprika-vdagent --backend x11 session-start
```

The startup helper requires an existing absolute `XDG_RUNTIME_DIR` and the selected display variable. A `WAYLAND_SOCKET` file descriptor alone cannot be forwarded to the service, so Wayland service startup requires `WAYLAND_DISPLAY`.

### X11 and desktops supporting XDG autostart

Add `paprika-vdagent session-start` to your desktop or window manager's autostart configuration. For desktops supporting XDG autostart:

```sh
mkdir -p ~/.config/autostart
cat > ~/.config/autostart/paprika-vdagent.desktop <<'EOF'
[Desktop Entry]
Type=Application
Name=Paprika SPICE clipboard bridge
Exec=/usr/bin/paprika-vdagent session-start
Terminal=false
EOF
```

This entry can be used across X11 and Wayland sessions when the desktop supports XDG autostart. Replace an older X11-only entry that starts the bridge directly with `--backend x11`; use one launch method per session. The stock SPICE agents must remain stopped and the port permissions from steps 2 and 3 still apply.

The X11 backend uses `DISPLAY` and the session's X authentication settings, including `XAUTHORITY` when set. It supports regular clipboard and primary selection, UTF-8 text, Latin-1 `STRING` conversion, and incremental transfers for large selections. XFixes supplies change notifications when available; otherwise the bridge polls using `--poll-ms`. Use `--poll-only` to force polling when troubleshooting unreliable notifications. A Wayland seat setting is ignored during automatic X11 selection; omit `--seat` when forcing the X11 backend.

### Hyprland: Lua configuration

Add this to `~/.config/hypr/hyprland.lua`, or put the `hl.exec_cmd` call inside an existing `hyprland.start` callback:

```lua
hl.on("hyprland.start", function()
    hl.exec_cmd("paprika-vdagent session-start")
end)
```

This uses Hyprland's [startup event](https://wiki.hypr.land/configuring/core/autostart/). It runs when Hyprland starts, rather than on configuration reload. Log out and back in to test it.

### Hyprland: older hyprlang configuration

For versions still using `~/.config/hypr/hyprland.conf`:

```ini
exec-once = paprika-vdagent session-start
```

### Sway

Add this to `~/.config/sway/config` using Sway's [`exec` startup command](https://github.com/swaywm/sway/blob/master/sway/sway.5.scd):

```text
exec paprika-vdagent session-start
```

### Other compositors and session managers

Use the compositor's login/autostart facility to run:

```sh
paprika-vdagent session-start
```

If the session manager already supplies the correct display environment and manages systemd graphical-session startup, use its documented service integration. The supplied unit is enabled under `default.target`; enabling it by itself does not guarantee that a display is ready. With the hooks above, `session-start` supplies the service settings at login.

On systems without systemd, use your init system to prevent the stock agents from starting and run `paprika-vdagent` directly from the graphical session's autostart facility after arranging port permissions. Backend detection is the same; `session-start` requires systemd.

GNOME and KDE Plasma installations must also expose one of the required data-control protocols. Do not assume compatibility just because the session uses Wayland; a missing-protocol error cannot be fixed by enabling the service.

### Session settings and switching desktops

`session-start` atomically replaces `$XDG_RUNTIME_DIR/systemd/user/paprika-vdagent.service.d/90-paprika-session.conf`, with permissions `0600`. The override selects the resolved backend and display for Paprika only. It is runtime configuration, so it is removed when the runtime directory is removed; the helper refreshes it at every login.

For Wayland it records `WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR`, and the Hyprland instance signature when present. For X11 it records `DISPLAY`, `XDG_RUNTIME_DIR`, and `XAUTHORITY` when present, resolving relative authentication paths before saving them. Variables belonging to the other backend, absent optional variables, and `WAYLAND_SOCKET` are unset for this service. Missing `XAUTHORITY` stays unset so normal X11 authentication fallback works. X11 startup also unsets a service-level Wayland seat setting.

Run the same helper after switching desktop sessions. One user service follows the most recent startup invocation; simultaneous sessions for the same user need separate service instances. To inspect the effective configuration, use `systemctl --user cat paprika-vdagent.service`.

If you stop using the helper, remove its runtime override and reload the manager:

```sh
rm -f "$XDG_RUNTIME_DIR/systemd/user/paprika-vdagent.service.d/90-paprika-session.conf"
systemctl --user daemon-reload
```

## 5. Verify and troubleshoot

Run diagnostics from your graphical session without sudo:

```sh
paprika-vdagent doctor
```

The command checks the display connection, Wayland data-control protocols and requested seat or X11 selection capabilities and XFixes support, SPICE port permissions, competing stock agents, download directory, and systemd user service. It does not open the SPICE port, read or replace clipboard contents, create directories, or start/stop services. `[FAIL]` indicates a blocking problem and produces a non-zero exit status. `[WARN]` indicates an optional, incomplete, or unverified check, such as a missing service when you launch the binary directly. Successful diagnostics establish local prerequisites; test actual host/guest clipboard and file transfers separately.

Place custom options before the subcommand, or use the same environment variables as the bridge:

```sh
paprika-vdagent --virtio-port /dev/virtio-ports/com.redhat.spice.0 --seat seat0 --file-dir "$HOME/Downloads" doctor
```

Stop any manually launched Paprika process before starting the service, then run these inside your graphical session to test immediately:

```sh
paprika-vdagent session-start
systemctl --user status paprika-vdagent.service
journalctl --user -u paprika-vdagent.service -b -n 50 --no-pager
```

After reboot and login, check the status and logs again and test clipboard sync in both directions with your SPICE client connected.

Copy fresh plain text on the host and paste into a guest text editor, then copy different guest text and paste on the host. Use Ctrl+V in a text editor or Ctrl+Shift+V in most terminals. Test an incoming file transfer separately if your viewer supports it; incoming files go to `~/Downloads` by default.

| Symptom | Check |
| --- | --- |
| `cargo: command not found` | Install a Rust toolchain, or use a release binary/package. |
| Permission denied opening the virtio port | Apply step 3 for the desktop user and verify device access. |
| Port missing or service condition skipped | Verify the VM's SPICE channel; start the service again after it appears. |
| Wrong backend selected | Check `XDG_SESSION_TYPE` and display variables, or use `--backend wayland` or `--backend x11`. |
| Cannot connect to the X11 server | Check `DISPLAY`, `XAUTHORITY`, and session authentication; run without sudo. |
| Cannot connect to the Wayland compositor | Run inside the guest session and import its current environment; avoid sudo. |
| Required data-control protocol unavailable | Check compositor support for `ext-data-control` or `wlr-data-control`. |
| Service unit not found | Install the service file and run `systemctl --user daemon-reload`. |
| Starts, but clipboard does not sync | Check for competing stock/manual agents and inspect debug logs. |
| Competing `spice-vdagent` just after login, then diagnostics pass | Disable the stock desktop autostart entry in step 2; systemd masks alone do not stop it. |

For debug logs, use `systemctl --user edit paprika-vdagent.service`:

```ini
[Service]
Environment=RUST_LOG=paprika_vdagent=debug
```

Then restart the service. Other settings can be supplied in the same override, for example `Environment=PAPRIKA_VDAGENT_FILE_DIR=/home/your-user/Downloads`.

### Clipboard works in only one direction

After installing or restarting the guest agent, reconnect the host viewer and copy fresh plain text before testing again. Check the viewer's clipboard-sharing settings if available.

If the problem persists, enable Paprika's debug logs as above and watch the guest journal while copying on the host. A host copy offering UTF-8 text normally produces `host claimed clipboard ownership`, followed by `received host clipboard text`. A `discarding stale host clipboard GRAB` message means an announcement arrived but failed serial validation. If there are no incoming clipboard messages during the attempt, inspect the host viewer and QEMU clipboard configuration as well as the guest configuration. Use the viewer's own diagnostics to check whether it detects host clipboard changes and forwards them to the VM.

Remove the Paprika debug environment line from your service override when finished, then reload and restart the user service to return to normal logging.

### Clipboard timeouts and recovery

Clipboard payload reads have a total deadline of 2000 ms, including sources that send partial data but never finish. In X11 this includes selection target negotiation and incremental transfer events. A timed-out read leaves the cached clipboard intact and schedules another attempt after the polling interval (`--poll-ms`, default 250 ms). A fresh selection event supersedes a stalled source's retry delay. The deadline applies to receiving the payload from the clipboard owner; display connection setup and server roundtrips still depend on the display server responding.

To allow more time for a slow source, set `--clipboard-timeout-ms` when running manually, or add this to your service override:

```ini
[Service]
Environment=PAPRIKA_VDAGENT_CLIPBOARD_TIMEOUT_MS=5000
```

Restart the service after editing its override. Both the clipboard timeout and polling interval must be greater than zero.

If the event watcher disconnects or a Wayland data-control device finishes, the bridge falls back to polling. If the display server itself is gone or the SPICE transport fails, the bridge exits with an error and the supplied systemd service restarts it. Without systemd, your session supervisor must restart it. A restarted compositor may have a different socket or session environment; run `paprika-vdagent session-start` from the new session to update its service settings.

A SPICE client disconnect cancels in-progress file transfers and resets capability negotiation. The visible clipboard stays available in the guest and is announced again after the next client negotiates its clipboard capabilities. Interrupted transfers are removed when cancelled or when the bridge exits normally, including error returns. Termination by a signal (including stopping the service), or a power failure, can leave `.paprika-vdagent-*.part` files; remove these only after stopping the bridge.

## Returning to the stock agent

Remove the Paprika autostart entry and its runtime session override as described above, then:

```sh
systemctl --user disable --now paprika-vdagent.service
sudo rm -f /etc/udev/rules.d/99-paprika-vdagent.rules
sudo udevadm control --reload-rules
sudo systemctl unmask spice-vdagentd.service spice-vdagentd.socket
```

If you created the `Hidden=true` stock-agent override in step 2, remove `~/.config/autostart/spice-vdagent.desktop` to allow the system autostart entry again. If you modified a pre-existing override, restore its previous settings instead.

Reboot to restore device permissions and your distro's normal session startup. If the stock daemon or socket was previously disabled, re-enable the appropriate unit according to your distro's setup.
