use super::*;

struct PropertyData {
    kind: Atom,
    format: u8,
    bytes: Vec<u8>,
}

pub(super) fn read_text(
    display: &str,
    selection: ClipboardSelection,
    limit: usize,
    timeout: Duration,
) -> Result<ClipboardRead> {
    let (connection, screen) =
        x11rb::connect(Some(display)).context("failed to connect to X11 clipboard")?;
    let window = create_window(&connection, screen)?;
    let atoms = Atoms::new(&connection)?.reply()?;
    let selection = atoms.selection(selection);
    let owner = connection.get_selection_owner(selection)?.reply()?.owner;
    if owner == NONE {
        return Ok(ClipboardRead::Ready(None));
    }
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("clipboard timeout is too large")?;
    let result = read_selection(&connection, window, &atoms, selection, limit, deadline);
    // Do not publish data from an owner superseded during the transfer.
    if connection.get_selection_owner(selection)?.reply()?.owner != owner {
        return Ok(ClipboardRead::Retry);
    }
    match result {
        Ok(text) => Ok(ClipboardRead::Ready(text)),
        Err(err)
            if err
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::TimedOut)
                || err.is::<InvalidPayload>() =>
        {
            warn!("{err:#}; keeping the cached X11 selection and retrying");
            Ok(ClipboardRead::Retry)
        }
        Err(err) => Err(err).context("failed to read X11 selection"),
    }
    // Dropping the dedicated connection destroys the request window and any
    // remaining properties, including after an aborted INCR transfer.
}

fn read_selection(
    connection: &RustConnection,
    window: Window,
    atoms: &Atoms,
    selection: Atom,
    limit: usize,
    deadline: Instant,
) -> Result<Option<String>> {
    let available = convert(
        connection,
        window,
        atoms,
        selection,
        atoms.TARGETS,
        4096,
        deadline,
    )?;
    let targets: Vec<Atom> = match available {
        Some(property) if property.kind == AtomEnum::ATOM.into() && property.format == 32 => {
            let offered: Vec<Atom> = property
                .bytes
                .chunks_exact(4)
                .map(|chunk| Atom::from_ne_bytes(chunk.try_into().unwrap()))
                .collect();
            atoms
                .text_targets()
                .into_iter()
                .filter(|target| offered.contains(target))
                .collect()
        }
        _ => atoms.text_targets().to_vec(),
    };
    for target in targets {
        let Some(property) = convert(
            connection, window, atoms, selection, target, limit, deadline,
        )?
        else {
            continue;
        };
        if property.format != 8 {
            continue;
        }
        let text = if property.kind == AtomEnum::STRING.into() {
            property.bytes.into_iter().map(char::from).collect()
        } else if [atoms.UTF8_STRING, atoms.MIME_UTF8, atoms.MIME_TEXT].contains(&property.kind) {
            String::from_utf8(property.bytes)
                .map_err(|_| InvalidPayload("X11 text is not valid UTF-8"))?
        } else {
            continue;
        };
        if text.len() > limit {
            return Err(InvalidPayload("X11 text exceeds the configured byte limit").into());
        }
        return Ok(Some(text));
    }
    Ok(None)
}

fn convert(
    connection: &RustConnection,
    window: Window,
    atoms: &Atoms,
    selection: Atom,
    target: Atom,
    limit: usize,
    deadline: Instant,
) -> Result<Option<PropertyData>> {
    connection.delete_property(window, atoms.PROPERTY)?;
    connection.convert_selection(window, selection, target, atoms.PROPERTY, CURRENT_TIME)?;
    connection.flush()?;
    loop {
        match next_event(connection, deadline)? {
            Event::SelectionNotify(event)
                if event.requestor == window
                    && event.selection == selection
                    && event.target == target =>
            {
                if event.property == NONE {
                    return Ok(None);
                }
                if event.property != atoms.PROPERTY {
                    return Err(
                        InvalidPayload("X11 owner used an unexpected selection property").into(),
                    );
                }
                break;
            }
            Event::Error(error) => bail!("X11 selection request failed: {error:?}"),
            _ => {}
        }
    }
    let (property, header_exceeded) =
        get_property(connection, window, atoms.PROPERTY, limit.max(4))?;
    if property.kind != atoms.INCR {
        if header_exceeded || property.bytes.len() > limit {
            return Err(InvalidPayload("X11 selection exceeds the configured byte limit").into());
        }
        return Ok(Some(property));
    }
    if property.format != 32 || property.bytes.len() != 4 {
        return Err(InvalidPayload("invalid X11 INCR header").into());
    }
    let mut exceeded = u32::from_ne_bytes(property.bytes[..4].try_into().unwrap()) as usize > limit;
    connection.flush()?; // GetProperty deleted the INCR header to acknowledge it.
    let mut result: Option<PropertyData> = None;
    loop {
        match next_event(connection, deadline)? {
            Event::PropertyNotify(event)
                if event.window == window
                    && event.atom == atoms.PROPERTY
                    && event.state == Property::NEW_VALUE =>
            {
                let used = result.as_ref().map_or(0, |property| property.bytes.len());
                // Complete an oversized source's handshake without accumulating
                // its data. Closing mid-INCR can break otherwise usable owners.
                let budget = if exceeded { 4096 } else { limit - used };
                let (chunk, chunk_exceeded) =
                    get_property(connection, window, atoms.PROPERTY, budget)?;
                connection.flush()?;
                if chunk.kind == NONE {
                    continue;
                }
                exceeded |= chunk_exceeded;
                if exceeded {
                    result = None;
                    if chunk.bytes.is_empty() {
                        return Err(InvalidPayload(
                            "X11 selection exceeds the configured byte limit",
                        )
                        .into());
                    }
                    continue;
                }
                if let Some(property) = &mut result {
                    if (property.kind, property.format) != (chunk.kind, chunk.format) {
                        return Err(InvalidPayload("X11 INCR transfer changed its encoding").into());
                    }
                    if chunk.bytes.is_empty() {
                        return Ok(result);
                    }
                    property.bytes.extend(chunk.bytes);
                } else if chunk.bytes.is_empty() {
                    return Ok(Some(chunk));
                } else {
                    result = Some(chunk);
                }
            }
            Event::Error(error) => bail!("X11 INCR transfer failed: {error:?}"),
            _ => {}
        }
    }
}

fn get_property(
    connection: &RustConnection,
    window: Window,
    property: Atom,
    limit: usize,
) -> Result<(PropertyData, bool)> {
    let words = limit.saturating_add(4) / 4;
    let reply = connection
        .get_property(
            true,
            window,
            property,
            AtomEnum::ANY,
            0,
            u32::try_from(words).unwrap_or(u32::MAX),
        )?
        .reply()?;
    let exceeded = reply.bytes_after != 0 || reply.value.len() > limit;
    if reply.bytes_after != 0 {
        // GetProperty only deletes a property when the complete value was read.
        // Explicitly discard the remainder before acknowledging an INCR chunk.
        connection.delete_property(window, property)?.check()?;
    }
    Ok((
        PropertyData {
            kind: reply.type_,
            format: reply.format,
            bytes: reply.value,
        },
        exceeded,
    ))
}

#[derive(Debug)]
struct InvalidPayload(&'static str);
impl std::fmt::Display for InvalidPayload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}
impl std::error::Error for InvalidPayload {}
