//! Automation scripts: a tiny line-based language for driving the layout
//! (throttle moves, functions, turnouts, power, waits, loops).
//!
//! The runner is deliberately non-blocking: the app ticks it once per frame
//! and it hands back the commands that are due. A `wait` just arms a
//! deadline -- nothing ever sleeps on the GUI thread, matching how the rest
//! of the app treats the link.
//!
//! Script text is parsed in full before anything is sent, so a typo on
//! line 30 can't leave a loco already moving from line 3.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::config::MAX_ADDR;
use crate::panel::MAX_SPEED;

/// Commands emitted in one tick are capped so a tight loop can't flood the
/// station; the runner simply resumes next frame.
const MAX_CMDS_PER_TICK: usize = 32;
/// Instructions stepped per tick -- keeps a command-free `repeat` body from
/// spinning the frame forever.
const MAX_STEPS_PER_TICK: usize = 1000;

#[derive(Clone, Debug, PartialEq)]
pub enum Instr {
    /// Set speed (keeps the remembered direction).
    Speed(u32, u8),
    /// Set direction, 1 = forward (resends the remembered speed).
    Dir(u32, u8),
    Estop,
    Func(u32, u8, bool),
    Wait(f32),
    /// Pre-built command, e.g. "<1 MAIN>" from `power on main` or a `send`.
    Cmd(String),
    /// Turnout id, thrown?
    Turnout(u32, bool),
    /// `end` holds the index of the matching End instruction.
    Repeat { count: u32, end: usize },
    /// Holds the index of the matching Repeat instruction.
    End { start: usize },
}

/// (source line, instruction) -- the line number feeds the status display.
pub type Prog = Vec<(usize, Instr)>;

fn req_int(
    tok: Option<&str>,
    lo: i64,
    hi: i64,
    what: &str,
    line: usize,
) -> Result<i64, (usize, String)> {
    match tok.and_then(|t| t.parse::<i64>().ok()) {
        Some(v) if (lo..=hi).contains(&v) => Ok(v),
        _ => Err((line, format!("{what} must be {lo}-{hi}"))),
    }
}

fn req_f32(
    tok: Option<&str>,
    lo: f32,
    hi: f32,
    what: &str,
    line: usize,
) -> Result<f32, (usize, String)> {
    match tok.and_then(|t| t.parse::<f32>().ok()) {
        Some(v) if (lo..=hi).contains(&v) => Ok(v),
        _ => Err((line, format!("{what} must be {lo}-{hi} seconds"))),
    }
}

fn cab_arg(tok: Option<&str>, line: usize) -> Result<u32, (usize, String)> {
    Ok(req_int(tok, 1, MAX_ADDR as i64, "address", line)? as u32)
}

/// Parse a whole script. Errors carry the 1-based source line.
pub fn parse(text: &str) -> Result<Prog, (usize, String)> {
    let mut prog: Prog = Vec::new();
    // Indices of open `repeat`s, waiting for their `end`.
    let mut stack: Vec<usize> = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let line = idx + 1;
        let code = raw.split('#').next().unwrap_or("");
        let mut toks = code.split_whitespace();
        let Some(word) = toks.next() else { continue };
        // `send` keeps the rest of the line verbatim; everything else is
        // strict about trailing junk so a typo fails loudly at parse time.
        let mut rest_taken = false;
        match word.to_ascii_lowercase().as_str() {
            "speed" => {
                let cab = cab_arg(toks.next(), line)?;
                let s = req_int(toks.next(), 0, MAX_SPEED as i64, "speed", line)?;
                prog.push((line, Instr::Speed(cab, s as u8)));
            }
            "forward" | "fwd" => {
                let cab = cab_arg(toks.next(), line)?;
                prog.push((line, Instr::Dir(cab, 1)));
            }
            "reverse" | "rev" => {
                let cab = cab_arg(toks.next(), line)?;
                prog.push((line, Instr::Dir(cab, 0)));
            }
            "stop" => {
                let cab = cab_arg(toks.next(), line)?;
                prog.push((line, Instr::Speed(cab, 0)));
            }
            "estop" => prog.push((line, Instr::Estop)),
            "func" => {
                let cab = cab_arg(toks.next(), line)?;
                let n = req_int(toks.next(), 0, 28, "function", line)? as u8;
                let on = match toks.next().map(str::to_ascii_lowercase).as_deref() {
                    Some("on") | Some("1") => true,
                    Some("off") | Some("0") => false,
                    _ => return Err((line, "func needs on or off".to_string())),
                };
                prog.push((line, Instr::Func(cab, n, on)));
            }
            "pulse" => {
                let cab = cab_arg(toks.next(), line)?;
                let n = req_int(toks.next(), 0, 28, "function", line)? as u8;
                let secs = match toks.next() {
                    None => 0.5,
                    tok => req_f32(tok, 0.05, 60.0, "duration", line)?,
                };
                prog.push((line, Instr::Func(cab, n, true)));
                prog.push((line, Instr::Wait(secs)));
                prog.push((line, Instr::Func(cab, n, false)));
            }
            "wait" => {
                let secs = req_f32(toks.next(), 0.05, 3600.0, "wait", line)?;
                prog.push((line, Instr::Wait(secs)));
            }
            "power" => {
                let on = match toks.next().map(str::to_ascii_lowercase).as_deref() {
                    Some("on") | Some("1") => true,
                    Some("off") | Some("0") => false,
                    _ => return Err((line, "power needs on or off".to_string())),
                };
                let track = match toks.next().map(str::to_ascii_lowercase).as_deref() {
                    None => "",
                    Some("main") => " MAIN",
                    Some("prog") => " PROG",
                    _ => return Err((line, "expected main or prog".to_string())),
                };
                prog.push((line, Instr::Cmd(format!("<{}{track}>", on as u8))));
            }
            "throw" => {
                let id = req_int(toks.next(), 0, 32767, "turnout id", line)?;
                prog.push((line, Instr::Turnout(id as u32, true)));
            }
            "close" => {
                let id = req_int(toks.next(), 0, 32767, "turnout id", line)?;
                prog.push((line, Instr::Turnout(id as u32, false)));
            }
            "send" => {
                rest_taken = true;
                let rest = code
                    .trim()
                    .split_once(char::is_whitespace)
                    .map(|(_, r)| r.trim())
                    .unwrap_or("");
                if rest.is_empty() {
                    return Err((line, "send needs a command".to_string()));
                }
                let cmd = if rest.starts_with('<') {
                    rest.to_string()
                } else {
                    format!("<{rest}>")
                };
                prog.push((line, Instr::Cmd(cmd)));
            }
            "repeat" => {
                let count = req_int(toks.next(), 1, 1_000_000, "repeat count", line)?;
                stack.push(prog.len());
                // `end` is patched when the matching `end` line is parsed.
                prog.push((line, Instr::Repeat { count: count as u32, end: 0 }));
            }
            "end" => {
                let Some(start) = stack.pop() else {
                    return Err((line, "end without a matching repeat".to_string()));
                };
                let here = prog.len();
                if let Instr::Repeat { end, .. } = &mut prog[start].1 {
                    *end = here;
                }
                prog.push((line, Instr::End { start }));
            }
            other => return Err((line, format!("unknown command '{other}'"))),
        }
        if !rest_taken && toks.next().is_some() {
            return Err((line, "unexpected text after command".to_string()));
        }
    }
    if let Some(&start) = stack.first() {
        return Err((prog[start].0, "repeat without a matching end".to_string()));
    }
    Ok(prog)
}

pub struct Tick {
    pub cmds: Vec<String>,
    pub done: bool,
    /// Source line of the next instruction (for the status display).
    pub line: usize,
}

pub struct Runner {
    prog: Prog,
    pc: usize,
    wait_until: Option<Instant>,
    /// Open loops: (index of the Repeat instruction, iterations left).
    loops: Vec<(usize, u32)>,
    /// Last (speed, direction) issued per cab; direction defaults forward.
    /// Scripts say `speed` and `forward` separately, the wire wants both.
    cabs: HashMap<u32, (u8, u8)>,
}

impl Runner {
    pub fn new(prog: Prog) -> Self {
        Runner {
            prog,
            pc: 0,
            wait_until: None,
            loops: Vec::new(),
            cabs: HashMap::new(),
        }
    }

    fn current_line(&self) -> usize {
        self.prog
            .get(self.pc)
            .or(self.prog.last())
            .map(|(l, _)| *l)
            .unwrap_or(0)
    }

    /// Run until the next wait / end of program / per-tick cap, returning
    /// the commands that came due. Call once per frame.
    pub fn tick(&mut self, now: Instant) -> Tick {
        let mut cmds = Vec::new();
        if let Some(until) = self.wait_until {
            if now < until {
                return Tick { cmds, done: false, line: self.current_line() };
            }
            self.wait_until = None;
        }
        let mut steps = 0;
        while self.pc < self.prog.len()
            && cmds.len() < MAX_CMDS_PER_TICK
            && steps < MAX_STEPS_PER_TICK
        {
            steps += 1;
            match self.prog[self.pc].1.clone() {
                Instr::Speed(cab, s) => {
                    let e = self.cabs.entry(cab).or_insert((0, 1));
                    e.0 = s;
                    cmds.push(format!("<t {cab} {s} {}>", e.1));
                }
                Instr::Dir(cab, d) => {
                    let e = self.cabs.entry(cab).or_insert((0, 1));
                    e.1 = d;
                    cmds.push(format!("<t {cab} {} {d}>", e.0));
                }
                Instr::Estop => {
                    // The station zeroes every cab; mirror that so a later
                    // `forward` doesn't resurrect the pre-estop speed.
                    for e in self.cabs.values_mut() {
                        e.0 = 0;
                    }
                    cmds.push("<!>".to_string());
                }
                Instr::Func(cab, n, on) => {
                    cmds.push(format!("<F {cab} {n} {}>", on as u8));
                }
                Instr::Cmd(cmd) => cmds.push(cmd),
                Instr::Turnout(id, thrown) => {
                    cmds.push(format!("<T {id} {}>", thrown as u8));
                }
                Instr::Wait(secs) => {
                    self.wait_until = Some(now + Duration::from_secs_f32(secs));
                    self.pc += 1;
                    break;
                }
                Instr::Repeat { count, .. } => {
                    self.loops.push((self.pc, count));
                }
                Instr::End { start } => {
                    if let Some(top) = self.loops.last_mut()
                        && top.0 == start
                    {
                        top.1 -= 1;
                        if top.1 == 0 {
                            self.loops.pop();
                        } else {
                            self.pc = start + 1;
                            continue;
                        }
                    }
                }
            }
            self.pc += 1;
        }
        Tick {
            cmds,
            done: self.pc >= self.prog.len() && self.wait_until.is_none(),
            line: self.current_line(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_all(text: &str) -> Vec<String> {
        let mut r = Runner::new(parse(text).unwrap());
        let mut out = Vec::new();
        let mut now = Instant::now();
        loop {
            let t = r.tick(now);
            out.extend(t.cmds);
            if t.done {
                return out;
            }
            now += Duration::from_secs(3600); // any wait is long expired
        }
    }

    #[test]
    fn parse_reports_the_offending_line() {
        assert_eq!(parse("wait 1\nwarp 9").unwrap_err().0, 2);
        assert_eq!(parse("speed 3 200").unwrap_err().0, 1);
        assert_eq!(parse("speed 3 50 fast").unwrap_err().1, "unexpected text after command");
        assert_eq!(parse("end").unwrap_err().1, "end without a matching repeat");
        // the error points at the unclosed repeat, not EOF
        assert_eq!(parse("wait 1\nrepeat 2\nwait 1").unwrap_err().0, 2);
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let prog = parse("# a comment\n\n  wait 1 # trailing\n").unwrap();
        assert_eq!(prog.len(), 1);
        assert_eq!(prog[0], (3, Instr::Wait(1.0)));
    }

    #[test]
    fn speed_remembers_direction_and_estop_forgets_speed() {
        let cmds = run_all("reverse 3\nspeed 3 50\nestop\nforward 3");
        assert_eq!(cmds, ["<t 3 0 0>", "<t 3 50 0>", "<!>", "<t 3 0 1>"]);
    }

    #[test]
    fn pulse_expands_to_on_wait_off() {
        let cmds = run_all("pulse 3 2 1.0");
        assert_eq!(cmds, ["<F 3 2 1>", "<F 3 2 0>"]);
    }

    #[test]
    fn wait_defers_the_rest_of_the_program() {
        let mut r = Runner::new(parse("func 3 0 on\nwait 5\nfunc 3 0 off").unwrap());
        let now = Instant::now();
        let t = r.tick(now);
        assert_eq!(t.cmds, ["<F 3 0 1>"]);
        assert!(!t.done);
        // still waiting one second in
        let t = r.tick(now + Duration::from_secs(1));
        assert!(t.cmds.is_empty() && !t.done);
        let t = r.tick(now + Duration::from_secs(6));
        assert_eq!(t.cmds, ["<F 3 0 0>"]);
        assert!(t.done);
    }

    #[test]
    fn nested_repeats_multiply() {
        let cmds = run_all("repeat 2\nthrow 1\nrepeat 3\nclose 2\nend\nend");
        assert_eq!(cmds.iter().filter(|c| *c == "<T 1 1>").count(), 2);
        assert_eq!(cmds.iter().filter(|c| *c == "<T 2 0>").count(), 6);
    }

    #[test]
    fn misc_commands_render() {
        let cmds = run_all("power on main\nsend D CABS\nsend <s>\nstop 44");
        assert_eq!(cmds, ["<1 MAIN>", "<D CABS>", "<s>", "<t 44 0 1>"]);
    }
}
