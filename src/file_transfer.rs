use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use crate::spice::{
    SpiceTransport, VD_AGENT_FILE_XFER_STATUS_CAN_SEND_DATA, VD_AGENT_FILE_XFER_STATUS_CANCELLED,
    VD_AGENT_FILE_XFER_STATUS_ERROR, VD_AGENT_FILE_XFER_STATUS_SUCCESS,
};

const DEFAULT_MAX_ACTIVE_TRANSFERS: usize = 8;
const MAX_FILENAME_ATTEMPTS: usize = 64;
const TRANSFER_SECTION: &str = "vdagent-file-xfer";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTransferMetadata {
    pub name: String,
    pub size: u64,
}

pub struct FileTransferManager {
    save_dir: Option<PathBuf>,
    active: HashMap<u32, IncomingTransfer>,
    max_active_transfers: usize,
}

struct IncomingTransfer {
    id: u32,
    metadata: FileTransferMetadata,
    temp_path: PathBuf,
    final_path: PathBuf,
    file: File,
    written_size: u64,
}

impl FileTransferManager {
    pub fn new(save_dir: Option<PathBuf>, max_active_transfers: Option<usize>) -> Result<Self> {
        let max_active_transfers = max_active_transfers
            .unwrap_or(DEFAULT_MAX_ACTIVE_TRANSFERS)
            .max(1);
        let save_dir = prepare_save_dir(save_dir)?;

        Ok(Self {
            save_dir,
            active: HashMap::new(),
            max_active_transfers,
        })
    }

    pub fn save_dir(&self) -> Option<&Path> {
        self.save_dir.as_deref()
    }

    pub fn max_active_transfers(&self) -> usize {
        self.max_active_transfers
    }

    pub fn handle_start(
        &mut self,
        transport: &SpiceTransport,
        id: u32,
        metadata_bytes: &[u8],
    ) -> Result<()> {
        let Some(save_dir) = self.save_dir.as_ref() else {
            warn!("host started file transfer {id}, but no guest save directory is available");
            transport.send_file_xfer_status(id, VD_AGENT_FILE_XFER_STATUS_ERROR)?;
            return Ok(());
        };

        if self.active.contains_key(&id) {
            warn!("host reused active file transfer id={id}, rejecting it");
            transport.send_file_xfer_status(id, VD_AGENT_FILE_XFER_STATUS_ERROR)?;
            return Ok(());
        }

        if self.active.len() >= self.max_active_transfers {
            warn!(
                "too many active file transfers ({}), rejecting id={id}",
                self.active.len()
            );
            transport.send_file_xfer_status(id, VD_AGENT_FILE_XFER_STATUS_ERROR)?;
            return Ok(());
        }

        let metadata = match parse_file_transfer_metadata(metadata_bytes) {
            Ok(metadata) => metadata,
            Err(err) => {
                warn!("failed to parse file transfer metadata for id={id}: {err:#}");
                transport.send_file_xfer_status(id, VD_AGENT_FILE_XFER_STATUS_ERROR)?;
                return Ok(());
            }
        };

        let transfer = match IncomingTransfer::create(save_dir, id, metadata) {
            Ok(transfer) => transfer,
            Err(err) => {
                warn!("failed to prepare file transfer id={id}: {err:#}");
                transport.send_file_xfer_status(id, VD_AGENT_FILE_XFER_STATUS_ERROR)?;
                return Ok(());
            }
        };

        info!(
            "accepting host file transfer id={} name={} size={} bytes destination={}",
            transfer.id,
            transfer.metadata.name,
            transfer.metadata.size,
            transfer.final_path.display()
        );

        self.active.insert(id, transfer);
        transport.send_file_xfer_status(id, VD_AGENT_FILE_XFER_STATUS_CAN_SEND_DATA)?;
        Ok(())
    }

    pub fn handle_data(
        &mut self,
        transport: &SpiceTransport,
        id: u32,
        payload: &[u8],
    ) -> Result<()> {
        let Some(mut transfer) = self.active.remove(&id) else {
            warn!("received file transfer DATA for unknown id={id}, cancelling it");
            transport.send_file_xfer_status(id, VD_AGENT_FILE_XFER_STATUS_CANCELLED)?;
            return Ok(());
        };

        if let Err(err) = transfer.write_chunk(payload) {
            warn!("failed to write file transfer chunk for id={id}: {err:#}");
            transfer.cleanup();
            transport.send_file_xfer_status(id, VD_AGENT_FILE_XFER_STATUS_ERROR)?;
            return Ok(());
        }

        if transfer.written_size == transfer.metadata.size {
            match transfer.finish() {
                Ok(path) => {
                    info!("completed host file transfer id={id} -> {}", path.display());
                    transport.send_file_xfer_status(id, VD_AGENT_FILE_XFER_STATUS_SUCCESS)?;
                }
                Err(err) => {
                    warn!("failed to finalise file transfer id={id}: {err:#}");
                    transport.send_file_xfer_status(id, VD_AGENT_FILE_XFER_STATUS_ERROR)?;
                }
            }
            return Ok(());
        }

        if payload.is_empty() {
            warn!(
                "host file transfer id={} ended early at {} / {} bytes",
                id, transfer.written_size, transfer.metadata.size
            );
            transfer.cleanup();
            transport.send_file_xfer_status(id, VD_AGENT_FILE_XFER_STATUS_ERROR)?;
            return Ok(());
        }

        self.active.insert(id, transfer);
        Ok(())
    }

    pub fn handle_host_status(&mut self, id: u32, result: u32) {
        let Some(transfer) = self.active.remove(&id) else {
            return;
        };

        match result {
            VD_AGENT_FILE_XFER_STATUS_CANCELLED => {
                info!("host cancelled file transfer id={id}");
            }
            VD_AGENT_FILE_XFER_STATUS_ERROR => {
                warn!("host reported an error for file transfer id={id}");
            }
            other => {
                warn!("host sent unexpected file transfer status {other} for id={id}");
            }
        }

        transfer.cleanup();
    }

    pub fn cancel_all(&mut self, reason: &str) {
        for (id, transfer) in self.active.drain() {
            warn!("cancelling in-flight file transfer id={id}: {reason}");
            transfer.cleanup();
        }
    }
}

impl IncomingTransfer {
    fn create(save_dir: &Path, id: u32, metadata: FileTransferMetadata) -> Result<Self> {
        let final_path = choose_destination_path(save_dir, &metadata.name);
        let temp_path = choose_temp_path(save_dir, id);
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)
            .with_context(|| format!("failed to create temporary file {}", temp_path.display()))?;

        Ok(Self {
            id,
            metadata,
            temp_path,
            final_path,
            file,
            written_size: 0,
        })
    }

    fn write_chunk(&mut self, payload: &[u8]) -> Result<()> {
        let written_size = self
            .written_size
            .checked_add(payload.len() as u64)
            .context("file transfer size overflow")?;
        if written_size > self.metadata.size {
            bail!(
                "file transfer exceeded declared size: {written_size} > {}",
                self.metadata.size
            );
        }
        self.file.write_all(payload).with_context(|| {
            format!(
                "failed to write temporary file {}",
                self.temp_path.display()
            )
        })?;
        self.written_size = written_size;
        Ok(())
    }

    fn finish(mut self) -> Result<PathBuf> {
        self.file.flush().with_context(|| {
            format!(
                "failed to flush temporary file {}",
                self.temp_path.display()
            )
        })?;

        let parent = self
            .final_path
            .parent()
            .context("final file path is missing a parent directory")?;
        let final_path = if self.final_path.exists() {
            choose_destination_path(parent, &self.metadata.name)
        } else {
            self.final_path.clone()
        };

        fs::rename(&self.temp_path, &final_path).with_context(|| {
            format!(
                "failed to move {} into place at {}",
                self.temp_path.display(),
                final_path.display()
            )
        })?;

        Ok(final_path)
    }

    fn cleanup(self) {
        drop(self);
    }
}

impl Drop for IncomingTransfer {
    fn drop(&mut self) {
        // On success the temporary path has already been renamed. On every
        // other exit path, including errors while finishing, remove the part.
        let _ = fs::remove_file(&self.temp_path);
    }
}

impl Drop for FileTransferManager {
    fn drop(&mut self) {
        self.cancel_all("bridge stopped");
    }
}

pub fn parse_file_transfer_metadata(bytes: &[u8]) -> Result<FileTransferMetadata> {
    let raw = bytes.split(|byte| *byte == 0).next().unwrap_or_default();
    let text = std::str::from_utf8(raw).context("file transfer metadata is not valid UTF-8")?;

    let mut in_section = false;
    let mut name = None;
    let mut size = None;

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        if line.starts_with('[') && line.ends_with(']') {
            in_section = &line[1..line.len() - 1] == TRANSFER_SECTION;
            continue;
        }

        if !in_section {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            continue;
        };

        match key.trim() {
            "name" => {
                name = Some(unescape_keyfile_value(value.trim())?);
            }
            "size" => {
                size = Some(
                    value
                        .trim()
                        .parse::<u64>()
                        .context("file transfer size is not a valid integer")?,
                );
            }
            _ => {}
        }
    }

    let name = name.context("file transfer metadata does not include a file name")?;
    if name.trim().is_empty() {
        bail!("file transfer metadata contains an empty file name");
    }

    let size = size.context("file transfer metadata does not include a file size")?;

    Ok(FileTransferMetadata { name, size })
}

fn prepare_save_dir(save_dir: Option<PathBuf>) -> Result<Option<PathBuf>> {
    let Some(save_dir) = save_dir.or_else(default_save_dir) else {
        warn!("file transfer is disabled because no guest save directory could be determined");
        return Ok(None);
    };

    fs::create_dir_all(&save_dir).with_context(|| {
        format!(
            "failed to create file transfer directory {}",
            save_dir.display()
        )
    })?;

    if !save_dir.is_dir() {
        bail!(
            "configured file transfer directory is not a directory: {}",
            save_dir.display()
        );
    }

    info!(
        "host -> guest file transfers will be saved in {}",
        save_dir.display()
    );
    Ok(Some(save_dir))
}

fn default_save_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Downloads"))
}

fn choose_destination_path(dir: &Path, requested_name: &str) -> PathBuf {
    let sanitized = sanitize_filename(requested_name);
    let candidate = dir.join(&sanitized);
    if !candidate.exists() {
        return candidate;
    }

    let (stem, extension) = split_name_extension(&sanitized);
    for attempt in 1..=MAX_FILENAME_ATTEMPTS {
        let file_name = if extension.is_empty() {
            format!("{stem} ({attempt})")
        } else {
            format!("{stem} ({attempt}).{extension}")
        };
        let candidate = dir.join(file_name);
        if !candidate.exists() {
            return candidate;
        }
    }

    dir.join(format!("{sanitized}.{}", std::process::id()))
}

fn choose_temp_path(dir: &Path, id: u32) -> PathBuf {
    for attempt in 0..=MAX_FILENAME_ATTEMPTS {
        let candidate = dir.join(format!(".paprika-vdagent-{id}-{attempt}.part"));
        if !candidate.exists() {
            return candidate;
        }
    }

    dir.join(format!(
        ".paprika-vdagent-{id}-{}-fallback.part",
        std::process::id()
    ))
}

fn sanitize_filename(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | '\0' => '_',
            _ if ch.is_control() => '_',
            _ => ch,
        })
        .collect();

    let sanitized = sanitized.trim().trim_matches('.');
    if sanitized.is_empty() {
        "spice-transfer".to_string()
    } else {
        sanitized.to_string()
    }
}

fn split_name_extension(file_name: &str) -> (&str, &str) {
    match file_name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, extension),
        _ => (file_name, ""),
    }
}

fn unescape_keyfile_value(value: &str) -> Result<String> {
    let mut out = String::new();
    let mut chars = value.chars();

    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }

        let escaped = chars
            .next()
            .context("file transfer metadata ended with an incomplete escape")?;
        match escaped {
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            's' => out.push(' '),
            '\\' => out.push('\\'),
            other => out.push(other),
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("paprika-vdagent-test-{unique}"));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn manager_and_transport(dir: &TestDir) -> (FileTransferManager, SpiceTransport) {
        let manager = FileTransferManager::new(Some(dir.path.clone()), Some(8)).unwrap();
        let transport =
            SpiceTransport::test_writer(File::create(dir.path.join("status.bin")).unwrap());
        (manager, transport)
    }

    fn assert_no_parts(dir: &TestDir) {
        for entry in fs::read_dir(&dir.path).unwrap() {
            assert_ne!(
                entry
                    .unwrap()
                    .path()
                    .extension()
                    .and_then(|ext| ext.to_str()),
                Some("part")
            );
        }
    }

    #[test]
    fn cancellation_removes_partial_files_and_allows_transfer_id_reuse() {
        let dir = TestDir::new();
        let (mut manager, transport) = manager_and_transport(&dir);
        let metadata = b"[vdagent-file-xfer]\nname=hello.txt\nsize=5\n";
        manager.handle_start(&transport, 1, metadata).unwrap();
        manager.handle_data(&transport, 1, b"he").unwrap();
        manager.cancel_all("client disconnected");
        assert_no_parts(&dir);
        assert!(!dir.path.join("hello.txt").exists());
        manager.handle_start(&transport, 1, metadata).unwrap();
        manager.handle_data(&transport, 1, b"hello").unwrap();
        assert_eq!(fs::read(dir.path.join("hello.txt")).unwrap(), b"hello");
        assert_no_parts(&dir);
    }

    #[test]
    fn dropping_the_bridge_cleans_up_interrupted_transfers() {
        let dir = TestDir::new();
        let (mut manager, transport) = manager_and_transport(&dir);
        manager
            .handle_start(
                &transport,
                1,
                b"[vdagent-file-xfer]\nname=hello.txt\nsize=5\n",
            )
            .unwrap();
        manager.handle_data(&transport, 1, b"he").unwrap();
        drop(manager);
        assert_no_parts(&dir);
        assert!(!dir.path.join("hello.txt").exists());
    }

    #[test]
    fn failed_finalisation_removes_the_temporary_file() {
        let dir = TestDir::new();
        let (mut manager, transport) = manager_and_transport(&dir);
        manager
            .handle_start(
                &transport,
                1,
                b"[vdagent-file-xfer]\nname=hello.txt\nsize=5\n",
            )
            .unwrap();
        // Force rename failure without relying on root-sensitive permissions.
        manager.active.get_mut(&1).unwrap().final_path = dir.path.join("missing-parent/hello.txt");
        manager.handle_data(&transport, 1, b"hello").unwrap();
        assert_no_parts(&dir);
        assert!(manager.active.is_empty());
    }

    #[test]
    fn early_end_and_host_cancellation_remove_partial_files() {
        for cancelled in [false, true] {
            let dir = TestDir::new();
            let (mut manager, transport) = manager_and_transport(&dir);
            manager
                .handle_start(
                    &transport,
                    1,
                    b"[vdagent-file-xfer]\nname=hello.txt\nsize=5\n",
                )
                .unwrap();
            manager.handle_data(&transport, 1, b"he").unwrap();
            if cancelled {
                manager.handle_host_status(1, VD_AGENT_FILE_XFER_STATUS_CANCELLED);
            } else {
                manager.handle_data(&transport, 1, b"").unwrap();
            }
            assert_no_parts(&dir);
            assert!(manager.active.is_empty());
            assert!(!dir.path.join("hello.txt").exists());
        }
    }

    #[test]
    fn oversized_chunks_are_rejected_before_writing() {
        let dir = TestDir::new();
        let mut transfer = IncomingTransfer::create(
            &dir.path,
            1,
            FileTransferMetadata {
                name: "hello.txt".into(),
                size: 2,
            },
        )
        .unwrap();
        assert!(transfer.write_chunk(b"too large").is_err());
        assert_eq!(fs::metadata(&transfer.temp_path).unwrap().len(), 0);
        drop(transfer);
        assert_no_parts(&dir);
    }

    #[test]
    fn parses_spice_keyfile_metadata() {
        let metadata = parse_file_transfer_metadata(
            b"[vdagent-file-xfer]\nname=hello.txt\nsize=42\n\0ignored",
        )
        .expect("metadata should parse");

        assert_eq!(
            metadata,
            FileTransferMetadata {
                name: "hello.txt".to_string(),
                size: 42,
            }
        );
    }

    #[test]
    fn unescapes_keyfile_filename() {
        let metadata =
            parse_file_transfer_metadata(b"[vdagent-file-xfer]\nname=hello\\sworld.txt\nsize=1\n")
                .expect("metadata should parse");

        assert_eq!(metadata.name, "hello world.txt");
    }

    #[test]
    fn destination_path_avoids_collisions() {
        let dir = TestDir::new();
        let first = choose_destination_path(&dir.path, "hello.txt");
        fs::write(&first, b"existing").unwrap();

        let second = choose_destination_path(&dir.path, "hello.txt");
        assert_ne!(first, second);
        assert_eq!(second.file_name().unwrap(), "hello (1).txt");
    }

    #[test]
    fn sanitize_filename_strips_separators() {
        assert_eq!(sanitize_filename("../bad\\name.txt"), "_bad_name.txt");
    }
}
