# Paprika vdagent

`paprika-vdagent` is a standalone Wayland clipboard bridge for SPICE/QEMU Linux guests.

It is designed for Wayland sessions where the traditional X11-focused `spice-vdagent` session agent is not enough, with Hyprland and wlroots compositors as the first target.

## ✨ What It Does

- syncs the regular clipboard between host and guest
- syncs primary selection when both sides support it
- works over the standard SPICE/QEMU virtio serial channel
- does not require host-side changes
- does not rely on X11 or XWayland clipboard mirroring

## ⚙️ How It Works

`paprika-vdagent` runs inside the guest Wayland session and directly owns `/dev/virtio-ports/com.redhat.spice.0`.

It speaks the SPICE guest clipboard protocol itself and bridges that to the Wayland clipboard using `ext-data-control` or `wlr-data-control`. In practice, this means it replaces the clipboard part of the usual `spice-vdagentd` plus `spice-vdagent` guest path.

More background is in [docs/architecture.md](./docs/architecture.md).

## 📋 Prerequisites

- a SPICE/QEMU Linux guest with `/dev/virtio-ports/com.redhat.spice.0`
- a Wayland compositor exposing `ext-data-control` or `wlr-data-control`
- a Wayland session where this process can access the compositor and the virtio port
- the stock `spice-vdagentd` and `spice-vdagent` processes stopped, so they do not compete for the same virtio channel

To stop the stock agents:

```bash
sudo systemctl stop spice-vdagentd.service spice-vdagentd.socket
pkill -x spice-vdagent || true
```

## 📦 Install

Use the release artifact that matches your architecture:

- `x86_64` or `arm64` tarball
- `.deb`
- `.rpm`

Install the package with your distro tools, or unpack the tarball and run `paprika-vdagent` from your Wayland session.

The packaged installs include a systemd user service at `/usr/lib/systemd/user/paprika-vdagent.service`. It is not enabled automatically.

After installing a package, enable and start it with:

```bash
systemctl --user import-environment WAYLAND_DISPLAY XDG_RUNTIME_DIR HYPRLAND_INSTANCE_SIGNATURE
systemctl --user daemon-reload
systemctl --user enable --now paprika-vdagent.service
```

For tarball installs, run the binary manually or install the service file yourself from `packaging/systemd/user/paprika-vdagent.service`.

For pacman-based systems, the repo also includes a local package recipe in `packaging/pacman/`.

## 🛠️ Build

Install the Rust toolchain and the Wayland development packages for your distro, then build:

```bash
cargo build --release
```

The binary will be at:

```text
target/release/paprika-vdagent
```

Run it from the guest Wayland session:

```bash
RUST_LOG=paprika_vdagent=debug ./target/release/paprika-vdagent
```

If needed, you can pin a specific seat:

```bash
RUST_LOG=paprika_vdagent=debug ./target/release/paprika-vdagent --seat seat0
```

## ⚠️ Current Limitations

- clipboard sync only
- text only
- first target is Hyprland and wlroots-based compositors
- primary selection depends on compositor support and host viewer support
- secondary selection is not implemented
- image, HTML, URI list, and file transfer are not implemented
- if event-driven watching is unavailable, the bridge falls back to polling

## 🪪 License

`spice-vdagent` is released under the [GPL-3.0 LICENSE](./LICENSE)