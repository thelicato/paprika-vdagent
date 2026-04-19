mod codec;
mod protocol;
mod transport;

pub use protocol::{
    PeerCapabilities, TransportEvent, VD_AGENT_CLIPBOARD_UTF8_TEXT,
    VD_AGENT_FILE_XFER_STATUS_CAN_SEND_DATA, VD_AGENT_FILE_XFER_STATUS_CANCELLED,
    VD_AGENT_FILE_XFER_STATUS_ERROR, VD_AGENT_FILE_XFER_STATUS_SUCCESS,
};
pub use transport::SpiceTransport;
