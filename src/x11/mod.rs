mod owner;
mod read;

use std::os::fd::OwnedFd;
use std::sync::mpsc::{self, SyncSender};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::io::Errno;
use rustix::pipe::pipe;
use tracing::warn;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;
use x11rb::{COPY_FROM_PARENT, CURRENT_TIME, NONE};

use crate::clipboard::{Clipboard, ClipboardEvent, ClipboardRead};
use crate::selection::ClipboardSelection;
use owner::{Command, Reply};

x11rb::atom_manager! {
    pub(super) Atoms: AtomsCookie {
        CLIPBOARD, TARGETS, TIMESTAMP, MULTIPLE, ATOM_PAIR, UTF8_STRING, TEXT, INCR,
        MIME_UTF8: b"text/plain;charset=utf-8",
        MIME_TEXT: b"text/plain",
        PROPERTY: b"_PAPRIKA_SELECTION",
        CLOCK: b"_PAPRIKA_TIMESTAMP",
    }
}

impl Atoms {
    fn selection(&self, selection: ClipboardSelection) -> Atom {
        match selection {
            ClipboardSelection::Clipboard => self.CLIPBOARD,
            ClipboardSelection::Primary => AtomEnum::PRIMARY.into(),
        }
    }

    fn selection_id(&self, atom: Atom) -> Option<ClipboardSelection> {
        ClipboardSelection::ALL
            .into_iter()
            .find(|selection| self.selection(*selection) == atom)
    }

    fn text_targets(&self) -> [Atom; 5] {
        [
            self.UTF8_STRING,
            self.MIME_UTF8,
            self.MIME_TEXT,
            self.TEXT,
            AtomEnum::STRING.into(),
        ]
    }
}

pub struct X11Clipboard {
    display: String,
    commands: SyncSender<Command>,
    wake: OwnedFd,
    timeout: Duration,
    max_text_bytes: usize,
}

impl X11Clipboard {
    pub fn new(max_text_bytes: usize, timeout: Duration) -> Result<Self> {
        let display = std::env::var("DISPLAY")
            .context("DISPLAY is not set; run the X11 backend inside your X11 session")?;
        let (connection, screen) = x11rb::connect(Some(&display))
            .context("failed to connect to the X11 server; check DISPLAY and XAUTHORITY")?;
        let window = create_window(&connection, screen)?;
        let atoms = Atoms::new(&connection)?.reply()?;
        let (wake_read, wake_write) = pipe()?;
        for descriptor in [&wake_read, &wake_write] {
            fcntl_setfl(descriptor, fcntl_getfl(descriptor)? | OFlags::NONBLOCK)?;
        }
        let (commands, receiver) = mpsc::sync_channel(8);
        let service = owner::Owner::new(connection, window, atoms, timeout);
        std::thread::Builder::new()
            .name("paprika-x11-owner".into())
            .spawn(move || {
                service.run(receiver, wake_read);
            })
            .context("failed to start X11 clipboard service")?;
        Ok(Self {
            display,
            commands,
            wake: wake_write,
            timeout,
            max_text_bytes,
        })
    }

    fn command<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> Result<T> {
        let (sender, receiver) = mpsc::sync_channel(1);
        self.commands
            .try_send(make(sender))
            .map_err(|err| anyhow!("X11 clipboard service unavailable: {err}"))?;
        match rustix::io::write(&self.wake, b"x") {
            Ok(_) | Err(Errno::AGAIN) => {}
            Err(err) => return Err(err).context("could not wake the X11 clipboard service"),
        }
        receiver
            .recv_timeout(self.timeout.saturating_add(Duration::from_secs(1)))
            .context("X11 clipboard service did not respond")?
            .map_err(anyhow::Error::msg)
    }
}

impl Clipboard for X11Clipboard {
    fn name(&self) -> &'static str {
        "X11"
    }

    fn read_text(&self, selection: ClipboardSelection) -> Result<ClipboardRead> {
        read::read_text(&self.display, selection, self.max_text_bytes, self.timeout)
    }

    fn write_text(&self, selection: ClipboardSelection, text: &str) -> Result<bool> {
        if text.len() > self.max_text_bytes {
            bail!(
                "refusing to write {selection} text larger than configured limit: {} > {}",
                text.len(),
                self.max_text_bytes
            );
        }
        self.command(|reply| Command::Write {
            selection,
            text: text.into(),
            reply,
        })
    }

    fn clear(&self, selection: ClipboardSelection) -> Result<bool> {
        self.command(|reply| Command::Clear { selection, reply })
    }

    fn selection_supported(&self, _: ClipboardSelection) -> bool {
        true
    }

    fn start_watcher(&self, sender: mpsc::Sender<ClipboardEvent>) -> Result<()> {
        self.command(|reply| Command::Watch { sender, reply })
    }
}

pub fn probe() -> Result<bool> {
    let (connection, _) = x11rb::connect(None).context("check DISPLAY and XAUTHORITY")?;
    Ok(has_xfixes(&connection))
}

fn has_xfixes(connection: &RustConnection) -> bool {
    connection
        .extension_information(x11rb::protocol::xfixes::X11_EXTENSION_NAME)
        .ok()
        .flatten()
        .is_some()
        && connection
            .xfixes_query_version(1, 0)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some_and(|reply| reply.major_version >= 1)
}

fn create_window(connection: &RustConnection, screen: usize) -> Result<Window> {
    let window = connection.generate_id()?;
    connection
        .create_window(
            COPY_FROM_PARENT as u8,
            window,
            connection.setup().roots[screen].root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            COPY_FROM_PARENT,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?
        .check()?;
    Ok(window)
}

fn next_event(connection: &RustConnection, deadline: Instant) -> Result<Event> {
    loop {
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "X11 selection transfer timed out",
            )
            .into());
        }
        if let Some(event) = connection.poll_for_event()? {
            return Ok(event);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "X11 selection transfer timed out",
            )
            .into());
        }
        let timeout = Timespec::try_from(remaining)?;
        match poll(
            &mut [PollFd::new(connection.stream(), PollFlags::IN)],
            Some(&timeout),
        ) {
            Ok(_) | Err(Errno::INTR) => {}
            Err(err) => return Err(err.into()),
        }
    }
}
