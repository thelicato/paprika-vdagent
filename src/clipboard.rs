use std::sync::mpsc::Sender;

use anyhow::Result;

use crate::selection::ClipboardSelection;

/// A successful empty read is distinct from a failed transfer. Retry keeps the
/// bridge's cached selection intact until a later attempt succeeds.
#[derive(Debug, PartialEq, Eq)]
pub enum ClipboardRead {
    Ready(Option<String>),
    Retry,
}

#[derive(Debug)]
pub enum ClipboardEvent {
    SelectionChanged(ClipboardSelection),
    Disconnected(String),
}

/// Session clipboard operations used by the SPICE bridge. Implementations own
/// their display connections and translate native events into ClipboardEvent.
pub trait Clipboard {
    fn name(&self) -> &'static str;

    fn read_text(&self, selection: ClipboardSelection) -> Result<ClipboardRead>;

    /// Return false if the backend cannot serve this selection.
    fn write_text(&self, selection: ClipboardSelection, text: &str) -> Result<bool>;

    /// Return false if the backend cannot clear this selection.
    fn clear(&self, selection: ClipboardSelection) -> Result<bool>;

    fn selection_supported(&self, selection: ClipboardSelection) -> bool;

    /// Report startup failures synchronously so the bridge can poll instead.
    /// Once started, send Disconnected if watching stops unexpectedly.
    fn start_watcher(&self, sender: Sender<ClipboardEvent>) -> Result<()>;
}
