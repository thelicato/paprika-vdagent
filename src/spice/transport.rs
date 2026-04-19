use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc::Sender};
use std::thread;

use anyhow::{Context, Result, anyhow};
use tracing::{debug, info, trace, warn};

use crate::selection::ClipboardSelection;

use super::codec::{MessageAssembler, encode_message_frame, read_one_chunk};
use super::protocol::{
    PeerCapabilities, TransportEvent, TransportState, VD_AGENT_ANNOUNCE_CAPABILITIES,
    VD_AGENT_CAP_CLIPBOARD_BY_DEMAND, VD_AGENT_CAP_CLIPBOARD_GRAB_SERIAL,
    VD_AGENT_CAP_CLIPBOARD_NO_RELEASE_ON_REGRAB, VD_AGENT_CAP_CLIPBOARD_SELECTION,
    VD_AGENT_CLIENT_DISCONNECTED, VD_AGENT_CLIPBOARD, VD_AGENT_CLIPBOARD_GRAB,
    VD_AGENT_CLIPBOARD_NONE, VD_AGENT_CLIPBOARD_RELEASE, VD_AGENT_CLIPBOARD_REQUEST,
    VD_AGENT_CLIPBOARD_UTF8_TEXT, VD_AGENT_FILE_XFER_DATA, VD_AGENT_FILE_XFER_START,
    VD_AGENT_FILE_XFER_STATUS, VDP_CLIENT_PORT, has_capability, le_u32, parse_capabilities,
    parse_clipboard_data, parse_clipboard_grab, parse_clipboard_release, parse_clipboard_request,
    parse_file_xfer_data, parse_file_xfer_start, parse_file_xfer_status, selection_prefix,
    set_capability,
};

pub struct SpiceTransport {
    path: PathBuf,
    writer: Arc<Mutex<File>>,
    state: Arc<Mutex<TransportState>>,
    max_text_bytes: usize,
}

impl SpiceTransport {
    pub fn connect(
        path: &Path,
        sender: Sender<TransportEvent>,
        max_text_bytes: usize,
    ) -> Result<Self> {
        let file = File::options()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("failed to open virtio port {}", path.display()))?;

        let reader = file
            .try_clone()
            .context("failed to clone virtio port file descriptor")?;

        let writer = Arc::new(Mutex::new(file));
        let state = Arc::new(Mutex::new(TransportState::default()));

        let transport = Self {
            path: path.to_path_buf(),
            writer: Arc::clone(&writer),
            state: Arc::clone(&state),
            max_text_bytes,
        };

        transport.send_capabilities(true)?;

        let reader_path = path.to_path_buf();
        let event_sender = sender.clone();
        thread::Builder::new()
            .name("paprika-spice-reader".to_string())
            .spawn(move || {
                if let Err(err) = reader_loop(reader_path, reader, writer, state, sender) {
                    let message = format!("{err:#}");
                    let _ = event_sender.send(TransportEvent::Disconnected(message.clone()));
                    let _ = io::stderr()
                        .write_all(format!("paprika-spice-reader: {message}\n").as_bytes());
                }
            })
            .context("failed to spawn SPICE reader thread")?;

        Ok(transport)
    }

    pub fn peer_ready_for_clipboard(&self) -> bool {
        self.state
            .lock()
            .map(|state| state.clipboard_by_demand)
            .unwrap_or(false)
    }

    pub fn send_clipboard_grab_text(&self, selection: ClipboardSelection) -> Result<bool> {
        let (clipboard_selection, clipboard_grab_serial, serial) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow!("transport mutex poisoned"))?;
            if !state.clipboard_by_demand {
                debug!("peer has not announced clipboard-by-demand yet, deferring local grab");
                return Ok(false);
            }

            let serial = state.serial_counter;
            state.serial_counter = state.serial_counter.wrapping_add(1);
            (
                state.clipboard_selection,
                state.clipboard_grab_serial,
                serial,
            )
        };

        let mut payload = Vec::new();
        if clipboard_selection {
            payload.extend_from_slice(&selection_prefix(selection));
        }
        if clipboard_grab_serial {
            payload.extend_from_slice(&serial.to_le_bytes());
        }
        payload.extend_from_slice(&VD_AGENT_CLIPBOARD_UTF8_TEXT.to_le_bytes());

        self.send_message(VD_AGENT_CLIPBOARD_GRAB, &payload)?;
        info!("sent guest -> host {selection} GRAB (utf8-text, serial={serial})");
        Ok(true)
    }

    pub fn send_clipboard_request_text(&self, selection: ClipboardSelection) -> Result<()> {
        let clipboard_selection = self
            .state
            .lock()
            .map_err(|_| anyhow!("transport mutex poisoned"))?
            .clipboard_selection;

        let mut payload = Vec::new();
        if clipboard_selection {
            payload.extend_from_slice(&selection_prefix(selection));
        }
        payload.extend_from_slice(&VD_AGENT_CLIPBOARD_UTF8_TEXT.to_le_bytes());
        self.send_message(VD_AGENT_CLIPBOARD_REQUEST, &payload)?;
        info!("sent guest -> host {selection} REQUEST (utf8-text)");
        Ok(())
    }

    pub fn send_clipboard_text(&self, selection: ClipboardSelection, text: &str) -> Result<bool> {
        if text.len() > self.max_text_bytes {
            warn!(
                "refusing to send {selection} text larger than configured limit: {} > {}",
                text.len(),
                self.max_text_bytes
            );
            return Ok(false);
        }

        let clipboard_selection = self
            .state
            .lock()
            .map_err(|_| anyhow!("transport mutex poisoned"))?
            .clipboard_selection;

        let mut payload = Vec::new();
        if clipboard_selection {
            payload.extend_from_slice(&selection_prefix(selection));
        }
        payload.extend_from_slice(&VD_AGENT_CLIPBOARD_UTF8_TEXT.to_le_bytes());
        payload.extend_from_slice(text.as_bytes());

        self.send_message(VD_AGENT_CLIPBOARD, &payload)?;
        info!("sent guest -> host {selection} DATA ({} bytes)", text.len());
        Ok(true)
    }

    pub fn send_empty_clipboard_data(&self, selection: ClipboardSelection) -> Result<()> {
        let clipboard_selection = self
            .state
            .lock()
            .map_err(|_| anyhow!("transport mutex poisoned"))?
            .clipboard_selection;

        let mut payload = Vec::new();
        if clipboard_selection {
            payload.extend_from_slice(&selection_prefix(selection));
        }
        payload.extend_from_slice(&VD_AGENT_CLIPBOARD_NONE.to_le_bytes());

        self.send_message(VD_AGENT_CLIPBOARD, &payload)?;
        info!("sent guest -> host empty {selection} DATA");
        Ok(())
    }

    pub fn send_clipboard_release(&self, selection: ClipboardSelection) -> Result<bool> {
        let clipboard_selection = self
            .state
            .lock()
            .map_err(|_| anyhow!("transport mutex poisoned"))?
            .clipboard_selection;

        if !self.peer_ready_for_clipboard() {
            debug!("peer has not announced clipboard-by-demand yet, no RELEASE sent");
            return Ok(false);
        }

        let mut payload = Vec::new();
        if clipboard_selection {
            payload.extend_from_slice(&selection_prefix(selection));
        }

        self.send_message(VD_AGENT_CLIPBOARD_RELEASE, &payload)?;
        info!("sent guest -> host {selection} RELEASE");
        Ok(true)
    }

    pub fn send_file_xfer_status(&self, id: u32, result: u32) -> Result<()> {
        let mut payload = Vec::with_capacity(8);
        payload.extend_from_slice(&id.to_le_bytes());
        payload.extend_from_slice(&result.to_le_bytes());
        self.send_message(VD_AGENT_FILE_XFER_STATUS, &payload)?;
        info!("sent guest -> host file transfer STATUS id={id} result={result}");
        Ok(())
    }

    fn send_capabilities(&self, request: bool) -> Result<()> {
        let mut caps = 0u32;
        set_capability(&mut caps, VD_AGENT_CAP_CLIPBOARD_BY_DEMAND);
        set_capability(&mut caps, VD_AGENT_CAP_CLIPBOARD_SELECTION);
        set_capability(&mut caps, VD_AGENT_CAP_CLIPBOARD_NO_RELEASE_ON_REGRAB);
        set_capability(&mut caps, VD_AGENT_CAP_CLIPBOARD_GRAB_SERIAL);

        let mut payload = Vec::new();
        payload.extend_from_slice(&(request as u32).to_le_bytes());
        payload.extend_from_slice(&caps.to_le_bytes());

        self.send_message(VD_AGENT_ANNOUNCE_CAPABILITIES, &payload)?;
        info!(
            "sent capability announcement to {} (request={request})",
            self.path.display()
        );
        Ok(())
    }

    fn send_message(&self, message_type: u32, payload: &[u8]) -> Result<()> {
        let frame = encode_message_frame(message_type, payload);

        let mut writer = self
            .writer
            .lock()
            .map_err(|_| anyhow!("writer mutex poisoned"))?;
        writer
            .write_all(&frame)
            .with_context(|| format!("failed to write SPICE message type {message_type}"))?;
        writer
            .flush()
            .context("failed to flush SPICE virtio message")?;
        trace!(
            "wrote SPICE message type={message_type} payload={} bytes",
            payload.len()
        );
        Ok(())
    }
}

fn reader_loop(
    path: PathBuf,
    mut reader: File,
    writer: Arc<Mutex<File>>,
    state: Arc<Mutex<TransportState>>,
    sender: Sender<TransportEvent>,
) -> Result<()> {
    info!("listening for SPICE messages on {}", path.display());
    let mut assembler = MessageAssembler::default();

    loop {
        let (port, chunk) = read_one_chunk(&mut reader)?;
        let Some((port, message_type, data)) = assembler.push_chunk(port, &chunk)? else {
            continue;
        };
        trace!(
            "received SPICE frame port={port} type={message_type} size={}",
            data.len()
        );

        if port != VDP_CLIENT_PORT {
            trace!("ignoring non-client SPICE port {port}");
            continue;
        }

        match message_type {
            VD_AGENT_ANNOUNCE_CAPABILITIES => {
                let caps = parse_capabilities(&data)?;
                {
                    let mut guard = state.lock().map_err(|_| anyhow!("state mutex poisoned"))?;
                    guard.peer_caps = caps.clone();
                    guard.clipboard_by_demand =
                        has_capability(&caps, VD_AGENT_CAP_CLIPBOARD_BY_DEMAND);
                    guard.clipboard_selection =
                        has_capability(&caps, VD_AGENT_CAP_CLIPBOARD_SELECTION);
                    guard.clipboard_grab_serial =
                        has_capability(&caps, VD_AGENT_CAP_CLIPBOARD_GRAB_SERIAL);
                    guard.serial_counter = 0;

                    debug!(
                        "peer capabilities: clipboard_by_demand={} selection={} grab_serial={}",
                        guard.clipboard_by_demand,
                        guard.clipboard_selection,
                        guard.clipboard_grab_serial
                    );
                }

                let peer = {
                    let guard = state.lock().map_err(|_| anyhow!("state mutex poisoned"))?;
                    PeerCapabilities {
                        clipboard_by_demand: guard.clipboard_by_demand,
                        clipboard_selection: guard.clipboard_selection,
                        clipboard_grab_serial: guard.clipboard_grab_serial,
                    }
                };
                sender
                    .send(TransportEvent::PeerCapabilities(peer))
                    .context("failed to send PeerCapabilities event")?;

                if le_u32(&data[0..4]) != 0 {
                    debug!("peer requested capability reply");
                    let transport = SpiceTransport {
                        path: path.clone(),
                        writer: Arc::clone(&writer),
                        state: Arc::clone(&state),
                        max_text_bytes: usize::MAX,
                    };
                    transport.send_capabilities(false)?;
                }
            }
            VD_AGENT_CLIPBOARD_GRAB => {
                let (selection_id, serial, types) = parse_clipboard_grab(&state, &data)?;
                let Some(selection) = ClipboardSelection::from_spice_id(selection_id) else {
                    debug!("ignoring unsupported clipboard selection {}", selection_id);
                    continue;
                };
                sender
                    .send(TransportEvent::HostGrab {
                        selection,
                        types,
                        serial,
                    })
                    .context("failed to send HostGrab event")?;
            }
            VD_AGENT_CLIPBOARD_REQUEST => {
                let (selection_id, data_type) = parse_clipboard_request(&state, &data)?;
                let Some(selection) = ClipboardSelection::from_spice_id(selection_id) else {
                    debug!(
                        "ignoring unsupported clipboard request selection {}",
                        selection_id
                    );
                    continue;
                };
                sender
                    .send(TransportEvent::HostRequest {
                        selection,
                        data_type,
                    })
                    .context("failed to send HostRequest event")?;
            }
            VD_AGENT_CLIPBOARD => {
                let (selection_id, data_type, payload) = parse_clipboard_data(&state, &data)?;
                let Some(selection) = ClipboardSelection::from_spice_id(selection_id) else {
                    debug!(
                        "ignoring unsupported clipboard data selection {}",
                        selection_id
                    );
                    continue;
                };
                sender
                    .send(TransportEvent::HostData {
                        selection,
                        data_type,
                        data: payload,
                    })
                    .context("failed to send HostData event")?;
            }
            VD_AGENT_CLIPBOARD_RELEASE => {
                let selection_id = parse_clipboard_release(&state, &data)?;
                let Some(selection) = ClipboardSelection::from_spice_id(selection_id) else {
                    debug!(
                        "ignoring unsupported clipboard release selection {}",
                        selection_id
                    );
                    continue;
                };
                sender
                    .send(TransportEvent::HostRelease { selection })
                    .context("failed to send HostRelease event")?;
            }
            VD_AGENT_FILE_XFER_START => {
                let (id, metadata) = parse_file_xfer_start(&data)?;
                sender
                    .send(TransportEvent::HostFileXferStart { id, metadata })
                    .context("failed to send HostFileXferStart event")?;
            }
            VD_AGENT_FILE_XFER_DATA => {
                let (id, payload) = parse_file_xfer_data(&data)?;
                sender
                    .send(TransportEvent::HostFileXferData { id, data: payload })
                    .context("failed to send HostFileXferData event")?;
            }
            VD_AGENT_FILE_XFER_STATUS => {
                let (id, result) = parse_file_xfer_status(&data)?;
                sender
                    .send(TransportEvent::HostFileXferStatus { id, result })
                    .context("failed to send HostFileXferStatus event")?;
            }
            VD_AGENT_CLIENT_DISCONNECTED => {
                sender
                    .send(TransportEvent::HostClientDisconnected)
                    .context("failed to send HostClientDisconnected event")?;
            }
            other => {
                trace!("ignoring unsupported SPICE message type {other}");
            }
        }
    }
}
