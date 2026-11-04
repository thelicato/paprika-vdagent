use std::io;
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::io::{Errno, read};

/// Read at most limit + 1 bytes, so callers can detect oversized offers without
/// allocating the entire payload. The deadline covers the whole transfer,
/// including a source which keeps writing but never closes the pipe.
pub fn read_clipboard_bytes(
    pipe: impl AsFd,
    limit: usize,
    timeout: Duration,
) -> io::Result<Vec<u8>> {
    let max_bytes = limit.checked_add(1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "clipboard byte limit is too large",
        )
    })?;
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "clipboard timeout is too large",
        )
    })?;
    let flags = fcntl_getfl(&pipe)?;
    fcntl_setfl(&pipe, flags | OFlags::NONBLOCK)?;
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 8192];
    while bytes.len() < max_bytes {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(timed_out());
        }
        let timeout = Timespec::try_from(remaining).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "clipboard timeout is too large",
            )
        })?;
        let mut descriptors = [PollFd::new(&pipe, PollFlags::IN)];
        match poll(&mut descriptors, Some(&timeout)) {
            Ok(0) => return Err(timed_out()),
            Ok(_) => {}
            Err(Errno::INTR) => continue,
            Err(err) => return Err(err.into()),
        }
        let wanted = buffer.len().min(max_bytes - bytes.len());
        match read(&pipe, &mut buffer[..wanted]) {
            Ok(0) => return Ok(bytes),
            Ok(count) => bytes.extend_from_slice(&buffer[..count]),
            Err(Errno::AGAIN | Errno::INTR) => continue,
            Err(err) => return Err(err.into()),
        }
    }
    Ok(bytes)
}

fn timed_out() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "Wayland clipboard source did not finish before the deadline",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::io::write;
    use rustix::pipe::pipe;
    use std::thread;

    #[test]
    fn reads_complete_text_and_an_empty_selection() {
        for payload in [b"hello".as_slice(), b""] {
            let (reader, writer) = pipe().unwrap();
            write(&writer, payload).unwrap();
            drop(writer);
            assert_eq!(
                read_clipboard_bytes(reader, 16, Duration::from_secs(1)).unwrap(),
                payload
            );
        }
    }

    #[test]
    fn detects_oversized_data_without_waiting_for_eof() {
        let (reader, writer) = pipe().unwrap();
        write(&writer, b"too much data").unwrap();
        assert_eq!(
            read_clipboard_bytes(reader, 3, Duration::from_secs(1)).unwrap(),
            b"too "
        );
    }

    #[test]
    fn times_out_a_source_which_never_writes_or_closes() {
        let (reader, _writer) = pipe().unwrap();
        let started = Instant::now();
        let error = read_clipboard_bytes(reader, 16, Duration::from_millis(50)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn discards_partial_data_when_the_source_stalls() {
        let (reader, writer) = pipe().unwrap();
        write(&writer, b"partial").unwrap();
        let error = read_clipboard_bytes(reader, 16, Duration::from_millis(50)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn deadline_is_not_extended_by_a_trickling_source() {
        let (reader, writer) = pipe().unwrap();
        let source = thread::spawn(move || {
            for _ in 0..100 {
                if write(&writer, b"x").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
        });
        let started = Instant::now();
        let error = read_clipboard_bytes(reader, 1024, Duration::from_millis(50)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_millis(400));
        source.join().unwrap();
    }

    #[test]
    fn a_new_source_can_be_read_after_a_stalled_source() {
        let (reader, _writer) = pipe().unwrap();
        assert_eq!(
            read_clipboard_bytes(reader, 16, Duration::from_millis(10))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        let (reader, writer) = pipe().unwrap();
        write(&writer, b"recovered").unwrap();
        drop(writer);
        assert_eq!(
            read_clipboard_bytes(reader, 16, Duration::from_secs(1)).unwrap(),
            b"recovered"
        );
    }
}
