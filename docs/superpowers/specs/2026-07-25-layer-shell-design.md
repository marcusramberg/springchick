# wlr-layer-shell + touch routing + virtual keyboard

Date: 2026-07-25
Status: approved for implementation

## Problem

wvkbd (the on-screen keyboard) connects to springchick now, but fails with `layer_shell not
available`. It is a `zwlr_layer_shell_v1` client that renders its UI as a layer surface,
receives touch on that surface, and injects the resulting keystrokes via
`zwp_virtual_keyboard_v1`. springchick implements none of these, and — more fundamentally —
never routes touch or pointer to any client surface; all touch is consumed by the home
gesture funnel.

## Goal

Run wvkbd end to end: it maps a bottom-anchored keyboard, the focused app shrinks to the area
above it, tapping keys highlights them, and typed characters reach the focused app.

## Non-goals

- Layer-shell popups, per-output targeting (single output only), keyboard-interactivity focus
  models beyond what wvkbd needs.
- Pointer routing to app windows (touch only; the device is touch).
- Multi-edge / centered exclusive zones (v1 reserves only for single-edge-anchored surfaces).

## Components

- `crates/sc-layout/src/layer.rs` (new, pure): `usable_area` and `layer_rect`. No wayland types.
- `crates/sc-compositor/src/layer_shell.rs` (new): `WlrLayerShellState`, handler,
  `delegate_layer_shell!`; tracks mapped layer surfaces and their computed geometry.
- `crates/sc-compositor/src/touch.rs` (new): hit-test + wl_touch forwarding vs gesture funnel.
- `render.rs`: composite layer surfaces by z-order.
- `main.rs`: `VirtualKeyboardManagerState` wiring; `reconfigure_toplevels`; app sizing from the
  usable area.

## Layout (pure, in sc-layout)

`usable_area(output: Size, reserved: &[Reservation]) -> Rect` folds exclusive zones into the
app rectangle. A `Reservation { edge, size }` shrinks the usable rect on that edge.

`layer_rect(output: Size, usable: Rect, anchor, size, margins) -> Rect` places one layer
surface against the full output per its anchors, requested size (0 = stretch to anchored
span), and margins.

Exclusive-zone rules:
- `> 0`: reserve that many px on the single anchored edge.
- `0`: laid out, reserves nothing (overlaps the app).
- `-1`: spans the full output, reserves nothing.
- Multi-edge or centered anchor with a positive zone: log and ignore the reservation (v1).

## Data flow

```
new_layer_surface → store → send configure (size from anchors + usable area)
commit             → update geometry → recompute usable area
                     → if changed, reconfigure app toplevels to fit
render             → bg/bottom → app (in usable area) → top/overlay
                     → springchick bar + OSD (chrome, always topmost)
touch down         → hit-test top/overlay surfaces, topmost first
                       hit  → grab; forward wl_touch down/move/up to that client
                       miss → existing gesture funnel (unchanged)
virtual key        → forward straight to the focused app (no keybind matcher)
```

Layer surfaces are always placed against the full output; only the app is confined to the
usable area. springchick's own bar/OSD ignore exclusive zones.

## Testing

- Pure `sc-layout::layer`: `usable_area` with 0/1/2 reservations, each edge, `-1`, and a zone
  larger than the output (clamped); `layer_rect` for bottom-full-width, each anchor, margins,
  centered-no-anchor.
- Pure touch hit-test: point on a rect → that surface; overlapping → topmost; miss → none;
  only Top/Overlay are hit-testable.
- Compositor unit: a usable-area change triggers exactly one reconfigure; an injected virtual
  key bypasses the keybind matcher.
- Manual on-device: wvkbd maps and draws at the bottom; the app shrinks above it; key taps
  highlight; typed characters land in a focused client; dismissing wvkbd restores full size.

No headless e2e — layer-shell + touch + virtual-keyboard needs a real layer-shell client,
which the debug socket does not model. The pure geometry/routing carry automated coverage;
wvkbd on the phone is the integration test.

## Risks

- **Touch grab correctness.** A touch that starts on the keyboard must keep going there for
  the whole sequence even if the finger slides off; a miss must not leak the down to gestures
  mid-sequence. Covered by hit-test unit tests + on-device.
- **App resize churn.** Reconfiguring toplevels on every layer commit could thrash; only
  reconfigure when the computed usable area actually changes.
- **Virtual-keyboard focus.** Injected keys need a focused client; if none, they are dropped.
