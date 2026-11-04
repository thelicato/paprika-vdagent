# Paprika agent

`paprika-vdagent` is a standalone X11 and Wayland clipboard and file-transfer bridge for SPICE/QEMU Linux guests.

It supports native Wayland clipboard access on Hyprland and other compatible compositors, plus native X11 sessions. Wayland is the default backend; select `--backend x11` in an X11 session.

## ✨ What It Does

- syncs the regular clipboard between host and guest
- syncs primary selection when both sides support it
- receives client-to-guest file transfers into the guest
- works over the standard SPICE/QEMU virtio serial channel
- does not require host-side changes
- uses native Wayland clipboard access without relying on XWayland mirroring
- supports native X11 clipboard ownership and primary selection

## ⚙️ How It Works

`paprika-vdagent` runs inside the guest graphical session and directly owns `/dev/virtio-ports/com.redhat.spice.0`.

It speaks the SPICE guest clipboard and file-transfer protocol itself. Wayland clipboard sync uses `ext-data-control` or `wlr-data-control`; the X11 backend serves native X selections and watches ownership changes with XFixes, falling back to polling when necessary. Incoming files are saved into `~/Downloads` by default or a custom directory set with `--file-dir`.

In practice, this means it replaces the clipboard and file-transfer part of the usual `spice-vdagentd` plus `spice-vdagent` guest path.

More background is in [docs/architecture.md](./docs/architecture.md).

## 📋 Prerequisites

- a SPICE/QEMU Linux guest with `/dev/virtio-ports/com.redhat.spice.0`
- a Wayland compositor exposing `ext-data-control` or `wlr-data-control`, or a native X11 session
- access to the session display and the virtio port
- a writable guest download directory such as `~/Downloads`, or a custom path passed with `--file-dir`
- the stock `spice-vdagentd` and `spice-vdagent` processes stopped, so they do not compete for the same virtio channel

To stop the stock agents:

```bash
sudo systemctl stop spice-vdagentd.service spice-vdagentd.socket
pkill -x spice-vdagent || true
```

## 📝 Usage

```
Standalone X11 and Wayland SPICE clipboard and file-transfer bridge for Linux guests

Usage: paprika-vdagent [OPTIONS] [COMMAND]

Commands:
  doctor         Check session readiness without opening the SPICE port or reading the clipboard
  session-start  Import the current Wayland environment and restart the systemd user service
  help           Print this message or the help of the given subcommand(s)

Options:
      --backend <BACKEND>
          Session clipboard backend [env: PAPRIKA_VDAGENT_BACKEND=] [default: wayland] [possible values: wayland, x11]
      --virtio-port <VIRTIO_PORT>
          [env: PAPRIKA_VDAGENT_PORT=] [default: /dev/virtio-ports/com.redhat.spice.0]
      --poll-ms <POLL_MS>
          [env: PAPRIKA_VDAGENT_POLL_MS=] [default: 250]
      --poll-only
          Poll clipboard changes instead of using native change notifications [env: PAPRIKA_VDAGENT_POLL_ONLY=]
      --max-text-bytes <MAX_TEXT_BYTES>
          [env: PAPRIKA_VDAGENT_MAX_TEXT_BYTES=] [default: 1048576]
      --clipboard-timeout-ms <CLIPBOARD_TIMEOUT_MS>
          Maximum time to wait for a clipboard source to finish sending its payload [env: PAPRIKA_VDAGENT_CLIPBOARD_TIMEOUT_MS=] [default: 2000]
      --seat <SEAT>
          [env: PAPRIKA_VDAGENT_SEAT=]
      --file-dir <FILE_DIR>
          [env: PAPRIKA_VDAGENT_FILE_DIR=]
      --max-active-file-transfers <MAX_ACTIVE_FILE_TRANSFERS>
          [env: PAPRIKA_VDAGENT_MAX_ACTIVE_FILE_TRANSFERS=] [default: 8]
  -h, --help
          Print help
  -V, --version
          Print version
```

## 📦 Install

Use the release artifact that matches your architecture:

- `x86_64` or `arm64` tarball
- `x86_64` pacman package
- `.deb`
- `.rpm`

Install the package with your distro tools, or unpack the tarball and run `paprika-vdagent` from your graphical session.

The packaged installs include a systemd user service at `/usr/lib/systemd/user/paprika-vdagent.service`. It is not enabled automatically.

For persistent setup, follow **[Installation and session startup](docs/setup.md)**. The guide covers package installation on common distributions, disabling competing stock agents, persistent port permissions, Hyprland Lua and legacy configuration, Sway, other session managers, and troubleshooting.

Start Paprika from your graphical session after login so it receives the current display environment. Enabling the user service alone does not configure port permissions or guarantee that environment is available.

For pacman-based systems, the repo also includes a local package recipe in `packaging/pacman/`.

To check your setup without starting the bridge, run `paprika-vdagent doctor` for Wayland or `paprika-vdagent --backend x11 doctor` for X11. It reports problems with actionable fixes and returns a non-zero exit status when required checks fail. See [diagnostics](docs/setup.md#5-verify-and-troubleshoot) for details.

## 🛠️ Build

Install the Rust toolchain and the Wayland development packages for your distro, then build:

```bash
cargo build --locked --release
```

The binary will be at:

```text
target/release/paprika-vdagent
```

Run it from the guest Wayland session:

```bash
RUST_LOG=paprika_vdagent=debug ./target/release/paprika-vdagent
```

For a native X11 session:

```bash
RUST_LOG=paprika_vdagent=debug ./target/release/paprika-vdagent --backend x11
```

The X11 backend uses `DISPLAY` and the usual X authentication settings, including `XAUTHORITY`. It does not require `xsel` or `xclip` at runtime. The `session-start` helper configures Wayland startup; use the [X11 startup instructions](docs/setup.md#native-x11-sessions) for X11.

If you want incoming files somewhere else:

```bash
RUST_LOG=paprika_vdagent=debug ./target/release/paprika-vdagent --file-dir /path/to/save/files
```

If needed, you can pin a specific seat:

```bash
RUST_LOG=paprika_vdagent=debug ./target/release/paprika-vdagent --seat seat0
```

## Testing

Run `cargo test --locked` for the standard suite. Full-bridge integration tests use private headless Sway or authenticated Xvfb instances and a simulated SPICE connection. See [Testing](docs/testing.md) for dependencies, commands, coverage, and limitations.

## 🪪 License

`paprika-vdagent` is released under the [GPL-3.0 LICENSE](./LICENSE)
