//! One locomotive tab: everything specific to a single address -- the
//! throttle state, function states, and the per-loco send/sync state.
//! The app owns the transport and fans each <l> broadcast out to every
//! panel driving that address; a panel never touches the socket.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use crate::config::{LocoCfg, FUNCTIONS};

pub const MAX_SPEED: u8 = 126;
pub const NFUNC: usize = 29; // F0-F28

/// Per-loco Setup window state. Name edits apply live (straight onto the
/// panel); show/labels apply when the window closes, matching the Tk app.
pub struct SetupState {
    pub show: [bool; NFUNC],
    pub labels: [String; NFUNC],
    pub confirm_remove: bool,
}

pub struct LocoPanel {
    // configuration
    pub name: String,
    pub active_cab: u32,
    pub toggle_funcs: BTreeSet<u8>,
    pub show_funcs: BTreeSet<u8>,
    pub labels: BTreeMap<u8, String>,

    // widget state
    pub cab_entry: String,       // address box text; applies on Enter/focus-out
    pub speed: u8,               // slider position, 0-126
    pub direction: u8,           // 1 = forward, 0 = reverse
    pub func_state: [bool; NFUNC], // the true state, mirrored from <l>
    pub held: [bool; NFUNC],     // momentary buttons currently pressed

    // send/sync state -- see CLAUDE.md "Key invariants"
    pub pending_speed: Option<u8>, // one-shot request, not a slider mirror
    pub last_state: Option<(u8, u8)>, // (speed, dir) actually sent; None = unknown
    pub last_sent: Instant,

    pub setup: Option<SetupState>, // the one open Setup window, or None
    pub id: u64,                   // stable egui Id source across tab moves
}

impl Default for LocoPanel {
    fn default() -> Self {
        LocoPanel::from_cfg(&LocoCfg::default(), 0)
    }
}

impl LocoPanel {
    pub fn from_cfg(cfg: &LocoCfg, id: u64) -> Self {
        LocoPanel {
            name: cfg.name.clone(),
            active_cab: cfg.address,
            toggle_funcs: cfg.toggle_funcs.clone(),
            show_funcs: cfg.show_funcs.clone(),
            labels: cfg.labels.clone(),
            cab_entry: cfg.address.to_string(),
            speed: 0,
            direction: 1,
            func_state: [false; NFUNC],
            held: [false; NFUNC],
            pending_speed: None,
            last_state: None,
            last_sent: Instant::now(),
            setup: None,
            id,
        }
    }

    pub fn to_cfg(&self) -> LocoCfg {
        LocoCfg {
            name: self.name.clone(),
            address: self.active_cab,
            toggle_funcs: self.toggle_funcs.clone(),
            show_funcs: self.show_funcs.clone(),
            labels: self.labels.clone(),
        }
    }

    /// A named loco's tab is just the name (owner requirement -- no address
    /// suffix); an unnamed one falls back to "Loco <addr>".
    pub fn tab_title(&self) -> String {
        if self.name.is_empty() {
            format!("Loco {}", self.active_cab)
        } else {
            self.name.clone()
        }
    }

    /// Button text: "F3 Whistle" if labelled, else "F3".
    pub fn func_text(&self, n: u8) -> String {
        match self.labels.get(&n) {
            Some(label) => format!("F{n} {label}"),
            None => format!("F{n}"),
        }
    }

    /// Bare F-numbers flow ten per row; as soon as any visible button
    /// carries a label the flow drops to six per row so the text has room.
    pub fn func_columns(&self) -> usize {
        let labelled = self
            .show_funcs
            .iter()
            .any(|n| self.labels.contains_key(n));
        if labelled {
            6
        } else {
            10
        }
    }

    pub fn visible_funcs(&self) -> Vec<u8> {
        FUNCTIONS.filter(|n| self.show_funcs.contains(n)).collect()
    }

    /// Forget anything owed to or believed about the station.
    ///
    /// Called on every (re)connect: a slider dragged while offline is not a
    /// command the user meant to issue now, and nothing is known about the
    /// loco until <l> arrives. Never let a value survive from one connection
    /// into the next.
    pub fn reset_link_state(&mut self) {
        self.pending_speed = None;
        self.last_state = None;
    }

    /// After a successful <!>: this loco is stopped, whatever it was doing.
    pub fn estop_zero(&mut self) {
        self.speed = 0;
        self.pending_speed = None;
        self.last_state = Some((0, self.direction));
    }

    /// Mirror an inbound <l> for this panel's address into the state.
    /// The station just told us the truth; any queued slider intent dies.
    pub fn sync_from_broadcast(&mut self, speed_byte: u8, func_map: u32) {
        self.direction = if speed_byte & 0x80 != 0 { 1 } else { 0 };
        let raw = speed_byte & 0x7F;
        self.speed = if raw <= 1 { 0 } else { raw - 1 }; // 1 = emergency stop
        for n in 0..NFUNC {
            self.func_state[n] = func_map & (1 << n) != 0;
        }
        self.last_state = Some((self.speed, self.direction));
        self.pending_speed = None;
    }

    /// Zero the panel for a new address; the caller then requests state.
    pub fn zero_for_new_cab(&mut self, cab: u32) {
        self.active_cab = cab;
        self.speed = 0;
        self.direction = 1;
        self.func_state = [false; NFUNC];
        self.pending_speed = None;
        self.last_state = None;
    }

    pub fn open_setup(&mut self) {
        if self.setup.is_some() {
            return; // one window per loco, ever
        }
        let mut show = [false; NFUNC];
        let mut labels: [String; NFUNC] = std::array::from_fn(|_| String::new());
        for n in FUNCTIONS {
            show[n as usize] = self.show_funcs.contains(&n);
            if let Some(l) = self.labels.get(&n) {
                labels[n as usize] = l.clone();
            }
        }
        self.setup = Some(SetupState {
            show,
            labels,
            confirm_remove: false,
        });
    }

    /// Take the results of the Setup window: visible set and labels.
    /// (The name already applied live, keystroke by keystroke.)
    pub fn apply_setup(&mut self) {
        let Some(setup) = self.setup.take() else {
            return;
        };
        self.show_funcs = FUNCTIONS.filter(|&n| setup.show[n as usize]).collect();
        self.labels = FUNCTIONS
            .filter_map(|n| {
                let label = setup.labels[n as usize].trim();
                if label.is_empty() {
                    None
                } else {
                    Some((n, label.to_string()))
                }
            })
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panel() -> LocoPanel {
        LocoPanel::from_cfg(&LocoCfg::default_with(3), 1)
    }

    #[test]
    fn speed_byte_decoding() {
        let mut p = panel();
        // low 7 bits: 0 = stop, 1 = emergency stop, 2..127 -> speed 1..126
        p.sync_from_broadcast(0x00, 0);
        assert_eq!((p.speed, p.direction), (0, 0));
        p.sync_from_broadcast(0x81, 0); // estop, forward
        assert_eq!((p.speed, p.direction), (0, 1));
        p.sync_from_broadcast(0x80 | 2, 0);
        assert_eq!((p.speed, p.direction), (1, 1));
        p.sync_from_broadcast(0x7F, 0); // 127 -> full speed reverse
        assert_eq!((p.speed, p.direction), (126, 0));
    }

    #[test]
    fn broadcast_retires_pending_and_sets_last_state() {
        let mut p = panel();
        p.pending_speed = Some(50);
        p.sync_from_broadcast(0x80 | 31, 0b101);
        // the station just told us the truth; queued slider intent dies
        assert_eq!(p.pending_speed, None);
        assert_eq!(p.last_state, Some((30, 1)));
        assert!(p.func_state[0] && !p.func_state[1] && p.func_state[2]);
    }

    #[test]
    fn reset_link_state_forgets_everything() {
        let mut p = panel();
        p.pending_speed = Some(90);
        p.last_state = Some((90, 1));
        p.reset_link_state();
        assert_eq!(p.pending_speed, None);
        assert_eq!(p.last_state, None); // forces the next slider move to send
    }

    #[test]
    fn tab_title_name_or_address() {
        let mut p = panel();
        assert_eq!(p.tab_title(), "Loco 3");
        p.name = "Big Boy".to_string();
        assert_eq!(p.tab_title(), "Big Boy"); // no address suffix
    }

    #[test]
    fn labelled_layout_drops_to_six_columns() {
        let mut p = panel();
        assert_eq!(p.func_columns(), 10);
        p.labels.insert(7, "Horn".to_string());
        assert_eq!(p.func_columns(), 6);
        // a label on a hidden function does not change the flow
        p.labels.clear();
        p.show_funcs.remove(&7);
        p.labels.insert(7, "Horn".to_string());
        assert_eq!(p.func_columns(), 10);
    }

    #[test]
    fn apply_setup_trims_and_filters() {
        let mut p = panel();
        p.open_setup();
        {
            let s = p.setup.as_mut().unwrap();
            s.show = [false; NFUNC];
            s.show[0] = true;
            s.show[3] = true;
            s.labels[0] = "  Lights  ".to_string();
            s.labels[3] = "   ".to_string(); // whitespace-only label dropped
        }
        p.apply_setup();
        assert!(p.setup.is_none());
        assert_eq!(p.show_funcs, [0u8, 3].into_iter().collect());
        assert_eq!(p.labels.get(&0).map(String::as_str), Some("Lights"));
        assert!(!p.labels.contains_key(&3));
    }
}
