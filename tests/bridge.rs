#![cfg(target_os = "linux")]

mod support;

use std::fs;
use support::*;

fn roundtrip(selection: Selection) {
    let mut fixture = Fixture::new();
    fixture.negotiate();
    let guest = "guest clipboard: £ and café\nsecond line";
    fixture.guest_copy(selection, guest.as_bytes());
    fixture.expect_grab(selection);
    assert_eq!(fixture.request_text(selection), guest.as_bytes());
    fixture.quiet();

    let host = "host clipboard: 日本語\nsecond line";
    fixture.host_copy(selection, host.as_bytes());
    fixture.quiet(); // Injected host data must not become a new guest GRAB.
    fixture.host.send(RELEASE, &selection.prefix());
    fixture.wait_for_clipboard(selection, None);
    fixture.quiet(); // Clearing host-owned text must not echo RELEASE either.
}

#[test]
#[ignore = "requires Sway, wl-copy, wl-paste and permission to create private Wayland sockets"]
fn regular_clipboard_syncs_in_both_directions_without_echoes() {
    roundtrip(Selection::Clipboard);
}

#[test]
#[ignore = "requires headless Sway and wl-clipboard"]
fn primary_selection_syncs_in_both_directions_without_echoes() {
    roundtrip(Selection::Primary);
}

#[test]
#[ignore = "requires headless Sway and wl-clipboard"]
fn copying_previous_host_text_after_a_guest_edit_is_announced() {
    let mut fixture = Fixture::new();
    fixture.negotiate();
    fixture.host_copy(Selection::Clipboard, b"host text");
    fixture.quiet();
    for text in [b"guest edit".as_slice(), b"host text"] {
        fixture.guest_copy(Selection::Clipboard, text);
        fixture.expect_grab(Selection::Clipboard);
        assert_eq!(fixture.request_text(Selection::Clipboard), text);
    }
}

#[test]
#[ignore = "requires headless Sway and wl-clipboard"]
fn guest_clear_after_a_host_release_is_announced() {
    let mut fixture = Fixture::new();
    fixture.negotiate();
    fixture.host_copy(Selection::Clipboard, b"host text");
    fixture.quiet();
    fixture.host.send(RELEASE, &Selection::Clipboard.prefix());
    fixture.wait_for_clipboard(Selection::Clipboard, None);
    fixture.quiet();
    fixture.guest_copy(Selection::Clipboard, b"guest edit");
    fixture.expect_grab(Selection::Clipboard);
    fixture.guest_clear(Selection::Clipboard);
    assert_eq!(fixture.expect(RELEASE), Selection::Clipboard.prefix());
}

#[test]
#[ignore = "requires headless Sway and wl-clipboard"]
fn reconnect_announces_retained_text_to_the_next_spice_client() {
    let mut fixture = Fixture::new();
    fixture.negotiate();
    fixture.host_copy(Selection::Clipboard, b"retained host text");
    fixture.quiet();
    fixture.host.send(CLIENT_DISCONNECTED, &[]);
    fixture.wait_for_log("SPICE client disconnected");
    fixture.wait_for_clipboard(Selection::Clipboard, Some(b"retained host text"));
    fixture.negotiate();
    fixture.expect_grab(Selection::Clipboard);
    assert_eq!(
        fixture.request_text(Selection::Clipboard),
        b"retained host text"
    );
    fixture.quiet();
}

#[test]
#[ignore = "requires headless Sway and wl-clipboard"]
fn interrupted_file_transfer_can_restart_with_the_same_id() {
    let mut fixture = Fixture::new();
    fixture.negotiate();
    let bytes: Vec<u8> = (0..8192).map(|index| (index % 256) as u8).collect();
    fixture.start_transfer(7, "received.bin", bytes.len());
    fixture.transfer_data(7, &bytes[..10]);
    fixture.host.send(CLIENT_DISCONNECTED, &[]);
    fixture.wait_for_log("SPICE client disconnected");
    fixture.wait_for_no_parts();
    assert!(!fixture.downloads().join("received.bin").exists());
    fixture.negotiate();
    fixture.start_transfer(7, "received.bin", bytes.len());
    fixture.transfer_data(7, &bytes);
    fixture.expect_status(7, 3);
    assert_eq!(
        fs::read(fixture.downloads().join("received.bin")).unwrap(),
        bytes
    );
    fixture.wait_for_no_parts();
    fixture.quiet();
}

#[test]
#[ignore = "requires headless Sway and wl-clipboard"]
fn host_cancellation_removes_partial_downloads() {
    let mut fixture = Fixture::new();
    fixture.negotiate();
    fixture.start_transfer(11, "cancelled.txt", 10);
    fixture.transfer_data(11, b"partial");
    let mut cancellation = 11u32.to_le_bytes().to_vec();
    cancellation.extend(1u32.to_le_bytes());
    fixture.host.send(FILE_STATUS, &cancellation);
    fixture.wait_for_log("host cancelled file transfer id=11");
    fixture.wait_for_no_parts();
    assert!(!fixture.downloads().join("cancelled.txt").exists());
    fixture.start_transfer(11, "replacement.txt", 4);
    fixture.transfer_data(11, b"done");
    fixture.expect_status(11, 3);
    assert_eq!(
        fs::read(fixture.downloads().join("replacement.txt")).unwrap(),
        b"done"
    );
}

#[test]
#[ignore = "requires headless Sway and wl-clipboard"]
fn transport_failure_cleans_downloads_and_a_new_bridge_can_start() {
    let mut fixture = Fixture::new();
    fixture.negotiate();
    fixture.guest_copy(Selection::Clipboard, b"survives agent restart");
    fixture.expect_grab(Selection::Clipboard);
    fixture.start_transfer(21, "interrupted.txt", 10);
    fixture.transfer_data(21, b"partial");
    fixture.host.close();
    assert!(!fixture.bridge.wait().success());
    fixture.wait_for_no_parts();
    assert!(!fixture.downloads().join("interrupted.txt").exists());
    fixture.restart_bridge();
    fixture.negotiate();
    fixture.expect_grab(Selection::Clipboard);
    assert_eq!(
        fixture.request_text(Selection::Clipboard),
        b"survives agent restart"
    );
}

#[test]
#[ignore = "requires headless Sway and wl-clipboard"]
fn compositor_failure_exits_the_bridge_and_cleans_downloads() {
    let mut fixture = Fixture::new();
    fixture.negotiate();
    fixture.start_transfer(31, "interrupted.txt", 10);
    fixture.transfer_data(31, b"partial");
    fixture.compositor.stop();
    assert!(!fixture.bridge.wait().success());
    fixture.wait_for_no_parts();
    assert!(!fixture.downloads().join("interrupted.txt").exists());
}

#[test]
#[ignore = "requires headless Sway and wl-clipboard"]
fn large_text_crosses_spice_chunk_boundaries_in_both_directions() {
    let mut fixture = Fixture::new();
    fixture.negotiate();
    let text = "£ clipboard text\n".repeat(512);
    fixture.guest_copy(Selection::Clipboard, text.as_bytes());
    fixture.expect_grab(Selection::Clipboard);
    assert_eq!(fixture.request_text(Selection::Clipboard), text.as_bytes());
    let host_text = "host 日本語\n".repeat(512);
    fixture.host_copy(Selection::Clipboard, host_text.as_bytes());
    fixture.quiet();
}

#[test]
#[ignore = "requires headless Sway and wl-clipboard"]
fn regular_and_primary_selections_keep_independent_ownership() {
    let mut fixture = Fixture::new();
    fixture.negotiate();
    fixture.guest_copy(Selection::Clipboard, b"regular text");
    fixture.expect_grab(Selection::Clipboard);
    fixture.guest_copy(Selection::Primary, b"primary text");
    fixture.expect_grab(Selection::Primary);
    fixture.host_copy(Selection::Clipboard, b"host regular text");
    fixture.quiet();
    fixture.wait_for_clipboard(Selection::Primary, Some(b"primary text"));
    assert_eq!(fixture.request_text(Selection::Primary), b"primary text");
    fixture.host.send(RELEASE, &Selection::Clipboard.prefix());
    fixture.wait_for_clipboard(Selection::Clipboard, None);
    fixture.wait_for_clipboard(Selection::Primary, Some(b"primary text"));
    fixture.quiet();
}

// The integration fixture invokes just this test in an isolated subprocess to
// host the deliberately stalled source. Ordinary test runs do no work here.
#[test]
fn clipboard_source_helper() {
    if std::env::var_os("PAPRIKA_TEST_SOURCE").as_deref() == Some(std::ffi::OsStr::new("stall")) {
        support::stalled_source::run();
    }
}

#[test]
#[ignore = "requires headless Sway and wl-clipboard"]
fn stalled_owner_is_retried_without_releasing_the_cached_clipboard() {
    let mut fixture = Fixture::new();
    fixture.negotiate();
    fixture.guest_copy(Selection::Clipboard, b"cached text");
    fixture.expect_grab(Selection::Clipboard);
    fixture.stall_first_clipboard_read();
    fixture.wait_for_log("read timed out");
    // The next wire message must be a GRAB for the retry result, not a RELEASE
    // caused by treating a timeout as an empty clipboard.
    fixture.expect_grab(Selection::Clipboard);
    assert_eq!(
        fixture.request_text(Selection::Clipboard),
        b"recovered from a stalled owner"
    );
    fixture.quiet();
}

#[test]
#[ignore = "requires headless Sway and wl-clipboard"]
fn auto_detection_keeps_wayland_when_xwayland_display_is_also_set() {
    let mut fixture = Fixture::with_xwayland_hint(true);
    fixture.wait_for_log("selected wayland clipboard backend");
    fixture.negotiate();
    fixture.guest_copy(Selection::Clipboard, b"native Wayland selection");
    fixture.expect_grab(Selection::Clipboard);
    assert_eq!(
        fixture.request_text(Selection::Clipboard),
        b"native Wayland selection"
    );
    fixture.host_copy(Selection::Clipboard, b"host Wayland text");
    fixture.quiet();
}

#[test]
#[ignore = "requires headless Sway and wl-clipboard"]
fn session_start_captures_the_private_wayland_session_without_using_xwayland() {
    let fixture = Fixture::with_xwayland_hint(true);
    let config = fixture.session_start();
    assert!(config.contains("Environment=\"PAPRIKA_VDAGENT_BACKEND=wayland\""));
    assert!(config.contains(
        "UnsetEnvironment=WAYLAND_SOCKET DISPLAY XAUTHORITY HYPRLAND_INSTANCE_SIGNATURE"
    ));
    assert!(!config.contains("/old-x11-authority"));
}
