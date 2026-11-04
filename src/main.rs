mod clipboard_io;
mod doctor;
mod file_transfer;
mod selection;
mod session;
mod spice;
mod wayland;
mod wayland_watch;

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use file_transfer::FileTransferManager;
use selection::ClipboardSelection;
use spice::{PeerCapabilities, SpiceTransport, TransportEvent, VD_AGENT_CLIPBOARD_UTF8_TEXT};
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;
use wayland::{ClipboardRead, SeatSelector, WaylandClipboard};
use wayland_watch::{WatchEvent, spawn_selection_watcher};

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<CliCommand>,

    #[arg(
        long,
        env = "PAPRIKA_VDAGENT_PORT",
        default_value = "/dev/virtio-ports/com.redhat.spice.0"
    )]
    virtio_port: PathBuf,

    #[arg(long, env = "PAPRIKA_VDAGENT_POLL_MS", default_value_t = 250, value_parser = clap::value_parser!(u64).range(1..))]
    poll_ms: u64,

    #[arg(long, env = "PAPRIKA_VDAGENT_MAX_TEXT_BYTES", default_value_t = 1024 * 1024)]
    max_text_bytes: usize,

    /// Maximum time to wait for a clipboard source to finish sending its payload
    #[arg(long, env = "PAPRIKA_VDAGENT_CLIPBOARD_TIMEOUT_MS", default_value_t = 2000, value_parser = clap::value_parser!(u64).range(1..))]
    clipboard_timeout_ms: u64,

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

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Check session readiness without opening the SPICE port or reading the clipboard
    Doctor,

    /// Import the current Wayland environment and restart the systemd user service
    SessionStart,
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
    retry_since: Option<Instant>,
    read_pending: bool,
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
    fn client_disconnected(&mut self) {
        self.peer_caps = None;
        for selection in ClipboardSelection::ALL {
            let state = self.selection_state_mut(selection);
            // Keep the visible clipboard available to the guest and announce it
            // as local data when a new client negotiates its capabilities.
            state.guest_cache = state.last_wayland_text.clone();
            state.owner = if state.guest_cache.is_some() {
                ClipboardOwner::Guest
            } else {
                ClipboardOwner::Empty
            };
            state.host_cache = None;
            state.suppress_text = None;
            state.suppress_empty = false;
        }
    }

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
    match cli.command {
        Some(CliCommand::SessionStart) => session::start(),
        Some(CliCommand::Doctor) => doctor::run(
            &cli.virtio_port,
            cli.seat.as_deref(),
            cli.file_dir.as_deref(),
        ),
        None => run(cli),
    }
}

fn run(cli: Cli) -> Result<()> {
    let seat = cli
        .seat
        .as_ref()
        .map(|seat| SeatSelector::Specific(seat.clone()))
        .unwrap_or(SeatSelector::Unspecified);
    let clipboard = WaylandClipboard::new(
        cli.max_text_bytes,
        seat.clone(),
        Duration::from_millis(cli.clipboard_timeout_ms),
    )?;
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
        let snapshot = clipboard.read_text(selection)?;
        handle_wayland_read(&transport, &mut state, selection, snapshot)?;
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

    let mut next_poll = Instant::now() + poll_interval;
    loop {
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
                // Limit each batch so retry work cannot be starved by SPICE traffic.
                for event in bridge_rx.try_iter().take(64) {
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

        let poll_due = Instant::now() >= next_poll;
        for selection in ClipboardSelection::ALL {
            let selection_state = state.selection_state(selection);
            let read_due = selection_state.read_pending
                && selection_state
                    .retry_since
                    .is_none_or(|since| since.elapsed() >= poll_interval);
            if (!watcher_active && poll_due) || read_due {
                let snapshot = clipboard.read_text(selection)?;
                handle_wayland_read(&transport, &mut state, selection, snapshot)?;
            }
        }
        if poll_due {
            next_poll = Instant::now() + poll_interval;
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
                state.client_disconnected();
            }
            TransportEvent::Disconnected(message) => {
                file_transfers.cancel_all("SPICE transport disconnected");
                bail!("SPICE transport disconnected: {message}");
            }
        },
        BridgeEvent::WaylandSelectionChanged(selection) => {
            // Coalesce selection events before reading. A new owner also
            // supersedes a stalled source's retry delay.
            let selection_state = state.selection_state_mut(selection);
            selection_state.read_pending = true;
            selection_state.retry_since = None;
        }
        BridgeEvent::WaylandWatcherDisconnected(message) => {
            warn!("{message}; switching back to polling mode");
            *watcher_active = false;
        }
    }

    Ok(())
}

fn handle_wayland_read(
    transport: &SpiceTransport,
    state: &mut BridgeState,
    selection: ClipboardSelection,
    snapshot: ClipboardRead,
) -> Result<()> {
    match snapshot {
        ClipboardRead::Ready(contents) => {
            let selection_state = state.selection_state_mut(selection);
            selection_state.read_pending = false;
            selection_state.retry_since = None;
            handle_wayland_snapshot(transport, state, selection, contents)
        }
        ClipboardRead::Retry => {
            let selection_state = state.selection_state_mut(selection);
            selection_state.read_pending = true;
            selection_state.retry_since = Some(Instant::now());
            Ok(())
        }
    }
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

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use std::fs::File;

    fn transport() -> SpiceTransport {
        SpiceTransport::test_writer(File::options().write(true).open("/dev/null").unwrap())
    }

    #[test]
    fn a_timed_out_read_keeps_cached_data_and_a_successful_retry_updates_it() {
        let transport = transport();
        let mut state = BridgeState::default();
        state.clipboard.owner = ClipboardOwner::Guest;
        state.clipboard.guest_cache = Some("cached".into());
        state.clipboard.last_wayland_text = Some("cached".into());
        handle_wayland_read(
            &transport,
            &mut state,
            ClipboardSelection::Clipboard,
            ClipboardRead::Retry,
        )
        .unwrap();
        assert_eq!(state.clipboard.owner, ClipboardOwner::Guest);
        assert_eq!(state.clipboard.guest_cache.as_deref(), Some("cached"));
        assert_eq!(state.clipboard.last_wayland_text.as_deref(), Some("cached"));
        assert!(state.clipboard.read_pending);
        assert!(state.clipboard.retry_since.is_some());
        handle_wayland_read(
            &transport,
            &mut state,
            ClipboardSelection::Clipboard,
            ClipboardRead::Ready(Some("recovered".into())),
        )
        .unwrap();
        assert_eq!(state.clipboard.guest_cache.as_deref(), Some("recovered"));
        assert!(!state.clipboard.read_pending);
        assert!(state.clipboard.retry_since.is_none());
    }

    #[test]
    fn a_timed_out_read_does_not_release_host_ownership() {
        let transport = transport();
        let mut state = BridgeState::default();
        state.primary.owner = ClipboardOwner::Host;
        state.primary.host_cache = Some("host text".into());
        state.primary.last_wayland_text = Some("host text".into());
        state.primary.suppress_text = Some("host text".into());
        handle_wayland_read(
            &transport,
            &mut state,
            ClipboardSelection::Primary,
            ClipboardRead::Retry,
        )
        .unwrap();
        assert_eq!(state.primary.owner, ClipboardOwner::Host);
        assert_eq!(state.primary.host_cache.as_deref(), Some("host text"));
        assert_eq!(state.primary.suppress_text.as_deref(), Some("host text"));
    }

    #[test]
    fn disconnected_client_leaves_visible_text_ready_for_the_next_client() {
        let mut state = BridgeState {
            peer_caps: Some(PeerCapabilities {
                clipboard_by_demand: true,
                clipboard_selection: true,
                clipboard_grab_serial: true,
            }),
            ..BridgeState::default()
        };
        state.clipboard.owner = ClipboardOwner::Host;
        state.clipboard.host_cache = Some("visible text".into());
        state.clipboard.last_wayland_text = Some("visible text".into());
        state.clipboard.suppress_text = Some("visible text".into());
        state.primary.suppress_empty = true;
        state.client_disconnected();
        assert!(state.peer_caps.is_none());
        assert_eq!(state.clipboard.owner, ClipboardOwner::Guest);
        assert_eq!(state.clipboard.guest_cache.as_deref(), Some("visible text"));
        assert_eq!(
            state.clipboard.last_wayland_text.as_deref(),
            Some("visible text")
        );
        assert!(state.clipboard.host_cache.is_none());
        assert!(state.clipboard.suppress_text.is_none());
        assert!(!state.primary.suppress_empty);
        assert_eq!(state.primary.owner, ClipboardOwner::Empty);
    }

    #[test]
    fn zero_poll_interval_and_zero_clipboard_timeout_are_rejected() {
        for flag in ["--poll-ms", "--clipboard-timeout-ms"] {
            assert!(Cli::try_parse_from(["paprika-vdagent", flag, "0"]).is_err());
        }
    }
}
