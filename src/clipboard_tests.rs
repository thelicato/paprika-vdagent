use std::cell::{Cell, RefCell};
use std::fs::{self, File};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

#[derive(Default)]
struct MemoryClipboard {
    selections: RefCell<[Option<String>; 2]>,
    sender: RefCell<Option<mpsc::Sender<ClipboardEvent>>>,
    clears: Cell<usize>,
    primary_supported: bool,
}

impl Clipboard for MemoryClipboard {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn read_text(&self, selection: ClipboardSelection) -> Result<ClipboardRead> {
        Ok(ClipboardRead::Ready(
            self.selections.borrow()[selection.spice_id() as usize].clone(),
        ))
    }

    fn write_text(&self, selection: ClipboardSelection, text: &str) -> Result<bool> {
        if !self.selection_supported(selection) {
            return Ok(false);
        }
        self.selections.borrow_mut()[selection.spice_id() as usize] = Some(text.into());
        Ok(true)
    }

    fn clear(&self, selection: ClipboardSelection) -> Result<bool> {
        if !self.selection_supported(selection) {
            return Ok(false);
        }
        self.clears.set(self.clears.get() + 1);
        self.selections.borrow_mut()[selection.spice_id() as usize] = None;
        Ok(true)
    }

    fn selection_supported(&self, selection: ClipboardSelection) -> bool {
        selection == ClipboardSelection::Clipboard || self.primary_supported
    }

    fn start_watcher(&self, sender: mpsc::Sender<ClipboardEvent>) -> Result<()> {
        *self.sender.borrow_mut() = Some(sender);
        Ok(())
    }
}

struct Bridge {
    transport: SpiceTransport,
    state: BridgeState,
    files: FileTransferManager,
    watcher_active: bool,
    directory: PathBuf,
}

impl Bridge {
    fn new() -> Self {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "paprika-interface-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let files = FileTransferManager::new(Some(directory.clone()), Some(1)).unwrap();
        Self {
            transport: SpiceTransport::test_writer(
                File::options().write(true).open("/dev/null").unwrap(),
            ),
            state: BridgeState::default(),
            files,
            watcher_active: true,
            directory,
        }
    }

    fn event(&mut self, clipboard: &dyn Clipboard, event: BridgeEvent) {
        handle_bridge_event(
            &self.transport,
            clipboard,
            &mut self.state,
            &mut self.files,
            event,
            &mut self.watcher_active,
        )
        .unwrap();
    }

    fn host_copy(&mut self, clipboard: &dyn Clipboard, selection: ClipboardSelection, text: &str) {
        self.event(
            clipboard,
            BridgeEvent::Transport(TransportEvent::HostData {
                selection,
                data_type: VD_AGENT_CLIPBOARD_UTF8_TEXT,
                data: text.as_bytes().to_vec(),
            }),
        );
    }

    fn read(&mut self, clipboard: &dyn Clipboard, selection: ClipboardSelection) {
        let snapshot = clipboard.read_text(selection).unwrap();
        handle_clipboard_read(&self.transport, &mut self.state, selection, snapshot).unwrap();
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.files.cancel_all("test complete");
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn bridge_serves_host_text_and_release_through_the_clipboard_interface() {
    let clipboard = MemoryClipboard {
        primary_supported: true,
        ..MemoryClipboard::default()
    };
    let backend: &dyn Clipboard = &clipboard;
    let mut bridge = Bridge::new();
    bridge.host_copy(backend, ClipboardSelection::Primary, "host text");
    assert_eq!(
        backend.read_text(ClipboardSelection::Primary).unwrap(),
        ClipboardRead::Ready(Some("host text".into()))
    );
    bridge.read(backend, ClipboardSelection::Primary);
    assert_eq!(
        bridge.state.primary.owner,
        ClipboardOwner::Host,
        "reading back host text must not claim guest ownership"
    );
    bridge.event(
        backend,
        BridgeEvent::Transport(TransportEvent::HostRelease {
            selection: ClipboardSelection::Primary,
        }),
    );
    assert_eq!(
        backend.read_text(ClipboardSelection::Primary).unwrap(),
        ClipboardRead::Ready(None)
    );
    assert_eq!(clipboard.clears.get(), 1);
    assert_eq!(bridge.state.primary.owner, ClipboardOwner::Empty);
}

#[test]
fn host_release_does_not_clear_new_guest_text_from_another_backend() {
    let clipboard = MemoryClipboard::default();
    let mut bridge = Bridge::new();
    bridge.host_copy(&clipboard, ClipboardSelection::Clipboard, "host text");
    clipboard
        .write_text(ClipboardSelection::Clipboard, "guest edit")
        .unwrap();
    bridge.read(&clipboard, ClipboardSelection::Clipboard);
    bridge.event(
        &clipboard,
        BridgeEvent::Transport(TransportEvent::HostRelease {
            selection: ClipboardSelection::Clipboard,
        }),
    );
    assert_eq!(
        clipboard.read_text(ClipboardSelection::Clipboard).unwrap(),
        ClipboardRead::Ready(Some("guest edit".into()))
    );
    assert_eq!(clipboard.clears.get(), 0);
    assert_eq!(bridge.state.clipboard.owner, ClipboardOwner::Guest);
}

#[test]
fn unsupported_selections_do_not_change_bridge_ownership() {
    let clipboard = MemoryClipboard::default();
    let mut bridge = Bridge::new();
    bridge.host_copy(&clipboard, ClipboardSelection::Primary, "unsupported");
    assert_eq!(bridge.state.primary.owner, ClipboardOwner::Empty);
    assert!(bridge.state.primary.host_cache.is_none());
}

#[test]
fn generic_watcher_events_schedule_reads_and_fall_back_to_polling() {
    let clipboard = MemoryClipboard::default();
    let mut bridge = Bridge::new();
    let (sender, events) = mpsc::channel();
    spawn_clipboard_forwarder(&clipboard, sender).unwrap();
    let sender = clipboard.sender.borrow().as_ref().unwrap().clone();
    sender
        .send(ClipboardEvent::SelectionChanged(
            ClipboardSelection::Clipboard,
        ))
        .unwrap();
    bridge.event(
        &clipboard,
        events.recv_timeout(Duration::from_secs(1)).unwrap(),
    );
    assert!(bridge.state.clipboard.read_pending);
    sender
        .send(ClipboardEvent::Disconnected("test watcher finished".into()))
        .unwrap();
    bridge.event(
        &clipboard,
        events.recv_timeout(Duration::from_secs(1)).unwrap(),
    );
    assert!(!bridge.watcher_active);
}
