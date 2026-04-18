use std::fmt;
use std::io::Read;

use anyhow::{Context, Result, bail};
use tracing::warn;
use wl_clipboard_rs::copy::{
    ClipboardType as CopyClipboardType, MimeType as CopyMimeType, Options as CopyOptions,
    Seat as CopySeat, Source as CopySource, clear as copy_clear, copy,
};
use wl_clipboard_rs::paste::{
    ClipboardType as PasteClipboardType, Error as PasteError, MimeType as PasteMimeType,
    Seat as PasteSeat, get_contents,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeatSelector {
    Unspecified,
    Specific(String),
}

impl SeatSelector {
    fn paste_seat(&self) -> PasteSeat<'_> {
        match self {
            Self::Unspecified => PasteSeat::Unspecified,
            Self::Specific(name) => PasteSeat::Specific(name.as_str()),
        }
    }

    fn copy_seat(&self) -> CopySeat {
        match self {
            Self::Unspecified => CopySeat::All,
            Self::Specific(name) => CopySeat::Specific(name.clone()),
        }
    }
}

impl fmt::Display for SeatSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unspecified => write!(f, "unspecified"),
            Self::Specific(name) => write!(f, "{name}"),
        }
    }
}

pub struct WaylandClipboard {
    max_text_bytes: usize,
    seat: SeatSelector,
}

impl WaylandClipboard {
    pub fn new(max_text_bytes: usize, seat: SeatSelector) -> Result<Self> {
        let clipboard = Self {
            max_text_bytes,
            seat,
        };
        clipboard.probe()?;
        Ok(clipboard)
    }

    pub fn read_text(&self) -> Result<Option<String>> {
        match get_contents(
            PasteClipboardType::Regular,
            self.seat.paste_seat(),
            PasteMimeType::Text,
        ) {
            Ok((pipe, _mime)) => {
                let mut bytes = Vec::new();
                pipe.take((self.max_text_bytes + 1) as u64)
                    .read_to_end(&mut bytes)
                    .context("failed to read Wayland clipboard contents")?;

                if bytes.len() > self.max_text_bytes {
                    warn!(
                        "ignoring Wayland clipboard payload larger than configured limit: {} > {}",
                        bytes.len(),
                        self.max_text_bytes
                    );
                    return Ok(None);
                }

                let text = String::from_utf8(bytes)
                    .context("Wayland clipboard did not contain valid UTF-8 text")?;
                Ok(Some(text))
            }
            Err(PasteError::NoSeats | PasteError::ClipboardEmpty | PasteError::NoMimeType) => {
                Ok(None)
            }
            Err(PasteError::MissingProtocol { name, version }) => {
                bail!("required Wayland clipboard protocol {name} v{version} is not available")
            }
            Err(err) => Err(err).context("failed to read Wayland clipboard"),
        }
    }

    pub fn write_text(&self, text: &str) -> Result<()> {
        if text.len() > self.max_text_bytes {
            bail!(
                "refusing to write clipboard text larger than configured limit: {} > {}",
                text.len(),
                self.max_text_bytes
            );
        }

        let mut options = CopyOptions::new();
        options.clipboard(CopyClipboardType::Regular);
        options.seat(self.seat.copy_seat());
        copy(
            options,
            CopySource::Bytes(text.as_bytes().to_vec().into()),
            CopyMimeType::Autodetect,
        )
        .context("failed to write Wayland clipboard text")
    }

    pub fn clear(&self) -> Result<()> {
        copy_clear(CopyClipboardType::Regular, self.seat.copy_seat())
            .context("failed to clear Wayland clipboard")
    }

    pub fn seat(&self) -> &SeatSelector {
        &self.seat
    }

    fn probe(&self) -> Result<()> {
        match self.read_text() {
            Ok(_) => Ok(()),
            Err(err) => Err(err),
        }
    }
}
