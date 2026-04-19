use std::io::Read;

use anyhow::{Context, Result, bail};

use super::protocol::{VD_AGENT_PROTOCOL, VDI_CHUNK_MAX_DATA, VDP_CLIENT_PORT, le_u32};

#[derive(Default)]
pub(super) struct MessageAssembler {
    current_port: Option<u32>,
    buffer: Vec<u8>,
    expected_len: Option<usize>,
}

impl MessageAssembler {
    pub(super) fn push_chunk(
        &mut self,
        port: u32,
        chunk: &[u8],
    ) -> Result<Option<(u32, u32, Vec<u8>)>> {
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

pub(super) fn read_one_chunk<R: Read>(reader: &mut R) -> Result<(u32, Vec<u8>)> {
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

pub(super) fn encode_message_frame(message_type: u32, payload: &[u8]) -> Vec<u8> {
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

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::spice::protocol::{VD_AGENT_CLIPBOARD, VD_AGENT_FILE_XFER_DATA};

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
}
