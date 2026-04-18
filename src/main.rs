mod spice;
mod wayland;
mod wayland_watch;

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardOwner {
    Empty,
    Guest,
    Host,
}

#[derive(Debug)]
struct BridgeState {
    owner: ClipboardOwner,
    guest_cache: Option<String>,
    host_cache: Option<String>,
    last_wayland_text: Option<String>,
    suppress_text: Option<String>,
    suppress_empty: bool,
    peer_caps: Option<PeerCapabilities>,
}

enum BridgeEvent {
    Transport(TransportEvent),
    WaylandSelectionChanged,
    WaylandWatcherDisconnected(String),
}

impl Default for BridgeState {
    fn default() -> Self {
        Self {
            owner: ClipboardOwner::Empty,
            guest_cache: None,
            host_cache: None,
            last_wayland_text: None,
            suppress_text: None,
            suppress_empty: false,
            peer_caps: None,
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

    if let Some(text) = clipboard.read_text()? {
        info!(
            "detected existing guest clipboard text at startup ({} bytes)",
            text.len()
        );
        state.owner = ClipboardOwner::Guest;
        state.guest_cache = Some(text.clone());
        state.last_wayland_text = Some(text);
    }

    let poll_interval = Duration::from_millis(cli.poll_ms);
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
                event,
                &mut watcher_active,
            )?;

            while let Ok(event) = bridge_rx.try_recv() {
                handle_bridge_event(
                    &transport,
                    &clipboard,
                    &mut state,
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
                        event,
                        &mut watcher_active,
                    )?;
                    while let Ok(event) = bridge_rx.try_recv() {
                        handle_bridge_event(
                            &transport,
                            &clipboard,
                            &mut state,
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

            let snapshot = clipboard.read_text()?;
            handle_wayland_snapshot(&transport, &mut state, snapshot)?;
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
                    WatchEvent::SelectionChanged => BridgeEvent::WaylandSelectionChanged,
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

                if state.owner == ClipboardOwner::Guest {
                    if let Some(text) = state.guest_cache.as_ref() {
                        let sent = transport.send_clipboard_grab_text()?;
                        if sent {
                            info!("announced cached guest clipboard after capability negotiation");
                            debug!("cached guest clipboard length={} bytes", text.len());
                        }
                    }
                }
            }
            TransportEvent::HostGrab { types, serial } => {
                if types.is_empty() {
                    debug!("discarded stale or empty host clipboard GRAB");
                    return Ok(());
                }

                info!("host claimed clipboard ownership (serial={serial:?}, types={types:?})");
                if !types.contains(&VD_AGENT_CLIPBOARD_UTF8_TEXT) {
                    warn!("host clipboard GRAB does not offer UTF-8 text, ignoring for v1");
                    if state.owner == ClipboardOwner::Host && state.last_wayland_text.is_some() {
                        clipboard.clear()?;
                        state.suppress_empty = true;
                        state.last_wayland_text = None;
                        state.host_cache = None;
                    }
                    return Ok(());
                }

                transport.send_clipboard_request_text()?;
            }
            TransportEvent::HostRequest { data_type } => {
                info!("host requested clipboard data type={data_type}");
                if data_type != VD_AGENT_CLIPBOARD_UTF8_TEXT {
                    warn!(
                        "host requested unsupported clipboard type {data_type}, sending empty data"
                    );
                    transport.send_empty_clipboard_data()?;
                    return Ok(());
                }

                if let Some(text) = state
                    .guest_cache
                    .as_deref()
                    .or(state.last_wayland_text.as_deref())
                {
                    transport.send_clipboard_text(text)?;
                } else {
                    warn!("host requested clipboard text but guest cache is empty");
                    transport.send_empty_clipboard_data()?;
                }
            }
            TransportEvent::HostData { data_type, data } => {
                if data_type != VD_AGENT_CLIPBOARD_UTF8_TEXT {
                    warn!("host sent unsupported clipboard data type {data_type}, ignoring");
                    return Ok(());
                }

                let text = String::from_utf8(data)
                    .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned());

                info!(
                    "received host clipboard text ({} bytes), injecting into Wayland clipboard",
                    text.len()
                );

                clipboard.write_text(&text)?;
                state.owner = ClipboardOwner::Host;
                state.guest_cache = None;
                state.host_cache = Some(text.clone());
                state.suppress_text = Some(text.clone());
                state.last_wayland_text = Some(text);
            }
            TransportEvent::HostRelease => {
                info!("host released clipboard ownership");
                let host_text = state.host_cache.take();

                if state.owner == ClipboardOwner::Host {
                    if let Some(ref released_text) = host_text {
                        if state.last_wayland_text.as_deref() == Some(released_text.as_str()) {
                            clipboard.clear()?;
                            state.suppress_empty = true;
                            state.last_wayland_text = None;
                            state.owner = ClipboardOwner::Empty;
                            info!(
                                "cleared Wayland clipboard because the active host-owned clipboard was released"
                            );
                        }
                    }
                }
            }
            TransportEvent::Disconnected(message) => {
                bail!("SPICE transport disconnected: {message}");
            }
        },
        BridgeEvent::WaylandSelectionChanged => {
            let snapshot = clipboard.read_text()?;
            handle_wayland_snapshot(transport, state, snapshot)?;
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
    snapshot: Option<String>,
) -> Result<()> {
    match snapshot {
        Some(text) => {
            if state.last_wayland_text.as_ref() == Some(&text) {
                return Ok(());
            }

            if state.suppress_text.as_ref() == Some(&text) {
                debug!("suppressed host->guest clipboard echo for matching Wayland text");
                state.suppress_text = None;
                state.last_wayland_text = Some(text);
                return Ok(());
            }

            info!("guest Wayland clipboard changed ({} bytes)", text.len());
            state.owner = ClipboardOwner::Guest;
            state.guest_cache = Some(text.clone());
            state.host_cache = None;
            state.last_wayland_text = Some(text);

            let _ = transport.send_clipboard_grab_text()?;
        }
        None => {
            if state.last_wayland_text.is_none() {
                return Ok(());
            }

            if state.suppress_empty {
                debug!("suppressed local empty clipboard event caused by host RELEASE");
                state.suppress_empty = false;
                state.last_wayland_text = None;
                return Ok(());
            }

            info!("guest Wayland clipboard became empty");
            state.owner = ClipboardOwner::Empty;
            state.guest_cache = None;
            state.last_wayland_text = None;
            let _ = transport.send_clipboard_release()?;
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
