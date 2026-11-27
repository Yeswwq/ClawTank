//! Client input → host. IronRDP decodes keyboard/mouse PDUs into events; we translate them
//! into a small, flat [`InputEvent`] and hand them to the host (which injects CGEvents).
//! Nothing here interprets keys: scancode → macOS keycode mapping lives in the host.

use std::collections::HashMap;
use std::sync::Arc;

use ironrdp_server::{
    CsReadyPdu, DismissHoveringTouchContactPdu, KeyboardEvent, MouseButton as RdpMouseButton,
    MouseEvent, RdpInputProtocolVersion, RdpServerInputHandler, RdpeiHandler, RdpeiServer,
    RdpeiServerFactory, ServerEvent, ServerEventSender, TouchContactFlags, TouchEventPdu,
};
use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};

/// Mouse buttons, numbered as in the FFI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left = 0,
    Right = 1,
    Middle = 2,
    X1 = 3,
    X2 = 4,
}

/// One input event in desktop coordinates (the negotiated session size).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    /// Set-1 scancode; `extended` is the 0xE0 prefix.
    Key {
        code: u8,
        extended: bool,
        down: bool,
    },
    /// A UTF-16 code unit (IME / on-screen keyboards).
    Unicode {
        unit: u16,
        down: bool,
    },
    /// Lock-key state from the client (bit 0 Scroll, 1 Num, 2 Caps, 3 Kana).
    Sync {
        flags: u32,
    },
    Move {
        x: u16,
        y: u16,
    },
    RelMove {
        dx: i32,
        dy: i32,
    },
    Button {
        button: MouseButton,
        down: bool,
    },
    /// Wheel rotation units: 120 per notch, positive = away from the user / right.
    Wheel {
        vertical: i32,
        horizontal: i32,
    },
    /// One RDPEI contact sample. `flags` is the validated MS-RDPEI contactFlags value.
    Touch {
        contact_id: u8,
        flags: u32,
        x: i32,
        y: i32,
    },
}

pub type InputSink = Arc<dyn Fn(InputEvent) + Send + Sync>;

/// IronRDP input handler that forwards everything to the host.
pub struct InputForwarder {
    sink: InputSink,
}

impl InputForwarder {
    pub fn new(sink: InputSink) -> Self {
        Self { sink }
    }

    fn emit(&self, event: InputEvent) {
        trace!(?event, "input");
        (self.sink)(event);
    }
}

fn translate_button(button: RdpMouseButton) -> Option<MouseButton> {
    Some(match button {
        RdpMouseButton::Left => MouseButton::Left,
        RdpMouseButton::Right => MouseButton::Right,
        RdpMouseButton::Middle => MouseButton::Middle,
        RdpMouseButton::X1 => MouseButton::X1,
        RdpMouseButton::X2 => MouseButton::X2,
        _ => return None,
    })
}

pub fn translate_mouse(event: MouseEvent) -> Vec<InputEvent> {
    match event {
        MouseEvent::Move { x, y } => vec![InputEvent::Move { x, y }],
        MouseEvent::RelMove { x, y } => vec![InputEvent::RelMove { dx: x, dy: y }],
        MouseEvent::Button {
            x,
            y,
            button,
            pressed,
        } => {
            let Some(button) = translate_button(button) else {
                return Vec::new();
            };
            vec![
                InputEvent::Move { x, y },
                InputEvent::Button {
                    button,
                    down: pressed,
                },
            ]
        }
        MouseEvent::ButtonRel {
            x,
            y,
            button,
            pressed,
        } => {
            let Some(button) = translate_button(button) else {
                return Vec::new();
            };
            vec![
                InputEvent::RelMove { dx: x, dy: y },
                InputEvent::Button {
                    button,
                    down: pressed,
                },
            ]
        }
        MouseEvent::VerticalScroll { value } => vec![InputEvent::Wheel {
            vertical: i32::from(value),
            horizontal: 0,
        }],
        MouseEvent::HorizontalScroll { value } => vec![InputEvent::Wheel {
            vertical: 0,
            horizontal: i32::from(value),
        }],
        // Advanced-input (ainput) wheel deltas, same 120-per-notch convention.
        MouseEvent::Scroll { x, y } => vec![InputEvent::Wheel {
            vertical: y,
            horizontal: x,
        }],
        _ => Vec::new(),
    }
}

pub fn translate_keyboard(event: KeyboardEvent) -> Option<InputEvent> {
    Some(match event {
        KeyboardEvent::Pressed { code, extended } => InputEvent::Key {
            code,
            extended,
            down: true,
        },
        KeyboardEvent::Released { code, extended } => InputEvent::Key {
            code,
            extended,
            down: false,
        },
        KeyboardEvent::UnicodePressed(unit) => InputEvent::Unicode { unit, down: true },
        KeyboardEvent::UnicodeReleased(unit) => InputEvent::Unicode { unit, down: false },
        KeyboardEvent::Synchronize(flags) => InputEvent::Sync {
            flags: u32::from(flags.bits()),
        },
    })
}

impl RdpServerInputHandler for InputForwarder {
    fn keyboard(&mut self, event: KeyboardEvent) {
        if let Some(e) = translate_keyboard(event) {
            self.emit(e);
        }
    }

    fn mouse(&mut self, event: MouseEvent) {
        for e in translate_mouse(event) {
            self.emit(e);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TouchState {
    Hovering,
    Engaged,
}

/// Per-connection RDPEI handler. The protocol channel is connection-scoped, so the active
/// contact map must never be shared between clients.
struct TouchRdpeiHandler {
    sink: InputSink,
    max_contacts: u16,
    ready: bool,
    active: HashMap<u8, (TouchState, i32, i32)>,
    closed: bool,
}

impl TouchRdpeiHandler {
    fn new(sink: InputSink) -> Self {
        Self {
            sink,
            max_contacts: 0,
            ready: false,
            active: HashMap::new(),
            closed: false,
        }
    }

    fn emit(&self, contact_id: u8, flags: TouchContactFlags, x: i32, y: i32) {
        trace!(contact_id, flags = flags.bits(), x, y, "rdpei touch input");
        (self.sink)(InputEvent::Touch {
            contact_id,
            flags: flags.bits(),
            x,
            y,
        });
    }

    fn release_all(&mut self) {
        let contacts: Vec<_> = self
            .active
            .drain()
            .map(|(id, (state, x, y))| (id, state, x, y))
            .collect();
        for (id, state, x, y) in contacts {
            let flags = match state {
                TouchState::Hovering => TouchContactFlags::UPDATE | TouchContactFlags::CANCELED,
                TouchState::Engaged => TouchContactFlags::UP | TouchContactFlags::CANCELED,
            };
            self.emit(id, flags, x, y);
        }
    }

    fn apply_contact(&mut self, contact_id: u8, flags: TouchContactFlags, x: i32, y: i32) {
        if !flags.is_legal() {
            warn!(
                contact_id,
                flags = flags.bits(),
                "ignoring illegal RDPEI touch flags"
            );
            return;
        }
        let previous = self.active.get(&contact_id).copied();
        let next = if flags.contains(TouchContactFlags::DOWN) {
            if previous.is_some_and(|(state, _, _)| state == TouchState::Engaged) {
                warn!(contact_id, "RDPEI DOWN repeated for active contact");
                return;
            }
            Some(TouchState::Engaged)
        } else if flags.contains(TouchContactFlags::UP) {
            if previous.is_none() {
                warn!(contact_id, "RDPEI UP received for unknown contact");
            }
            if flags.contains(TouchContactFlags::INRANGE) {
                Some(TouchState::Hovering)
            } else {
                None
            }
        } else if flags.contains(TouchContactFlags::UPDATE) {
            let state = if flags.contains(TouchContactFlags::INCONTACT) {
                TouchState::Engaged
            } else {
                TouchState::Hovering
            };
            if previous.is_none() {
                debug!(contact_id, "RDPEI UPDATE opened an implicit contact");
            }
            Some(state)
        } else {
            warn!(
                contact_id,
                flags = flags.bits(),
                "RDPEI contact has no transition flag"
            );
            return;
        };

        if previous.is_none()
            && next.is_some()
            && self.active.len() >= usize::from(self.max_contacts.min(256))
        {
            warn!(
                contact_id,
                active_contacts = self.active.len(),
                max_contacts = self.max_contacts,
                "ignoring RDPEI contact above negotiated active-contact limit"
            );
            return;
        }

        self.emit(contact_id, flags, x, y);
        match next {
            Some(state) => {
                self.active.insert(contact_id, (state, x, y));
            }
            None => {
                self.active.remove(&contact_id);
            }
        }
    }
}

impl RdpeiHandler for TouchRdpeiHandler {
    fn cs_ready(&mut self, pdu: CsReadyPdu) {
        self.max_contacts = pdu.max_touch_contacts.min(256);
        self.ready = pdu.max_touch_contacts > 0;
        info!(
            protocol_version = ?pdu.protocol_version,
            max_touch_contacts = pdu.max_touch_contacts,
            flags = pdu.flags.bits(),
            "RDPEI client ready"
        );
        if !self.ready {
            warn!("RDPEI client advertised zero touch contacts");
        } else if pdu.max_touch_contacts > 256 {
            warn!(
                advertised_max_contacts = pdu.max_touch_contacts,
                effective_max_contacts = 256,
                "clamping RDPEI contacts to the protocol contact-ID range"
            );
        }
    }

    fn touch(&mut self, pdu: TouchEventPdu) {
        if !self.ready {
            warn!("ignoring RDPEI touch event before CS_READY");
            return;
        }
        for frame in pdu.frames {
            if frame.contacts.len() > usize::from(self.max_contacts) {
                warn!(
                    contacts = frame.contacts.len(),
                    max_contacts = self.max_contacts,
                    "ignoring RDPEI frame above advertised contact limit"
                );
                continue;
            }
            for contact in frame.contacts {
                self.apply_contact(
                    contact.contact_id,
                    contact.contact_flags,
                    contact.x,
                    contact.y,
                );
            }
        }
    }

    fn dismiss_hovering(&mut self, pdu: DismissHoveringTouchContactPdu) {
        if let Some((state, x, y)) = self.active.remove(&pdu.contact_id) {
            debug!(
                contact_id = pdu.contact_id,
                "RDPEI hovering contact dismissed"
            );
            let flags = match state {
                TouchState::Hovering => TouchContactFlags::UPDATE | TouchContactFlags::CANCELED,
                TouchState::Engaged => TouchContactFlags::UP | TouchContactFlags::CANCELED,
            };
            self.emit(pdu.contact_id, flags, x, y);
        }
    }

    fn close(&mut self) {
        if !self.closed {
            self.closed = true;
            self.release_all();
        }
    }
}

#[derive(Clone)]
pub struct TouchRdpeiFactory {
    sink: InputSink,
}

impl TouchRdpeiFactory {
    pub fn new(sink: InputSink) -> Self {
        Self { sink }
    }
}

impl ServerEventSender for TouchRdpeiFactory {
    fn set_sender(&mut self, _sender: mpsc::UnboundedSender<ServerEvent>) {}
}

impl RdpeiServerFactory for TouchRdpeiFactory {
    fn build_server(&self) -> RdpeiServer {
        RdpeiServer::new(Box::new(TouchRdpeiHandler::new(Arc::clone(&self.sink))))
            .with_protocol_version(RdpInputProtocolVersion::V300)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironrdp_server::{CsReadyFlags, TouchContact, TouchFrame};
    use std::sync::Mutex;

    fn touch(flags: TouchContactFlags, contact_id: u8, x: i32, y: i32) -> TouchEventPdu {
        TouchEventPdu::new(
            0,
            vec![TouchFrame::new(
                0,
                vec![TouchContact::new(contact_id, x, y, flags)],
            )],
        )
    }

    fn touch_handler() -> (TouchRdpeiHandler, Arc<Mutex<Vec<InputEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let sink: InputSink = Arc::new(move |event| captured.lock().unwrap().push(event));
        (TouchRdpeiHandler::new(sink), events)
    }

    #[test]
    fn mouse_events_translate() {
        assert_eq!(
            translate_mouse(MouseEvent::Button {
                x: 1,
                y: 2,
                button: RdpMouseButton::X1,
                pressed: true,
            }),
            vec![
                InputEvent::Move { x: 1, y: 2 },
                InputEvent::Button {
                    button: MouseButton::X1,
                    down: true
                }
            ]
        );
        assert_eq!(
            translate_mouse(MouseEvent::HorizontalScroll { value: -120 }),
            vec![InputEvent::Wheel {
                vertical: 0,
                horizontal: -120
            }]
        );
        assert_eq!(
            translate_mouse(MouseEvent::Move { x: 10, y: 20 }),
            vec![InputEvent::Move { x: 10, y: 20 }]
        );
        assert_eq!(
            translate_mouse(MouseEvent::ButtonRel {
                x: -2,
                y: 3,
                button: RdpMouseButton::Right,
                pressed: false,
            }),
            vec![
                InputEvent::RelMove { dx: -2, dy: 3 },
                InputEvent::Button {
                    button: MouseButton::Right,
                    down: false,
                }
            ]
        );
    }

    #[test]
    fn keyboard_events_translate() {
        assert_eq!(
            translate_keyboard(KeyboardEvent::Pressed {
                code: 0x1d,
                extended: true
            }),
            Some(InputEvent::Key {
                code: 0x1d,
                extended: true,
                down: true
            })
        );
        assert_eq!(
            translate_keyboard(KeyboardEvent::UnicodeReleased(0x00e9)),
            Some(InputEvent::Unicode {
                unit: 0x00e9,
                down: false
            })
        );
    }

    #[test]
    fn rdpei_forwards_touch_lifecycle_and_cancels_on_close() {
        let (mut handler, events) = touch_handler();
        handler.cs_ready(CsReadyPdu::new(
            CsReadyFlags::SHOW_TOUCH_VISUALS,
            RdpInputProtocolVersion::V300,
            2,
        ));
        handler.touch(touch(
            TouchContactFlags::DOWN | TouchContactFlags::INRANGE | TouchContactFlags::INCONTACT,
            7,
            100,
            200,
        ));
        handler.touch(touch(
            TouchContactFlags::UPDATE | TouchContactFlags::INRANGE | TouchContactFlags::INCONTACT,
            7,
            110,
            210,
        ));
        handler.close();

        assert_eq!(
            *events.lock().unwrap(),
            vec![
                InputEvent::Touch {
                    contact_id: 7,
                    flags: (TouchContactFlags::DOWN
                        | TouchContactFlags::INRANGE
                        | TouchContactFlags::INCONTACT)
                        .bits(),
                    x: 100,
                    y: 200,
                },
                InputEvent::Touch {
                    contact_id: 7,
                    flags: (TouchContactFlags::UPDATE
                        | TouchContactFlags::INRANGE
                        | TouchContactFlags::INCONTACT)
                        .bits(),
                    x: 110,
                    y: 210,
                },
                InputEvent::Touch {
                    contact_id: 7,
                    flags: (TouchContactFlags::UP | TouchContactFlags::CANCELED).bits(),
                    x: 110,
                    y: 210,
                },
            ]
        );
    }

    #[test]
    fn rdpei_ignores_illegal_flags_and_accepts_hover_to_down_transition() {
        let (mut handler, events) = touch_handler();
        handler.cs_ready(CsReadyPdu::new(
            CsReadyFlags::empty(),
            RdpInputProtocolVersion::V300,
            1,
        ));
        handler.touch(touch(
            TouchContactFlags::DOWN | TouchContactFlags::INRANGE,
            1,
            10,
            20,
        ));
        handler.touch(touch(
            TouchContactFlags::UPDATE | TouchContactFlags::INRANGE,
            1,
            10,
            20,
        ));
        handler.touch(touch(
            TouchContactFlags::DOWN | TouchContactFlags::INRANGE | TouchContactFlags::INCONTACT,
            1,
            10,
            20,
        ));

        assert_eq!(events.lock().unwrap().len(), 2);
        assert_eq!(
            handler.active.get(&1).map(|entry| entry.0),
            Some(TouchState::Engaged)
        );
    }

    #[test]
    fn rdpei_cancels_hovering_contact_with_update_cancel() {
        let (mut handler, events) = touch_handler();
        handler.cs_ready(CsReadyPdu::new(
            CsReadyFlags::empty(),
            RdpInputProtocolVersion::V300,
            1,
        ));
        handler.touch(touch(
            TouchContactFlags::UP | TouchContactFlags::INRANGE,
            1,
            10,
            20,
        ));
        handler.close();

        assert_eq!(
            events.lock().unwrap().last(),
            Some(&InputEvent::Touch {
                contact_id: 1,
                flags: (TouchContactFlags::UPDATE | TouchContactFlags::CANCELED).bits(),
                x: 10,
                y: 20,
            })
        );
    }
}
