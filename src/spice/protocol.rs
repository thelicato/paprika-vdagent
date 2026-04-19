use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use tracing::debug;

use crate::selection::ClipboardSelection;

pub(super) const VDP_CLIENT_PORT: u32 = 1;
pub(super) const VD_AGENT_PROTOCOL: u32 = 1;
pub(super) const VDI_CHUNK_MAX_DATA: usize = 1024;

pub(super) const VD_AGENT_CLIPBOARD: u32 = 4;
pub(super) const VD_AGENT_ANNOUNCE_CAPABILITIES: u32 = 6;
pub(super) const VD_AGENT_CLIPBOARD_GRAB: u32 = 7;
pub(super) const VD_AGENT_CLIPBOARD_REQUEST: u32 = 8;
pub(super) const VD_AGENT_CLIPBOARD_RELEASE: u32 = 9;
pub(super) const VD_AGENT_FILE_XFER_START: u32 = 10;
pub(super) const VD_AGENT_FILE_XFER_STATUS: u32 = 11;
pub(super) const VD_AGENT_FILE_XFER_DATA: u32 = 12;
pub(super) const VD_AGENT_CLIENT_DISCONNECTED: u32 = 13;

pub(super) const VD_AGENT_CLIPBOARD_NONE: u32 = 0;
pub const VD_AGENT_CLIPBOARD_UTF8_TEXT: u32 = 1;

pub const VD_AGENT_FILE_XFER_STATUS_CAN_SEND_DATA: u32 = 0;
pub const VD_AGENT_FILE_XFER_STATUS_CANCELLED: u32 = 1;
pub const VD_AGENT_FILE_XFER_STATUS_ERROR: u32 = 2;
pub const VD_AGENT_FILE_XFER_STATUS_SUCCESS: u32 = 3;

pub(super) const VD_AGENT_CAP_CLIPBOARD_BY_DEMAND: usize = 5;
pub(super) const VD_AGENT_CAP_CLIPBOARD_SELECTION: usize = 6;
pub(super) const VD_AGENT_CAP_CLIPBOARD_NO_RELEASE_ON_REGRAB: usize = 16;
pub(super) const VD_AGENT_CAP_CLIPBOARD_GRAB_SERIAL: usize = 17;

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
pub(super) struct TransportState {
    pub(super) peer_caps: Vec<u32>,
    pub(super) clipboard_by_demand: bool,
    pub(super) clipboard_selection: bool,
    pub(super) clipboard_grab_serial: bool,
    pub(super) serial_counter: u32,
}

pub(super) fn parse_capabilities(data: &[u8]) -> Result<Vec<u32>> {
    if data.len() < 8 || data.len() % 4 != 0 {
        bail!("invalid capabilities payload size {}", data.len());
    }

    let mut caps = Vec::new();
    for chunk in data[4..].chunks_exact(4) {
        caps.push(le_u32(chunk));
    }
    Ok(caps)
}

pub(super) fn parse_clipboard_grab(
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

pub(super) fn parse_clipboard_request(
    state: &Arc<Mutex<TransportState>>,
    data: &[u8],
) -> Result<(u8, u32)> {
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

pub(super) fn parse_clipboard_data(
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

pub(super) fn parse_clipboard_release(
    state: &Arc<Mutex<TransportState>>,
    data: &[u8],
) -> Result<u8> {
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

pub(super) fn parse_file_xfer_start(data: &[u8]) -> Result<(u32, Vec<u8>)> {
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

pub(super) fn parse_file_xfer_data(data: &[u8]) -> Result<(u32, Vec<u8>)> {
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

pub(super) fn parse_file_xfer_status(data: &[u8]) -> Result<(u32, u32)> {
    if data.len() < 8 {
        bail!("file transfer STATUS is too short");
    }

    let id = le_u32(&data[0..4]);
    let result = le_u32(&data[4..8]);
    debug!("received host -> guest file transfer STATUS id={id} result={result}");
    Ok((id, result))
}

pub(super) fn selection_prefix(selection: ClipboardSelection) -> [u8; 4] {
    [selection.spice_id(), 0, 0, 0]
}

pub(super) fn set_capability(bits: &mut u32, cap: usize) {
    *bits |= 1u32 << cap;
}

pub(super) fn has_capability(caps: &[u32], cap: usize) -> bool {
    let index = cap / 32;
    let bit = cap % 32;
    caps.get(index)
        .map(|value| (value & (1u32 << bit)) != 0)
        .unwrap_or(false)
}

pub(super) fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[0..4].try_into().expect("slice length checked"))
}

pub(super) fn le_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes[0..8].try_into().expect("slice length checked"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
