# Installation and session startup

Paprika runs **inside the guest's graphical session**, after login. It needs access to the SPICE virtio port and either a Wayland compositor exposing `ext-data-control` or `wlr-data-control`, or an X11 server. Wayland is the default backend; native X11 sessions use `--backend x11`. A distro's package format does not determine display compatibility. The steps below assume one desktop user per guest.

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
```

Tarballs include this guide in `docs/` and the service in `packaging/systemd/user/`. For tarballs, install the extracted binary at `/usr/bin/paprika-vdagent` and the [service file](../packaging/systemd/user/paprika-vdagent.service) at `/usr/lib/systemd/user/paprika-vdagent.service`, or run the binary directly using your compositor's autostart mechanism.

## 2. Release the SPICE port

Paprika owns `/dev/virtio-ports/com.redhat.spice.0` directly. The stock agents must not use it at the same time. On systemd systems:

```sh
sudo systemctl mask --now spice-vdagentd.service spice-vdagentd.socket
pkill -x spice-vdagent || true
```

If your distribution uses different unit names, inspect `systemctl list-unit-files 'spice*'` and stop/mask the corresponding daemon and socket. If no stock agent is installed, skip this step. Remove any explicit `spice-vdagent` launch from your session startup configuration as well.

Masking persists across reboots and prevents socket activation. Paprika replaces the stock clipboard and incoming file-transfer path; other stock-agent features are outside its scope.

## 3. Persist port permissions

Check that the SPICE channel exists:

```sh
ls -l /dev/virtio-ports/com.redhat.spice.0
```

If absent, configure a SPICE guest-agent channel in the VM settings on the host. For libvirt/QEMU this is a virtio channel named `com.redhat.spice.0`. The QEMU guest-agent channel `org.qemu.guest_agent.0` serves a different purpose.

If your desktop user already has read/write access, no rule is needed. Otherwise, run the following **as the desktop user**, using sudo only as shown. This grants that user access to this specific port, including after reboot:

```sh
printf 'SUBSYSTEM=="virtio-ports", ATTR{name}=="com.redhat.spice.0", OWNER="%s", MODE="0600"\n' "$(id -un)" |
  sudo tee /etc/udev/rules.d/99-paprika-vdagent.rules
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=virtio-ports
```

Verify access without sudo:

```sh
test -r /dev/virtio-ports/com.redhat.spice.0 &&
  test -w /dev/virtio-ports/com.redhat.spice.0 && echo 'Port access OK'
```

See the [udev rule reference](https://www.freedesktop.org/software/systemd/man/latest/udev.html) for the matching and permission fields. This rule selects one user; multi-user guests need an administrator-managed group or active-session access policy instead. Running Paprika with sudo is unnecessary and can prevent it from connecting to your Wayland session.

For temporary testing only, the following grants access until the device is recreated. The `setfacl` command is provided by the `acl` package on Arch/CachyOS:

```sh
sudo setfacl -m "u:$(id -un):rw" /dev/virtio-ports/com.redhat.spice.0
```

## 4. Start with your graphical session

Run this once after installing or changing the service:

```sh
systemctl --user daemon-reload
```

The installed binary provides `paprika-vdagent session-start`, which imports the current Wayland environment and restarts the systemd user service. It does not change permissions or stop competing agents.

Choose **one** startup method below. Importing the Wayland environment must happen at each login; doing it once in a terminal does not configure future sessions. Start the agent after login, rather than as a root system service.

### Native X11 sessions

Run the installed binary from your X11 session:

```sh
paprika-vdagent --backend x11
```

The backend uses `DISPLAY` and the session's X authentication settings, including `XAUTHORITY` when set. It supports regular clipboard and primary selection, UTF-8 text, Latin-1 `STRING` conversion, and incremental transfers for large selections. XFixes supplies change notifications when available; otherwise the bridge polls using `--poll-ms`. Use `--poll-only` to force polling when troubleshooting unreliable notifications. `--seat` is specific to Wayland and must be omitted for X11.

The Wayland `session-start` helper is not used for X11. Add `paprika-vdagent --backend x11` to your X11 desktop or window manager's autostart configuration. For desktops supporting XDG autostart, create `~/.config/autostart/paprika-vdagent.desktop` with:

```ini
[Desktop Entry]
Type=Application
Name=Paprika SPICE clipboard bridge
Exec=/usr/bin/paprika-vdagent --backend x11
Terminal=false
```

Use one Paprika launch method per session. Stop the Paprika user service before using direct X11 autostart, and disable its automatic startup if enabled. The stock SPICE agents must remain stopped and the port permissions from steps 2 and 3 still apply. This desktop entry is for X11 sessions; remove it before switching to a Wayland autostart method.

### Wayland sessions

The remaining startup examples use the default Wayland backend and its `session-start` helper.

### Hyprland: Lua configuration

Add this to `~/.config/hypr/hyprland.lua`, or put the `hl.exec_cmd` call inside an existing `hyprland.start` callback:

```lua
hl.on("hyprland.start", function()
    hl.exec_cmd("paprika-vdagent session-start")
end)
```

This uses Hyprland's [startup event](https://wiki.hypr.land/configuring/core/autostart/). It runs when Hyprland starts, rather than on configuration reload. Log out and back in to test it. This is the setup verified on CachyOS with Hyprland 0.56.2.

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

If the session manager already imports the Wayland environment and manages systemd graphical-session startup, use its documented service integration. The supplied unit is enabled under `default.target`; enabling it by itself does not guarantee the compositor environment is available. The explicit session hooks above do not require `systemctl --user enable`.

On systems without systemd, use your init system to prevent the stock agents from starting and run `paprika-vdagent` directly from the compositor's autostart facility after arranging port permissions.

GNOME and KDE Plasma installations must also expose one of the required data-control protocols. Do not assume compatibility just because the session uses Wayland; a missing-protocol error cannot be fixed by enabling the service.

## 5. Verify and troubleshoot

Run diagnostics from your session without sudo (choose the matching backend):

```sh
paprika-vdagent doctor
paprika-vdagent --backend x11 doctor
```

The command checks the display connection, Wayland data-control protocols and requested seat or X11 selection capabilities and XFixes support, SPICE port permissions, competing stock agents, download directory, and systemd user service. It does not open the SPICE port, read or replace clipboard contents, create directories, or start/stop services. `[FAIL]` indicates a blocking problem and produces a non-zero exit status. `[WARN]` indicates an optional, incomplete, or unverified check, such as a missing service when you launch the binary directly. Successful diagnostics establish local prerequisites; test actual host/guest clipboard and file transfers separately.

Place custom options before the subcommand, or use the same environment variables as the bridge:

```sh
paprika-vdagent --virtio-port /dev/virtio-ports/com.redhat.spice.0 --seat seat0 --file-dir "$HOME/Downloads" doctor
```

Stop any manually launched Paprika process before starting the service, then run these inside your Wayland session to test immediately:

```sh
paprika-vdagent session-start
systemctl --user status paprika-vdagent.service
journalctl --user -u paprika-vdagent.service -b --no-pager
```

After reboot and login, check the status and logs again and test clipboard sync in both directions with your SPICE client connected.

| Symptom | Check |
| --- | --- |
| `cargo: command not found` | Install a Rust toolchain, or use a release binary/package. |
| Permission denied opening the virtio port | Apply step 3 for the desktop user and verify device access. |
| Port missing or service condition skipped | Verify the VM's SPICE channel; start the service again after it appears. |
| Cannot connect to the X11 server | Check `DISPLAY`, `XAUTHORITY`, and session authentication; run without sudo. |
| Cannot connect to the Wayland compositor | Run inside the guest session and import its current environment; avoid sudo. |
| Required data-control protocol unavailable | Check compositor support for `ext-data-control` or `wlr-data-control`. |
| Service unit not found | Install the service file and run `systemctl --user daemon-reload`. |
| Starts, but clipboard does not sync | Check for competing stock/manual agents and inspect debug logs. |

For debug logs, use `systemctl --user edit paprika-vdagent.service`:

```ini
[Service]
Environment=RUST_LOG=paprika_vdagent=debug
```

Then restart the service. Other settings can be supplied in the same override, for example `Environment=PAPRIKA_VDAGENT_FILE_DIR=/home/your-user/Downloads`.

### Clipboard timeouts and recovery

Clipboard payload reads have a total deadline of 2000 ms, including sources that send partial data but never finish. In X11 this includes selection target negotiation and incremental transfer events. A timed-out read leaves the cached clipboard intact and schedules another attempt after the polling interval (`--poll-ms`, default 250 ms). A fresh selection event supersedes a stalled source's retry delay. The deadline applies to receiving the payload from the clipboard owner; Display connection setup and server roundtrips still depend on the display server responding.

To allow more time for a slow source, set `--clipboard-timeout-ms` when running manually, or add this to your service override:

```ini
[Service]
Environment=PAPRIKA_VDAGENT_CLIPBOARD_TIMEOUT_MS=5000
```

Restart the service after editing its override. Both the clipboard timeout and polling interval must be greater than zero.

If the event watcher disconnects or a Wayland data-control device finishes, the bridge falls back to polling. If the display server itself is gone or the SPICE transport fails, the bridge exits with an error and the supplied systemd service restarts it. Without systemd, your session supervisor must restart it. A restarted compositor may have a different socket or session environment; run `paprika-vdagent session-start` from the new session to import it.

A SPICE client disconnect cancels in-progress file transfers and resets capability negotiation. The visible clipboard stays available in the guest and is announced again after the next client negotiates its clipboard capabilities. Interrupted transfers are removed when cancelled or when the bridge exits normally, including error returns. Termination by a signal (including stopping the service), or a power failure, can leave `.paprika-vdagent-*.part` files; remove these only after stopping the bridge.

## Returning to the stock agent

Remove the Paprika autostart entry, then:

```sh
systemctl --user disable --now paprika-vdagent.service
sudo rm -f /etc/udev/rules.d/99-paprika-vdagent.rules
sudo udevadm control --reload-rules
sudo systemctl unmask spice-vdagentd.service spice-vdagentd.socket
```

Reboot to restore device permissions and your distro's normal session startup. If the stock daemon or socket was previously disabled, re-enable the appropriate unit according to your distro's setup.
