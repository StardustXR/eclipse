use color_eyre::eyre::Result;
use glam::vec2;
use input::event::keyboard::KeyboardEventTrait;
use input::event::pointer::{Axis, PointerScrollEvent};
use input::event::tablet_pad::{ButtonState, KeyState};
use input::event::PointerEvent;
use input::{Libinput, LibinputInterface};
use libc::{O_RDONLY, O_RDWR, O_WRONLY};
use stardust_xr_fusion::client::{Client, FrameInfo, RootHandler};
use stardust_xr_fusion::core::values::Transform;
use stardust_xr_fusion::data::{NewReceiverInfo, PulseReceiver, PulseSender, PulseSenderHandler};
use stardust_xr_fusion::fields::UnknownField;
use stardust_xr_fusion::node::NodeType;
use stardust_xr_fusion::HandlerWrapper;
use stardust_xr_molecules::keyboard::{KeyboardEvent, KEYBOARD_MASK};
use stardust_xr_molecules::mouse::{MouseEvent, MOUSE_MASK};
use std::fs::{File, OpenOptions};
use std::os::unix::{fs::OpenOptionsExt, io::OwnedFd};
use std::path::Path;
use tokio::sync::mpsc::Receiver;
use xkbcommon::xkb::{Context, Keymap};

struct Interface;

impl LibinputInterface for Interface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        OpenOptions::new()
            .custom_flags(flags)
            .read((flags & O_RDONLY != 0) | (flags & O_RDWR != 0))
            .write((flags & O_WRONLY != 0) | (flags & O_RDWR != 0))
            .open(path)
            .map(|file| file.into())
            .map_err(|err| err.raw_os_error().unwrap())
    }
    fn close_restricted(&mut self, fd: OwnedFd) {
        drop(File::from(fd));
    }
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    color_eyre::install().unwrap();
    let (client, event_loop) = Client::connect_with_async_loop()
        .await
        .expect("Couldn't connect");

    let (stop_libinput_tx, mut stop_libinput_rx) = tokio::sync::oneshot::channel();

    let (mouse_event_tx, mouse_event_rx) = tokio::sync::mpsc::channel(64);
    let (keyboard_event_tx, keyboard_event_rx) = tokio::sync::mpsc::channel(64);
    let _eclipse =
        client.wrap_root(Eclipse::create(&client, mouse_event_rx, keyboard_event_rx)?)?;

    tokio::task::spawn_blocking(move || {
        let mut input = Libinput::new_with_udev(Interface);
        input.udev_assign_seat("seat0").unwrap();
        let keymap =
            Keymap::new_from_names(&Context::new(0), "evdev", "", "", "", None, 0).unwrap();
        loop {
            if stop_libinput_rx.try_recv().is_ok() {
                return;
            }
            input.dispatch().unwrap();
            for event in &mut input {
                match event {
                    input::Event::Keyboard(input::event::KeyboardEvent::Key(k)) => {
                        let event = KeyboardEvent::new(
                            Some(&keymap),
                            (k.key_state() == KeyState::Released).then(|| vec![k.key()]),
                            (k.key_state() == KeyState::Pressed).then(|| vec![k.key()]),
                        );
                        let _ = keyboard_event_tx.try_send(event);
                    }
                    input::Event::Pointer(PointerEvent::Button(p)) => {
                        let _ = mouse_event_tx.try_send(MouseEvent::new(
                            None,
                            None,
                            None,
                            (p.button_state() == ButtonState::Released).then(|| vec![p.button()]),
                            (p.button_state() == ButtonState::Pressed).then(|| vec![p.button()]),
                        ));
                    }
                    input::Event::Pointer(PointerEvent::Motion(m)) => {
                        let _ = mouse_event_tx.try_send(MouseEvent::new(
                            Some(vec2(m.dx() as f32, m.dy() as f32).into()),
                            None,
                            None,
                            None,
                            None,
                        ));
                    }
                    input::Event::Pointer(PointerEvent::ScrollContinuous(s)) => {
                        let _ = mouse_event_tx.try_send(MouseEvent::new(
                            None,
                            Some(
                                vec2(
                                    s.scroll_value(Axis::Horizontal) as f32,
                                    s.scroll_value(Axis::Vertical) as f32,
                                )
                                .into(),
                            ),
                            None,
                            None,
                            None,
                        ));
                    }
                    input::Event::Pointer(PointerEvent::ScrollWheel(s)) => {
                        let _ = mouse_event_tx.try_send(MouseEvent::new(
                            None,
                            None,
                            Some(
                                (vec2(
                                    s.scroll_value_v120(Axis::Horizontal) as f32,
                                    s.scroll_value_v120(Axis::Vertical) as f32,
                                ) / 120.0)
                                    .into(),
                            ),
                            None,
                            None,
                        ));
                    }
                    _ => (),
                }
            }
        }
    });

    let result = tokio::select! {
        biased;
        _ = tokio::signal::ctrl_c() => Ok(()),
        e = event_loop => e?.map_err(|e| e.into()),
    };
    let _ = stop_libinput_tx.send(());

    result
}

struct Eclipse {
    mouse_event_rx: Receiver<MouseEvent>,
    mouse_pulse_sender: HandlerWrapper<PulseSender, DummyHandler>,
    keyboard_event_rx: Receiver<KeyboardEvent>,
    keyboard_pulse_sender: HandlerWrapper<PulseSender, DummyHandler>,
}
impl Eclipse {
    pub fn create(
        client: &Client,
        mouse_event_rx: Receiver<MouseEvent>,
        keyboard_event_rx: Receiver<KeyboardEvent>,
    ) -> Result<Self> {
        let mouse_pulse_sender =
            PulseSender::create(client.get_root(), Transform::identity(), &MOUSE_MASK)?
                .wrap(DummyHandler)?;
        let keyboard_pulse_sender =
            PulseSender::create(client.get_root(), Transform::identity(), &KEYBOARD_MASK)?
                .wrap(DummyHandler)?;

        Ok(Eclipse {
            mouse_event_rx,
            mouse_pulse_sender,
            keyboard_event_rx,
            keyboard_pulse_sender,
        })
    }
}
impl RootHandler for Eclipse {
    fn frame(&mut self, _info: FrameInfo) {
        while let Ok(mouse_event) = self.mouse_event_rx.try_recv() {
            let receivers = self.mouse_pulse_sender.node().receivers();
            let Some((receiver, _field)) = receivers.values().nth(0) else {break};
            dbg!(&mouse_event);
            mouse_event.send_event(self.mouse_pulse_sender.node(), &[receiver])
        }
        while let Ok(keyboard_event) = self.keyboard_event_rx.try_recv() {
            let receivers = self.keyboard_pulse_sender.node().receivers();
            let Some((receiver, _field)) = receivers.values().nth(0) else {break};
            dbg!(&receiver.node().get_path());
            dbg!(&keyboard_event);
            keyboard_event.send_event(self.keyboard_pulse_sender.node(), &[receiver])
        }
    }
}

struct DummyHandler;
impl PulseSenderHandler for DummyHandler {
    fn new_receiver(
        &mut self,
        _info: NewReceiverInfo,
        _receiver: PulseReceiver,
        _field: UnknownField,
    ) {
    }

    fn drop_receiver(&mut self, _uid: &str) {}
}
