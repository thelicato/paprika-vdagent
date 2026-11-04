//! A real data-control source in a separate test process. Its first transfer
//! keeps the pipe open without writing; subsequent requests return the payload.
use std::fs::{self, File};
use std::io::Write;
use std::os::fd::OwnedFd;

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, delegate_noop, event_created_child,
};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_device_v1::{
    self, ZwlrDataControlDeviceV1,
};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1;
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_offer_v1::ZwlrDataControlOfferV1;
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_source_v1::{
    self, ZwlrDataControlSourceV1,
};

struct SourceState {
    payload: Vec<u8>,
    stalled: Vec<OwnedFd>,
    requests: usize,
    cancelled: bool,
}

pub fn run() {
    let connection = Connection::connect_to_env().unwrap();
    let (globals, mut queue) = registry_queue_init::<SourceState>(&connection).unwrap();
    let qh = queue.handle();
    let manager = globals
        .bind::<ZwlrDataControlManagerV1, _, _>(&qh, 2..=2, ())
        .unwrap();
    let seat = globals.bind::<WlSeat, _, _>(&qh, 2..=2, ()).unwrap();
    let device = manager.get_data_device(&seat, &qh, ());
    let source = manager.create_data_source(&qh, ());
    source.offer("text/plain;charset=utf-8".into());
    device.set_selection(Some(&source));
    let mut state = SourceState {
        payload: b"recovered from a stalled owner".to_vec(),
        stalled: Vec::new(),
        requests: 0,
        cancelled: false,
    };
    queue.roundtrip(&mut state).unwrap();
    fs::write(
        std::env::var_os("PAPRIKA_TEST_SOURCE_READY").unwrap(),
        b"ready",
    )
    .unwrap();
    while !state.cancelled {
        queue.blocking_dispatch(&mut state).unwrap();
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for SourceState {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlSourceV1,
        event: <ZwlrDataControlSourceV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { fd, .. } => {
                state.requests += 1;
                if state.requests == 1 {
                    state.stalled.push(fd);
                } else {
                    File::from(fd).write_all(&state.payload).unwrap();
                }
            }
            zwlr_data_control_source_v1::Event::Cancelled => state.cancelled = true,
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for SourceState {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: <ZwlrDataControlDeviceV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::Selection { id: Some(offer) }
            | zwlr_data_control_device_v1::Event::PrimarySelection { id: Some(offer) } => {
                offer.destroy()
            }
            _ => {}
        }
    }

    event_created_child!(SourceState, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ())
    ]);
}

impl Dispatch<WlRegistry, GlobalListContents> for SourceState {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(SourceState: ignore WlSeat);
delegate_noop!(SourceState: ignore ZwlrDataControlManagerV1);
delegate_noop!(SourceState: ignore ZwlrDataControlOfferV1);
