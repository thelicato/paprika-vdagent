use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc::Sender};
use std::thread;

use anyhow::{Context, Result, anyhow, bail};
use tracing::{debug, info, trace, warn};

const VDP_CLIENT_PORT: u32 = 1;
const VD_AGENT_PROTOCOL: u32 = 1;

const VD_AGENT_CLIPBOARD: u32 = 4;
const VD_AGENT_ANNOUNCE_CAPABILITIES: u32 = 6;
const VD_AGENT_CLIPBOARD_GRAB: u32 = 7;
const VD_AGENT_CLIPBOARD_REQUEST: u32 = 8;
const VD_AGENT_CLIPBOARD_RELEASE: u32 = 9;

const VD_AGENT_CLIPBOARD_NONE: u32 = 0;
pub const VD_AGENT_CLIPBOARD_UTF8_TEXT: u32 = 1;

const VD_AGENT_CAP_CLIPBOARD_BY_DEMAND: usize = 5;
const VD_AGENT_CAP_CLIPBOARD_SELECTION: usize = 6;
const VD_AGENT_CAP_CLIPBOARD_NO_RELEASE_ON_REGRAB: usize = 16;
const VD_AGENT_CAP_CLIPBOARD_GRAB_SERIAL: usize = 17;

const VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD: u8 = 0;

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
        types: Vec<u32>,
        serial: Option<u32>,
    },
    HostRequest {
        data_type: u32,
    },
    HostData {
        data_type: u32,
        data: Vec<u8>,
    },
    HostRelease,
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

    pub fn send_clipboard_grab_text(&self) -> Result<bool> {
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
            payload.extend_from_slice(&selection_prefix());
        }
        if clipboard_grab_serial {
            payload.extend_from_slice(&serial.to_le_bytes());
        }
        payload.extend_from_slice(&VD_AGENT_CLIPBOARD_UTF8_TEXT.to_le_bytes());

        self.send_message(VD_AGENT_CLIPBOARD_GRAB, &payload)?;
        info!("sent guest -> host clipboard GRAB (utf8-text, serial={serial})");
        Ok(true)
    }

    pub fn send_clipboard_request_text(&self) -> Result<()> {
        let clipboard_selection = self
            .state
            .lock()
            .map_err(|_| anyhow!("transport mutex poisoned"))?
            .clipboard_selection;

        let mut payload = Vec::new();
        if clipboard_selection {
            payload.extend_from_slice(&selection_prefix());
        }
        payload.extend_from_slice(&VD_AGENT_CLIPBOARD_UTF8_TEXT.to_le_bytes());
        self.send_message(VD_AGENT_CLIPBOARD_REQUEST, &payload)?;
        info!("sent guest -> host clipboard REQUEST (utf8-text)");
        Ok(())
    }

    pub fn send_clipboard_text(&self, text: &str) -> Result<bool> {
        if text.len() > self.max_text_bytes {
            warn!(
                "refusing to send clipboard text larger than configured limit: {} > {}",
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
            payload.extend_from_slice(&selection_prefix());
        }
        payload.extend_from_slice(&VD_AGENT_CLIPBOARD_UTF8_TEXT.to_le_bytes());
        payload.extend_from_slice(text.as_bytes());

        self.send_message(VD_AGENT_CLIPBOARD, &payload)?;
        info!("sent guest -> host clipboard DATA ({} bytes)", text.len());
        Ok(true)
    }

    pub fn send_empty_clipboard_data(&self) -> Result<()> {
        let clipboard_selection = self
            .state
            .lock()
            .map_err(|_| anyhow!("transport mutex poisoned"))?
            .clipboard_selection;

        let mut payload = Vec::new();
        if clipboard_selection {
            payload.extend_from_slice(&selection_prefix());
        }
        payload.extend_from_slice(&VD_AGENT_CLIPBOARD_NONE.to_le_bytes());

        self.send_message(VD_AGENT_CLIPBOARD, &payload)?;
        info!("sent guest -> host empty clipboard DATA");
        Ok(())
    }

    pub fn send_clipboard_release(&self) -> Result<bool> {
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
            payload.extend_from_slice(&selection_prefix());
        }

        self.send_message(VD_AGENT_CLIPBOARD_RELEASE, &payload)?;
        info!("sent guest -> host clipboard RELEASE");
        Ok(true)
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
        let mut message = Vec::with_capacity(20 + payload.len());
        message.extend_from_slice(&VD_AGENT_PROTOCOL.to_le_bytes());
        message.extend_from_slice(&message_type.to_le_bytes());
        message.extend_from_slice(&0u64.to_le_bytes());
        message.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        message.extend_from_slice(payload);

        let mut frame = Vec::with_capacity(8 + message.len());
        frame.extend_from_slice(&VDP_CLIENT_PORT.to_le_bytes());
        frame.extend_from_slice(&(message.len() as u32).to_le_bytes());
        frame.extend_from_slice(&message);

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

    loop {
        let (port, message_type, data) = read_one_message(&mut reader)?;
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
                let (selection, serial, types) = parse_clipboard_grab(&state, &data)?;
                if selection != VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD {
                    debug!("ignoring non-regular clipboard selection {selection}");
                    continue;
                }
                sender
                    .send(TransportEvent::HostGrab { types, serial })
                    .context("failed to send HostGrab event")?;
            }
            VD_AGENT_CLIPBOARD_REQUEST => {
                let (selection, data_type) = parse_clipboard_request(&state, &data)?;
                if selection != VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD {
                    debug!("ignoring non-regular clipboard request for selection {selection}");
                    continue;
                }
                sender
                    .send(TransportEvent::HostRequest { data_type })
                    .context("failed to send HostRequest event")?;
            }
            VD_AGENT_CLIPBOARD => {
                let (selection, data_type, payload) = parse_clipboard_data(&state, &data)?;
                if selection != VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD {
                    debug!("ignoring non-regular clipboard data for selection {selection}");
                    continue;
                }
                sender
                    .send(TransportEvent::HostData {
                        data_type,
                        data: payload,
                    })
                    .context("failed to send HostData event")?;
            }
            VD_AGENT_CLIPBOARD_RELEASE => {
                let selection = parse_clipboard_release(&state, &data)?;
                if selection != VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD {
                    debug!("ignoring non-regular clipboard release for selection {selection}");
                    continue;
                }
                sender
                    .send(TransportEvent::HostRelease)
                    .context("failed to send HostRelease event")?;
            }
            other => {
                trace!("ignoring unsupported SPICE message type {other}");
            }
        }
    }
}

fn read_one_message(reader: &mut File) -> Result<(u32, u32, Vec<u8>)> {
    let mut chunk_header = [0u8; 8];
    reader
        .read_exact(&mut chunk_header)
        .context("failed to read SPICE chunk header")?;

    let port = le_u32(&chunk_header[0..4]);
    let chunk_size = le_u32(&chunk_header[4..8]) as usize;
    if chunk_size < 20 {
        bail!("invalid SPICE message chunk size {chunk_size}");
    }

    let mut message = vec![0u8; chunk_size];
    reader
        .read_exact(&mut message)
        .context("failed to read SPICE message body")?;

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
    let mut selection = VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD;
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

    debug!(
        "received host -> guest clipboard GRAB selection={selection} serial={serial:?} types={types:?}"
    );
    Ok((selection, serial, types))
}

fn parse_clipboard_request(state: &Arc<Mutex<TransportState>>, data: &[u8]) -> Result<(u8, u32)> {
    let guard = state.lock().map_err(|_| anyhow!("state mutex poisoned"))?;
    let mut offset = 0usize;
    let mut selection = VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD;

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
    debug!("received host -> guest clipboard REQUEST selection={selection} type={data_type}");
    Ok((selection, data_type))
}

fn parse_clipboard_data(
    state: &Arc<Mutex<TransportState>>,
    data: &[u8],
) -> Result<(u8, u32, Vec<u8>)> {
    let guard = state.lock().map_err(|_| anyhow!("state mutex poisoned"))?;
    let mut offset = 0usize;
    let mut selection = VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD;

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
        "received host -> guest clipboard DATA selection={selection} type={data_type} bytes={}",
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
        VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD
    };

    debug!("received host -> guest clipboard RELEASE selection={selection}");
    Ok(selection)
}

fn selection_prefix() -> [u8; 4] {
    [VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD, 0, 0, 0]
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
