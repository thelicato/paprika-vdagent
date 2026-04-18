# Paprika vdagent v1 architecture

## Decision

For v1, `paprika-vdagent` should **directly own** `/dev/virtio-ports/com.redhat.spice.0`.

That means it replaces the clipboard-related guest-side SPICE agent path instead of coexisting with `spice-vdagentd`.

## Why `spice-vdagentd` is not a clean dependency

Upstream Linux SPICE guest agent architecture is explicitly split into:

- `spice-vdagentd`: a system daemon
- `spice-vdagent`: an X11 session agent

The Arch manual pages still describe the Linux guest agent that way:

- `spice-vdagent` is the "X11 session agent"
- `spice-vdagentd` is the system daemon

Upstream daemon-side code also shows that:

- `spice-vdagentd` owns `/dev/virtio-ports/com.redhat.spice.0`
- clipboard messages are forwarded over a private Unix socket protocol to the active session agent
- clipboard is ignored if there is no active session connection
- the daemon's virtio-port lifetime is tied to session/Xorg-resolution state

That last point matters a lot for Wayland. The upstream daemon has an internal `check_xorg_resolution()` path that opens the virtio channel only when the active session agent has reported Xorg-style resolution state. Reusing the daemon from a Wayland-only clipboard bridge would therefore require:

- speaking the daemon's private UDSCS / `vdagentd-proto.h` protocol
- participating in its active-session model
- satisfying X11/Xorg-era assumptions that are outside clipboard itself

Technically, a custom Wayland agent could talk to `/run/spice-vdagentd/spice-vdagent-sock`, but that would not be a clean or robust standalone design.

## Compatibility classification

- `spice-vdagent`: incompatible with this tool's goals and not required
- `spice-vdagentd`: not a clean reusable dependency for Wayland clipboard v1
- Direct virtio ownership: the recommended architecture for the standalone tool

## Clipboard ownership flow

### Guest Wayland -> host

1. The bridge watches the Wayland clipboard selections using `ext-data-control` or `wlr-data-control` selection events.
2. When text changes, the bridge caches the UTF-8 text locally.
3. It sends `VD_AGENT_CLIPBOARD_GRAB` for `VD_AGENT_CLIPBOARD_UTF8_TEXT` for the matching selection.
4. When the host later sends `VD_AGENT_CLIPBOARD_REQUEST`, the bridge replies with `VD_AGENT_CLIPBOARD`.
5. If the guest clipboard becomes empty, the bridge sends `VD_AGENT_CLIPBOARD_RELEASE`.

### Host -> guest Wayland

1. The host side sends `VD_AGENT_CLIPBOARD_GRAB`.
2. The bridge requests the text payload with `VD_AGENT_CLIPBOARD_REQUEST`.
3. After `VD_AGENT_CLIPBOARD` arrives, the bridge writes that text into the matching Wayland selection using data-control.
4. The bridge suppresses the immediate self-induced clipboard echo so the injected host clipboard is not reflected straight back to the host as a new guest grab.
5. If the host sends `VD_AGENT_CLIPBOARD_RELEASE` and the bridge is still serving the host-owned text locally, the bridge clears the matching Wayland selection.

## Wayland backend

The prototype uses `wl-clipboard-rs` as the Wayland backend.

That crate uses:

- `ext-data-control` when available
- `wlr-data-control` otherwise

For the first target, Hyprland/wlroots, `wlr-data-control` compatibility is the important path.

Clipboard read/write still uses `wl-clipboard-rs`, but guest clipboard change detection now prefers a direct watcher thread using `ext-data-control` when available and `wlr-data-control` otherwise. If that watcher cannot be started, the bridge falls back to polling.

Regular clipboard and primary selection are both supported in the current implementation. Secondary selection is still intentionally unsupported.

## SPICE protocol subset implemented in v1

- `VD_AGENT_ANNOUNCE_CAPABILITIES`
- `VD_AGENT_CLIPBOARD_GRAB`
- `VD_AGENT_CLIPBOARD_REQUEST`
- `VD_AGENT_CLIPBOARD`
- `VD_AGENT_CLIPBOARD_RELEASE`

Capabilities used:

- `VD_AGENT_CAP_CLIPBOARD_BY_DEMAND`
- `VD_AGENT_CAP_CLIPBOARD_SELECTION`
- `VD_AGENT_CAP_CLIPBOARD_NO_RELEASE_ON_REGRAB`
- `VD_AGENT_CAP_CLIPBOARD_GRAB_SERIAL`

Currently implemented selections:

- regular clipboard
- primary selection

## Known limitations

- Direct-virtio mode means this prototype must not run at the same time as `spice-vdagentd`
- Clipboard only
- Text only
- Secondary selection is not implemented
- Primary selection depends on compositor support and host viewer/client support
- Event-driven watch support currently depends on `ext-data-control` or `wlr-data-control`
- Environments without both `ext-data-control` and `wlr-data-control` fall back to polling

## References

- SPICE agent protocol: https://www.spice-space.org/agent-protocol.html
- QEMU UI clipboard docs: https://www.qemu.org/docs/master/devel/ui.html
- QEMU D-Bus clipboard docs: https://www.qemu.org/docs/master/interop/dbus-display
- Arch `spice-vdagent(1)`: https://man.archlinux.org/man/spice-vdagent.1.en
- Arch `spice-vdagentd(1)`: https://man.archlinux.org/man/spice-vdagentd.1.en
- Upstream `spice/linux/vd_agent`: https://gitlab.freedesktop.org/spice/linux/vd_agent
