use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc::Sender};
use std::thread;

use anyhow::{Context, Result, anyhow, bail};
use tracing::{debug, info, trace, warn};

use crate::selection::ClipboardSelection;

const VDP_CLIENT_PORT: u32 = 1;
const VD_AGENT_PROTOCOL: u32 = 1;

const VD_AGENT_CLIPBOARD: u32 = 4;
const VD_AGENT_ANNOUNCE_CAPABILITIES: u32 = 6;
const VD_AGENT_CLIPBOARD_GRAB: u32 = 7;
const VD_AGENT_CLIPBOARD_REQUEST: u32 = 8;
const VD_AGENT_CLIPBOARD_RELEASE: u32 = 9;
const VD_AGENT_FILE_XFER_START: u32 = 10;
const VD_AGENT_FILE_XFER_STATUS: u32 = 11;
const VD_AGENT_FILE_XFER_DATA: u32 = 12;
const VD_AGENT_CLIENT_DISCONNECTED: u32 = 13;

const VD_AGENT_CLIPBOARD_NONE: u32 = 0;
pub const VD_AGENT_CLIPBOARD_UTF8_TEXT: u32 = 1;
pub const VD_AGENT_FILE_XFER_STATUS_CAN_SEND_DATA: u32 = 0;
pub const VD_AGENT_FILE_XFER_STATUS_CANCELLED: u32 = 1;
pub const VD_AGENT_FILE_XFER_STATUS_ERROR: u32 = 2;
pub const VD_AGENT_FILE_XFER_STATUS_SUCCESS: u32 = 3;
const VDI_CHUNK_MAX_DATA: usize = 1024;

const VD_AGENT_CAP_CLIPBOARD_BY_DEMAND: usize = 5;
const VD_AGENT_CAP_CLIPBOARD_SELECTION: usize = 6;
const VD_AGENT_CAP_CLIPBOARD_NO_RELEASE_ON_REGRAB: usize = 16;
const VD_AGENT_CAP_CLIPBOARD_GRAB_SERIAL: usize = 17;

#[derive(Debug, Clone)]
pub struct PeerCapabilities {
    pub clipboard_by_demand: bool,
    pub clipboard_selection: bool,
    pub clipboard_grab_serial: bool,
}

#[derive(Debug)]
pub enum TransportEvent {
    PeerCapabilities(PeerCapabilities),
    HostGrab {
        selection: ClipboardSelection,
        types: Vec<u32>,
        serial: Option<u32>,
    },
    HostRequest {
        selection: ClipboardSelection,
        data_type: u32,
    },
    HostData {
        selection: ClipboardSelection,
        data_type: u32,
        data: Vec<u8>,
    },
    HostRelease {
        selection: ClipboardSelection,
    },
    HostFileXferStart {
        id: u32,
        metadata: Vec<u8>,
    },
    HostFileXferData {
        id: u32,
        data: Vec<u8>,
    },
    HostFileXferStatus {
        id: u32,
        result: u32,
    },
    HostClientDisconnected,
    Disconnected(String),
}

#[derive(Debug, Default)]
struct TransportState {
    peer_caps: Vec<u32>,
    clipboard_by_demand: bool,
    clipboard_selection: bool,
    clipboard_grab_serial: bool,
    serial_counter: u32,
}

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

fn read_one_chunk<R: Read>(reader: &mut R) -> Result<(u32, Vec<u8>)> {
    let mut chunk_header = [0u8; 8];
    reader
        .read_exact(&mut chunk_header)
        .context("failed to read SPICE chunk header")?;

    let port = le_u32(&chunk_header[0..4]);
    let chunk_size = le_u32(&chunk_header[4..8]) as usize;
    if chunk_size == 0 {
        bail!("invalid SPICE message chunk size 0");
    }

    let mut chunk = vec![0u8; chunk_size];
    reader
        .read_exact(&mut chunk)
        .context("failed to read SPICE message chunk body")?;

    Ok((port, chunk))
}

fn decode_message(port: u32, message: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    if message.len() < 20 {
        bail!(
            "incomplete SPICE agent message header: {} bytes",
            message.len()
        );
    }

    let protocol = le_u32(&message[0..4]);
    if protocol != VD_AGENT_PROTOCOL {
        bail!("unexpected SPICE agent protocol version {protocol}");
    }

    let message_type = le_u32(&message[4..8]);
    let payload_size = le_u32(&message[16..20]) as usize;
    if 20 + payload_size > message.len() {
        bail!(
            "invalid SPICE message size field: header says {payload_size} bytes, frame has {}",
            message.len()
        );
    }

    let payload = message[20..20 + payload_size].to_vec();
    Ok((port, message_type, payload))
}

#[derive(Default)]
struct MessageAssembler {
    current_port: Option<u32>,
    buffer: Vec<u8>,
    expected_len: Option<usize>,
}

impl MessageAssembler {
    fn push_chunk(&mut self, port: u32, chunk: &[u8]) -> Result<Option<(u32, u32, Vec<u8>)>> {
        if self.buffer.is_empty() {
            if chunk.len() < 20 {
                bail!(
                    "initial SPICE agent chunk is too short for a header: {} bytes",
                    chunk.len()
                );
            }

            let payload_size = le_u32(&chunk[16..20]) as usize;
            let expected_len = 20 + payload_size;
            self.current_port = Some(port);
            self.expected_len = Some(expected_len);
            self.buffer.reserve(expected_len);
        } else if self.current_port != Some(port) {
            bail!(
                "SPICE message continuation changed ports: {:?} -> {port}",
                self.current_port
            );
        }

        self.buffer.extend_from_slice(chunk);
        let expected_len = self.expected_len.unwrap_or_default();
        if self.buffer.len() < expected_len {
            return Ok(None);
        }

        if self.buffer.len() > expected_len {
            bail!(
                "SPICE message assembly exceeded declared size: {} > {}",
                self.buffer.len(),
                expected_len
            );
        }

        let port = self.current_port.take().unwrap_or(port);
        let message = std::mem::take(&mut self.buffer);
        self.expected_len = None;
        decode_message(port, &message).map(Some)
    }
}

fn encode_message_frame(message_type: u32, payload: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(20 + payload.len());
    message.extend_from_slice(&VD_AGENT_PROTOCOL.to_le_bytes());
    message.extend_from_slice(&message_type.to_le_bytes());
    message.extend_from_slice(&0u64.to_le_bytes());
    message.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    message.extend_from_slice(payload);

    let mut frame = Vec::with_capacity(8 + message.len() + (message.len() / VDI_CHUNK_MAX_DATA));
    let mut offset = 0usize;
    while offset < message.len() {
        let chunk_len = (message.len() - offset).min(VDI_CHUNK_MAX_DATA);
        frame.extend_from_slice(&VDP_CLIENT_PORT.to_le_bytes());
        frame.extend_from_slice(&(chunk_len as u32).to_le_bytes());
        frame.extend_from_slice(&message[offset..offset + chunk_len]);
        offset += chunk_len;
    }
    frame
}

fn parse_capabilities(data: &[u8]) -> Result<Vec<u32>> {
    if data.len() < 8 || data.len() % 4 != 0 {
        bail!("invalid capabilities payload size {}", data.len());
    }

    let mut caps = Vec::new();
    for chunk in data[4..].chunks_exact(4) {
        caps.push(le_u32(chunk));
    }
    Ok(caps)
}

fn parse_clipboard_grab(
    state: &Arc<Mutex<TransportState>>,
    data: &[u8],
) -> Result<(u8, Option<u32>, Vec<u32>)> {
    let guard = state.lock().map_err(|_| anyhow!("state mutex poisoned"))?;
    let mut offset = 0usize;
    let mut selection = ClipboardSelection::Clipboard.spice_id();
    let mut serial = None;

    if guard.clipboard_selection {
        if data.len() < 4 {
            bail!("clipboard GRAB missing selection prefix");
        }
        selection = data[0];
        offset += 4;
    }

    if guard.clipboard_grab_serial {
        if data.len() < offset + 4 {
            bail!("clipboard GRAB missing serial field");
        }
        let value = le_u32(&data[offset..offset + 4]);
        if value < guard.serial_counter {
            debug!(
                "discarding stale host clipboard GRAB serial {} < {}",
                value, guard.serial_counter
            );
            return Ok((selection, Some(value), Vec::new()));
        }
        serial = Some(value);
        drop(guard);

        let mut guard = state.lock().map_err(|_| anyhow!("state mutex poisoned"))?;
        guard.serial_counter = value;
        offset += 4;
    }

    let types_bytes = &data[offset..];
    if types_bytes.len() % 4 != 0 {
        bail!("clipboard GRAB has non-aligned type list");
    }

    let mut types = Vec::new();
    for chunk in types_bytes.chunks_exact(4) {
        types.push(le_u32(chunk));
    }

    debug!("received host -> guest GRAB selection={selection} serial={serial:?} types={types:?}");
    Ok((selection, serial, types))
}

fn parse_clipboard_request(state: &Arc<Mutex<TransportState>>, data: &[u8]) -> Result<(u8, u32)> {
    let guard = state.lock().map_err(|_| anyhow!("state mutex poisoned"))?;
    let mut offset = 0usize;
    let mut selection = ClipboardSelection::Clipboard.spice_id();

    if guard.clipboard_selection {
        if data.len() < 8 {
            bail!("clipboard REQUEST too short for selection + type");
        }
        selection = data[0];
        offset += 4;
    } else if data.len() < 4 {
        bail!("clipboard REQUEST too short");
    }

    let data_type = le_u32(&data[offset..offset + 4]);
    debug!("received host -> guest REQUEST selection={selection} type={data_type}");
    Ok((selection, data_type))
}

fn parse_clipboard_data(
    state: &Arc<Mutex<TransportState>>,
    data: &[u8],
) -> Result<(u8, u32, Vec<u8>)> {
    let guard = state.lock().map_err(|_| anyhow!("state mutex poisoned"))?;
    let mut offset = 0usize;
    let mut selection = ClipboardSelection::Clipboard.spice_id();

    if guard.clipboard_selection {
        if data.len() < 8 {
            bail!("clipboard DATA too short for selection + type");
        }
        selection = data[0];
        offset += 4;
    } else if data.len() < 4 {
        bail!("clipboard DATA too short");
    }

    let data_type = le_u32(&data[offset..offset + 4]);
    offset += 4;
    let payload = data[offset..].to_vec();
    debug!(
        "received host -> guest DATA selection={selection} type={data_type} bytes={}",
        payload.len()
    );
    Ok((selection, data_type, payload))
}

fn parse_clipboard_release(state: &Arc<Mutex<TransportState>>, data: &[u8]) -> Result<u8> {
    let guard = state.lock().map_err(|_| anyhow!("state mutex poisoned"))?;
    let selection = if guard.clipboard_selection {
        if data.len() < 4 {
            bail!("clipboard RELEASE too short for selection");
        }
        data[0]
    } else {
        ClipboardSelection::Clipboard.spice_id()
    };

    debug!("received host -> guest RELEASE selection={selection}");
    Ok(selection)
}

fn parse_file_xfer_start(data: &[u8]) -> Result<(u32, Vec<u8>)> {
    if data.len() < 5 {
        bail!("file transfer START is too short");
    }

    let id = le_u32(&data[0..4]);
    let metadata = data[4..].to_vec();
    debug!(
        "received host -> guest file transfer START id={id} metadata={} bytes",
        metadata.len()
    );
    Ok((id, metadata))
}

fn parse_file_xfer_data(data: &[u8]) -> Result<(u32, Vec<u8>)> {
    if data.len() < 12 {
        bail!("file transfer DATA is too short");
    }

    let id = le_u32(&data[0..4]);
    let declared_size = le_u64(&data[4..12]) as usize;
    let payload = data[12..].to_vec();
    if payload.len() != declared_size {
        bail!(
            "file transfer DATA size mismatch for id {id}: header says {declared_size}, payload has {}",
            payload.len()
        );
    }

    debug!(
        "received host -> guest file transfer DATA id={id} bytes={}",
        payload.len()
    );
    Ok((id, payload))
}

fn parse_file_xfer_status(data: &[u8]) -> Result<(u32, u32)> {
    if data.len() < 8 {
        bail!("file transfer STATUS is too short");
    }

    let id = le_u32(&data[0..4]);
    let result = le_u32(&data[4..8]);
    debug!("received host -> guest file transfer STATUS id={id} result={result}");
    Ok((id, result))
}

fn selection_prefix(selection: ClipboardSelection) -> [u8; 4] {
    [selection.spice_id(), 0, 0, 0]
}

fn set_capability(bits: &mut u32, cap: usize) {
    *bits |= 1u32 << cap;
}

fn has_capability(caps: &[u32], cap: usize) -> bool {
    let index = cap / 32;
    let bit = cap % 32;
    caps.get(index)
        .map(|value| (value & (1u32 << bit)) != 0)
        .unwrap_or(false)
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[0..4].try_into().expect("slice length checked"))
}

fn le_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes[0..8].try_into().expect("slice length checked"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn state_with(
        selection: bool,
        serial: bool,
        serial_counter: u32,
    ) -> Arc<Mutex<TransportState>> {
        Arc::new(Mutex::new(TransportState {
            peer_caps: Vec::new(),
            clipboard_by_demand: true,
            clipboard_selection: selection,
            clipboard_grab_serial: serial,
            serial_counter,
        }))
    }

    #[test]
    fn encoded_frame_roundtrips_through_reader() {
        let payload = b"hello clipboard";
        let frame = encode_message_frame(VD_AGENT_CLIPBOARD, payload);
        let mut cursor = Cursor::new(frame);
        let mut assembler = MessageAssembler::default();

        let mut decoded = None;
        while (cursor.position() as usize) < cursor.get_ref().len() {
            let (port, chunk) = read_one_chunk(&mut cursor).expect("chunk should decode");
            decoded = assembler
                .push_chunk(port, &chunk)
                .expect("message assembly should succeed");
        }

        let (port, message_type, decoded_payload) = decoded.expect("frame should decode");

        assert_eq!(port, VDP_CLIENT_PORT);
        assert_eq!(message_type, VD_AGENT_CLIPBOARD);
        assert_eq!(decoded_payload, payload);
    }

    #[test]
    fn large_frame_roundtrips_across_multiple_chunks() {
        let payload = vec![0x5a; 4096];
        let frame = encode_message_frame(VD_AGENT_FILE_XFER_DATA, &payload);
        let mut cursor = Cursor::new(frame);
        let mut assembler = MessageAssembler::default();
        let mut decoded = None;

        while (cursor.position() as usize) < cursor.get_ref().len() {
            let (port, chunk) = read_one_chunk(&mut cursor).expect("chunk should decode");
            decoded = assembler
                .push_chunk(port, &chunk)
                .expect("message assembly should succeed");
        }

        let (port, message_type, decoded_payload) = decoded.expect("message should decode");
        assert_eq!(port, VDP_CLIENT_PORT);
        assert_eq!(message_type, VD_AGENT_FILE_XFER_DATA);
        assert_eq!(decoded_payload, payload);
    }

    #[test]
    fn parse_capabilities_extracts_words_after_request_flag() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(&0x1234_5678u32.to_le_bytes());
        payload.extend_from_slice(&0x9abc_def0u32.to_le_bytes());

        let caps = parse_capabilities(&payload).expect("capabilities should parse");
        assert_eq!(caps, vec![0x1234_5678, 0x9abc_def0]);
    }

    #[test]
    fn clipboard_grab_parses_clipboard_selection_and_serial() {
        let state = state_with(true, true, 4);
        let mut payload = Vec::new();
        payload.extend_from_slice(&selection_prefix(ClipboardSelection::Clipboard));
        payload.extend_from_slice(&9u32.to_le_bytes());
        payload.extend_from_slice(&VD_AGENT_CLIPBOARD_UTF8_TEXT.to_le_bytes());

        let (selection, serial, types) =
            parse_clipboard_grab(&state, &payload).expect("clipboard grab should parse");

        assert_eq!(selection, ClipboardSelection::Clipboard.spice_id());
        assert_eq!(serial, Some(9));
        assert_eq!(types, vec![VD_AGENT_CLIPBOARD_UTF8_TEXT]);
        assert_eq!(state.lock().unwrap().serial_counter, 9);
    }

    #[test]
    fn clipboard_grab_parses_primary_selection() {
        let state = state_with(true, false, 0);
        let mut payload = Vec::new();
        payload.extend_from_slice(&selection_prefix(ClipboardSelection::Primary));
        payload.extend_from_slice(&VD_AGENT_CLIPBOARD_UTF8_TEXT.to_le_bytes());

        let (selection, serial, types) =
            parse_clipboard_grab(&state, &payload).expect("clipboard grab should parse");

        assert_eq!(selection, ClipboardSelection::Primary.spice_id());
        assert_eq!(serial, None);
        assert_eq!(types, vec![VD_AGENT_CLIPBOARD_UTF8_TEXT]);
    }

    #[test]
    fn stale_clipboard_grab_serial_is_discarded() {
        let state = state_with(true, true, 10);
        let mut payload = Vec::new();
        payload.extend_from_slice(&selection_prefix(ClipboardSelection::Clipboard));
        payload.extend_from_slice(&3u32.to_le_bytes());
        payload.extend_from_slice(&VD_AGENT_CLIPBOARD_UTF8_TEXT.to_le_bytes());

        let (selection, serial, types) =
            parse_clipboard_grab(&state, &payload).expect("clipboard grab should parse");

        assert_eq!(selection, ClipboardSelection::Clipboard.spice_id());
        assert_eq!(serial, Some(3));
        assert!(types.is_empty());
        assert_eq!(state.lock().unwrap().serial_counter, 10);
    }

    #[test]
    fn parse_file_xfer_start_extracts_id_and_metadata() {
        let (id, metadata) =
            parse_file_xfer_start(&[7, 0, 0, 0, b'a', b'b']).expect("start should parse");

        assert_eq!(id, 7);
        assert_eq!(metadata, b"ab");
    }

    #[test]
    fn parse_file_xfer_data_validates_declared_size() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&9u32.to_le_bytes());
        payload.extend_from_slice(&3u64.to_le_bytes());
        payload.extend_from_slice(b"hey");

        let (id, data) = parse_file_xfer_data(&payload).expect("data should parse");
        assert_eq!(id, 9);
        assert_eq!(data, b"hey");
    }
}
