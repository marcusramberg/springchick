//! Minimal `zwp_virtual_keyboard_v1` support for on-screen keyboards (wvkbd).
//!
//! smithay's built-in virtual-keyboard handler loads the client's uploaded xkb
//! keymap via `xkb_keymap_new_from_buffer(.., size - 1, ..)`, which assumes the
//! advertised size includes the trailing NUL (the `wl_keyboard` convention).
//! wvkbd sends `size = strlen` (no NUL), so smithay chops the last byte, the
//! keymap fails to compile, and wvkbd's next `modifiers` request is answered
//! with a fatal `NoKeymap` protocol error that kills the keyboard.
//!
//! We sidestep the whole keymap dance: the virtual keyboard sends standard evdev
//! keycodes, so we forward them straight through our own seat keyboard (which
//! already advertised springchick's keymap to the focused client). The uploaded
//! keymap is ignored.

use smithay::input::keyboard::FilterResult;
use smithay::reexports::wayland_protocols_misc::zwp_virtual_keyboard_v1::server::{
    zwp_virtual_keyboard_manager_v1::{self, ZwpVirtualKeyboardManagerV1},
    zwp_virtual_keyboard_v1::{self, ZwpVirtualKeyboardV1},
};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New,
};
use smithay::utils::SERIAL_COUNTER;

use crate::State;

/// evdev → xkb keycode offset. `wl_keyboard`/virtual-keyboard carry evdev codes;
/// smithay's `KeyboardHandle::input` takes xkb keycodes and converts back to
/// evdev when it forwards to the client.
const XKB_OFFSET: u32 = 8;

/// Register the `zwp_virtual_keyboard_manager_v1` global.
pub fn init(dh: &DisplayHandle) {
    dh.create_global::<State, ZwpVirtualKeyboardManagerV1, ()>(1, ());
}

impl GlobalDispatch<ZwpVirtualKeyboardManagerV1, ()> for State {
    fn bind(
        _state: &mut State,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwpVirtualKeyboardManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, State>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for State {
    fn request(
        _state: &mut State,
        _client: &Client,
        _resource: &ZwpVirtualKeyboardManagerV1,
        request: zwp_virtual_keyboard_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        if let zwp_virtual_keyboard_manager_v1::Request::CreateVirtualKeyboard { id, .. } = request
        {
            data_init.init(id, ());
        }
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for State {
    fn request(
        state: &mut State,
        _client: &Client,
        _resource: &ZwpVirtualKeyboardV1,
        request: zwp_virtual_keyboard_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        match request {
            // The client's keymap is deliberately ignored — the key events it
            // sends are standard evdev codes, which we interpret with our own
            // seat keymap.
            zwp_virtual_keyboard_v1::Request::Keymap { .. } => {}
            zwp_virtual_keyboard_v1::Request::Key {
                time,
                key,
                state: key_state,
            } => {
                let keyboard = state.keyboard.clone();
                let code = (key + XKB_OFFSET).into();
                // The protocol carries a raw u32 (wl_keyboard::KeyState): 1 =
                // pressed, 0 = released.
                let ks = if key_state == 1 {
                    smithay::backend::input::KeyState::Pressed
                } else {
                    smithay::backend::input::KeyState::Released
                };
                keyboard.input::<(), _>(
                    state,
                    code,
                    ks,
                    SERIAL_COUNTER.next_serial(),
                    time,
                    |_, _, _| FilterResult::Forward,
                );
            }
            // Modifiers are applied by the seat keymap from the forwarded
            // keycodes, so explicit modifier state from the OSK is dropped.
            zwp_virtual_keyboard_v1::Request::Modifiers { .. } => {}
            _ => {}
        }
    }
}
