//! DCC-EX Native Protocol Throttle (Rust)
//! =======================================
//! An egui/eframe throttle that speaks the DCC-EX native command protocol
//! directly to an EX-CommandStation (EX-CSB1 etc.) over TCP or USB serial.
//!
//!   TCP    : default port 2560
//!   Serial : default 115200 baud
//!
//! The command station auto-selects native vs WiThrottle protocol based on
//! the first command it receives, so this client sends <s> immediately on
//! connect to lock it into native mode and pull the version/status.
//!
//! A line-for-line functional port of dccex_throttle.py; see that file and
//! CLAUDE.md for the protocol notes and the state-machine invariants. It
//! shares the same config file (~/.config/dccex-throttle.json).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod cv;
mod panel;
mod transport;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("DCC-EX Native Throttle")
            .with_inner_size([1180.0, 980.0])
            .with_min_inner_size([720.0, 640.0]),
        ..Default::default()
    };
    eframe::run_native(
        "DCC-EX Native Throttle",
        options,
        Box::new(|cc| Ok(Box::new(app::ThrottleApp::new(cc)))),
    )
}
