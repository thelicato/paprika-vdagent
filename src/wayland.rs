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
use wl_clipboard_rs::utils::{PrimarySelectionCheckError, is_primary_selection_supported};

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

pub struct WaylandClipboard {
    max_text_bytes: usize,
    seat: SeatSelector,
    primary_supported: bool,
}

impl WaylandClipboard {
    pub fn new(max_text_bytes: usize, seat: SeatSelector) -> Result<Self> {
        let clipboard = Self {
            max_text_bytes,
            seat,
            primary_supported: detect_primary_selection_support()?,
        };
        clipboard.probe()?;
        Ok(clipboard)
    }

    pub fn read_text(&self, selection: ClipboardSelection) -> Result<Option<String>> {
        if !self.selection_supported(selection) {
            return Ok(None);
        }

        match get_contents(
            self.paste_clipboard_type(selection),
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
                        "ignoring Wayland {} payload larger than configured limit: {} > {}",
                        selection,
                        bytes.len(),
                        self.max_text_bytes
                    );
                    return Ok(None);
                }

                let text = String::from_utf8(bytes)
                    .context("Wayland clipboard did not contain valid UTF-8 text")?;
                Ok(Some(text))
            }
            Err(
                PasteError::NoSeats
                | PasteError::ClipboardEmpty
                | PasteError::NoMimeType
                | PasteError::PrimarySelectionUnsupported,
            ) => Ok(None),
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
        match self.read_text(ClipboardSelection::Clipboard) {
            Ok(_) => Ok(()),
            Err(err) => Err(err),
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
