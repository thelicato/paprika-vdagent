use std::collections::{HashMap, HashSet};
use std::sync::{Arc, mpsc};

use x11rb::protocol::xfixes::SelectionEventMask;
use x11rb::wrapper::ConnectionExt as _;

use super::*;

pub(super) type Reply<T> = mpsc::SyncSender<Result<T, String>>;

pub(super) enum Command {
    Write {
        selection: ClipboardSelection,
        text: String,
        reply: Reply<bool>,
    },
    Clear {
        selection: ClipboardSelection,
        reply: Reply<bool>,
    },
    Watch {
        sender: mpsc::Sender<ClipboardEvent>,
        reply: Reply<()>,
    },
}

#[derive(Clone)]
struct OwnedSelection {
    text: Arc<str>,
    timestamp: Timestamp,
}

struct Outgoing {
    data: Arc<[u8]>,
    kind: Atom,
    offset: usize,
    deadline: Instant,
}

pub(super) struct Owner {
    connection: RustConnection,
    window: Window,
    atoms: Atoms,
    owned: [Option<OwnedSelection>; 2],
    outgoing: HashMap<(Window, Atom), Outgoing>,
    watcher: Option<mpsc::Sender<ClipboardEvent>>,
    timeout: Duration,
    chunk_size: usize,
}

impl Owner {
    pub(super) fn new(
        connection: RustConnection,
        window: Window,
        atoms: Atoms,
        timeout: Duration,
    ) -> Self {
        let chunk_size = connection
            .maximum_request_bytes()
            .saturating_sub(64)
            .min(64 * 1024);
        Self {
            connection,
            window,
            atoms,
            owned: [None, None],
            outgoing: HashMap::new(),
            watcher: None,
            timeout,
            chunk_size,
        }
    }

    pub(super) fn run(mut self, receiver: mpsc::Receiver<Command>, wake: OwnedFd) {
        if let Err(error) = self.run_loop(receiver, wake) {
            let message = format!("X11 clipboard service disconnected: {error:#}");
            if let Some(sender) = &self.watcher {
                let _ = sender.send(ClipboardEvent::Disconnected(message.clone()));
            }
            warn!("{message}");
        }
    }

    fn run_loop(&mut self, receiver: mpsc::Receiver<Command>, wake: OwnedFd) -> Result<()> {
        loop {
            for _ in 0..8 {
                match receiver.try_recv() {
                    Ok(command) => self.command(command),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                }
            }
            self.expire();
            let mut events = 0;
            while events < 64 {
                let Some(event) = self.connection.poll_for_event()? else {
                    break;
                };
                if let Err(error) = self.event(event) {
                    warn!("X11 clipboard request failed: {error:#}");
                }
                events += 1;
            }
            self.connection.flush()?;
            if events == 64 {
                continue;
            }
            let wait = self
                .outgoing
                .values()
                .map(|transfer| transfer.deadline.saturating_duration_since(Instant::now()))
                .min()
                .unwrap_or(Duration::from_secs(30));
            let timeout = Timespec::try_from(wait)?;
            match poll(
                &mut [
                    PollFd::new(self.connection.stream(), PollFlags::IN),
                    PollFd::new(&wake, PollFlags::IN),
                ],
                Some(&timeout),
            ) {
                Ok(_) | Err(Errno::INTR) => {}
                Err(error) => return Err(error.into()),
            }
            let mut buffer = [0u8; 64];
            loop {
                match rustix::io::read(&wake, &mut buffer) {
                    Ok(0) => return Ok(()),
                    Ok(_) | Err(Errno::INTR) => {}
                    Err(Errno::AGAIN) => break,
                    Err(error) => return Err(error.into()),
                }
            }
        }
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Write {
                selection,
                text,
                reply,
            } => {
                let result = self
                    .claim(selection, text)
                    .map_err(|error| format!("{error:#}"));
                let _ = reply.send(result);
            }
            Command::Clear { selection, reply } => {
                let result = self.clear(selection).map_err(|error| format!("{error:#}"));
                let _ = reply.send(result);
            }
            Command::Watch { sender, reply } => {
                let result = self.watch(sender).map_err(|error| format!("{error:#}"));
                let _ = reply.send(result);
            }
        }
    }

    fn claim(&mut self, selection: ClipboardSelection, text: String) -> Result<bool> {
        // Get an actual server timestamp for ownership and TIMESTAMP requests.
        self.connection
            .change_property8(
                PropMode::REPLACE,
                self.window,
                self.atoms.CLOCK,
                AtomEnum::INTEGER,
                &[0],
            )?
            .check()?;
        self.connection.flush()?;
        let deadline = Instant::now()
            .checked_add(self.timeout)
            .context("clipboard timeout is too large")?;
        let timestamp = loop {
            match next_event(&self.connection, deadline)? {
                Event::PropertyNotify(event)
                    if event.window == self.window
                        && event.atom == self.atoms.CLOCK
                        && event.state == Property::NEW_VALUE =>
                {
                    break event.time;
                }
                event => {
                    if let Err(error) = self.event(event) {
                        warn!("X11 clipboard request failed: {error:#}");
                    }
                }
            }
        };
        let atom = self.atoms.selection(selection);
        self.connection
            .set_selection_owner(self.window, atom, timestamp)?
            .check()?;
        if self.connection.get_selection_owner(atom)?.reply()?.owner != self.window {
            bail!("could not claim X11 {selection} ownership");
        }
        self.owned[selection.spice_id() as usize] = Some(OwnedSelection {
            text: text.into(),
            timestamp,
        });
        Ok(true)
    }

    fn clear(&mut self, selection: ClipboardSelection) -> Result<bool> {
        let atom = self.atoms.selection(selection);
        let Some(owned) = self.owned[selection.spice_id() as usize].take() else {
            return Ok(false);
        };
        if self.connection.get_selection_owner(atom)?.reply()?.owner != self.window {
            return Ok(false);
        }
        // A later owner change makes this timestamp stale, so the server will
        // reject the clear instead of releasing somebody else's clipboard.
        self.connection
            .set_selection_owner(NONE, atom, owned.timestamp)?
            .check()?;
        Ok(self.connection.get_selection_owner(atom)?.reply()?.owner == NONE)
    }

    fn watch(&mut self, sender: mpsc::Sender<ClipboardEvent>) -> Result<()> {
        if !has_xfixes(&self.connection) {
            bail!("XFixes selection notifications are unavailable; clipboard polling is required");
        }
        for selection in ClipboardSelection::ALL {
            self.connection
                .xfixes_select_selection_input(
                    self.window,
                    self.atoms.selection(selection),
                    SelectionEventMask::SET_SELECTION_OWNER
                        | SelectionEventMask::SELECTION_WINDOW_DESTROY
                        | SelectionEventMask::SELECTION_CLIENT_CLOSE,
                )?
                .check()?;
        }
        self.watcher = Some(sender);
        Ok(())
    }

    fn event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::SelectionRequest(request) => self.serve(request)?,
            Event::SelectionClear(event) => {
                if let Some(selection) = self.atoms.selection_id(event.selection) {
                    if self
                        .connection
                        .get_selection_owner(event.selection)?
                        .reply()?
                        .owner
                        != self.window
                    {
                        self.owned[selection.spice_id() as usize] = None;
                    }
                }
            }
            Event::XfixesSelectionNotify(event) => {
                if let Some(selection) = self.atoms.selection_id(event.selection) {
                    if let Some(sender) = &self.watcher {
                        let _ = sender.send(ClipboardEvent::SelectionChanged(selection));
                    }
                }
            }
            Event::PropertyNotify(event) if event.state == Property::DELETE => {
                self.advance(event.window, event.atom)?
            }
            Event::DestroyNotify(event) => self
                .outgoing
                .retain(|(window, _), _| *window != event.window),
            Event::Error(error) => warn!("X11 clipboard protocol error: {error:?}"),
            _ => {}
        }
        Ok(())
    }

    fn serve(&mut self, request: SelectionRequestEvent) -> Result<()> {
        let property = if request.property == NONE {
            request.target
        } else {
            request.property
        };
        let result = (|| -> Result<bool> {
            let Some(selection) = self.atoms.selection_id(request.selection) else {
                return Ok(false);
            };
            let Some(owned) = self.owned[selection.spice_id() as usize].clone() else {
                return Ok(false);
            };
            if request.requestor == self.window
                || (request.time != CURRENT_TIME
                    && (request.time.wrapping_sub(owned.timestamp) as i32) < 0)
            {
                return Ok(false);
            }
            if request.target == self.atoms.MULTIPLE {
                if request.property == NONE {
                    return Ok(false);
                }
                self.multiple(&request, &owned)
            } else {
                self.provide(request.requestor, property, request.target, &owned)
            }
        })();
        let accepted = match result {
            Ok(accepted) => accepted,
            Err(error) => {
                warn!("could not serve X11 selection: {error:#}");
                false
            }
        };
        let event = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: request.time,
            requestor: request.requestor,
            selection: request.selection,
            target: request.target,
            property: if accepted { property } else { NONE },
        };
        self.connection
            .send_event(false, request.requestor, EventMask::NO_EVENT, event)?
            .check()?;
        Ok(())
    }

    fn multiple(
        &mut self,
        request: &SelectionRequestEvent,
        owned: &OwnedSelection,
    ) -> Result<bool> {
        let property = self
            .connection
            .get_property(
                false,
                request.requestor,
                request.property,
                AtomEnum::ANY,
                0,
                2048,
            )?
            .reply()?;
        if property.format != 32
            || property.bytes_after != 0
            || ![self.atoms.ATOM_PAIR, AtomEnum::ATOM.into()].contains(&property.type_)
        {
            return Ok(false);
        }
        let mut pairs: Vec<_> = property
            .value32()
            .context("invalid MULTIPLE property")?
            .collect();
        if pairs.len() % 2 != 0 {
            return Ok(false);
        }
        let mut properties = HashSet::new();
        for pair in pairs.chunks_exact_mut(2) {
            let valid = pair[0] != self.atoms.MULTIPLE
                && pair[1] != NONE
                && pair[1] != request.property
                && properties.insert(pair[1]);
            if !valid
                || !self
                    .provide(request.requestor, pair[1], pair[0], owned)
                    .unwrap_or(false)
            {
                pair[0] = NONE;
            }
        }
        self.connection
            .change_property32(
                PropMode::REPLACE,
                request.requestor,
                request.property,
                self.atoms.ATOM_PAIR,
                &pairs,
            )?
            .check()?;
        Ok(true)
    }

    fn provide(
        &mut self,
        requestor: Window,
        property: Atom,
        target: Atom,
        owned: &OwnedSelection,
    ) -> Result<bool> {
        if self.outgoing.contains_key(&(requestor, property)) {
            return Ok(false);
        }
        if target == self.atoms.TARGETS {
            let mut targets = vec![
                self.atoms.TARGETS,
                self.atoms.TIMESTAMP,
                self.atoms.MULTIPLE,
                self.atoms.UTF8_STRING,
                self.atoms.TEXT,
                self.atoms.MIME_UTF8,
                self.atoms.MIME_TEXT,
            ];
            if owned.text.chars().all(|ch| u32::from(ch) <= 255) {
                targets.push(AtomEnum::STRING.into());
            }
            self.connection
                .change_property32(
                    PropMode::REPLACE,
                    requestor,
                    property,
                    AtomEnum::ATOM,
                    &targets,
                )?
                .check()?;
            return Ok(true);
        }
        if target == self.atoms.TIMESTAMP {
            self.connection
                .change_property32(
                    PropMode::REPLACE,
                    requestor,
                    property,
                    AtomEnum::INTEGER,
                    &[owned.timestamp],
                )?
                .check()?;
            return Ok(true);
        }
        let (kind, data): (Atom, Arc<[u8]>) = if target == AtomEnum::STRING.into() {
            let Some(bytes) = owned
                .text
                .chars()
                .map(|ch| u8::try_from(u32::from(ch)).ok())
                .collect::<Option<Vec<_>>>()
            else {
                return Ok(false);
            };
            (target, bytes.into())
        } else if self.atoms.text_targets().contains(&target) {
            (
                if target == self.atoms.TEXT {
                    self.atoms.UTF8_STRING
                } else {
                    target
                },
                owned.text.as_bytes().into(),
            )
        } else {
            return Ok(false);
        };
        if data.len() <= self.chunk_size {
            self.connection
                .change_property8(PropMode::REPLACE, requestor, property, kind, &data)?
                .check()?;
        } else {
            if self.outgoing.len() >= 32 {
                return Ok(false);
            }
            self.connection
                .change_window_attributes(
                    requestor,
                    &ChangeWindowAttributesAux::new()
                        .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
                )?
                .check()?;
            self.connection
                .change_property32(
                    PropMode::REPLACE,
                    requestor,
                    property,
                    self.atoms.INCR,
                    &[u32::try_from(data.len())?],
                )?
                .check()?;
            let deadline = Instant::now()
                .checked_add(self.timeout)
                .context("clipboard timeout is too large")?;
            self.outgoing.insert(
                (requestor, property),
                Outgoing {
                    data,
                    kind,
                    offset: 0,
                    deadline,
                },
            );
        }
        Ok(true)
    }

    fn advance(&mut self, window: Window, property: Atom) -> Result<()> {
        let Some(mut transfer) = self.outgoing.remove(&(window, property)) else {
            return Ok(());
        };
        let end = (transfer.offset + self.chunk_size).min(transfer.data.len());
        let result = self
            .connection
            .change_property8(
                PropMode::REPLACE,
                window,
                property,
                transfer.kind,
                &transfer.data[transfer.offset..end],
            )?
            .check();
        if result.is_ok() && transfer.offset < transfer.data.len() {
            transfer.offset = end;
            self.outgoing.insert((window, property), transfer);
        } else {
            self.unwatch_requestor(window);
        }
        result?;
        Ok(())
    }

    fn expire(&mut self) {
        let expired: Vec<_> = self
            .outgoing
            .iter()
            .filter(|(_, transfer)| Instant::now() >= transfer.deadline)
            .map(|(key, _)| *key)
            .collect();
        for (window, property) in expired {
            self.outgoing.remove(&(window, property));
            self.unwatch_requestor(window);
            warn!(
                "X11 clipboard requestor did not finish its incremental transfer before the deadline"
            );
        }
    }

    fn unwatch_requestor(&self, window: Window) {
        if !self
            .outgoing
            .keys()
            .any(|(requestor, _)| *requestor == window)
        {
            if let Ok(cookie) = self.connection.change_window_attributes(
                window,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT),
            ) {
                let _ = cookie.check();
            }
        }
    }
}
