mod file_transfer;
mod selection;
mod spice;
mod wayland;
mod wayland_watch;

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use file_transfer::FileTransferManager;
use selection::ClipboardSelection;
use spice::{PeerCapabilities, SpiceTransport, TransportEvent, VD_AGENT_CLIPBOARD_UTF8_TEXT};
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;
use wayland::{SeatSelector, WaylandClipboard};
use wayland_watch::{WatchEvent, spawn_selection_watcher};

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Cli {
    #[arg(
        long,
        env = "PAPRIKA_VDAGENT_PORT",
        default_value = "/dev/virtio-ports/com.redhat.spice.0"
    )]
    virtio_port: PathBuf,

    #[arg(long, env = "PAPRIKA_VDAGENT_POLL_MS", default_value_t = 250)]
    poll_ms: u64,

    #[arg(long, env = "PAPRIKA_VDAGENT_MAX_TEXT_BYTES", default_value_t = 1024 * 1024)]
    max_text_bytes: usize,

    #[arg(long, env = "PAPRIKA_VDAGENT_SEAT")]
    seat: Option<String>,

    #[arg(long, env = "PAPRIKA_VDAGENT_FILE_DIR")]
    file_dir: Option<PathBuf>,

    #[arg(
        long,
        env = "PAPRIKA_VDAGENT_MAX_ACTIVE_FILE_TRANSFERS",
        default_value_t = 8
    )]
    max_active_file_transfers: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardOwner {
    Empty,
    Guest,
    Host,
}

#[derive(Debug, Default)]
struct SelectionState {
    owner: ClipboardOwner,
    guest_cache: Option<String>,
    host_cache: Option<String>,
    last_wayland_text: Option<String>,
    suppress_text: Option<String>,
    suppress_empty: bool,
}

#[derive(Debug, Default)]
struct BridgeState {
    clipboard: SelectionState,
    primary: SelectionState,
    peer_caps: Option<PeerCapabilities>,
}

enum BridgeEvent {
    Transport(TransportEvent),
    WaylandSelectionChanged(ClipboardSelection),
    WaylandWatcherDisconnected(String),
}

impl Default for ClipboardOwner {
    fn default() -> Self {
        Self::Empty
    }
}

impl BridgeState {
    fn selection_state(&self, selection: ClipboardSelection) -> &SelectionState {
        match selection {
            ClipboardSelection::Clipboard => &self.clipboard,
            ClipboardSelection::Primary => &self.primary,
        }
    }

    fn selection_state_mut(&mut self, selection: ClipboardSelection) -> &mut SelectionState {
        match selection {
            ClipboardSelection::Clipboard => &mut self.clipboard,
            ClipboardSelection::Primary => &mut self.primary,
        }
    }
}

fn main() -> Result<()> {
    init_logging();
    let cli = Cli::parse();
    run(cli)
}

fn run(cli: Cli) -> Result<()> {
    let seat = cli
        .seat
        .as_ref()
        .map(|seat| SeatSelector::Specific(seat.clone()))
        .unwrap_or(SeatSelector::Unspecified);
    let clipboard = WaylandClipboard::new(cli.max_text_bytes, seat.clone())?;
    let mut file_transfers =
        FileTransferManager::new(cli.file_dir.clone(), Some(cli.max_active_file_transfers))?;

    if !clipboard.selection_supported(ClipboardSelection::Primary) {
        info!(
            "Wayland primary selection is not supported by this compositor; primary sync is disabled"
        );
    }

    let (bridge_tx, bridge_rx) = mpsc::channel();

    let (transport_tx, transport_rx) = mpsc::channel();
    let transport = SpiceTransport::connect(&cli.virtio_port, transport_tx, cli.max_text_bytes)?;
    spawn_transport_forwarder(transport_rx, bridge_tx.clone())?;

    let mut state = BridgeState::default();
    let mut watcher_active = false;

    match spawn_wayland_forwarder(seat, bridge_tx.clone()) {
        Ok(()) => {
            watcher_active = true;
            info!(
                "using event-driven Wayland clipboard watching on seat {}",
                clipboard.seat()
            );
        }
        Err(err) => {
            warn!(
                "failed to start event-driven Wayland watcher: {err:#}; falling back to polling every {} ms",
                cli.poll_ms
            );
        }
    }

    for selection in ClipboardSelection::ALL {
        if let Some(text) = clipboard.read_text(selection)? {
            info!(
                "detected existing guest {selection} text at startup ({} bytes)",
                text.len()
            );
            let selection_state = state.selection_state_mut(selection);
            selection_state.owner = ClipboardOwner::Guest;
            selection_state.guest_cache = Some(text.clone());
            selection_state.last_wayland_text = Some(text);
        }
    }

    let poll_interval = Duration::from_millis(cli.poll_ms);
    if let Some(save_dir) = file_transfers.save_dir() {
        info!(
            "file transfer support is enabled (save_dir={}, max_active={})",
            save_dir.display(),
            file_transfers.max_active_transfers()
        );
    } else {
        info!("file transfer support is disabled");
    }
    info!(
        "starting bridge loop on {} (seat={}, poll fallback={} ms)",
        cli.virtio_port.display(),
        clipboard.seat(),
        cli.poll_ms
    );

    loop {
        if watcher_active {
            let event = bridge_rx
                .recv()
                .context("all bridge event senders disconnected unexpectedly")?;
            handle_bridge_event(
                &transport,
                &clipboard,
                &mut state,
                &mut file_transfers,
                event,
                &mut watcher_active,
            )?;

            while let Ok(event) = bridge_rx.try_recv() {
                handle_bridge_event(
                    &transport,
                    &clipboard,
                    &mut state,
                    &mut file_transfers,
                    event,
                    &mut watcher_active,
                )?;
            }
        } else {
            match bridge_rx.recv_timeout(poll_interval) {
                Ok(event) => {
                    handle_bridge_event(
                        &transport,
                        &clipboard,
                        &mut state,
                        &mut file_transfers,
                        event,
                        &mut watcher_active,
                    )?;
                    while let Ok(event) = bridge_rx.try_recv() {
                        handle_bridge_event(
                            &transport,
                            &clipboard,
                            &mut state,
                            &mut file_transfers,
                            event,
                            &mut watcher_active,
                        )?;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("all bridge event senders disconnected unexpectedly")
                }
            }

            for selection in ClipboardSelection::ALL {
                let snapshot = clipboard.read_text(selection)?;
                handle_wayland_snapshot(&transport, &mut state, selection, snapshot)?;
            }
        }
    }
}

fn spawn_transport_forwarder(
    rx: Receiver<TransportEvent>,
    tx: mpsc::Sender<BridgeEvent>,
) -> Result<()> {
    thread::Builder::new()
        .name("paprika-transport-forward".to_string())
        .spawn(move || {
            while let Ok(event) = rx.recv() {
                if tx.send(BridgeEvent::Transport(event)).is_err() {
                    break;
                }
            }
        })
        .context("failed to spawn transport event forwarder")?;

    Ok(())
}

fn spawn_wayland_forwarder(seat: SeatSelector, tx: mpsc::Sender<BridgeEvent>) -> Result<()> {
    let (watch_tx, watch_rx) = mpsc::channel();
    spawn_selection_watcher(seat, watch_tx)?;

    thread::Builder::new()
        .name("paprika-watch-forward".to_string())
        .spawn(move || {
            while let Ok(event) = watch_rx.recv() {
                let bridge_event = match event {
                    WatchEvent::SelectionChanged(selection) => {
                        BridgeEvent::WaylandSelectionChanged(selection)
                    }
                    WatchEvent::Disconnected(message) => {
                        BridgeEvent::WaylandWatcherDisconnected(message)
                    }
                };

                if tx.send(bridge_event).is_err() {
                    break;
                }
            }
        })
        .context("failed to spawn Wayland watcher forwarder")?;

    Ok(())
}

fn handle_bridge_event(
    transport: &SpiceTransport,
    clipboard: &WaylandClipboard,
    state: &mut BridgeState,
    file_transfers: &mut FileTransferManager,
    event: BridgeEvent,
    watcher_active: &mut bool,
) -> Result<()> {
    match event {
        BridgeEvent::Transport(event) => match event {
            TransportEvent::PeerCapabilities(peer) => {
                info!(
                    "peer capabilities announced: clipboard_by_demand={} selection={} grab_serial={}",
                    peer.clipboard_by_demand, peer.clipboard_selection, peer.clipboard_grab_serial
                );
                state.peer_caps = Some(peer);

                for selection in ClipboardSelection::ALL {
                    let selection_state = state.selection_state(selection);
                    if selection_state.owner == ClipboardOwner::Guest {
                        if let Some(text) = selection_state.guest_cache.as_ref() {
                            let sent = transport.send_clipboard_grab_text(selection)?;
                            if sent {
                                info!(
                                    "announced cached guest {selection} after capability negotiation"
                                );
                                debug!("cached guest {selection} length={} bytes", text.len());
                            }
                        }
                    }
                }
            }
            TransportEvent::HostGrab {
                selection,
                types,
                serial,
            } => {
                if types.is_empty() {
                    debug!("discarded stale or empty host {selection} GRAB");
                    return Ok(());
                }

                info!("host claimed {selection} ownership (serial={serial:?}, types={types:?})");
                if !types.contains(&VD_AGENT_CLIPBOARD_UTF8_TEXT) {
                    warn!("host {selection} GRAB does not offer UTF-8 text, ignoring for now");

                    let should_clear = {
                        let selection_state = state.selection_state(selection);
                        selection_state.owner == ClipboardOwner::Host
                            && selection_state.last_wayland_text.is_some()
                    };

                    if should_clear {
                        let _ = clipboard.clear(selection)?;
                        let selection_state = state.selection_state_mut(selection);
                        selection_state.suppress_empty = true;
                        selection_state.last_wayland_text = None;
                        selection_state.host_cache = None;
                    }

                    return Ok(());
                }

                transport.send_clipboard_request_text(selection)?;
            }
            TransportEvent::HostRequest {
                selection,
                data_type,
            } => {
                info!("host requested {selection} data type={data_type}");
                if data_type != VD_AGENT_CLIPBOARD_UTF8_TEXT {
                    warn!(
                        "host requested unsupported {selection} type {data_type}, sending empty data"
                    );
                    transport.send_empty_clipboard_data(selection)?;
                    return Ok(());
                }

                let selection_state = state.selection_state(selection);
                if let Some(text) = selection_state
                    .guest_cache
                    .as_deref()
                    .or(selection_state.last_wayland_text.as_deref())
                {
                    transport.send_clipboard_text(selection, text)?;
                } else {
                    warn!("host requested {selection} text but guest cache is empty");
                    transport.send_empty_clipboard_data(selection)?;
                }
            }
            TransportEvent::HostData {
                selection,
                data_type,
                data,
            } => {
                if data_type != VD_AGENT_CLIPBOARD_UTF8_TEXT {
                    warn!("host sent unsupported {selection} data type {data_type}, ignoring");
                    return Ok(());
                }

                let text = String::from_utf8(data)
                    .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned());

                info!(
                    "received host {selection} text ({} bytes), injecting into Wayland",
                    text.len()
                );

                if !clipboard.write_text(selection, &text)? {
                    warn!(
                        "guest compositor does not support Wayland {selection}; ignoring incoming host data"
                    );
                    return Ok(());
                }

                let selection_state = state.selection_state_mut(selection);
                selection_state.owner = ClipboardOwner::Host;
                selection_state.guest_cache = None;
                selection_state.host_cache = Some(text.clone());
                selection_state.suppress_text = Some(text.clone());
                selection_state.last_wayland_text = Some(text);
            }
            TransportEvent::HostRelease { selection } => {
                info!("host released {selection} ownership");

                let (should_clear, released_text) = {
                    let selection_state = state.selection_state_mut(selection);
                    let released_text = selection_state.host_cache.take();
                    let should_clear = selection_state.owner == ClipboardOwner::Host
                        && released_text
                            .as_ref()
                            .and_then(|released_text| {
                                selection_state
                                    .last_wayland_text
                                    .as_ref()
                                    .map(|current| current == released_text)
                            })
                            .unwrap_or(false);
                    (should_clear, released_text)
                };

                if should_clear {
                    let cleared = clipboard.clear(selection)?;
                    let selection_state = state.selection_state_mut(selection);
                    if cleared {
                        selection_state.suppress_empty = true;
                    }
                    selection_state.last_wayland_text = None;
                    selection_state.owner = ClipboardOwner::Empty;
                    info!(
                        "cleared Wayland {selection} because the active host-owned text was released"
                    );
                } else {
                    let _ = released_text;
                }
            }
            TransportEvent::HostFileXferStart { id, metadata } => {
                file_transfers.handle_start(transport, id, &metadata)?;
            }
            TransportEvent::HostFileXferData { id, data } => {
                file_transfers.handle_data(transport, id, &data)?;
            }
            TransportEvent::HostFileXferStatus { id, result } => {
                file_transfers.handle_host_status(id, result);
            }
            TransportEvent::HostClientDisconnected => {
                info!("SPICE client disconnected, cancelling in-flight file transfers");
                file_transfers.cancel_all("SPICE client disconnected");
            }
            TransportEvent::Disconnected(message) => {
                file_transfers.cancel_all("SPICE transport disconnected");
                bail!("SPICE transport disconnected: {message}");
            }
        },
        BridgeEvent::WaylandSelectionChanged(selection) => {
            let snapshot = clipboard.read_text(selection)?;
            handle_wayland_snapshot(transport, state, selection, snapshot)?;
        }
        BridgeEvent::WaylandWatcherDisconnected(message) => {
            warn!("{message}; switching back to polling mode");
            *watcher_active = false;
        }
    }

    Ok(())
}

fn handle_wayland_snapshot(
    transport: &SpiceTransport,
    state: &mut BridgeState,
    selection: ClipboardSelection,
    snapshot: Option<String>,
) -> Result<()> {
    let selection_state = state.selection_state_mut(selection);

    match snapshot {
        Some(text) => {
            if selection_state.last_wayland_text.as_ref() == Some(&text) {
                return Ok(());
            }

            if selection_state.suppress_text.as_ref() == Some(&text) {
                debug!("suppressed host->guest {selection} echo for matching Wayland text");
                selection_state.suppress_text = None;
                selection_state.last_wayland_text = Some(text);
                return Ok(());
            }

            info!("guest Wayland {selection} changed ({} bytes)", text.len());
            selection_state.owner = ClipboardOwner::Guest;
            selection_state.guest_cache = Some(text.clone());
            selection_state.host_cache = None;
            selection_state.last_wayland_text = Some(text);

            let _ = transport.send_clipboard_grab_text(selection)?;
        }
        None => {
            if selection_state.last_wayland_text.is_none() {
                return Ok(());
            }

            if selection_state.suppress_empty {
                debug!("suppressed local empty {selection} event caused by host RELEASE");
                selection_state.suppress_empty = false;
                selection_state.last_wayland_text = None;
                return Ok(());
            }

            info!("guest Wayland {selection} became empty");
            selection_state.owner = ClipboardOwner::Empty;
            selection_state.guest_cache = None;
            selection_state.last_wayland_text = None;
            let _ = transport.send_clipboard_release(selection)?;
        }
    }

    Ok(())
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("paprika_vdagent=info"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_names(true)
        .compact()
        .init();
}
