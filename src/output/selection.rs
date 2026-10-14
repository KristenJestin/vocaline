//! A phrase offered on the Wayland clipboard by a source this process serves itself, through
//! the ext-data-control protocol. `wl-clipboard-rs` returns before the selection has even been
//! sent to the compositor, and says nothing of who reads it; the worker needs both.

use rustix::fs::{OFlags, fcntl_setfl};
use std::fs::File;
use std::io::{self, Write};
use std::sync::mpsc;
use std::thread;
use std::time::Instant;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry::WlRegistry, wl_seat::WlSeat};
use wayland_client::{Connection, Dispatch, QueueHandle, event_created_child};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::ExtDataControlOfferV1,
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};

/// The types text is offered as: the ones `wl-copy` offers.
const TEXT: [&str; 5] = [
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

/// A clipboard history that honours this hint keeps no copy of what was dictated.
const SENSITIVE: &str = "x-kde-passwordManagerHint";

struct State {
    text: Vec<u8>,
    reads: mpsc::Sender<Instant>,
    /// Another selection replaced ours, or the compositor dropped it: nothing more to serve.
    over: bool,
}

/// Makes `text` the selection, and returns once the compositor holds it. Each read of it is
/// told on the channel, once served, by when it was asked.
pub fn offer(text: &str) -> io::Result<mpsc::Receiver<Instant>> {
    let (reads, told) = mpsc::channel();
    let (ready, held) = mpsc::sync_channel(1);
    let text = text.as_bytes().to_vec();
    // The Wayland objects are made on the thread that serves them.
    thread::spawn(move || {
        let mut state = State {
            text,
            reads,
            over: false,
        };
        let mut queue = match hold(&mut state) {
            Ok(queue) => {
                let _ = ready.send(Ok(()));
                queue
            }
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        while !state.over {
            if queue.blocking_dispatch(&mut state).is_err() {
                return;
            }
        }
    });
    held.recv()
        .map_err(|_| io::Error::other("the clipboard thread ended"))??;
    Ok(told)
}

fn hold(state: &mut State) -> io::Result<wayland_client::EventQueue<State>> {
    let connection = Connection::connect_to_env().map_err(io::Error::other)?;
    let (globals, mut queue) =
        registry_queue_init::<State>(&connection).map_err(io::Error::other)?;
    let qh = queue.handle();
    let seat: WlSeat = globals.bind(&qh, 1..=1, ()).map_err(io::Error::other)?;
    let manager: ExtDataControlManagerV1 =
        globals.bind(&qh, 1..=1, ()).map_err(io::Error::other)?;
    let device = manager.get_data_device(&seat, &qh, ());
    let source = manager.create_data_source(&qh, ());
    for mime in TEXT.into_iter().chain([SENSITIVE]) {
        source.offer(mime.into());
    }
    device.set_selection(Some(&source));
    // The compositor answers once it has handled the selection, and so sent it to the focused
    // application: a key typed afterwards reaches that application after it.
    queue.roundtrip(state).map_err(io::Error::other)?;
    if state.over {
        return Err(io::Error::other("the compositor dropped the selection"));
    }
    Ok(queue)
}

impl Dispatch<ExtDataControlSourceV1, ()> for State {
    fn event(
        state: &mut State,
        source: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                let asked = Instant::now();
                let hint = mime_type == SENSITIVE;
                let bytes: &[u8] = if hint { b"secret" } else { &state.text };
                // A reader's pipe may be non-blocking: the text would be cut where it fills.
                let served = fcntl_setfl(&fd, OFlags::empty())
                    .map_err(io::Error::from)
                    .and_then(|()| File::from(fd).write_all(bytes));
                if served.is_ok() && !hint {
                    let _ = state.reads.send(asked);
                }
            }
            ext_data_control_source_v1::Event::Cancelled => {
                source.destroy();
                state.over = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut State,
        device: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        match event {
            // Ours, or the next one: this device only sets the selection, it reads none.
            ext_data_control_device_v1::Event::Selection { id: Some(offer) }
            | ext_data_control_device_v1::Event::PrimarySelection { id: Some(offer) } => {
                offer.destroy()
            }
            ext_data_control_device_v1::Event::Finished => {
                device.destroy();
                state.over = true;
            }
            _ => {}
        }
    }

    event_created_child!(State, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for State {
    fn event(
        _: &mut State,
        _: &ExtDataControlOfferV1,
        _: <ExtDataControlOfferV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for State {
    fn event(
        _: &mut State,
        _: &ExtDataControlManagerV1,
        _: <ExtDataControlManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut State,
        _: &WlSeat,
        _: <WlSeat as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut State,
        _: &WlRegistry,
        _: <WlRegistry as wayland_client::Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
    }
}
