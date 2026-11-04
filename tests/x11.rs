#![cfg(target_os = "linux")]

#[path = "support/common.rs"]
mod common;
#[path = "support/x11.rs"]
mod fixture;

use common::*;
use fixture::*;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{CURRENT_TIME, NONE};

fn roundtrip(selection: Selection) {
    let mut fixture = Fixture::new();
    fixture.negotiate();
    let text = "guest £ and 日本語\nsecond line";
    fixture.guest_copy(selection, text.as_bytes());
    fixture.expect_grab(selection);
    assert_eq!(fixture.request_text(selection), text.as_bytes());
    fixture.host_copy(selection, b"host text");
    fixture.quiet();
    fixture.host.send(RELEASE, &selection.prefix());
    fixture.wait_for_clipboard(selection, b"");
    fixture.quiet();
    fixture.guest_copy(selection, b"guest edit");
    fixture.expect_grab(selection);
    fixture.guest_copy(selection, b"host text");
    fixture.expect_grab(selection);
    fixture.guest_clear(selection);
    assert_eq!(fixture.expect(RELEASE), selection.prefix());
}

#[test]
#[ignore = "requires Xvfb and xsel"]
fn clipboard_syncs_both_directions_and_preserves_ownership_changes() {
    roundtrip(Selection::Clipboard);
}

#[test]
#[ignore = "requires Xvfb and xsel"]
fn primary_selection_syncs_both_directions() {
    roundtrip(Selection::Primary);
}

#[test]
#[ignore = "requires Xvfb and xsel"]
fn large_clipboard_transfers_use_incr_in_both_directions() {
    let mut fixture = Fixture::new();
    fixture.negotiate();
    let text = "large £ clipboard\n".repeat(16384);
    fixture.guest_copy(Selection::Clipboard, text.as_bytes());
    fixture.expect_grab(Selection::Clipboard);
    assert_eq!(fixture.request_text(Selection::Clipboard), text.as_bytes());
    fixture.host_copy(Selection::Clipboard, text.as_bytes());
    fixture.quiet();
}

#[test]
#[ignore = "requires Xvfb and xsel"]
fn polling_mode_reads_and_serves_clipboard_selections() {
    let mut fixture = Fixture::with_options(false, 1024 * 1024, 2000);
    fixture.wait_for_log("using clipboard polling every");
    fixture.negotiate();
    fixture.guest_copy(Selection::Clipboard, b"polled guest text");
    fixture.expect_grab(Selection::Clipboard);
    assert_eq!(
        fixture.request_text(Selection::Clipboard),
        b"polled guest text"
    );
    fixture.host_copy(Selection::Clipboard, b"polled host text");
    fixture.quiet();
}

#[test]
#[ignore = "requires Xvfb and xsel"]
fn selections_remain_independent_and_client_reconnect_retains_text() {
    let mut fixture = Fixture::new();
    fixture.negotiate();
    fixture.guest_copy(Selection::Primary, b"primary text");
    fixture.expect_grab(Selection::Primary);
    fixture.host_copy(Selection::Clipboard, b"host text");
    fixture.quiet();
    fixture.wait_for_clipboard(Selection::Primary, b"primary text");
    fixture.host.send(CLIENT_DISCONNECTED, &[]);
    fixture.wait_for_log("SPICE client disconnected");
    fixture.negotiate();
    fixture.expect_grab(Selection::Clipboard);
    fixture.expect_grab(Selection::Primary);
    assert_eq!(fixture.request_text(Selection::Clipboard), b"host text");
    assert_eq!(fixture.request_text(Selection::Primary), b"primary text");
}

#[test]
#[ignore = "requires Xvfb and xsel"]
fn targets_timestamp_and_multiple_requests_are_served() {
    let mut fixture = Fixture::new();
    fixture.negotiate();
    fixture.host_copy(Selection::Clipboard, b"served text");
    let (connection, window) = fixture.client();
    let clipboard = atom(&connection, b"CLIPBOARD");
    let targets = atom(&connection, b"TARGETS");
    let timestamp = atom(&connection, b"TIMESTAMP");
    let multiple = atom(&connection, b"MULTIPLE");
    let utf8 = atom(&connection, b"UTF8_STRING");
    let property = atom(&connection, b"TEST_PROPERTY");
    let offered = selection_property(&connection, window, clipboard, targets, property);
    let offered: Vec<_> = offered.value32().unwrap().collect();
    for target in [targets, timestamp, multiple, utf8] {
        assert!(offered.contains(&target));
    }
    let time = selection_property(&connection, window, clipboard, timestamp, property);
    assert_eq!(time.format, 32);
    assert_ne!(time.value32().unwrap().next(), Some(0));
    let text_property = atom(&connection, b"TEST_TEXT");
    let bad_property = atom(&connection, b"TEST_BAD");
    let bad_target = atom(&connection, b"UNSUPPORTED_TARGET");
    let atom_pair = atom(&connection, b"ATOM_PAIR");
    connection
        .change_property32(
            PropMode::REPLACE,
            window,
            property,
            atom_pair,
            &[utf8, text_property, bad_target, bad_property],
        )
        .unwrap()
        .check()
        .unwrap();
    let converted = selection_property(&connection, window, clipboard, multiple, property);
    assert_eq!(
        converted.value32().unwrap().collect::<Vec<_>>(),
        [utf8, text_property, NONE, bad_property]
    );
    assert_eq!(
        connection
            .get_property(true, window, text_property, AtomEnum::ANY, 0, 1024)
            .unwrap()
            .reply()
            .unwrap()
            .value,
        b"served text"
    );
    fixture.quiet();
}

#[test]
#[ignore = "requires Xvfb and xsel"]
fn requestor_disappearing_during_incr_does_not_break_the_owner() {
    let mut fixture = Fixture::new();
    fixture.negotiate();
    let text = "large text ".repeat(10000);
    fixture.host_copy(Selection::Clipboard, text.as_bytes());
    let (connection, window) = fixture.client();
    let property = selection_property(
        &connection,
        window,
        atom(&connection, b"CLIPBOARD"),
        atom(&connection, b"UTF8_STRING"),
        atom(&connection, b"TEST_TRANSFER"),
    );
    assert_eq!(property.type_, atom(&connection, b"INCR"));
    drop(connection); // Leave the INCR header unacknowledged and destroy the window.
    fixture.wait_for_clipboard(Selection::Clipboard, text.as_bytes());
    fixture.host_copy(Selection::Clipboard, b"still serving");
    fixture.quiet();
}

#[test]
#[ignore = "requires Xvfb and xsel"]
fn server_disconnect_cleans_partial_downloads() {
    let mut fixture = Fixture::new();
    fixture.negotiate();
    fixture.start_transfer(9);
    fixture.partial_transfer(9);
    fixture.server.stop();
    assert!(!fixture.bridge.wait().success());
    fixture.assert_no_partial_files();
}

#[test]
#[ignore = "requires Xvfb and xsel"]
fn transport_disconnect_cleans_partial_downloads() {
    let mut fixture = Fixture::new();
    fixture.negotiate();
    fixture.start_transfer(9);
    fixture.partial_transfer(9);
    fixture.host.close();
    assert!(!fixture.bridge.wait().success());
    fixture.assert_no_partial_files();
}

#[test]
#[ignore = "requires Xvfb and xsel"]
fn doctor_checks_x11_without_requiring_wayland() {
    let fixture = Fixture::new();
    let output = fixture.doctor();
    assert!(output.contains("[OK] Connected to the X11 server"));
    assert!(output.contains("[OK] XFixes selection notifications"));
    assert!(!output.contains("Wayland connection failed"));
}

#[derive(Clone, Copy)]
enum SourceMode {
    Stall,
    StallIncr,
    Latin1,
}

struct Source {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Source {
    fn new(fixture: &Fixture, mode: SourceMode) -> Self {
        let (connection, window) = fixture.client();
        let clipboard = atom(&connection, b"CLIPBOARD");
        let targets = atom(&connection, b"TARGETS");
        let utf8 = atom(&connection, b"UTF8_STRING");
        let incr = atom(&connection, b"INCR");
        connection
            .set_selection_owner(window, clipboard, CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let finished = stop.clone();
        let worker = std::thread::spawn(move || {
            let mut requests = 0;
            while !finished.load(std::sync::atomic::Ordering::Relaxed) {
                let event = match connection.poll_for_event() {
                    Ok(Some(event)) => event,
                    Ok(None) => {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                        continue;
                    }
                    Err(_) => break,
                };
                let x11rb::protocol::Event::SelectionRequest(request) = event else {
                    continue;
                };
                let property = if request.property == NONE {
                    request.target
                } else {
                    request.property
                };
                if request.target == targets {
                    let target = if matches!(mode, SourceMode::Latin1) {
                        AtomEnum::STRING.into()
                    } else {
                        utf8
                    };
                    connection
                        .change_property32(
                            PropMode::REPLACE,
                            request.requestor,
                            property,
                            AtomEnum::ATOM,
                            &[target],
                        )
                        .unwrap()
                        .check()
                        .unwrap();
                } else {
                    requests += 1;
                    if requests == 1 && matches!(mode, SourceMode::Stall) {
                        continue;
                    }
                    if requests == 1 && matches!(mode, SourceMode::StallIncr) {
                        connection
                            .change_property32(
                                PropMode::REPLACE,
                                request.requestor,
                                property,
                                incr,
                                &[0],
                            )
                            .unwrap()
                            .check()
                            .unwrap();
                    } else {
                        let (kind, data): (Atom, &[u8]) = if matches!(mode, SourceMode::Latin1) {
                            (AtomEnum::STRING.into(), b"caf\xe9")
                        } else {
                            (utf8, b"recovered X11 text")
                        };
                        connection
                            .change_property8(
                                PropMode::REPLACE,
                                request.requestor,
                                property,
                                kind,
                                data,
                            )
                            .unwrap()
                            .check()
                            .unwrap();
                    }
                }
                connection
                    .send_event(
                        false,
                        request.requestor,
                        EventMask::NO_EVENT,
                        SelectionNotifyEvent {
                            response_type: SELECTION_NOTIFY_EVENT,
                            sequence: 0,
                            time: request.time,
                            requestor: request.requestor,
                            selection: request.selection,
                            target: request.target,
                            property,
                        },
                    )
                    .unwrap()
                    .check()
                    .unwrap();
            }
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn stalled_owner(mode: SourceMode) {
    let mut fixture = Fixture::with_options(true, 1024 * 1024, 100);
    fixture.negotiate();
    fixture.guest_copy(Selection::Clipboard, b"cached text");
    fixture.expect_grab(Selection::Clipboard);
    let _source = Source::new(&fixture, mode);
    fixture.wait_for_log("X11 selection transfer timed out");
    fixture.expect_grab(Selection::Clipboard); // No spurious RELEASE for the timeout.
    assert_eq!(
        fixture.request_text(Selection::Clipboard),
        b"recovered X11 text"
    );
    fixture.quiet();
}

#[test]
#[ignore = "requires Xvfb and xsel"]
fn stalled_selection_owner_is_retried_without_losing_the_cache() {
    stalled_owner(SourceMode::Stall);
}

#[test]
#[ignore = "requires Xvfb and xsel"]
fn stalled_incr_transfer_is_retried_without_losing_the_cache() {
    stalled_owner(SourceMode::StallIncr);
}

#[test]
#[ignore = "requires Xvfb and xsel"]
fn legacy_latin1_text_is_converted_to_utf8() {
    let mut fixture = Fixture::new();
    fixture.negotiate();
    let _source = Source::new(&fixture, SourceMode::Latin1);
    fixture.expect_grab(Selection::Clipboard);
    assert_eq!(
        fixture.request_text(Selection::Clipboard),
        "café".as_bytes()
    );
}

#[test]
#[ignore = "requires Xvfb and xsel"]
fn oversized_guest_text_keeps_the_previous_cache() {
    let mut fixture = Fixture::with_options(true, 64, 2000);
    fixture.negotiate();
    fixture.guest_copy(Selection::Clipboard, b"cached text");
    fixture.expect_grab(Selection::Clipboard);
    fixture.guest_copy(Selection::Clipboard, &vec![b'x'; 4096]);
    fixture.wait_for_log("X11 selection exceeds the configured byte limit");
    assert_eq!(fixture.request_text(Selection::Clipboard), b"cached text");
    fixture.quiet();
}

#[test]
#[ignore = "requires Xvfb and xsel"]
fn legacy_client_can_request_latin1_string_data() {
    let mut fixture = Fixture::new();
    fixture.negotiate();
    fixture.host_copy(Selection::Clipboard, "café".as_bytes());
    let (connection, window) = fixture.client();
    let clipboard = atom(&connection, b"CLIPBOARD");
    let property = atom(&connection, b"TEST_STRING");
    let reply = selection_property(
        &connection,
        window,
        clipboard,
        AtomEnum::STRING.into(),
        property,
    );
    assert_eq!(reply.type_, AtomEnum::STRING.into());
    assert_eq!(reply.value, b"caf\xe9");
    fixture.host_copy(Selection::Clipboard, "日本語".as_bytes());
    let targets = selection_property(
        &connection,
        window,
        clipboard,
        atom(&connection, b"TARGETS"),
        property,
    );
    assert!(
        !targets
            .value32()
            .unwrap()
            .any(|target| target == AtomEnum::STRING.into())
    );
    fixture.quiet();
}

#[test]
#[ignore = "requires Xvfb and xsel"]
fn abandoned_incr_request_expires_without_stopping_the_owner() {
    let mut fixture = Fixture::with_options(true, 1024 * 1024, 500);
    fixture.negotiate();
    let text = "large text ".repeat(10000);
    fixture.host_copy(Selection::Clipboard, text.as_bytes());
    let (connection, window) = fixture.client();
    let reply = selection_property(
        &connection,
        window,
        atom(&connection, b"CLIPBOARD"),
        atom(&connection, b"UTF8_STRING"),
        atom(&connection, b"TEST_ABANDONED"),
    );
    assert_eq!(reply.type_, atom(&connection, b"INCR"));
    fixture.wait_for_log("requestor did not finish its incremental transfer before the deadline");
    fixture.wait_for_clipboard(Selection::Clipboard, text.as_bytes());
    fixture.quiet();
}

#[test]
#[ignore = "requires Xvfb and xsel"]
fn delayed_host_release_preserves_a_new_guest_owner() {
    let mut fixture = Fixture::new();
    fixture.negotiate();
    fixture.host_copy(Selection::Clipboard, b"host text");
    let _source = Source::new(&fixture, SourceMode::Latin1);
    fixture.host.send(RELEASE, &Selection::Clipboard.prefix());
    fixture.expect_grab(Selection::Clipboard);
    assert_eq!(
        fixture.request_text(Selection::Clipboard),
        "café".as_bytes()
    );
    let (connection, _) = fixture.client();
    assert_ne!(
        connection
            .get_selection_owner(atom(&connection, b"CLIPBOARD"))
            .unwrap()
            .reply()
            .unwrap()
            .owner,
        NONE
    );
}
