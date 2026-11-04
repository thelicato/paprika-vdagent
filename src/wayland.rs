use std::fmt;
use std::io;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tracing::warn;
use wl_clipboard_rs::copy::{
    ClipboardType as CopyClipboardType, MimeType as CopyMimeType, Options as CopyOptions,
    Seat as CopySeat, Source as CopySource, clear as copy_clear, copy,
};
use wl_clipboard_rs::paste::{
    ClipboardType as PasteClipboardType, Error as PasteError, MimeType as PasteMimeType,
    Seat as PasteSeat, get_contents, get_mime_types,
};
use wl_clipboard_rs::utils::{PrimarySelectionCheckError, is_primary_selection_supported};

use crate::clipboard_io::read_clipboard_bytes;
use crate::selection::ClipboardSelection;

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

#[derive(Debug, PartialEq, Eq)]
pub enum ClipboardRead {
    Ready(Option<String>),
    Retry,
}

pub struct WaylandClipboard {
    read_timeout: Duration,
    max_text_bytes: usize,
    seat: SeatSelector,
    primary_supported: bool,
}

impl WaylandClipboard {
    pub fn new(max_text_bytes: usize, seat: SeatSelector, read_timeout: Duration) -> Result<Self> {
        let clipboard = Self {
            read_timeout,
            max_text_bytes,
            seat,
            primary_supported: detect_primary_selection_support()?,
        };
        clipboard.probe()?;
        Ok(clipboard)
    }

    pub fn read_text(&self, selection: ClipboardSelection) -> Result<ClipboardRead> {
        if !self.selection_supported(selection) {
            return Ok(ClipboardRead::Ready(None));
        }

        match get_contents(
            self.paste_clipboard_type(selection),
            self.seat.paste_seat(),
            PasteMimeType::Text,
        ) {
            Ok((pipe, _mime)) => {
                let bytes = match read_clipboard_bytes(pipe, self.max_text_bytes, self.read_timeout)
                {
                    Ok(bytes) => bytes,
                    Err(err) if err.kind() == io::ErrorKind::TimedOut => {
                        warn!(
                            "Wayland {selection} read timed out after {} ms; keeping the cached selection and retrying",
                            self.read_timeout.as_millis()
                        );
                        return Ok(ClipboardRead::Retry);
                    }
                    Err(err) => {
                        return Err(err).context("failed to read Wayland clipboard contents");
                    }
                };

                if bytes.len() > self.max_text_bytes {
                    warn!(
                        "ignoring Wayland {} payload larger than configured limit: {} > {}",
                        selection,
                        bytes.len(),
                        self.max_text_bytes
                    );
                    return Ok(ClipboardRead::Ready(None));
                }

                let text = String::from_utf8(bytes)
                    .context("Wayland clipboard did not contain valid UTF-8 text")?;
                Ok(ClipboardRead::Ready(Some(text)))
            }
            Err(
                PasteError::NoSeats
                | PasteError::ClipboardEmpty
                | PasteError::NoMimeType
                | PasteError::PrimarySelectionUnsupported,
            ) => Ok(ClipboardRead::Ready(None)),
            Err(PasteError::MissingProtocol { name, version }) => {
                bail!("required Wayland clipboard protocol {name} v{version} is not available")
            }
            Err(err) => Err(err).context("failed to read Wayland clipboard"),
        }
    }

    pub fn write_text(&self, selection: ClipboardSelection, text: &str) -> Result<bool> {
        if !self.selection_supported(selection) {
            return Ok(false);
        }

        if text.len() > self.max_text_bytes {
            bail!(
                "refusing to write {} text larger than configured limit: {} > {}",
                selection,
                text.len(),
                self.max_text_bytes
            );
        }

        let mut options = CopyOptions::new();
        options.clipboard(self.copy_clipboard_type(selection));
        options.seat(self.seat.copy_seat());
        copy(
            options,
            CopySource::Bytes(text.as_bytes().to_vec().into()),
            CopyMimeType::Autodetect,
        )
        .context("failed to write Wayland clipboard text")?;

        Ok(true)
    }

    pub fn clear(&self, selection: ClipboardSelection) -> Result<bool> {
        if !self.selection_supported(selection) {
            return Ok(false);
        }

        copy_clear(self.copy_clipboard_type(selection), self.seat.copy_seat())
            .context("failed to clear Wayland clipboard")?;

        Ok(true)
    }

    pub fn seat(&self) -> &SeatSelector {
        &self.seat
    }

    pub fn selection_supported(&self, selection: ClipboardSelection) -> bool {
        match selection {
            ClipboardSelection::Clipboard => true,
            ClipboardSelection::Primary => self.primary_supported,
        }
    }

    fn probe(&self) -> Result<()> {
        // Probe the protocol and seat without asking a clipboard owner for data.
        match get_mime_types(PasteClipboardType::Regular, self.seat.paste_seat()) {
            Ok(_) | Err(PasteError::ClipboardEmpty | PasteError::NoMimeType) => Ok(()),
            Err(err) => Err(err).context("failed to probe Wayland clipboard"),
        }
    }

    fn copy_clipboard_type(&self, selection: ClipboardSelection) -> CopyClipboardType {
        match selection {
            ClipboardSelection::Clipboard => CopyClipboardType::Regular,
            ClipboardSelection::Primary => CopyClipboardType::Primary,
        }
    }

    fn paste_clipboard_type(&self, selection: ClipboardSelection) -> PasteClipboardType {
        match selection {
            ClipboardSelection::Clipboard => PasteClipboardType::Regular,
            ClipboardSelection::Primary => PasteClipboardType::Primary,
        }
    }
}

fn detect_primary_selection_support() -> Result<bool> {
    match is_primary_selection_supported() {
        Ok(supported) => Ok(supported),
        Err(PrimarySelectionCheckError::NoSeats | PrimarySelectionCheckError::MissingProtocol) => {
            Ok(false)
        }
        Err(err) => Err(err).context("failed to detect Wayland primary selection support"),
    }
}
