//! The throttle application: transport ownership, shared UI (connection,
//! track power, console), the Run/Programming/Automation/Layout tabs and
//! the loco tab roster.
//!
//! Functional port of the Tk app. Immediate mode changes one thing for the
//! better: widget callbacks don't exist, so the `syncing` re-entrancy guard
//! from the Tk version isn't needed -- an inbound broadcast just writes the
//! state and the next frame draws it. Everything else (pending_speed
//! lifecycle, last_state, revert-on-failed-send) ports unchanged.

use std::collections::BTreeSet;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText, Stroke};

use crate::config::{self, LocoCfg, ScriptCfg, MAX_ADDR};
use crate::cv::{cv_desc, cv_name};
use crate::layout::{self, Cell, Layout, Piece, TKind, Tool};
use crate::panel::{LocoPanel, MAX_SPEED, NFUNC};
use crate::script::{self, Runner};
use crate::transport::{self, Link, RxEvent};

const SEND_INTERVAL: Duration = Duration::from_millis(80); // throttle sends while dragging
/// A pulsed function button holds the function on this long per press:
/// a short blip -- the decoder sees the edge and fires its effect once.
const FUNC_PULSE: Duration = Duration::from_millis(80);
// The station never pushes current -- <c> is request/reply only -- so
// this poll rate IS the meter's refresh rate. 4 Hz is still trivial
// traffic for the station and the link.
const CURRENT_POLL: Duration = Duration::from_millis(250);

// Current-meter ballistics. The <c> samples jump around with every
// motor, so the display shows an exponential average: slow enough on
// the way up to smooth the jitter (~1.5 s to settle at 4 Hz), and
// gentler still on the way down, so the reading holds steady through
// dips and drains over several seconds when the load really drops.
const CURRENT_ATTACK: f32 = 0.15;
const CURRENT_DECAY: f32 = 0.125;

/// Fold one <c> sample into the running average; the first sample primes it.
fn smooth_current(avg: Option<f32>, sample: f32) -> f32 {
    let Some(avg) = avg else {
        return sample;
    };
    let alpha = if sample >= avg { CURRENT_ATTACK } else { CURRENT_DECAY };
    avg + alpha * (sample - avg)
}

// Console tag colours (same palette as the Tk app).
const COL_TX: Color32 = Color32::from_rgb(0x88, 0xc0, 0xd0);
const COL_RX: Color32 = Color32::from_rgb(0xa3, 0xbe, 0x8c);
const COL_ERR: Color32 = Color32::from_rgb(0xbf, 0x61, 0x6a);
const COL_INFO: Color32 = Color32::from_rgb(0xeb, 0xcb, 0x8b);
const COL_CONSOLE_BG: Color32 = Color32::from_rgb(0x10, 0x14, 0x18);

const COL_FWD: Color32 = Color32::from_rgb(0x2e, 0x7d, 0x32);
const COL_REV: Color32 = Color32::from_rgb(0xef, 0x6c, 0x00);
const COL_ESTOP: Color32 = Color32::from_rgb(0xc6, 0x28, 0x28);
const COL_FUNC_ON: Color32 = Color32::from_rgb(0x2e, 0x7d, 0x32);

// Layout canvas palette.
const COL_TRACK: Color32 = Color32::from_rgb(0x8a, 0x91, 0x99);
const COL_ROUTE_SET: Color32 = Color32::from_rgb(0x4c, 0xaf, 0x50);
const COL_ROUTE_OFF: Color32 = Color32::from_rgb(0x45, 0x4a, 0x50);
const COL_GRID: Color32 = Color32::from_rgb(0x28, 0x2e, 0x34);

/// Starter script shown when the config has none yet; doubles as a demo of
/// the command set.
const EXAMPLE_SCRIPT: &str = "\
# Example: drive loco 3 out and back, tooting the horn on each departure.
# See \"Command reference\" below for the full command list.
power on
forward 3
pulse 3 2 1.0      # horn (F2) for one second
speed 3 60
wait 8
stop 3
wait 2
reverse 3
pulse 3 2 1.0
speed 3 60
wait 8
stop 3
";

#[derive(Clone, Copy, PartialEq)]
pub enum Tag {
    Tx,
    Rx,
    Err,
    Info,
}

impl Tag {
    fn color(self) -> Color32 {
        match self {
            Tag::Tx => COL_TX,
            Tag::Rx => COL_RX,
            Tag::Err => COL_ERR,
            Tag::Info => COL_INFO,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum MainTab {
    Run,
    Programming,
    Automation,
    Layout,
}

pub struct ThrottleApp {
    // connection
    mode_serial: bool,
    host: String,
    port: String,
    serial_port: String,
    serial_ports: Vec<String>,
    baud: String,
    link: Option<Link>,
    rx: Receiver<RxEvent>,
    tx: Sender<RxEvent>,
    status: String,

    // track power / current
    power_state: String,
    current_avg: Option<f32>, // smoothed <c> readings; None = unknown
    max_ma: Option<i64>,     // motor driver capability
    trip_ma: Option<i64>,    // software circuit breaker limit
    overload: bool,          // latched by <p2>, cleared by <p0>/<p1>
    poll_current: bool,
    last_poll: Instant,

    // tabs
    main_tab: MainTab,
    panels: Vec<LocoPanel>,
    selected: usize, // POM and friends target this panel
    next_id: u64,

    // programming
    prog_addr: String,
    prog_cv: String,
    prog_val: String,
    pom_cv: String,
    pom_val: String,
    prog_result: String,
    cv29_bits: [bool; 6],
    cv29_high: u8, // CV29 bits 6-7, preserved from the last confirmed read
    cv29_text: String,

    // automation
    scripts: Vec<ScriptCfg>,
    script_sel: usize,
    script_dirty: bool, // editor/name changes not yet written to disk
    runner: Option<Runner>,
    run_status: String,

    // layout
    layout: Layout,
    layout_edit: bool,
    tool: Tool,
    turnout_kind: TKind, // orientation the Turnout tool places
    sel_turnout: Option<(u32, u32)>,
    turnout_id_entry: String,

    // console
    log: Vec<(Tag, String)>,
    raw: String,
}

impl ThrottleApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // ONE text size everywhere (owner requirement): every proportional
        // text style carries 16 pt so nothing can mismatch; the console's
        // monospace is 13.
        cc.egui_ctx.all_styles_mut(|style| {
            for (text_style, font_id) in style.text_styles.iter_mut() {
                font_id.size = match text_style {
                    egui::TextStyle::Monospace => 13.0,
                    _ => 16.0,
                };
            }
            style.spacing.button_padding = egui::vec2(8.0, 4.0);
        });

        let (tx, rx) = channel();
        let cfg = config::load_app_cfg();
        let mut scripts = cfg.scripts;
        if scripts.is_empty() {
            scripts.push(ScriptCfg {
                name: "Example".to_string(),
                text: EXAMPLE_SCRIPT.to_string(),
            });
        }
        let mut app = ThrottleApp {
            mode_serial: false,
            host: "192.168.4.1".to_string(),
            port: "2560".to_string(),
            serial_port: String::new(),
            serial_ports: Vec::new(),
            baud: "115200".to_string(),
            link: None,
            rx,
            tx,
            status: "Disconnected".to_string(),
            power_state: "power: unknown".to_string(),
            current_avg: None,
            max_ma: None,
            trip_ma: None,
            overload: false,
            poll_current: true,
            last_poll: Instant::now(),
            main_tab: MainTab::Run,
            panels: Vec::new(),
            selected: 0,
            next_id: 1,
            prog_addr: String::new(),
            prog_cv: String::new(),
            prog_val: String::new(),
            pom_cv: String::new(),
            pom_val: String::new(),
            prog_result: "result: --".to_string(),
            cv29_bits: [false; 6],
            cv29_high: 0,
            cv29_text: "CV29 = --".to_string(),
            scripts,
            script_sel: 0,
            script_dirty: false,
            runner: None,
            run_status: "idle".to_string(),
            layout: Layout::from_json(&cfg.layout),
            layout_edit: false,
            tool: Tool::Track(Piece::EW),
            turnout_kind: TKind::EwNe,
            sel_turnout: None,
            turnout_id_entry: String::new(),
            log: Vec::new(),
            raw: String::new(),
        };
        for loco in cfg.locos {
            let id = app.next_id;
            app.next_id += 1;
            app.panels.push(LocoPanel::from_cfg(&loco, id));
        }
        app.refresh_ports();
        app
    }

    // ---------------- logging / config ----------------
    fn log(&mut self, text: impl Into<String>, tag: Tag) {
        self.log.push((tag, text.into()));
        if self.log.len() > 500 {
            self.log.drain(..100);
        }
    }

    fn save_config(&mut self) {
        let cfgs: Vec<LocoCfg> = self.panels.iter().map(|p| p.to_cfg()).collect();
        match config::save_app_cfg(&cfgs, &self.scripts, self.layout.to_json()) {
            // Every save writes the whole file, scripts included, so any
            // successful save clears the Automation tab's dirty marker.
            Ok(()) => self.script_dirty = false,
            Err(e) => {
                let path = config::config_path();
                self.log(format!("-- could not save {}: {e}", path.display()), Tag::Err);
            }
        }
    }

    // ---------------- outbound ----------------
    /// True if the command actually reached the wire.
    ///
    /// Callers that flipped state before sending must revert it on false, or
    /// the UI ends up asserting a state the loco was never told about (a
    /// failed send also tears down the transport, so no broadcast will ever
    /// arrive to correct it). `quiet` suppresses the console echo for
    /// polled traffic.
    fn send_cmd(&mut self, cmd: &str, quiet: bool) -> bool {
        let Some(link) = self.link.as_mut() else {
            if !quiet {
                self.log("not connected", Tag::Err);
            }
            return false;
        };
        match link.send(cmd) {
            Ok(()) => {
                if !quiet {
                    self.log(format!(">> {cmd}"), Tag::Tx);
                }
                true
            }
            Err(e) => {
                self.disconnect(&format!("Send failed: {e}"));
                false
            }
        }
    }

    /// Throttle send that commits last_state only after a successful send.
    fn send_throttle(&mut self, panel: &mut LocoPanel, speed: u8, direction: u8) -> bool {
        if !self.send_cmd(&format!("<t {} {speed} {direction}>", panel.active_cab), false) {
            return false;
        }
        panel.last_state = Some((speed, direction));
        panel.last_sent = Instant::now();
        true
    }

    // ---------------- connection ----------------
    fn refresh_ports(&mut self) {
        self.serial_ports = transport::list_serial_ports();
        if !self.serial_ports.is_empty() && self.serial_port.is_empty() {
            self.serial_port = self
                .serial_ports
                .iter()
                .find(|p| p.to_lowercase().contains("usb"))
                .unwrap_or(&self.serial_ports[0])
                .clone();
        }
    }

    fn toggle_connect(&mut self, ctx: &egui::Context) {
        if self.link.is_some() {
            self.disconnect("Disconnected");
            return;
        }
        let repaint = {
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        };
        let result: Result<(Link, String), String> = if self.mode_serial {
            match self.baud.trim().parse::<u32>() {
                Ok(baud) => {
                    Link::serial(self.serial_port.trim(), baud, self.tx.clone(), repaint)
                        .map(|l| (l, format!("{} @ {}", self.serial_port.trim(), baud)))
                }
                Err(_) => Err("baud must be a number".to_string()),
            }
        } else {
            match self.port.trim().parse::<u16>() {
                Ok(port) => {
                    Link::tcp(self.host.trim(), port, self.tx.clone(), repaint)
                        .map(|l| (l, format!("{}:{port}", self.host.trim())))
                        .map_err(|e| e.to_string())
                }
                Err(_) => Err("port must be a number".to_string()),
            }
        };
        match result {
            Ok((link, where_)) => {
                self.link = Some(link);
                self.status = format!("Connected to {where_}");
                self.log(format!("-- connected to {where_}"), Tag::Info);
                for p in &mut self.panels {
                    p.reset_link_state();
                }
                self.send_cmd("<s>", false); // forces native mode + returns status
                let cabs: Vec<u32> = self.panels.iter().map(|p| p.active_cab).collect();
                for cab in cabs {
                    // sync every loco tab instead of guessing
                    self.send_cmd(&format!("<t {cab}>"), false);
                }
                // Turnout states for the Layout tab; the <jT> roster reply
                // fans out into one <JT id> query per turnout.
                self.send_cmd("<JT>", false);
            }
            Err(e) => {
                self.status = format!("Connection failed: {e}");
                self.log(format!("-- connection failed: {e}"), Tag::Err);
            }
        }
    }

    fn disconnect(&mut self, why: &str) {
        if let Some(mut link) = self.link.take() {
            link.close();
        }
        self.status = why.to_string();
        self.power_state = "power: unknown".to_string();
        self.reset_current(); // a stale reading is worse than no reading
        self.layout.states.clear(); // same rule for turnout states
        if self.runner.take().is_some() {
            self.run_status = "stopped: disconnected".to_string();
        }
        self.log(format!("-- {why}"), Tag::Info);
    }

    fn reset_current(&mut self) {
        self.current_avg = None;
        self.max_ma = None;
        self.trip_ma = None;
        self.overload = false;
    }

    // ---------------- periodic work (from update()) ----------------
    /// Rate-limited throttle sends so dragging a slider doesn't flood the
    /// ESP32. Every path out must retire pending_speed -- leaving it armed
    /// makes this re-test the same value every frame forever.
    fn speed_tick(&mut self) {
        let now = Instant::now();
        for i in 0..self.panels.len() {
            if self.link.is_none() {
                return;
            }
            let Some(pending) = self.panels[i].pending_speed else {
                continue;
            };
            let mut panel = std::mem::take(&mut self.panels[i]);
            let target = (pending, panel.direction);
            if Some(target) == panel.last_state {
                // Slider landed back where the station already is. Retire
                // the request instead of re-testing it every frame.
                panel.pending_speed = None;
            } else if now.duration_since(panel.last_sent) >= SEND_INTERVAL {
                self.send_throttle(&mut panel, target.0, target.1);
                // Cleared even on a failed send: the transport is gone, and
                // a stale value must not be replayed at the loco on
                // reconnect.
                panel.pending_speed = None;
            }
            self.panels[i] = panel;
        }
    }

    /// Turn off pulsed functions whose hold time has elapsed.
    fn pulse_tick(&mut self) {
        let now = Instant::now();
        for i in 0..self.panels.len() {
            if self.link.is_none() {
                return;
            }
            let mut panel = std::mem::take(&mut self.panels[i]);
            for n in 0..NFUNC {
                if panel.pulse_off[n].is_some_and(|t| now >= t) {
                    // Cleared even on a failed send: the transport is gone
                    // and reset_link_state will zero everything anyway.
                    panel.pulse_off[n] = None;
                    if self.send_cmd(&format!("<F {} {n} 0>", panel.active_cab), false) {
                        panel.func_state[n] = false;
                    }
                }
            }
            self.panels[i] = panel;
        }
    }

    /// Poll <c> while connected. Quiet: this would otherwise own the log.
    fn current_tick(&mut self) {
        if self.link.is_some()
            && self.poll_current
            && self.last_poll.elapsed() >= CURRENT_POLL
        {
            self.last_poll = Instant::now();
            self.send_cmd("<c>", true);
        }
    }

    fn estop_all(&mut self) {
        if !self.send_cmd("<!>", false) {
            return;
        }
        for p in &mut self.panels {
            p.estop_zero();
        }
    }

    /// Advance the running automation script, if any. The runner is taken
    /// out for the duration so send_cmd/estop_all can borrow self; any
    /// early return without putting it back means the script stopped.
    fn script_tick(&mut self) {
        let Some(mut runner) = self.runner.take() else {
            return;
        };
        let tick = runner.tick(Instant::now());
        for cmd in &tick.cmds {
            if cmd == "<!>" {
                // Route through estop_all so the loco tabs zero too.
                self.estop_all();
                if self.link.is_none() {
                    self.run_status = "stopped: send failed".to_string();
                    return;
                }
            } else if !self.send_cmd(cmd, false) {
                self.run_status = "stopped: send failed".to_string();
                return;
            }
        }
        if tick.done {
            self.run_status = "finished".to_string();
            self.log("-- script finished", Tag::Info);
        } else {
            self.run_status = format!("running (line {})", tick.line);
            self.runner = Some(runner);
        }
    }

    fn run_script(&mut self) {
        if self.link.is_none() {
            self.run_status = "not connected".to_string();
            return;
        }
        let sel = &self.scripts[self.script_sel];
        let (name, text) = (sel.name.clone(), sel.text.clone());
        match script::parse(&text) {
            Err((line, msg)) => {
                self.run_status = format!("error, line {line}: {msg}");
                self.log(format!("-- script error, line {line}: {msg}"), Tag::Err);
            }
            Ok(prog) if prog.is_empty() => {
                self.run_status = "script is empty".to_string();
            }
            Ok(prog) => {
                self.runner = Some(Runner::new(prog));
                self.run_status = "running...".to_string();
                self.log(format!("-- script '{name}' started"), Tag::Info);
            }
        }
    }

    // ---------------- inbound ----------------
    fn pump(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                RxEvent::Error(e) => self.disconnect(&format!("Disconnected: {e}")),
                RxEvent::Msg(body) => {
                    // A polled <c> reply every second would drown the
                    // console. Only hide it while we are the one asking --
                    // a hand-typed <c> with polling off still prints.
                    if !(self.poll_current && body.starts_with("c ")) {
                        self.log(format!("<< <{body}>"), Tag::Rx);
                    }
                    self.handle(&body);
                }
            }
        }
    }

    fn handle(&mut self, body: &str) {
        let parts: Vec<&str> = body.split_whitespace().collect();
        let Some(&head) = parts.first() else {
            return;
        };

        // <l cab reg speedByte functMap> -- fan out to every panel driving
        // that address (two tabs on one cab both stay in sync).
        if head == "l" && parts.len() >= 5 {
            let (Ok(cab), Ok(speed_byte), Ok(func_map)) = (
                parts[1].parse::<u32>(),
                parts[3].parse::<i64>(),
                parts[4].parse::<i64>(),
            ) else {
                return;
            };
            let speed_byte = (speed_byte & 0xFF) as u8;
            let func_map = func_map as u32;
            for p in &mut self.panels {
                if p.active_cab == cab {
                    p.sync_from_broadcast(speed_byte, func_map);
                }
            }
        }
        // <c "CurrentMAIN" mA C "Milli" "0" max "1" trip>
        else if head == "c" {
            self.handle_current(&parts);
        }
        // <p0> / <p1> / <p1 MAIN> / <p2 ...>
        else if head.len() == 2
            && head.starts_with('p')
            && matches!(&head[1..], "0" | "1" | "2")
        {
            let track = parts.get(1).copied().unwrap_or("ALL");
            let state = match &head[1..] {
                "0" => "OFF",
                "1" => "ON",
                _ => "OVERLOAD",
            };
            self.power_state = format!("power: {track} {state}");
            // p2 latches the overload colour; any later p0/p1 is the
            // all-clear. A <c> reply must never clear it -- after a trip the
            // station often reports a low current, which would repaint the
            // display as healthy while the track is dead.
            self.overload = head == "p2";
        }
        // <v cv value> -- CV read result from the prog track, -1 = failed
        else if head == "v" && parts.len() >= 3 {
            let (cv, value) = (parts[1], parts[2]);
            if value == "-1" {
                self.prog_result = format!("{} read FAILED", cv_desc(cv));
            } else {
                self.prog_result = format!("{} = {value}", cv_desc(cv));
                self.prog_val = value.to_string(); // prime for a read-modify-write
                if cv == "29" {
                    if let Ok(v) = value.parse::<u32>() {
                        self.cv29_sync(v);
                    }
                }
            }
        }
        // <r cv value> -- CV write ack; <r address> -- address read result.
        // Same opcode, disambiguated purely by argument count.
        else if head == "r" {
            if parts.len() >= 3 {
                let (cv, value) = (parts[1], parts[2]);
                self.prog_result = if value == "-1" {
                    format!("{} write FAILED", cv_desc(cv))
                } else {
                    format!("{} written: {value}", cv_desc(cv))
                };
                if cv == "29" {
                    if let Ok(v) = value.parse::<u32>() {
                        self.cv29_sync(v);
                    }
                }
            } else if parts.len() == 2 {
                let addr = parts[1];
                if addr == "-1" {
                    self.prog_result = "address read FAILED".to_string();
                } else {
                    self.prog_result = format!("loco address = {addr}");
                    self.prog_addr = addr.to_string();
                }
            }
        }
        // <w cab> -- address write ack (the POM <w cab cv val> has no reply)
        else if head == "w" && parts.len() == 2 {
            let addr = parts[1];
            self.prog_result = if addr == "-1" {
                "address write FAILED".to_string()
            } else {
                format!("address written: {addr}")
            };
        }
        // <H id 0|1> turnout broadcast; also the longer definition forms
        // (<H id DCC addr subaddr state> etc.) -- id first, state last.
        else if head == "H" && parts.len() >= 3 {
            if let Ok(id) = parts[1].parse::<u32>() {
                match *parts.last().unwrap() {
                    "1" | "T" => {
                        self.layout.states.insert(id, true);
                    }
                    "0" | "C" => {
                        self.layout.states.insert(id, false);
                    }
                    _ => {}
                }
            }
        }
        // <jT id1 id2 ...> turnout roster -> query each one;
        // <jT id T|C|X ["desc"]> is the per-turnout answer.
        else if head == "jT" {
            let is_state = parts.len() >= 3 && matches!(parts[2], "T" | "C" | "X" | "0" | "1");
            if is_state {
                if let Ok(id) = parts[1].parse::<u32>() {
                    match parts[2] {
                        "T" | "1" => {
                            self.layout.states.insert(id, true);
                        }
                        "C" | "0" => {
                            self.layout.states.insert(id, false);
                        }
                        _ => {} // X = unknown; leave it unknown
                    }
                }
            } else {
                let ids: Vec<u32> =
                    parts[1..].iter().filter_map(|p| p.parse().ok()).collect();
                for id in ids {
                    self.send_cmd(&format!("<JT {id}>"), true);
                }
            }
        }
        // <iDCC-EX V-5.x.x ...>
        else if head.starts_with('i') {
            self.status = body.trim_start_matches('i').trim().to_string();
        }
    }

    /// <c "CurrentMAIN" current C "Milli" "0" max_ma "1" trip_ma>
    ///
    /// Pulled out by position of the *bare* integers rather than fixed
    /// index: the filler fields are quoted ("0", "1"), so a quoted token
    /// never parses as a number and the unquoted ones are exactly current,
    /// max, trip in that order. That also tolerates the shorter <c current>
    /// some older builds emit.
    fn handle_current(&mut self, parts: &[&str]) {
        let nums: Vec<i64> = parts[1..]
            .iter()
            .filter_map(|p| p.parse::<i64>().ok())
            .collect();
        if nums.is_empty() {
            return;
        }
        self.current_avg = Some(smooth_current(self.current_avg, nums[0] as f32));
        if nums.len() >= 3 {
            self.max_ma = Some(nums[1]);
            self.trip_ma = Some(nums[2]);
        }
    }

    // ---------------- programming helpers ----------------
    /// Parse an entry as an int in [lo, hi]; log and return None if not.
    fn prog_int(&mut self, text: &str, lo: i64, hi: i64, name: &str) -> Option<i64> {
        match text.trim().parse::<i64>() {
            Ok(v) if (lo..=hi).contains(&v) => Some(v),
            _ => {
                self.log(format!("{name} must be {lo}-{hi}"), Tag::Err);
                None
            }
        }
    }

    /// Compose CV29 from the checkboxes plus the preserved high bits.
    fn cv29_value(&self) -> u8 {
        let mut val = self.cv29_high;
        for (bit, &on) in self.cv29_bits.iter().enumerate() {
            if on {
                val |= 1 << bit;
            }
        }
        val
    }

    /// Mirror a confirmed CV29 (from <v 29 x> or <r 29 x>) into the editor.
    ///
    /// Bits 6-7 aren't editable (reserved / accessory-decoder flag) but are
    /// preserved so a later write doesn't clobber them.
    fn cv29_sync(&mut self, value: u32) {
        self.cv29_high = (value & 0xC0) as u8;
        for bit in 0..6 {
            self.cv29_bits[bit] = value & (1 << bit) != 0;
        }
        self.cv29_text = format!("CV29 = {value}");
    }

    // ================= UI =================
    fn connection_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.group(|ui| {
            ui.label(RichText::new("Connection").strong());
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.mode_serial, false, "TCP");
                ui.label("Host:");
                ui.add(egui::TextEdit::singleline(&mut self.host).desired_width(150.0));
                ui.label("Port:");
                ui.add(egui::TextEdit::singleline(&mut self.port).desired_width(60.0));
                let label = if self.link.is_some() { "Disconnect" } else { "Connect" };
                if ui.button(label).clicked() {
                    self.toggle_connect(ctx);
                }
            });
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.mode_serial, true, "Serial");
                let enabled = self.mode_serial;
                ui.add_enabled_ui(enabled, |ui| {
                    let ports = self.serial_ports.clone();
                    egui::ComboBox::from_id_salt("serial_port")
                        .width(280.0)
                        .selected_text(self.serial_port.clone())
                        .show_ui(ui, |ui| {
                            for p in &ports {
                                ui.selectable_value(&mut self.serial_port, p.clone(), p);
                            }
                        });
                    if ui.button("Refresh").clicked() {
                        self.refresh_ports();
                    }
                    ui.label("Baud:");
                    ui.add(egui::TextEdit::singleline(&mut self.baud).desired_width(80.0));
                });
            });
            ui.weak(&self.status);
        });
    }

    fn power_ui(&mut self, ui: &mut egui::Ui) {
        ui.group(|ui| {
            ui.label(RichText::new("Track Power").strong());
            ui.horizontal(|ui| {
                for (label, cmd) in [
                    ("ALL ON", "<1>"),
                    ("ALL OFF", "<0>"),
                    ("MAIN ON", "<1 MAIN>"),
                    ("MAIN OFF", "<0 MAIN>"),
                    ("PROG ON", "<1 PROG>"),
                    ("PROG OFF", "<0 PROG>"),
                ] {
                    if ui.button(label).clicked() {
                        self.send_cmd(cmd, false);
                    }
                }
                ui.add_space(12.0);
                ui.label(&self.power_state);
            });
            ui.horizontal(|ui| {
                ui.label("Current:");
                // The bar scales to trip_ma, not max_ma: the software
                // circuit breaker is the number that matters. Readings above
                // trip clamp the bar but the text still shows the true value.
                let limit = self.trip_ma.filter(|&v| v > 0).or(self.max_ma.filter(|&v| v > 0));
                let (frac, text, color) = if self.overload {
                    (1.0, "OVERLOAD".to_string(), Some(COL_ERR))
                } else {
                    match (self.current_avg, limit) {
                        (None, _) => (0.0, "current: --".to_string(), None),
                        (Some(avg), Some(limit)) => (
                            (avg / limit as f32).clamp(0.0, 1.0),
                            format!("{} mA avg / {limit} mA trip", avg.round() as i64),
                            None,
                        ),
                        (Some(avg), None) => {
                            (0.0, format!("{} mA avg", avg.round() as i64), None)
                        }
                    }
                };
                ui.add(egui::ProgressBar::new(frac).desired_width(300.0));
                match color {
                    Some(c) => ui.label(RichText::new(text).color(c).strong()),
                    None => ui.label(text),
                };
                ui.add_space(8.0);
                ui.checkbox(&mut self.poll_current, "Poll");
            });
        });
    }

    fn loco_tab_bar(&mut self, ui: &mut egui::Ui) {
        let mut clicked: Option<usize> = None;
        let mut add = false;
        ui.horizontal_wrapped(|ui| {
            for (i, p) in self.panels.iter().enumerate() {
                if ui.selectable_label(self.selected == i, p.tab_title()).clicked() {
                    clicked = Some(i);
                }
            }
            // Selecting "+" creates a loco on the next free address.
            if ui.button(" + ").clicked() {
                add = true;
            }
        });
        if let Some(i) = clicked {
            self.selected = i;
        }
        if add {
            self.add_loco();
        }
    }

    fn add_loco(&mut self) {
        let used: BTreeSet<u32> = self.panels.iter().map(|p| p.active_cab).collect();
        let mut addr = 3;
        while used.contains(&addr) {
            addr += 1;
        }
        let id = self.next_id;
        self.next_id += 1;
        let mut panel = LocoPanel::from_cfg(&LocoCfg::default_with(addr), id);
        // A new tab is "Loco <addr>" until named; open Setup straight away
        // so naming it is the first thing offered, not something to hunt for.
        panel.open_setup();
        self.panels.push(panel);
        self.selected = self.panels.len() - 1;
        self.save_config();
        if self.link.is_some() {
            self.send_cmd(&format!("<t {addr}>"), false);
        }
    }

    fn remove_loco(&mut self, idx: usize) {
        if self.panels.len() <= 1 {
            return; // the Setup window already refused the last one
        }
        self.panels.remove(idx);
        if self.selected == idx {
            self.selected = idx.saturating_sub(1);
        } else if self.selected > idx {
            self.selected -= 1;
        }
        self.selected = self.selected.min(self.panels.len() - 1);
    }

    fn panel_run_ui(&mut self, ui: &mut egui::Ui, idx: usize) {
        // The panel is taken out of the roster so the UI code can use both
        // it and &mut self (send_cmd, log) freely; it goes back at the end.
        // Nothing in between touches self.panels[idx].
        let mut panel = std::mem::take(&mut self.panels[idx]);
        let mut needs_save = false;
        let mut do_estop = false;
        let link_up = self.link.is_some();

        ui.group(|ui| {
            ui.label(RichText::new("Locomotive").strong());
            ui.horizontal(|ui| {
                ui.label("Address:");
                // Applies on Enter / focus-out, like the Tk spinbox bindings.
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut panel.cab_entry).desired_width(70.0),
                );
                if resp.lost_focus() {
                    match panel.cab_entry.trim().parse::<u32>() {
                        Ok(cab) if (1..=MAX_ADDR).contains(&cab) => {
                            if cab != panel.active_cab {
                                panel.zero_for_new_cab(cab);
                                panel.cab_entry = cab.to_string();
                                self.log(format!("-- loco {cab} selected"), Tag::Info);
                                needs_save = true;
                                if link_up {
                                    // ask the station to re-broadcast <l>
                                    self.send_cmd(&format!("<t {cab}>"), false);
                                }
                            }
                        }
                        _ => panel.cab_entry = panel.active_cab.to_string(),
                    }
                }
                ui.add_space(14.0);

                // One big colour-coded toggle instead of radio buttons
                // (owner requirement). Plain ASCII labels only -- arrows
                // hit font fallback on Linux. Send first, flip on success.
                let (text, color) = if panel.direction == 1 {
                    ("FORWARD", COL_FWD)
                } else {
                    ("REVERSE", COL_REV)
                };
                let dir_btn = egui::Button::new(
                    RichText::new(text).color(Color32::WHITE).strong(),
                )
                .fill(color)
                .min_size(egui::vec2(150.0, 30.0));
                if ui.add(dir_btn).clicked() {
                    let new_dir = 1 - panel.direction;
                    let speed = panel.speed;
                    if self.send_throttle(&mut panel, speed, new_dir) {
                        panel.direction = new_dir;
                    }
                }
                ui.add_space(14.0);

                if ui
                    .add(egui::Button::new(RichText::new("STOP").strong()).min_size(egui::vec2(90.0, 30.0)))
                    .clicked()
                {
                    let prior = panel.speed;
                    panel.speed = 0;
                    // An explicit stop outranks a queued speed.
                    panel.pending_speed = None;
                    let dir = panel.direction;
                    if !self.send_throttle(&mut panel, 0, dir) {
                        panel.speed = prior; // the loco never got the stop
                    }
                }
                let estop_btn = egui::Button::new(
                    RichText::new("E-STOP ALL").color(Color32::WHITE).strong(),
                )
                .fill(COL_ESTOP)
                .min_size(egui::vec2(110.0, 30.0));
                if ui.add(estop_btn).clicked() {
                    do_estop = true;
                }
                ui.add_space(14.0);
                if ui.button("Setup...").clicked() {
                    panel.open_setup();
                }
            });

            // Speed slider. changed() only fires on user interaction, so an
            // inbound broadcast writing panel.speed can't echo back out.
            ui.horizontal(|ui| {
                ui.spacing_mut().slider_width = ui.available_width() - 90.0;
                let resp = ui.add(
                    egui::Slider::new(&mut panel.speed, 0..=MAX_SPEED).text("speed"),
                );
                if resp.changed() {
                    panel.pending_speed = Some(panel.speed); // one-shot request
                }
            });
        });

        ui.group(|ui| {
            ui.label(
                RichText::new(
                    "Functions (right-click a button to cycle: hold / latching / pulsed)",
                )
                .strong(),
            );
            let visible = panel.visible_funcs();
            let cols = panel.func_columns();
            let spacing = ui.spacing().item_spacing.x;
            let bw = ((ui.available_width() - spacing * (cols as f32 - 1.0)) / cols as f32
                - 12.0)
                .max(40.0);
            for row in visible.chunks(cols) {
                ui.horizontal(|ui| {
                    for &n in row {
                        self.func_button(ui, &mut panel, n, bw, &mut needs_save);
                    }
                });
            }
            ui.add_space(4.0);
            if ui.button("All Functions Off").clicked() {
                // Deliberately covers hidden buttons too: func_state mirrors
                // the true state from <l>, and "all off" means all.
                for n in 0..NFUNC {
                    if !panel.func_state[n] {
                        continue;
                    }
                    if !self.send_cmd(&format!("<F {} {n} 0>", panel.active_cab), false) {
                        break; // link is down; leave the rest showing their real state
                    }
                    panel.func_state[n] = false;
                }
            }
        });

        self.panels[idx] = panel;
        if do_estop {
            self.estop_all();
        }
        if needs_save {
            self.save_config();
        }
    }

    /// One function button, in one of three modes cycled by right-click:
    ///  - hold (momentary): <F 1> on press, <F 0> on release;
    ///  - latching (toggle): one command per press, alternating 1/0 -- a
    ///    sound decoder acts on every edge, so any on/off pair per click
    ///    toots twice; shown as underlined text;
    ///  - pulsed: press fires <F 1> and the app sends the <F 0> itself
    ///    FUNC_PULSE later, whatever the mouse does; shown as three dots
    ///    under the text.
    ///
    /// A green border frame shows the true state from <l>.
    fn func_button(
        &mut self,
        ui: &mut egui::Ui,
        panel: &mut LocoPanel,
        n: u8,
        width: f32,
        needs_save: &mut bool,
    ) {
        let i = n as usize;
        let on = panel.func_state[i];
        let stroke = if on {
            Stroke::new(3.0, COL_FUNC_ON)
        } else {
            Stroke::new(3.0, Color32::TRANSPARENT)
        };
        egui::Frame::default()
            .stroke(stroke)
            .inner_margin(2.0)
            .corner_radius(4.0)
            .show(ui, |ui| {
                let mut text = RichText::new(panel.func_text(n)).strong();
                if panel.toggle_funcs.contains(&n) {
                    text = text.underline();
                }
                let resp = ui.add_sized([width, 36.0], egui::Button::new(text));
                if panel.pulse_funcs.contains(&n) {
                    // The pulse marker: three dots tucked under the label.
                    ui.painter().text(
                        egui::pos2(resp.rect.center().x, resp.rect.bottom() - 2.0),
                        egui::Align2::CENTER_BOTTOM,
                        ". . .",
                        egui::FontId::proportional(10.0),
                        ui.visuals().strong_text_color(),
                    );
                }

                // Act on press/release edges, like the Tk ButtonPress/
                // ButtonRelease bindings.
                let held_now = resp.is_pointer_button_down_on()
                    && ui.input(|inp| inp.pointer.primary_down());
                let was_held = panel.held[i];
                panel.held[i] = held_now;
                let momentary =
                    !panel.toggle_funcs.contains(&n) && !panel.pulse_funcs.contains(&n);
                if held_now && !was_held {
                    if panel.pulse_funcs.contains(&n) {
                        // Pulsed: fire once and let pulse_tick send the off.
                        // Strictly one pulse per press -- while a pulse is
                        // still running, further presses are ignored; the
                        // next one arms only after the off has gone out.
                        if panel.pulse_off[i].is_none()
                            && self.send_cmd(
                                &format!("<F {} {n} 1>", panel.active_cab),
                                false,
                            )
                        {
                            panel.func_state[i] = true;
                            panel.pulse_off[i] = Some(Instant::now() + FUNC_PULSE);
                        }
                    } else if panel.toggle_funcs.contains(&n) {
                        // Updated on a good send so a fast second press flips
                        // the right way before the <l> broadcast lands.
                        let state = !panel.func_state[i];
                        if self.send_cmd(
                            &format!("<F {} {n} {}>", panel.active_cab, state as u8),
                            false,
                        ) {
                            panel.func_state[i] = state;
                        }
                    } else {
                        // Momentary: nothing to revert on a failed send --
                        // the button latches nothing, and a release lost with
                        // the link is corrected by the next <l>.
                        self.send_cmd(&format!("<F {} {n} 1>", panel.active_cab), false);
                    }
                } else if was_held && !held_now && momentary {
                    self.send_cmd(&format!("<F {} {n} 0>", panel.active_cab), false);
                }

                // Right-click cycles hold -> latching -> pulsed -> hold.
                if resp.secondary_clicked() {
                    if panel.toggle_funcs.remove(&n) {
                        panel.pulse_funcs.insert(n);
                        self.log(format!("-- F{n} mode: pulsed"), Tag::Info);
                    } else if panel.pulse_funcs.remove(&n) {
                        panel.pulse_off[i] = None;
                        self.log(format!("-- F{n} mode: hold (momentary)"), Tag::Info);
                    } else {
                        panel.toggle_funcs.insert(n);
                        self.log(format!("-- F{n} mode: latching"), Tag::Info);
                    }
                    *needs_save = true;
                }
            });
    }

    /// The per-loco Setup windows, drawn whatever tab is in front (they are
    /// separate toplevels in the Tk app). One window per loco, ever.
    fn setup_windows(&mut self, ctx: &egui::Context) {
        let mut remove: Option<usize> = None;
        let mut save = false;
        let n_panels = self.panels.len();
        for i in 0..n_panels {
            if self.panels[i].setup.is_none() {
                continue;
            }
            let mut panel = std::mem::take(&mut self.panels[i]);
            let mut open = true;
            let mut done = false;
            let title = format!("Setup — {}", panel.tab_title());
            egui::Window::new(title)
                .id(egui::Id::new(("setup", panel.id)))
                .open(&mut open)
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Loco name:");
                        // Live rename: the tab retitles on every keystroke;
                        // the config still saves on close.
                        ui.add(
                            egui::TextEdit::singleline(&mut panel.name).desired_width(200.0),
                        );
                        ui.weak("(blank shows \"Loco <address>\")");
                    });
                    ui.group(|ui| {
                        ui.label(RichText::new("Functions on this tab").strong());
                        ui.label("Tick a function to show it; the label becomes the button text.");
                        let setup = panel.setup.as_mut().unwrap();
                        egui::Grid::new(("setup_grid", panel.id))
                            .num_columns(6)
                            .show(ui, |ui| {
                                // three decade columns of check + entry
                                for r in 0..10 {
                                    for col in 0..3 {
                                        let n = col * 10 + r;
                                        if n >= NFUNC {
                                            ui.label("");
                                            ui.label("");
                                            continue;
                                        }
                                        ui.checkbox(&mut setup.show[n], format!("F{n}"));
                                        ui.add(
                                            egui::TextEdit::singleline(&mut setup.labels[n])
                                                .desired_width(130.0),
                                        );
                                    }
                                    ui.end_row();
                                }
                            });
                    });
                    ui.horizontal(|ui| {
                        let setup = panel.setup.as_mut().unwrap();
                        if setup.confirm_remove {
                            ui.label(format!(
                                "Remove '{}' and its settings?",
                                panel.name.trim()
                            ));
                            if ui.button("Yes, remove").clicked() {
                                remove = Some(i);
                            }
                            if ui.button("Cancel").clicked() {
                                setup.confirm_remove = false;
                            }
                        } else {
                            let btn = ui.add_enabled(
                                n_panels > 1,
                                egui::Button::new("Remove This Loco"),
                            );
                            if btn
                                .on_disabled_hover_text("The last loco tab cannot be removed.")
                                .clicked()
                            {
                                setup.confirm_remove = true;
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.button("Done").clicked() {
                                        done = true;
                                    }
                                },
                            );
                        }
                    });
                    // Return applies + closes, like the Tk <Return> binding.
                    if ui.input(|inp| inp.key_pressed(egui::Key::Enter)) {
                        done = true;
                    }
                });

            if remove == Some(i) {
                // Removal closes without applying, matching the Tk dialog's
                // Remove path (close first, then remove).
                panel.setup = None;
                self.panels[i] = panel;
                continue;
            }
            if done || !open {
                // Every close path (Done, Return, window X) applies the
                // visible set and labels; the name already applied live.
                panel.apply_setup();
                panel.name = panel.name.trim().to_string();
                save = true;
            }
            self.panels[i] = panel;
        }
        if let Some(i) = remove {
            self.remove_loco(i);
            save = true;
        }
        if save {
            self.save_config();
        }
    }

    fn programming_ui(&mut self, ui: &mut egui::Ui) {
        ui.group(|ui| {
            ui.label(RichText::new("Programming Track (service mode)").strong());
            ui.label("Loco must be alone on the PROG track. Locos do not move in service mode.");
            ui.horizontal(|ui| {
                if ui.button("Read Address").clicked() && self.send_cmd("<R>", false) {
                    self.prog_result = "reading address...".to_string();
                }
                ui.label("Address:");
                ui.add(egui::TextEdit::singleline(&mut self.prog_addr).desired_width(70.0));
                if ui.button("Write Address").clicked() {
                    let text = self.prog_addr.clone();
                    if let Some(addr) = self.prog_int(&text, 1, MAX_ADDR as i64, "address") {
                        if self.send_cmd(&format!("<W {addr}>"), false) {
                            self.prog_result = "writing address...".to_string();
                        }
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label("CV:");
                ui.add(egui::TextEdit::singleline(&mut self.prog_cv).desired_width(60.0));
                ui.label("Value:");
                ui.add(egui::TextEdit::singleline(&mut self.prog_val).desired_width(50.0));
                if ui.button("Read CV").clicked() {
                    let text = self.prog_cv.clone();
                    if let Some(cv) = self.prog_int(&text, 1, 1024, "CV") {
                        if self.send_cmd(&format!("<R {cv}>"), false) {
                            self.prog_result =
                                format!("reading {}...", cv_desc(&cv.to_string()));
                        }
                    }
                }
                if ui.button("Write CV").clicked() {
                    let cv_text = self.prog_cv.clone();
                    let val_text = self.prog_val.clone();
                    if let Some(cv) = self.prog_int(&cv_text, 1, 1024, "CV") {
                        if let Some(val) = self.prog_int(&val_text, 0, 255, "value") {
                            if self.send_cmd(&format!("<W {cv} {val}>"), false) {
                                self.prog_result =
                                    format!("writing {}...", cv_desc(&cv.to_string()));
                            }
                        }
                    }
                }
                // Live CV name lookup beside the entry; unknown CVs show
                // nothing -- no guessing.
                if let Some(name) = self
                    .prog_cv
                    .trim()
                    .parse::<u32>()
                    .ok()
                    .and_then(cv_name)
                {
                    ui.weak(name);
                }
            });
            ui.label(&self.prog_result);
        });

        ui.group(|ui| {
            ui.label(RichText::new("CV29 Bit Editor (prog track)").strong());
            let bits = [
                "Reverse direction",
                "28/128 speed steps",
                "Analog (DC) mode",
                "RailCom",
                "Custom speed table",
                "Long address (CV17/18)",
            ];
            let mut edited = false;
            for chunk in [(0..3), (3..6)] {
                ui.horizontal(|ui| {
                    for bit in chunk {
                        // Toggling only updates the preview; nothing is sent
                        // until Write CV29.
                        if ui
                            .checkbox(&mut self.cv29_bits[bit], format!("{} (b{bit})", bits[bit]))
                            .changed()
                        {
                            edited = true;
                        }
                    }
                });
            }
            if edited {
                self.cv29_text = format!("CV29 = {} (not written)", self.cv29_value());
            }
            ui.horizontal(|ui| {
                ui.label(&self.cv29_text);
                if ui.button("Read CV29").clicked() && self.send_cmd("<R 29>", false) {
                    self.prog_result = "reading CV 29...".to_string();
                }
                if ui.button("Write CV29").clicked() {
                    let val = self.cv29_value();
                    if self.send_cmd(&format!("<W 29 {val}>"), false) {
                        self.prog_result = format!("writing CV 29 = {val}...");
                    }
                }
            });
            ui.label(
                "Bit 5 only selects which address is used -- change addresses with \
                 Write Address, not here.",
            );
        });

        ui.group(|ui| {
            ui.label(RichText::new("Program on Main (POM)").strong());
            ui.label(
                "Writes to the loco tab selected on the Run tab. \
                 No reply from the station -- watch the loco.",
            );
            ui.horizontal(|ui| {
                ui.label("CV:");
                ui.add(egui::TextEdit::singleline(&mut self.pom_cv).desired_width(60.0));
                ui.label("Value:");
                ui.add(egui::TextEdit::singleline(&mut self.pom_val).desired_width(50.0));
                if ui.button("Write on Main").clicked() {
                    let cv_text = self.pom_cv.clone();
                    let val_text = self.pom_val.clone();
                    if let Some(cv) = self.prog_int(&cv_text, 1, 1024, "CV") {
                        if let Some(val) = self.prog_int(&val_text, 0, 255, "value") {
                            // Targets the selected loco tab, read at click time.
                            let cab = self.panels[self.selected].active_cab;
                            self.send_cmd(&format!("<w {cab} {cv} {val}>"), false);
                        }
                    }
                }
                if let Some(name) = self
                    .pom_cv
                    .trim()
                    .parse::<u32>()
                    .ok()
                    .and_then(cv_name)
                {
                    ui.weak(name);
                }
            });
        });
    }

    fn automation_ui(&mut self, ui: &mut egui::Ui) {
        let mut save = false;
        let mut delete: Option<usize> = None;
        self.script_sel = self.script_sel.min(self.scripts.len() - 1);

        ui.group(|ui| {
            ui.label(RichText::new("Automation Scripts").strong());
            ui.horizontal(|ui| {
                ui.label("Script:");
                let current = self.scripts[self.script_sel].name.clone();
                egui::ComboBox::from_id_salt("script_sel")
                    .width(220.0)
                    .selected_text(current)
                    .show_ui(ui, |ui| {
                        for i in 0..self.scripts.len() {
                            let name = self.scripts[i].name.clone();
                            ui.selectable_value(&mut self.script_sel, i, name);
                        }
                    });
                if ui.button("New").clicked() {
                    self.scripts.push(ScriptCfg {
                        name: format!("Script {}", self.scripts.len() + 1),
                        text: String::new(),
                    });
                    self.script_sel = self.scripts.len() - 1;
                    save = true;
                }
                let deletable = self.scripts.len() > 1;
                if ui
                    .add_enabled(deletable, egui::Button::new("Delete"))
                    .on_disabled_hover_text("The last script cannot be deleted.")
                    .clicked()
                {
                    delete = Some(self.script_sel);
                }
                ui.add_space(12.0);
                ui.label("Name:");
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.scripts[self.script_sel].name)
                        .desired_width(180.0),
                );
                if resp.changed() {
                    self.script_dirty = true;
                }
                if resp.lost_focus() {
                    let name = &mut self.scripts[self.script_sel].name;
                    if name.trim().is_empty() {
                        // A nameless script would vanish on the next load.
                        *name = format!("Script {}", self.script_sel + 1);
                    }
                    save = true;
                }
            });
            ui.horizontal(|ui| {
                if self.runner.is_none() {
                    if ui
                        .add(egui::Button::new(RichText::new("Run").strong()))
                        .clicked()
                    {
                        save = true; // Run also commits the text to disk
                        self.run_script();
                    }
                } else {
                    let stop = egui::Button::new(
                        RichText::new("Stop Script").color(Color32::WHITE).strong(),
                    )
                    .fill(COL_ESTOP);
                    if ui.add(stop).clicked() {
                        self.runner = None;
                        self.run_status = "stopped".to_string();
                        self.log("-- script stopped", Tag::Info);
                    }
                }
                // Explicit save (Ctrl+S works anywhere on this tab). Edits
                // also auto-save when the editor loses focus, on Run, and
                // on exit -- the button makes "it's on disk" a certainty
                // instead of a side effect.
                let ctrl_s = ui.input(|inp| {
                    inp.modifiers.command && inp.key_pressed(egui::Key::S)
                });
                if ui
                    .add_enabled(self.script_dirty, egui::Button::new("Save"))
                    .on_disabled_hover_text("No unsaved changes.")
                    .clicked()
                    || (ctrl_s && self.script_dirty)
                {
                    save = true;
                }
                if self.script_dirty {
                    ui.label(RichText::new("unsaved changes").color(COL_INFO));
                }
                ui.label(&self.run_status);
            });
        });

        // The editor commits on focus loss (and on Save/Run/exit); a
        // running script keeps executing its parsed copy, so editing is
        // safe. changed() drives the unsaved-changes marker.
        let resp = ui.add(
            egui::TextEdit::multiline(&mut self.scripts[self.script_sel].text)
                .code_editor()
                .desired_width(f32::INFINITY)
                .desired_rows(18),
        );
        if resp.changed() {
            self.script_dirty = true;
        }
        if resp.lost_focus() {
            save = true;
        }

        egui::CollapsingHeader::new("Command reference")
            .default_open(false)
            .show(ui, |ui| {
                for line in [
                    "speed CAB N          set speed 0-126 (keeps direction)",
                    "forward CAB          direction forward (resends speed)",
                    "reverse CAB          direction reverse",
                    "stop CAB             speed 0",
                    "estop                emergency stop everything",
                    "func CAB N on|off    decoder function F0-F28",
                    "pulse CAB N [SECS]   momentary function, default 0.5 s",
                    "wait SECS            pause the script",
                    "power on|off [main|prog]",
                    "throw ID / close ID  turnouts (see the Layout tab)",
                    "send CMD             raw DCC-EX command, <> optional",
                    "repeat N ... end     loops, nestable",
                    "# comment            anywhere on a line",
                ] {
                    ui.label(RichText::new(line).monospace());
                }
            });

        if let Some(i) = delete {
            self.scripts.remove(i);
            if self.script_sel >= i && self.script_sel > 0 {
                self.script_sel -= 1;
            }
            save = true;
        }
        if save {
            self.save_config();
        }
    }

    fn layout_ui(&mut self, ui: &mut egui::Ui) {
        let mut save = false;
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.layout_edit, "Edit layout");
            ui.add_space(12.0);
            if self.layout_edit {
                ui.label("Grid:");
                let cols = ui.add(
                    egui::DragValue::new(&mut self.layout.cols)
                        .range(layout::MIN_SIZE..=layout::MAX_SIZE),
                );
                ui.label("x");
                let rows = ui.add(
                    egui::DragValue::new(&mut self.layout.rows)
                        .range(layout::MIN_SIZE..=layout::MAX_SIZE),
                );
                if cols.changed() || rows.changed() {
                    self.layout.prune();
                    save = true;
                }
            } else {
                if ui.button("Sync turnout states").clicked() {
                    self.send_cmd("<JT>", false);
                }
                ui.weak("Click a turnout to throw/close it. Green = the route currently set.");
            }
        });

        if self.layout_edit {
            ui.horizontal_wrapped(|ui| {
                ui.label("Tool:");
                let kind = self.turnout_kind;
                for (tool, tip) in [
                    (Tool::Erase, "Erase"),
                    (Tool::Track(Piece::EW), "Straight track, left-right"),
                    (Tool::Track(Piece::NS), "Straight track, up-down"),
                    (
                        Tool::CurveLeft,
                        "Curve Left: bends the run (or a turnout's diverging \
                         leg) to the left; orients itself from the adjacent track",
                    ),
                    (
                        Tool::CurveRight,
                        "Curve Right: bends the run (or a turnout's diverging \
                         leg) to the right; orients itself from the adjacent track",
                    ),
                    (
                        Tool::Diagonal,
                        "Diagonal: continues a turnout's diverging leg corner to \
                         corner and levels off into an adjacent straight",
                    ),
                    (Tool::Track(Piece::Cross), "Crossing (no switching)"),
                    (Tool::Turnout, "Turnout: pick the shape below, then click a cell"),
                ] {
                    let resp = tool_icon_button(ui, self.tool == tool, tip, |p, r, c| {
                        draw_tool_icon(p, r, c, tool, kind)
                    });
                    if resp.clicked() {
                        self.tool = tool;
                    }
                }
            });
            if self.tool == Tool::Turnout {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Turnout shape:");
                    for k in TKind::ALL {
                        let resp =
                            tool_icon_button(ui, self.turnout_kind == k, k.label(), |p, r, c| {
                                draw_turnout_icon(p, r, c, k)
                            });
                        if resp.clicked() {
                            self.turnout_kind = k;
                        }
                    }
                });
            }
            ui.weak(
                "Left-click or drag to paint. Curves orient themselves from the \
                 adjacent track (the arrow is which way the run bends) and also \
                 take a turnout's diverging leg or a diagonal back into a \
                 straight; re-click one to fix it after laying its neighbours. \
                 Right-click a turnout, or click it with the Turnout tool, to \
                 edit its DCC-EX turnout ID -- the same ID it has on the \
                 command station.",
            );
            self.turnout_editor_ui(ui, &mut save);
        }

        ui.separator();
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.layout_canvas(ui, &mut save);
            });
        if save {
            self.save_config();
        }
    }

    /// The "selected turnout" row shown in edit mode: ID and orientation.
    fn turnout_editor_ui(&mut self, ui: &mut egui::Ui, save: &mut bool) {
        let Some(key) = self.sel_turnout else {
            return;
        };
        // The selection can go stale (erased, grid shrunk); drop it quietly.
        let Some(Cell::Turnout { id, kind }) = self.layout.cells.get(&key).cloned() else {
            self.sel_turnout = None;
            return;
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("Selected turnout (cell {},{}) - ID:", key.0, key.1));
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.turnout_id_entry).desired_width(70.0),
            );
            if resp.lost_focus() {
                match self.turnout_id_entry.trim().parse::<u32>() {
                    Ok(new_id) if new_id <= layout::MAX_TURNOUT_ID && new_id != id => {
                        if let Some(Cell::Turnout { id, .. }) = self.layout.cells.get_mut(&key)
                        {
                            *id = new_id;
                        }
                        *save = true;
                    }
                    Ok(_) => {}
                    // not a number: put the real ID back rather than guess
                    Err(_) => self.turnout_id_entry = id.to_string(),
                }
            }
            ui.label("Shape:");
            let mut sel_kind = kind;
            for k in TKind::ALL {
                let resp = tool_icon_button(ui, sel_kind == k, k.label(), |p, r, c| {
                    draw_turnout_icon(p, r, c, k)
                });
                if resp.clicked() {
                    sel_kind = k;
                }
            }
            if sel_kind != kind {
                if let Some(Cell::Turnout { kind, .. }) = self.layout.cells.get_mut(&key) {
                    *kind = sel_kind;
                }
                *save = true;
            }
            if ui.button("Done").clicked() {
                self.sel_turnout = None;
            }
        });
    }

    fn layout_canvas(&mut self, ui: &mut egui::Ui, save: &mut bool) {
        use layout::CELL;
        let size = egui::vec2(
            self.layout.cols as f32 * CELL,
            self.layout.rows as f32 * CELL,
        );
        let (resp, painter) = ui.allocate_painter(size, egui::Sense::click_and_drag());
        let origin = resp.rect.min;
        painter.rect_filled(resp.rect, 4.0, COL_CONSOLE_BG);

        if self.layout_edit {
            for x in 0..=self.layout.cols {
                let px = origin.x + x as f32 * CELL;
                painter.line_segment(
                    [egui::pos2(px, origin.y), egui::pos2(px, origin.y + size.y)],
                    Stroke::new(1.0, COL_GRID),
                );
            }
            for y in 0..=self.layout.rows {
                let py = origin.y + y as f32 * CELL;
                painter.line_segment(
                    [egui::pos2(origin.x, py), egui::pos2(origin.x + size.x, py)],
                    Stroke::new(1.0, COL_GRID),
                );
            }
        }

        let track = Stroke::new(4.0, COL_TRACK);
        for (&(x, y), cell) in &self.layout.cells {
            let cell_min =
                egui::pos2(origin.x + x as f32 * CELL, origin.y + y as f32 * CELL);
            let to_pos =
                |p: (f32, f32)| egui::pos2(cell_min.x + p.0 * CELL, cell_min.y + p.1 * CELL);
            match cell {
                Cell::Track(piece) => {
                    for seg in piece.segments() {
                        painter.line_segment([to_pos(seg.0), to_pos(seg.1)], track);
                    }
                }
                Cell::Turnout { id, kind } => {
                    // Unknown state shows both routes in the warning colour
                    // -- honest, and a nudge to hit "Sync turnout states".
                    let (main_col, branch_col) = match self.layout.states.get(id) {
                        Some(false) => (COL_ROUTE_SET, COL_ROUTE_OFF),
                        Some(true) => (COL_ROUTE_OFF, COL_ROUTE_SET),
                        None => (COL_INFO, COL_INFO),
                    };
                    // The unset route paints first so the set route (they
                    // share the cell centre) draws on top of it.
                    let branch = kind.branch();
                    let mut segs: Vec<(layout::Seg, Color32)> = kind
                        .main()
                        .segments()
                        .iter()
                        .map(|&s| (s, main_col))
                        .collect();
                    let at = if main_col == COL_ROUTE_SET { 0 } else { segs.len() };
                    segs.insert(at, (branch, branch_col));
                    for (seg, col) in segs {
                        painter.line_segment(
                            [to_pos(seg.0), to_pos(seg.1)],
                            Stroke::new(4.0, col),
                        );
                    }
                    if self.layout_edit && self.sel_turnout == Some((x, y)) {
                        painter.rect_stroke(
                            egui::Rect::from_min_size(cell_min, egui::vec2(CELL, CELL)),
                            3.0,
                            Stroke::new(2.0, COL_INFO),
                            egui::StrokeKind::Inside,
                        );
                    }
                    painter.text(
                        cell_min + egui::vec2(3.0, 1.0),
                        egui::Align2::LEFT_TOP,
                        id.to_string(),
                        egui::FontId::proportional(11.0),
                        COL_TX,
                    );
                }
            }
        }

        // ---- interaction ----
        let Some(pos) = resp.interact_pointer_pos() else {
            return;
        };
        let (cx, cy) = (
            ((pos.x - origin.x) / CELL).floor() as i64,
            ((pos.y - origin.y) / CELL).floor() as i64,
        );
        if cx < 0 || cy < 0 || cx >= self.layout.cols as i64 || cy >= self.layout.rows as i64 {
            return;
        }
        let key = (cx as u32, cy as u32);

        if !self.layout_edit {
            // Run mode: a click on a turnout toggles it. No optimistic
            // repaint -- the colour flips when the <H> broadcast lands.
            if resp.clicked()
                && let Some(Cell::Turnout { id, .. }) = self.layout.cells.get(&key)
            {
                let id = *id;
                let thrown = self.layout.states.get(&id) == Some(&true);
                self.send_cmd(&format!("<T {id} {}>", !thrown as u8), false);
            }
            return;
        }

        // Edit mode. Right-click selects a turnout for the ID editor.
        if resp.secondary_clicked() {
            if let Some(Cell::Turnout { id, .. }) = self.layout.cells.get(&key) {
                self.turnout_id_entry = id.to_string();
                self.sel_turnout = Some(key);
            }
            return;
        }
        let painting = resp.clicked() || resp.dragged_by(egui::PointerButton::Primary);
        if !painting {
            return;
        }
        match self.tool {
            Tool::Erase => {
                if self.layout.cells.remove(&key).is_some() {
                    *save = true;
                }
            }
            Tool::Track(piece) => {
                let cell = Cell::Track(piece);
                if self.layout.cells.get(&key) != Some(&cell) {
                    self.layout.cells.insert(key, cell);
                    *save = true;
                }
            }
            Tool::CurveLeft | Tool::CurveRight | Tool::Diagonal => {
                // Click only: dragging would chain half-oriented pieces.
                // Re-clicking re-runs the pick against the current
                // neighbours, so a piece laid too early is one click to fix.
                if !resp.clicked() {
                    return;
                }
                let piece = match self.tool {
                    Tool::Diagonal => self.layout.pick_diagonal(key),
                    tool => self.layout.pick_curve(key, tool == Tool::CurveLeft),
                };
                let cell = Cell::Track(piece);
                if self.layout.cells.get(&key) != Some(&cell) {
                    self.layout.cells.insert(key, cell);
                    *save = true;
                }
            }
            Tool::Turnout => {
                // Click only -- dragging a turnout tool across the plan must
                // not scatter turnouts with auto-assigned IDs.
                if !resp.clicked() {
                    return;
                }
                if let Some(Cell::Turnout { id, .. }) = self.layout.cells.get(&key) {
                    // clicking an existing turnout selects it instead
                    self.turnout_id_entry = id.to_string();
                    self.sel_turnout = Some(key);
                } else {
                    let id = self.layout.next_free_id();
                    self.layout
                        .cells
                        .insert(key, Cell::Turnout { id, kind: self.turnout_kind });
                    self.turnout_id_entry = id.to_string();
                    self.sel_turnout = Some(key);
                    *save = true;
                }
            }
        }
    }

    fn console_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Console").strong());
        let entry_height = 34.0;
        egui::Frame::default()
            .fill(COL_CONSOLE_BG)
            .inner_margin(4.0)
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .max_height((ui.available_height() - entry_height).max(40.0))
                    .show(ui, |ui| {
                        for (tag, line) in &self.log {
                            ui.label(
                                RichText::new(line).monospace().color(tag.color()),
                            );
                        }
                    });
            });
        ui.horizontal(|ui| {
            ui.label("Raw:");
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.raw)
                    .desired_width(ui.available_width() - 70.0),
            );
            let entered =
                resp.lost_focus() && ui.input(|inp| inp.key_pressed(egui::Key::Enter));
            if ui.button("Send").clicked() || entered {
                let mut text = self.raw.trim().to_string();
                if !text.is_empty() {
                    // Bare input is wrapped in angle brackets, so "D CABS" works.
                    if !text.starts_with('<') {
                        text = format!("<{text}>");
                    }
                    self.send_cmd(&text, false);
                    self.raw.clear();
                }
                if entered {
                    resp.request_focus();
                }
            }
        });
    }
}

// ---------------- layout tool icons ----------------
// The palette buttons draw the piece they place instead of naming it --
// a compass word tells you nothing at a glance, the shape does.

fn unit_pos(r: egui::Rect, p: (f32, f32)) -> egui::Pos2 {
    egui::pos2(r.min.x + p.0 * r.width(), r.min.y + p.1 * r.height())
}

fn icon_segs(painter: &egui::Painter, r: egui::Rect, segs: &[layout::Seg], color: Color32) {
    for &(a, b) in segs {
        painter.line_segment([unit_pos(r, a), unit_pos(r, b)], Stroke::new(3.0, color));
    }
}

fn draw_turnout_icon(painter: &egui::Painter, r: egui::Rect, color: Color32, kind: TKind) {
    icon_segs(painter, r, kind.main().segments(), color);
    icon_segs(painter, r, &[kind.branch()], color);
}

fn draw_tool_icon(
    painter: &egui::Painter,
    r: egui::Rect,
    color: Color32,
    tool: Tool,
    turnout_kind: TKind,
) {
    match tool {
        Tool::Erase => icon_segs(
            painter,
            r,
            &[((0.2, 0.2), (0.8, 0.8)), ((0.8, 0.2), (0.2, 0.8))],
            color,
        ),
        Tool::Track(piece) => icon_segs(painter, r, piece.segments(), color),
        // The curve tools have no fixed shape (they orient on placement),
        // so the icon shows the idea: the run comes in, and bends the way
        // the arrow points.
        Tool::CurveLeft => icon_segs(
            painter,
            r,
            &[
                ((0.0, 0.5), (0.5, 0.5)),
                ((0.5, 0.5), (0.5, 0.1)),
                ((0.28, 0.36), (0.5, 0.08)),
                ((0.72, 0.36), (0.5, 0.08)),
            ],
            color,
        ),
        Tool::CurveRight => icon_segs(
            painter,
            r,
            &[
                ((0.0, 0.5), (0.5, 0.5)),
                ((0.5, 0.5), (0.5, 0.9)),
                ((0.28, 0.64), (0.5, 0.92)),
                ((0.72, 0.64), (0.5, 0.92)),
            ],
            color,
        ),
        Tool::Diagonal => icon_segs(painter, r, &[((0.1, 0.9), (0.9, 0.1))], color),
        Tool::Turnout => draw_turnout_icon(painter, r, color, turnout_kind),
    }
}

/// A paint-your-own-icon toggle button for the layout palette.
fn tool_icon_button(
    ui: &mut egui::Ui,
    selected: bool,
    tip: &str,
    draw: impl FnOnce(&egui::Painter, egui::Rect, Color32),
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(46.0, 34.0), egui::Sense::click());
    let visuals = ui.style().interact_selectable(&resp, selected);
    let fill = if selected {
        ui.visuals().selection.bg_fill
    } else {
        visuals.weak_bg_fill
    };
    ui.painter().rect_filled(rect, 4.0, fill);
    ui.painter()
        .rect_stroke(rect, 4.0, visuals.bg_stroke, egui::StrokeKind::Inside);
    let color = if selected {
        ui.visuals().selection.stroke.color
    } else {
        visuals.text_color()
    };
    draw(ui.painter(), rect.shrink(6.0), color);
    resp.on_hover_text(tip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_average_smooths_in_both_directions() {
        // first sample primes the average directly
        assert_eq!(smooth_current(None, 1000.0), 1000.0);
        // a spike is averaged in gently...
        let up = smooth_current(Some(1000.0), 2000.0);
        assert_eq!(up, 1000.0 + CURRENT_ATTACK * 1000.0);
        // ...and a dropped load drains even more gently: a single zero
        // sample dents the reading by an eighth, nothing more
        let down = smooth_current(Some(1000.0), 0.0);
        assert_eq!(down, 1000.0 - CURRENT_DECAY * 1000.0);
        assert!(down >= 800.0);
        // and the average converges to a steady reading (30 samples is
        // 7.5 s at the 4 Hz poll -- the slow direction, by design)
        let mut avg = 400.0;
        for _ in 0..30 {
            avg = smooth_current(Some(avg), 800.0);
        }
        assert!((avg - 800.0).abs() < 10.0);
    }
}

impl eframe::App for ThrottleApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.pump();
        self.speed_tick();
        self.pulse_tick();
        self.current_tick();
        self.script_tick();

        egui::Panel::top("top_band").show(ui, |ui| {
            self.connection_ui(ui, &ctx);
            self.power_ui(ui);
            ui.add_space(2.0);
        });

        egui::Panel::bottom("console_band")
            .resizable(true)
            .default_size(260.0)
            .min_size(120.0)
            .show(ui, |ui| {
                self.console_ui(ui);
            });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.main_tab, MainTab::Run, "Run");
                ui.selectable_value(&mut self.main_tab, MainTab::Programming, "Programming");
                ui.selectable_value(&mut self.main_tab, MainTab::Automation, "Automation");
                ui.selectable_value(&mut self.main_tab, MainTab::Layout, "Layout");
            });
            ui.separator();
            match self.main_tab {
                MainTab::Run => {
                    self.loco_tab_bar(ui);
                    let idx = self.selected.min(self.panels.len() - 1);
                    self.selected = idx;
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            self.panel_run_ui(ui, idx);
                        });
                }
                MainTab::Programming => {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            self.programming_ui(ui);
                        });
                }
                MainTab::Automation => {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            self.automation_ui(ui);
                        });
                }
                // layout_ui brings its own (two-way) scroll area
                MainTab::Layout => self.layout_ui(ui),
            }
        });

        self.setup_windows(&ctx);

        // Drives the speed tick and the 1 s current poll; also how inbound
        // traffic gets drawn promptly (the reader thread requests a repaint
        // too, but this keeps the cadence even while idle).
        ctx.request_repaint_after(Duration::from_millis(30));
    }

    fn on_exit(&mut self) {
        // A rename typed into a still-open Setup window has already hit the
        // panel (live edit) but not the file; catch it on the way out.
        self.save_config();
        if let Some(mut link) = self.link.take() {
            let _ = link.send("<0>"); // drop track power on exit
            link.close();
        }
    }
}
