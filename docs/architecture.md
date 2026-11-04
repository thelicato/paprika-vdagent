# Paprika vdagent v1 architecture

## Decision

For v1, `paprika-vdagent` should **directly own** `/dev/virtio-ports/com.redhat.spice.0`.

That means it replaces the clipboard and file-transfer guest-side SPICE agent path instead of coexisting with `spice-vdagentd`.

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
- `spice-vdagentd`: not a clean reusable dependency for Wayland clipboard / file-transfer v1
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
4. The bridge suppresses the immediate self-induced clipboard echo so the injected host clipboard is not reflected straight back to the host as a new guest grab. Suppression is consumed by the next successful snapshot, including an unchanged snapshot, so later guest edits and clears are announced normally.
5. If the host sends `VD_AGENT_CLIPBOARD_RELEASE` and the bridge is still serving the host-owned text locally, the bridge clears the matching Wayland selection.

## File transfer flow

### Host client -> guest

1. The SPICE client sends `VD_AGENT_FILE_XFER_START` with keyfile-style metadata describing the file name and size.
2. The bridge creates a temporary file in the guest save directory, which defaults to `~/Downloads` and can be overridden with `--file-dir`.
3. If that succeeds, the bridge replies with `VD_AGENT_FILE_XFER_STATUS_CAN_SEND_DATA`.
4. The host streams file contents with `VD_AGENT_FILE_XFER_DATA`.
5. The bridge writes each chunk to disk and tracks the declared size.
6. When the expected byte count has been received, the bridge renames the temporary file into place and replies with `VD_AGENT_FILE_XFER_STATUS_SUCCESS`.
7. If the host cancels the transfer or a local write fails, the bridge removes the temporary file and reports an error or cancellation back to the host.

## Clipboard backend interface

The SPICE bridge uses the `Clipboard` interface for reading, writing, clearing, selection capabilities, and starting a change watcher. Backends report `ClipboardEvent` notifications and return `ClipboardRead::Ready` for completed reads or `ClipboardRead::Retry` when the cached selection should be retained. The bridge owns clipboard caches, echo suppression, retry scheduling, SPICE negotiation, and file transfers independently of the display connection.

`WaylandClipboard` and `X11Clipboard` implement this interface. The default `auto` backend selects the native graphical session, with explicit `wayland` and `x11` overrides. The Wayland backend keeps the existing data-control protocols, selection support, payload deadline, and watcher fallback. Adding another backend does not require changing the clipboard ownership flow or SPICE transport.

## Backend detection and session startup

The bridge, diagnostics, and startup helper share one backend resolver. Explicit configuration wins over session hints. Automatic detection uses `XDG_SESSION_TYPE` first, then prefers Wayland display hints over `DISPLAY`. This protects native Wayland sessions from being mistaken for X11 when XWayland is available, while an explicit X11 session type overrides stale Wayland variables. Detection does not retry another backend after a connection or protocol failure.

`session-start` validates the selected display and runtime directory before writing a private runtime drop-in for the user service. It supplies service-specific display and authentication settings through `Environment=` and removes stale settings through `UnsetEnvironment=`. It does not import or unset variables in the user manager's global environment. Systemd quoting and specifier escaping preserve literal values; relative X11 authentication paths are made absolute. Replacing the drop-in, reloading the manager, and restarting the service applies each new session to the same service without changing the clipboard backend implementations.

## Wayland backend

The prototype uses `wl-clipboard-rs` as the Wayland backend.

That crate uses:

- `ext-data-control` when available
- `wlr-data-control` otherwise

For the first target, Hyprland/wlroots, `wlr-data-control` compatibility is the important path.

Clipboard read/write still uses `wl-clipboard-rs`, but guest clipboard change detection now prefers a direct watcher thread using `ext-data-control` when available and `wlr-data-control` otherwise. If that watcher cannot be started, the bridge falls back to polling.

Payload reads use a non-blocking pipe and a total deadline, configured by `--clipboard-timeout-ms` (default 2000 ms). Timeouts preserve the cached clipboard and schedule a retry. Clipboard change events are coalesced before reading each selection. A finished data-control device or watcher disconnect switches change detection to polling; a lost compositor or SPICE transport causes an error exit so the service can restart the bridge. Wayland protocol roundtrips are not covered by the payload deadline.

On SPICE client disconnect, transport capabilities and serial state are reset, in-progress file transfers are cancelled, and the visible clipboard is retained as guest-owned data for the next client's capability negotiation. Temporary downloads are removed on cancellation, transfer errors, and normal Rust resource cleanup, including an error exit. Termination by a signal and power loss can leave temporary files.

Regular clipboard and primary selection are both supported in the current implementation. Secondary selection is still intentionally unsupported.

## X11 backend

The native X11 backend uses `x11rb` with separate display connections for ownership serving and selection reads. An unmapped owner window and a background service keep host-provided text available while the bridge reads selections, handles SPICE traffic, or receives files. Commands wake the service through a pipe; XFixes selection notifications feed the same clipboard event channel as Wayland. If XFixes is unavailable, the bridge polls while the owner service continues answering requests. `--poll-only` explicitly selects polling on either backend.

Both `CLIPBOARD` and `PRIMARY` are supported. The backend negotiates text targets, prefers UTF-8, and converts legacy Latin-1 `STRING` data to UTF-8. It serves `TARGETS`, `TIMESTAMP`, and `MULTIPLE`, alongside `UTF8_STRING`, `TEXT`, `STRING` where representable, and plain-text MIME targets. It does not decode `COMPOUND_TEXT`.

Selection ownership uses server timestamps. Clearing checks the current owner and uses the acquisition timestamp so a later owner's clipboard is not released. Incremental transfers retain a snapshot of the requested data, allowing an existing transfer to finish after ownership changes.

Incoming reads use a dedicated request window and a total selection-transfer deadline. Byte limits apply to direct and incremental transfers, including UTF-8 expansion after Latin-1 conversion. Oversized incremental data is discarded while completing the handshake within the remaining deadline; it is not accumulated. Timeouts, invalid text, and exceeded limits preserve the bridge's cache and schedule retries. Closing the read connection removes its request window and temporary properties.

Outgoing incremental transfers use chunks of at most 64 KiB, respect the server's request-size limit, and have a total deadline. At most 32 incremental transfers may be active. A vanished requestor or expired deadline releases its transfer state and property-event subscription. Display-server connection setup and synchronous replies are not covered by the selection payload deadline.

The implementation follows the [ICCCM selection conventions](https://xorg.freedesktop.org/releases/current/doc/xorg-docs/icccm/icccm.html) and uses [x11rb](https://github.com/psychon/x11rb) for X11 and XFixes protocol access.

## SPICE protocol subset implemented in v1

- `VD_AGENT_ANNOUNCE_CAPABILITIES`
- `VD_AGENT_CLIPBOARD_GRAB`
- `VD_AGENT_CLIPBOARD_REQUEST`
- `VD_AGENT_CLIPBOARD`
- `VD_AGENT_CLIPBOARD_RELEASE`
- `VD_AGENT_FILE_XFER_START`
- `VD_AGENT_FILE_XFER_STATUS`
- `VD_AGENT_FILE_XFER_DATA`

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
- Text only
- Secondary selection is not implemented
- Primary selection depends on compositor support and host viewer/client support
- File transfer currently covers the standard client-to-guest path only
- Detailed SPICE file-transfer error payloads are not implemented
- Event-driven watch support currently depends on `ext-data-control` or `wlr-data-control`
- Polling fallback still requires `ext-data-control` or `wlr-data-control` for clipboard access

## References

- SPICE agent protocol: https://www.spice-space.org/agent-protocol.html
- QEMU UI clipboard docs: https://www.qemu.org/docs/master/devel/ui.html
- QEMU D-Bus clipboard docs: https://www.qemu.org/docs/master/interop/dbus-display
- Arch `spice-vdagent(1)`: https://man.archlinux.org/man/spice-vdagent.1.en
- Arch `spice-vdagentd(1)`: https://man.archlinux.org/man/spice-vdagentd.1.en
- Upstream `spice/linux/vd_agent`: https://gitlab.freedesktop.org/spice/linux/vd_agent
