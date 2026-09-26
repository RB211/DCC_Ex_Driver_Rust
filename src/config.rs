//! Per-loco configuration, shared with the Python app.
//!
//! CONFIG_PATH schema: {"locos": [{"name": str, "address": int,
//!   "toggle_funcs": [int], "show_funcs": [int], "labels": {"n": str}}]}
//! The pre-multi-loco file was {"toggle_funcs": [int]}; load_loco_cfgs()
//! migrates it to a single loco so nobody loses their button modes.

use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

/// F0-F28, exposed as buttons per loco tab.
pub const FUNCTIONS: std::ops::Range<u8> = 0..29;
/// Default toggle-mode buttons; F3 is the owner's short whistle.
pub const DEFAULT_TOGGLE: &[u8] = &[3];
pub const MAX_ADDR: u32 = 10293;

pub fn config_path() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config/dccex-throttle.json")
}

#[derive(Clone, Debug)]
pub struct LocoCfg {
    pub name: String,
    pub address: u32,
    pub toggle_funcs: BTreeSet<u8>,
    pub show_funcs: BTreeSet<u8>,
    pub labels: BTreeMap<u8, String>,
}

impl LocoCfg {
    pub fn default_with(address: u32) -> Self {
        LocoCfg {
            name: String::new(),
            address,
            toggle_funcs: DEFAULT_TOGGLE.iter().copied().collect(),
            show_funcs: FUNCTIONS.collect(),
            labels: BTreeMap::new(),
        }
    }
}

impl Default for LocoCfg {
    fn default() -> Self {
        Self::default_with(3)
    }
}

/// A JSON value as an integer, whether it arrived as a number or a string.
fn as_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// A set of function numbers from a JSON array; None if the field is not a
/// clean list of ints. Numbers outside FUNCTIONS are dropped.
fn func_set(v: &Value) -> Option<BTreeSet<u8>> {
    let arr = v.as_array()?;
    let mut out = BTreeSet::new();
    for item in arr {
        let n = as_int(item)?;
        if (0..29).contains(&n) {
            out.insert(n as u8);
        }
    }
    Some(out)
}

/// One sanitized loco from the config file, or None if hopeless.
///
/// Bad fields fall back to their defaults individually rather than failing
/// the whole loco.
fn clean_loco_cfg(raw: &Value) -> Option<LocoCfg> {
    let obj = raw.as_object()?;
    let mut cfg = LocoCfg::default();
    if let Some(addr) = obj.get("address").and_then(as_int) {
        if (1..=MAX_ADDR as i64).contains(&addr) {
            cfg.address = addr as u32;
        }
    }
    if let Some(name) = obj.get("name").and_then(Value::as_str) {
        cfg.name = name.trim().to_string();
    }
    if let Some(set) = obj.get("toggle_funcs").and_then(func_set) {
        cfg.toggle_funcs = set;
    }
    if let Some(set) = obj.get("show_funcs").and_then(func_set) {
        cfg.show_funcs = set;
    }
    if let Some(labels) = obj.get("labels").and_then(Value::as_object) {
        for (k, v) in labels {
            let (Ok(n), Some(text)) = (k.trim().parse::<u8>(), v.as_str()) else {
                continue;
            };
            if FUNCTIONS.contains(&n) && !text.trim().is_empty() {
                cfg.labels.insert(n, text.trim().to_string());
            }
        }
    }
    Some(cfg)
}

/// The loco list from the config file, migrated/sanitized, never empty.
pub fn load_loco_cfgs() -> Vec<LocoCfg> {
    match fs::read_to_string(config_path()) {
        Ok(text) => cfgs_from_json(&text),
        Err(_) => vec![LocoCfg::default()],
    }
}

/// Testable core of load_loco_cfgs: parse + migrate one JSON document.
fn cfgs_from_json(text: &str) -> Vec<LocoCfg> {
    let fallback = || vec![LocoCfg::default()];
    let Ok(data) = serde_json::from_str::<Value>(text) else {
        return fallback();
    };
    let Some(obj) = data.as_object() else {
        return fallback();
    };
    if let Some(locos) = obj.get("locos").and_then(Value::as_array) {
        let cfgs: Vec<LocoCfg> = locos.iter().filter_map(clean_loco_cfg).collect();
        if !cfgs.is_empty() {
            return cfgs;
        }
    }
    if let Some(toggles) = obj.get("toggle_funcs") {
        // pre-multi-loco file: one loco, keep its toggle modes
        let mut cfg = LocoCfg::default();
        if let Some(set) = func_set(toggles) {
            cfg.toggle_funcs = set;
        }
        return vec![cfg];
    }
    fallback()
}

pub fn save_loco_cfgs(cfgs: &[LocoCfg]) -> std::io::Result<()> {
    let locos: Vec<Value> = cfgs
        .iter()
        .map(|c| {
            json!({
                "name": c.name,
                "address": c.address,
                "toggle_funcs": c.toggle_funcs.iter().collect::<Vec<_>>(),
                "show_funcs": c.show_funcs.iter().collect::<Vec<_>>(),
                "labels": c.labels.iter()
                    .map(|(n, l)| (n.to_string(), l.clone()))
                    .collect::<BTreeMap<String, String>>(),
            })
        })
        .collect();
    let path = config_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(&path, serde_json::to_string_pretty(&json!({ "locos": locos }))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn garbled_file_falls_back_to_default_loco() {
        for text in ["not json at all", "[1,2,3]", "{}", "{\"locos\": \"nope\"}"] {
            let cfgs = cfgs_from_json(text);
            assert_eq!(cfgs.len(), 1);
            assert_eq!(cfgs[0].address, 3);
            assert_eq!(cfgs[0].toggle_funcs, DEFAULT_TOGGLE.iter().copied().collect());
        }
    }

    #[test]
    fn pre_multi_loco_file_migrates() {
        let cfgs = cfgs_from_json("{\"toggle_funcs\": [0, 5, 99]}");
        assert_eq!(cfgs.len(), 1);
        // out-of-range 99 dropped, the rest kept
        assert_eq!(cfgs[0].toggle_funcs, [0u8, 5].into_iter().collect());
    }

    #[test]
    fn bad_fields_fall_back_individually() {
        let cfgs = cfgs_from_json(
            "{\"locos\": [{\"name\": \" Big Boy \", \"address\": 99999, \
             \"toggle_funcs\": \"bad\", \"show_funcs\": [1, 2],
             \"labels\": {\"2\": \"Horn\", \"x\": \"junk\", \"40\": \"out\"}}]}",
        );
        assert_eq!(cfgs.len(), 1);
        let c = &cfgs[0];
        assert_eq!(c.name, "Big Boy"); // trimmed
        assert_eq!(c.address, 3); // 99999 out of range -> default
        assert_eq!(c.toggle_funcs, DEFAULT_TOGGLE.iter().copied().collect());
        assert_eq!(c.show_funcs, [1u8, 2].into_iter().collect());
        assert_eq!(c.labels.get(&2).map(String::as_str), Some("Horn"));
        assert_eq!(c.labels.len(), 1);
    }

    #[test]
    fn round_trip() {
        let mut cfg = LocoCfg::default_with(4711);
        cfg.name = "Shunter".to_string();
        cfg.labels.insert(0, "Lights".to_string());
        cfg.show_funcs = [0u8, 3, 12].into_iter().collect();
        let json = serde_json::json!({
            "locos": [{
                "name": cfg.name, "address": cfg.address,
                "toggle_funcs": cfg.toggle_funcs.iter().collect::<Vec<_>>(),
                "show_funcs": cfg.show_funcs.iter().collect::<Vec<_>>(),
                "labels": {"0": "Lights"},
            }]
        });
        let back = cfgs_from_json(&json.to_string());
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].name, cfg.name);
        assert_eq!(back[0].address, cfg.address);
        assert_eq!(back[0].show_funcs, cfg.show_funcs);
        assert_eq!(back[0].labels, cfg.labels);
    }
}
