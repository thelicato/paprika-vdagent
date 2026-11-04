# Testing

## Standard checks

Run these from the repository root with the Rust toolchain and build dependencies installed:

```sh
cargo test --locked
cargo fmt --check
cargo clippy --locked --all-targets
```

The standard suite covers protocol parsing and framing, transfer cancellation and cleanup, clipboard payload deadlines, cached clipboard state, startup commands, and diagnostics. Some tests create private Unix sockets and may need to run outside a restrictive sandbox. They do not require a desktop session or access to a real SPICE device.

## Full-bridge integration tests

Install Sway and `wl-clipboard` using your distribution's package manager. The suite needs `sway`, `wl-copy`, and `wl-paste` on `PATH`. For example, on Arch Linux or CachyOS:

```sh
sudo pacman -S --needed sway wl-clipboard
```

Run the tests as an ordinary user on Linux:

```sh
cargo test --locked --test bridge -- --ignored
```

These tests are explicitly ignored by the standard test command because they require external compositor tools and permission to create pseudoterminals and Unix sockets. An explicit integration run fails if dependencies are unavailable; it does not silently skip scenarios. A graphical session, GPU, running VM, and elevated privileges are unnecessary. Sway uses its [headless backend and software renderer](https://gitlab.freedesktop.org/wlroots/wlroots/-/blob/master/docs/env_vars.md).

Each scenario starts a real headless Sway instance and the built Paprika binary. A raw pseudoterminal stands in for the virtio serial channel, while an independent SPICE peer sends and checks protocol frames. `wl-copy` and `wl-paste` exercise the compositor's actual clipboard. A separate data-control client deliberately stalls its first payload transfer to test retry behaviour through the whole bridge.

The scenarios cover:

- Regular clipboard and primary selection in both directions, including UTF-8 and multiline text.
- Independent clipboard and primary-selection ownership.
- Clipboard payloads spanning multiple SPICE chunks.
- Host clipboard injection and release without reflected guest messages.
- Guest edits that reuse earlier host text, and guest clears after a host release.
- A stalled clipboard source, cache preservation, and a successful retry.
- SPICE client reconnection and reannouncing retained clipboard text.
- Interrupted file transfers, transfer ID reuse, host cancellation, and byte-for-byte file contents.
- SPICE transport failure, bridge restart, compositor failure, and partial-file cleanup.

Each fixture has a private runtime directory, download directory, compositor configuration, and Wayland socket. Child processes receive an explicitly constructed environment that excludes the invoking session's display, compositor IPC socket, D-Bus address, configuration, and Paprika options. The tests do not use the real SPICE device or call systemctl. Fixtures stop and reap their children and remove temporary files after success or failure. Failed assertions print the fixture's logs.

Readiness checks and protocol operations have deadlines instead of fixed startup delays. A short quiet interval checks for unwanted clipboard echoes. Tests can run in parallel; to make failures easier to follow, run them sequentially:

```sh
cargo test --locked --test bridge -- --ignored --test-threads=1
```

To use a Sway binary outside `PATH`, set `PAPRIKA_TEST_SWAY` to its path. If it needs extra library paths, supply a wrapper executable that sets them, since fixture children do not inherit arbitrary environment variables:

```sh
PAPRIKA_TEST_SWAY=/path/to/sway cargo test --locked --test bridge -- --ignored
```

The `clipboard_source_helper` test is an internal subprocess entry point. It does nothing during a normal test run and serves the deliberately stalled source only when invoked by an integration fixture.

These tests exercise local bridge behaviour with the protocols advertised by the installed Sway version. They do not replace testing against a real SPICE viewer, virtio driver, or other compositors. Systemd restart policies and compositor-specific session startup should also be verified in a guest VM.
