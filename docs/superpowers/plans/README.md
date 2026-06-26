# springchick milestone roadmap

springchick is a fused Wayland compositor + iOS-Springboard shell for the Fairphone 5.
See the design spec: `docs/superpowers/specs/2026-06-26-springchick-design.md`.

Each milestone gets its own plan via the superpowers:writing-plans skill, building on M1.

- **M1 — Foundation** (`2026-06-26-springchick-foundation.md`): Cargo workspace + Nix flake,
  pure-logic cores (`sc-anim` spring engine, `sc-input` gesture/nav classifier,
  `sc-shell-model` grid model, `sc-config` TOML persistence + `.desktop` catalog), the
  winit dev harness (`sc-compositor`), and the Skia-on-Smithay-GLES spike. **Done** —
  20/20 tests green, clippy clean. Remaining gate: human visual confirmation of the
  blurred rounded rect on a real display.
- **M2 — Home screen render:** wire `sc-shell-model` + `sc-config` + Skia into a real
  paginated grid + dock; launch apps as XDG toplevels composited as textures. Build the
  cached Skia `Surface`/`BackendRenderTarget` (the M1 spike rebuilds per frame — see the
  `SPIKE ONLY` note in `skia_gl.rs`).
- **M3 — Navigation:** bottom-bar grab/shrink/switcher/quick-switch driven by `sc-input`
  + `sc-anim`; on-harness tuning of `sc-input/src/thresholds.rs` and the spring constants.
- **M4 — Edit mode:** jiggle + drag-rearrange + delete (folders / page-reorder deferred).
- **M5 — Device bring-up:** the `drm` backend (currently `todo!()`), libinput touch,
  power button + idle blank on the Fairphone 5. Revisit the M1 spike assumptions flagged
  in `skia_gl.rs` (stencil bits, RGBA8 vs BGRA8 surface format) for the device GPU.
