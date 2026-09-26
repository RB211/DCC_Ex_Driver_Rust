//! Common NMRA S-9.2.2 CV names for the Programming tab's live lookup.

/// Best-effort NMRA name for a CV number, or None if unremarkable.
pub fn cv_name(cv: u32) -> Option<String> {
    let fixed = match cv {
        1 => "Primary (short) address",
        2 => "Vstart -- motor start voltage",
        3 => "Acceleration rate",
        4 => "Deceleration rate",
        5 => "Vhigh -- top speed voltage",
        6 => "Vmid -- mid speed voltage",
        7 => "Manufacturer version, read-only",
        8 => "Manufacturer ID -- writing it resets many decoders",
        17 => "Extended address high byte",
        18 => "Extended address low byte",
        19 => "Consist address",
        21 => "Consist functions F1-F8",
        22 => "Consist functions FL, F9-F12",
        23 => "Acceleration adjustment",
        24 => "Deceleration adjustment",
        28 => "RailCom configuration",
        29 => "Configuration data #1",
        30 => "Error information",
        65 => "Kick start",
        66 => "Forward trim",
        95 => "Reverse trim",
        105 => "User ID #1",
        106 => "User ID #2",
        _ => "",
    };
    if !fixed.is_empty() {
        return Some(fixed.to_string());
    }
    match cv {
        33..=46 => Some("Function output mapping".to_string()),
        67..=94 => Some(format!("Speed table entry {}/28", cv - 66)),
        112..=256 => Some("Manufacturer-specific".to_string()),
        _ => None,
    }
}

/// "CV 3 (Acceleration rate)", or plain "CV 3" for a nameless one.
/// `cv` arrives as raw reply text, so it may not even be a number.
pub fn cv_desc(cv: &str) -> String {
    match cv.parse::<u32>().ok().and_then(cv_name) {
        Some(name) => format!("CV {cv} ({name})"),
        None => format!("CV {cv}"),
    }
}
