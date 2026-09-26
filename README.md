# DCC-EX Native Throttle — Rust port

A Rust rewrite of the DCC-EX native-protocol throttle from
[RB211/DCC_Ex_Driver](https://github.com/RB211/DCC_Ex_Driver).
**Based on that repository's `Development` branch** at commit `77b28f3`
(the multi-loco-tabs state). The Python original is unchanged over there
and remains the reference implementation.

Feature-equivalent with the Python app and shares the same config file
(`~/.config/dccex-throttle.json`), including the pre-multi-loco migration:

- TCP (port 2560) and USB serial (115200) transports, `<s>` on connect
- Per-loco tabs with address, speed slider, colour-coded direction toggle,
  STOP / E-STOP ALL, and F0–F28 buttons (right-click cycles each button
  through hold / latching / pulsed — pulsed fires one timed one-shot per
  press and shows three dots under the label; per-loco visible subset and
  labels via Setup)
- Track power controls, current polling with trip-scaled bar and latched
  overload display
- Programming tab: service-mode address/CV read-write, CV29 bit editor
  (bits 6–7 preserved), POM writes, live NMRA CV name lookup
- Console with colour-tagged log and raw command entry

Beyond the Python app (Rust-only additions, stored under new keys in the
shared config file, which the Python app ignores):

- Automation tab: named scripts in a small line-based language (`speed`,
  `forward`/`reverse`, `stop`, `estop`, `func`, `pulse`, `wait`, `power`,
  `throw`/`close`, `send`, nestable `repeat N … end`, `#` comments),
  parsed in full before anything is sent and executed non-blocking from
  the frame loop with rate-capped sends
- Layout tab: a grid track plan (straights, auto-orienting Left/Right
  curves, diagonals with levelling ramps to carry a turnout's diverging
  leg into a siding, crossings, turnouts in eight orientations) with a
  built-in editor; clicking a turnout sends
  `<T id 1|0>`, and route colouring follows the station's `<H>` broadcasts
  (`<JT>` roster/state sync on connect). Turnouts must be defined on the
  command station under the same IDs.

Built on [egui/eframe](https://github.com/emilk/egui) (immediate mode — the
Tk version's `syncing` re-entrancy guard is unnecessary by construction; the
other state-machine invariants documented in the parent repository's
`CLAUDE.md` §4 are ported as-is), plus `serialport` and `serde_json`.

## Build

```
cargo build --release        # binary at target/release/dccex-throttle
cargo test                   # protocol framing, config, panel state tests
```

Linux needs libudev headers (Arch: part of `systemd`) for serial-port
enumeration. No other system dependencies beyond a working GPU stack for
egui's wgpu backend.
