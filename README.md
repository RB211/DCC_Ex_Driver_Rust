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
  STOP / E-STOP ALL, and F0–F28 buttons (per-button momentary/toggle via
  right-click, per-loco visible subset and labels via Setup)
- Track power controls, current polling with trip-scaled bar and latched
  overload display
- Programming tab: service-mode address/CV read-write, CV29 bit editor
  (bits 6–7 preserved), POM writes, live NMRA CV name lookup
- Console with colour-tagged log and raw command entry

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
