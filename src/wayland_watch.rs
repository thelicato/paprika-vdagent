use std::collections::HashMap;
use std::sync::mpsc::{self, Sender, SyncSender};
use std::thread;

use anyhow::{Context, Result, anyhow, bail};
use tracing::{debug, info, warn};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, event_created_child};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_device_v1::{
    self, ZwlrDataControlDeviceV1,
};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1;
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_offer_v1::ZwlrDataControlOfferV1;

use crate::wayland::SeatSelector;

#[derive(Debug)]
pub enum WatchEvent {
    SelectionChanged,
    Disconnected(String),
}

pub fn spawn_selection_watcher(seat: SeatSelector, sender: Sender<WatchEvent>) -> Result<()> {
    let (startup_tx, startup_rx) = mpsc::sync_channel(1);

    thread::Builder::new()
        .name("paprika-wayland-watch".to_string())
        .spawn(move || match initialize_watcher(seat, sender.clone()) {
            Ok((mut queue, mut state)) => {
                notify_startup(&startup_tx, Ok(()));

                if let Err(err) = watch_loop(&mut queue, &mut state) {
                    let message = format!("{err:#}");
                    let _ = sender.send(WatchEvent::Disconnected(message));
                }
            }
            Err(err) => {
                notify_startup(&startup_tx, Err(err));
            }
        })
        .context("failed to spawn Wayland watcher thread")?;

    match startup_rx.recv() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(message)) => Err(anyhow!(message)),
        Err(err) => Err(err).context("Wayland watcher thread exited before startup completed"),
    }
}

fn notify_startup(startup_tx: &SyncSender<Result<(), String>>, result: Result<(), anyhow::Error>) {
    match result {
        Ok(()) => {
            let _ = startup_tx.send(Ok(()));
        }
        Err(err) => {
            let _ = startup_tx.send(Err(format!("{err:#}")));
        }
    }
}

fn initialize_watcher(
    seat: SeatSelector,
    sender: Sender<WatchEvent>,
) -> Result<(EventQueue<WatchState>, WatchState)> {
    let conn = Connection::connect_to_env().context("failed to connect to Wayland compositor")?;
    let (globals, mut queue) = registry_queue_init::<WatchState>(&conn)
        .context("failed to initialize Wayland registry")?;
    let qh = queue.handle();

    let manager: ZwlrDataControlManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|_| anyhow!("required Wayland protocol wlr-data-control v1 is not available"))?;

    let registry = globals.registry();
    let mut seats: HashMap<WlSeat, SeatState> = globals.contents().with_list(|globals| {
        globals
            .iter()
            .filter(|global| global.interface == WlSeat::interface().name && global.version >= 2)
            .map(|global| (registry.bind(global.name, 2, &qh, ()), SeatState::default()))
            .collect()
    });

    if seats.is_empty() {
        bail!("no Wayland seats are available for clipboard watching");
    }

    let keys: Vec<WlSeat> = seats.keys().cloned().collect();
    for key in &keys {
        let device = manager.get_data_device(key, &qh, key.clone());
        seats.get_mut(key).unwrap().set_device(Some(device));
    }

    let mut state = WatchState {
        seat,
        sender: sender.clone(),
        seats,
        armed: false,
    };

    queue
        .roundtrip(&mut state)
        .context("failed to complete initial Wayland watcher roundtrip")?;

    if let SeatSelector::Specific(name) = &state.seat {
        let found = state
            .seats
            .values()
            .any(|seat| seat.name.as_deref() == Some(name.as_str()));
        if !found {
            bail!("requested Wayland seat '{name}' was not found");
        }
    }

    state.armed = true;
    info!("started event-driven Wayland clipboard watcher");

    Ok((queue, state))
}

fn watch_loop(queue: &mut EventQueue<WatchState>, state: &mut WatchState) -> Result<()> {
    loop {
        queue
            .blocking_dispatch(state)
            .context("Wayland watcher disconnected unexpectedly")?;
    }
}

#[derive(Default)]
struct SeatState {
    name: Option<String>,
    device: Option<ZwlrDataControlDeviceV1>,
}

impl SeatState {
    fn set_name(&mut self, name: String) {
        self.name = Some(name);
    }

    fn set_device(&mut self, device: Option<ZwlrDataControlDeviceV1>) {
        let old = self.device.take();
        self.device = device;
        if let Some(device) = old {
            device.destroy();
        }
    }
}

struct WatchState {
    seat: SeatSelector,
    sender: Sender<WatchEvent>,
    seats: HashMap<WlSeat, SeatState>,
    armed: bool,
}

impl WatchState {
    fn should_notify(&self, seat: &WlSeat) -> bool {
        match &self.seat {
            SeatSelector::Unspecified => true,
            SeatSelector::Specific(name) => self
                .seats
                .get(seat)
                .and_then(|seat| seat.name.as_deref())
                .map(|seat_name| seat_name == name)
                .unwrap_or(false),
        }
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for WatchState {
    fn event(
        _state: &mut Self,
        _proxy: &WlRegistry,
        _event: <WlRegistry as Proxy>::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for WatchState {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: <WlSeat as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Name { name } = event {
            if let Some(seat_state) = state.seats.get_mut(seat) {
                seat_state.set_name(name);
            }
        }
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for WatchState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrDataControlManagerV1,
        _event: <ZwlrDataControlManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, WlSeat> for WatchState {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrDataControlDeviceV1,
        event: <ZwlrDataControlDeviceV1 as Proxy>::Event,
        seat: &WlSeat,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::Selection { id } => {
                if state.armed && state.should_notify(seat) {
                    debug!("Wayland clipboard selection changed");
                    let _ = id;
                    let _ = state.sender.send(WatchEvent::SelectionChanged);
                }
            }
            zwlr_data_control_device_v1::Event::Finished => {
                warn!("Wayland data-control device was finished by the compositor");
                if let Some(seat_state) = state.seats.get_mut(seat) {
                    seat_state.set_device(None);
                }
            }
            _ => {}
        }
    }

    event_created_child!(WatchState, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ())
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for WatchState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrDataControlOfferV1,
        _event: <ZwlrDataControlOfferV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}
