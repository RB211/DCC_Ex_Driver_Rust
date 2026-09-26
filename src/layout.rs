//! The Layout tab model: a grid of track pieces and DCC turnouts.
//!
//! Pure data + geometry -- no egui. Each cell holds one piece; a piece is a
//! set of line segments in unit-cell coordinates ((0,0) top-left, (1,1)
//! bottom-right) that the app scales and paints. A turnout is a straight
//! main route plus a diagonal branch to one corner; which route draws
//! "set" comes from the station's <H id state> broadcasts, kept here in
//! `states` (runtime only, never saved -- the station owns the truth).

use std::collections::{BTreeMap, HashMap};

use serde_json::{json, Value};

/// Canvas cell size in points.
pub const CELL: f32 = 36.0;
pub const MIN_SIZE: u32 = 4;
pub const MAX_SIZE: u32 = 64;
/// DCC-EX turnout ids are int16.
pub const MAX_TURNOUT_ID: u32 = 32767;

const DEFAULT_COLS: u32 = 24;
const DEFAULT_ROWS: u32 = 12;

/// One line segment in unit-cell coordinates.
pub type Seg = ((f32, f32), (f32, f32));

/// A cell edge / compass direction on screen (N = up).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    N,
    S,
    E,
    W,
}

impl Dir {
    /// Probe order for neighbours; also the tie-break when several offer.
    pub const ALL: [Dir; 4] = [Dir::W, Dir::E, Dir::N, Dir::S];

    fn delta(self) -> (i64, i64) {
        match self {
            Dir::N => (0, -1),
            Dir::S => (0, 1),
            Dir::E => (1, 0),
            Dir::W => (-1, 0),
        }
    }

    fn opposite(self) -> Dir {
        match self {
            Dir::N => Dir::S,
            Dir::S => Dir::N,
            Dir::E => Dir::W,
            Dir::W => Dir::E,
        }
    }

    /// 90 degrees left of a travel heading (screen coordinates, y down:
    /// heading east, left is up/north).
    fn left(self) -> Dir {
        match self {
            Dir::E => Dir::N,
            Dir::N => Dir::W,
            Dir::W => Dir::S,
            Dir::S => Dir::E,
        }
    }

    fn right(self) -> Dir {
        self.left().opposite()
    }
}

/// A cell corner. Corners carry the diagonal routes: a turnout's
/// diverging leg exits through one, and a diagonal piece in the cell
/// across that corner picks it up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Corner {
    NE,
    NW,
    SE,
    SW,
}

impl Corner {
    /// Probe order for pick_diagonal's incoming corner (a siding usually
    /// climbs away from a turnout below, so SW first).
    pub const ALL: [Corner; 4] = [Corner::SW, Corner::NE, Corner::NW, Corner::SE];

    /// Offset of the cell diagonally across this corner.
    fn delta(self) -> (i64, i64) {
        match self {
            Corner::NE => (1, -1),
            Corner::NW => (-1, -1),
            Corner::SE => (1, 1),
            Corner::SW => (-1, 1),
        }
    }

    fn opposite(self) -> Corner {
        match self {
            Corner::NE => Corner::SW,
            Corner::SW => Corner::NE,
            Corner::NW => Corner::SE,
            Corner::SE => Corner::NW,
        }
    }

    /// The corner's position in unit-cell coordinates.
    pub fn point(self) -> (f32, f32) {
        match self {
            Corner::NE => (1.0, 0.0),
            Corner::NW => (0.0, 0.0),
            Corner::SE => (1.0, 1.0),
            Corner::SW => (0.0, 1.0),
        }
    }
}

fn corner_slice(c: Corner) -> &'static [Corner] {
    match c {
        Corner::NE => &[Corner::NE],
        Corner::NW => &[Corner::NW],
        Corner::SE => &[Corner::SE],
        Corner::SW => &[Corner::SW],
    }
}

/// Plain track through a cell. Straights are named by the edges they
/// join (EW = left-right, NS = top-bottom); curves by the corner of a
/// loop they form on screen -- Curve NW is the top-left corner, so it
/// joins the bottom and right edges of its cell.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Piece {
    EW,
    NS,
    NW,
    NE,
    SW,
    SE,
    Cross,
    /// Corner-to-corner diagonals: "/" rises (SW-NE), "\" falls (NW-SE).
    DiagUp,
    DiagDown,
    /// Ramps carry a diverging leg from a corner to a levelled edge exit:
    /// named corner first, exit edge second (RampSwE enters at the SW
    /// corner and leaves through the east edge midpoint).
    RampSwE,
    RampSwN,
    RampNwE,
    RampNwS,
    RampNeW,
    RampNeS,
    RampSeW,
    RampSeN,
}

impl Piece {
    pub const ALL: [Piece; 17] = [
        Piece::EW,
        Piece::NS,
        Piece::NW,
        Piece::NE,
        Piece::SW,
        Piece::SE,
        Piece::Cross,
        Piece::DiagUp,
        Piece::DiagDown,
        Piece::RampSwE,
        Piece::RampSwN,
        Piece::RampNwE,
        Piece::RampNwS,
        Piece::RampNeW,
        Piece::RampNeS,
        Piece::RampSeW,
        Piece::RampSeN,
    ];

    pub fn code(self) -> &'static str {
        match self {
            Piece::EW => "EW",
            Piece::NS => "NS",
            Piece::NW => "NW",
            Piece::NE => "NE",
            Piece::SW => "SW",
            Piece::SE => "SE",
            Piece::Cross => "X",
            Piece::DiagUp => "DU",
            Piece::DiagDown => "DD",
            Piece::RampSwE => "RSWE",
            Piece::RampSwN => "RSWN",
            Piece::RampNwE => "RNWE",
            Piece::RampNwS => "RNWS",
            Piece::RampNeW => "RNEW",
            Piece::RampNeS => "RNES",
            Piece::RampSeW => "RSEW",
            Piece::RampSeN => "RSEN",
        }
    }

    pub fn from_code(s: &str) -> Option<Piece> {
        Piece::ALL.into_iter().find(|p| p.code() == s)
    }

    /// The edges this piece's track leaves through.
    pub fn exits(self) -> &'static [Dir] {
        match self {
            Piece::EW => &[Dir::W, Dir::E],
            Piece::NS => &[Dir::N, Dir::S],
            Piece::NW => &[Dir::S, Dir::E],
            Piece::NE => &[Dir::S, Dir::W],
            Piece::SW => &[Dir::N, Dir::E],
            Piece::SE => &[Dir::N, Dir::W],
            Piece::Cross => &[Dir::N, Dir::S, Dir::E, Dir::W],
            Piece::DiagUp | Piece::DiagDown => &[],
            Piece::RampSwE | Piece::RampNwE => &[Dir::E],
            Piece::RampNeW | Piece::RampSeW => &[Dir::W],
            Piece::RampSwN | Piece::RampSeN => &[Dir::N],
            Piece::RampNwS | Piece::RampNeS => &[Dir::S],
        }
    }

    /// The corners this piece's track leaves through.
    pub fn corners(self) -> &'static [Corner] {
        match self {
            Piece::DiagUp => &[Corner::SW, Corner::NE],
            Piece::DiagDown => &[Corner::NW, Corner::SE],
            Piece::RampSwE | Piece::RampSwN => &[Corner::SW],
            Piece::RampNwE | Piece::RampNwS => &[Corner::NW],
            Piece::RampNeW | Piece::RampNeS => &[Corner::NE],
            Piece::RampSeW | Piece::RampSeN => &[Corner::SE],
            _ => &[],
        }
    }

    /// Segments drawn for this piece. Curves go edge-midpoint -> centre ->
    /// edge-midpoint; adjacent cells meet exactly at the shared midpoint.
    /// A curve's exits are the two edges *away* from its corner name:
    /// the NW (top-left) corner piece exits south and east.
    pub fn segments(self) -> &'static [Seg] {
        const EW: &[Seg] = &[((0.0, 0.5), (1.0, 0.5))];
        const NS: &[Seg] = &[((0.5, 0.0), (0.5, 1.0))];
        const NW: &[Seg] = &[((0.5, 1.0), (0.5, 0.5)), ((0.5, 0.5), (1.0, 0.5))];
        const NE: &[Seg] = &[((0.5, 1.0), (0.5, 0.5)), ((0.5, 0.5), (0.0, 0.5))];
        const SW: &[Seg] = &[((0.5, 0.0), (0.5, 0.5)), ((0.5, 0.5), (1.0, 0.5))];
        const SE: &[Seg] = &[((0.5, 0.0), (0.5, 0.5)), ((0.5, 0.5), (0.0, 0.5))];
        const CROSS: &[Seg] = &[((0.0, 0.5), (1.0, 0.5)), ((0.5, 0.0), (0.5, 1.0))];
        const DIAG_UP: &[Seg] = &[((0.0, 1.0), (1.0, 0.0))];
        const DIAG_DOWN: &[Seg] = &[((0.0, 0.0), (1.0, 1.0))];
        // ramps: corner -> centre -> edge midpoint
        const R_SW_E: &[Seg] = &[((0.0, 1.0), (0.5, 0.5)), ((0.5, 0.5), (1.0, 0.5))];
        const R_SW_N: &[Seg] = &[((0.0, 1.0), (0.5, 0.5)), ((0.5, 0.5), (0.5, 0.0))];
        const R_NW_E: &[Seg] = &[((0.0, 0.0), (0.5, 0.5)), ((0.5, 0.5), (1.0, 0.5))];
        const R_NW_S: &[Seg] = &[((0.0, 0.0), (0.5, 0.5)), ((0.5, 0.5), (0.5, 1.0))];
        const R_NE_W: &[Seg] = &[((1.0, 0.0), (0.5, 0.5)), ((0.5, 0.5), (0.0, 0.5))];
        const R_NE_S: &[Seg] = &[((1.0, 0.0), (0.5, 0.5)), ((0.5, 0.5), (0.5, 1.0))];
        const R_SE_W: &[Seg] = &[((1.0, 1.0), (0.5, 0.5)), ((0.5, 0.5), (0.0, 0.5))];
        const R_SE_N: &[Seg] = &[((1.0, 1.0), (0.5, 0.5)), ((0.5, 0.5), (0.5, 0.0))];
        match self {
            Piece::EW => EW,
            Piece::NS => NS,
            Piece::NW => NW,
            Piece::NE => NE,
            Piece::SW => SW,
            Piece::SE => SE,
            Piece::Cross => CROSS,
            Piece::DiagUp => DIAG_UP,
            Piece::DiagDown => DIAG_DOWN,
            Piece::RampSwE => R_SW_E,
            Piece::RampSwN => R_SW_N,
            Piece::RampNwE => R_NW_E,
            Piece::RampNwS => R_NW_S,
            Piece::RampNeW => R_NE_W,
            Piece::RampNeS => R_NE_S,
            Piece::RampSeW => R_SE_W,
            Piece::RampSeN => R_SE_N,
        }
    }
}

/// Turnout orientation: the straight main route (EW or NS through the
/// cell) plus which corner the diverging route exits.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum TKind {
    EwNe,
    EwNw,
    EwSe,
    EwSw,
    NsNe,
    NsNw,
    NsSe,
    NsSw,
}

impl TKind {
    pub const ALL: [TKind; 8] = [
        TKind::EwNe,
        TKind::EwNw,
        TKind::EwSe,
        TKind::EwSw,
        TKind::NsNe,
        TKind::NsNw,
        TKind::NsSe,
        TKind::NsSw,
    ];

    pub fn code(self) -> &'static str {
        match self {
            TKind::EwNe => "EW_NE",
            TKind::EwNw => "EW_NW",
            TKind::EwSe => "EW_SE",
            TKind::EwSw => "EW_SW",
            TKind::NsNe => "NS_NE",
            TKind::NsNw => "NS_NW",
            TKind::NsSe => "NS_SE",
            TKind::NsSw => "NS_SW",
        }
    }

    pub fn from_code(s: &str) -> Option<TKind> {
        TKind::ALL.into_iter().find(|k| k.code() == s)
    }

    pub fn label(self) -> &'static str {
        match self {
            TKind::EwNe => "EW, branch NE",
            TKind::EwNw => "EW, branch NW",
            TKind::EwSe => "EW, branch SE",
            TKind::EwSw => "EW, branch SW",
            TKind::NsNe => "NS, branch NE",
            TKind::NsNw => "NS, branch NW",
            TKind::NsSe => "NS, branch SE",
            TKind::NsSw => "NS, branch SW",
        }
    }

    /// The straight (closed) route.
    pub fn main(self) -> Piece {
        match self {
            TKind::EwNe | TKind::EwNw | TKind::EwSe | TKind::EwSw => Piece::EW,
            _ => Piece::NS,
        }
    }

    /// Which corner the diverging route exits through.
    pub fn branch_corner(self) -> Corner {
        match self {
            TKind::EwNe | TKind::NsNe => Corner::NE,
            TKind::EwNw | TKind::NsNw => Corner::NW,
            TKind::EwSe | TKind::NsSe => Corner::SE,
            TKind::EwSw | TKind::NsSw => Corner::SW,
        }
    }

    /// The diverging (thrown) route: centre to a corner.
    pub fn branch(self) -> Seg {
        ((0.5, 0.5), self.branch_corner().point())
    }
}

/// The curve piece joining two perpendicular exits, if they are.
/// The ramps that can carry a leg arriving at `corner`, best first
/// (levelling off to horizontal beats turning vertical).
fn ramp_options(corner: Corner) -> [(Dir, Piece); 2] {
    match corner {
        Corner::SW => [(Dir::E, Piece::RampSwE), (Dir::N, Piece::RampSwN)],
        Corner::NW => [(Dir::E, Piece::RampNwE), (Dir::S, Piece::RampNwS)],
        Corner::NE => [(Dir::W, Piece::RampNeW), (Dir::S, Piece::RampNeS)],
        Corner::SE => [(Dir::W, Piece::RampSeW), (Dir::N, Piece::RampSeN)],
    }
}

/// A 45-degree bend for a leg arriving at `corner`: the ramp whose exit
/// is left (or right) of the leg's diagonal heading. A leg from the SW
/// corner travels north-east, so left climbs to N, right levels to E.
fn ramp_bend(corner: Corner, left: bool) -> Piece {
    match (corner, left) {
        (Corner::SW, true) => Piece::RampSwN,
        (Corner::SW, false) => Piece::RampSwE,
        (Corner::NW, true) => Piece::RampNwE,
        (Corner::NW, false) => Piece::RampNwS,
        (Corner::NE, true) => Piece::RampNeS,
        (Corner::NE, false) => Piece::RampNeW,
        (Corner::SE, true) => Piece::RampSeW,
        (Corner::SE, false) => Piece::RampSeN,
    }
}

fn curve_joining(a: Dir, b: Dir) -> Option<Piece> {
    match (a, b) {
        (Dir::S, Dir::E) | (Dir::E, Dir::S) => Some(Piece::NW),
        (Dir::S, Dir::W) | (Dir::W, Dir::S) => Some(Piece::NE),
        (Dir::N, Dir::E) | (Dir::E, Dir::N) => Some(Piece::SW),
        (Dir::N, Dir::W) | (Dir::W, Dir::N) => Some(Piece::SE),
        _ => None,
    }
}

#[derive(Clone, PartialEq, Debug)]
pub enum Cell {
    Track(Piece),
    Turnout { id: u32, kind: TKind },
}

impl Cell {
    /// Edges this cell's track can leave through. A turnout's diverging
    /// leg exits through a corner, not an edge, so only its straight main
    /// route counts here -- the leg shows up in corners() instead.
    pub fn exits(&self) -> &'static [Dir] {
        match self {
            Cell::Track(p) => p.exits(),
            Cell::Turnout { kind, .. } => kind.main().exits(),
        }
    }

    /// Corners this cell's track can leave through: diagonals, ramps and
    /// the turnout's diverging leg.
    pub fn corners(&self) -> &'static [Corner] {
        match self {
            Cell::Track(p) => p.corners(),
            Cell::Turnout { kind, .. } => corner_slice(kind.branch_corner()),
        }
    }
}

/// The edit-mode tool in hand. The two curve tools carry no orientation:
/// the piece is picked from the neighbouring track at placement time
/// (see Layout::pick_curve). The turnout tool takes its orientation from
/// a separate picker so the palette stays one row.
#[derive(Clone, Copy, PartialEq)]
pub enum Tool {
    Erase,
    Track(Piece),
    CurveLeft,
    CurveRight,
    /// Diagonal / ramp for a turnout's diverging leg; the piece is picked
    /// from the surrounding corners and edges (see Layout::pick_diagonal).
    Diagonal,
    Turnout,
}

pub struct Layout {
    pub cols: u32,
    pub rows: u32,
    /// (col, row) -> cell; BTreeMap so saves are deterministic.
    pub cells: BTreeMap<(u32, u32), Cell>,
    /// Turnout id -> thrown?, mirrored from <H>/<jT>. Runtime only.
    pub states: HashMap<u32, bool>,
}

impl Layout {
    pub fn new() -> Self {
        Layout {
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            cells: BTreeMap::new(),
            states: HashMap::new(),
        }
    }

    /// True if the cell next door in direction `d` has track pointing
    /// back at `at`'s shared edge.
    fn neighbor_offers(&self, at: (u32, u32), d: Dir) -> bool {
        let (dx, dy) = d.delta();
        let (nx, ny) = (at.0 as i64 + dx, at.1 as i64 + dy);
        if nx < 0 || ny < 0 {
            return false;
        }
        self.cells
            .get(&(nx as u32, ny as u32))
            .is_some_and(|c| c.exits().contains(&d.opposite()))
    }

    /// The curve to drop at `at` for the Left/Right curve tool.
    ///
    /// Orientation comes from the surrounding track, not the tool: if
    /// exactly two perpendicular neighbours connect into this cell, the
    /// corner joining them is the only sensible piece and the hand is
    /// ignored. A leg arriving at a *corner* (a turnout's diverging leg
    /// or a diagonal) makes the curve a ramp: with a straight already
    /// alongside it snaps to the ramp joining them, otherwise the hand
    /// bends the leg 45 degrees left or right. Failing all that, the
    /// first connecting edge (probe order W, E, N, S) is taken as the
    /// incoming run and the curve bends it left or right as seen
    /// travelling *into* this cell -- so laying a loop clockwise is
    /// "Right" at every corner, counter-clockwise is "Left". With no
    /// connecting track at all, the incoming run defaults to the west;
    /// re-click the cell after laying its neighbours and it re-orients.
    pub fn pick_curve(&self, at: (u32, u32), left: bool) -> Piece {
        let edges: Vec<Dir> = Dir::ALL
            .into_iter()
            .filter(|&d| self.neighbor_offers(at, d))
            .collect();
        if edges.len() == 2
            && let Some(p) = curve_joining(edges[0], edges[1])
        {
            return p;
        }
        let corners: Vec<Corner> = Corner::ALL
            .into_iter()
            .filter(|&c| self.corner_offered(at, c))
            .collect();
        for &corner in &corners {
            for (edge, ramp) in ramp_options(corner) {
                if edges.contains(&edge) {
                    return ramp;
                }
            }
        }
        if let Some(&corner) = corners.first() {
            return ramp_bend(corner, left);
        }
        let incoming = edges.first().copied().unwrap_or(Dir::W);
        let travel = incoming.opposite();
        let out = if left { travel.left() } else { travel.right() };
        // incoming and out are perpendicular by construction
        curve_joining(incoming, out).unwrap()
    }

    /// True if the cell diagonally across `corner` has track exiting the
    /// shared corner (a diagonal, a ramp, or a turnout's diverging leg).
    fn corner_offered(&self, at: (u32, u32), corner: Corner) -> bool {
        let (dx, dy) = corner.delta();
        let (nx, ny) = (at.0 as i64 + dx, at.1 as i64 + dy);
        if nx < 0 || ny < 0 {
            return false;
        }
        self.cells
            .get(&(nx as u32, ny as u32))
            .is_some_and(|c| c.corners().contains(&corner.opposite()))
    }

    /// The piece the Diagonal tool drops at `at`.
    ///
    /// Both ends of a diagonal present -> that diagonal. One connecting
    /// corner (typically a turnout's diverging leg from below): if a
    /// neighbouring straight can take the leg, level off into the ramp
    /// joining them (flattening to horizontal wins over vertical when
    /// both connect); otherwise keep the diagonal climbing through that
    /// corner. Nothing connecting -> a rising "/" as a starting default;
    /// as with curves, re-clicking after the neighbours exist re-picks.
    pub fn pick_diagonal(&self, at: (u32, u32)) -> Piece {
        let offered: Vec<Corner> = Corner::ALL
            .into_iter()
            .filter(|&c| self.corner_offered(at, c))
            .collect();
        let has = |c: Corner| offered.contains(&c);
        if has(Corner::SW) && has(Corner::NE) {
            return Piece::DiagUp;
        }
        if has(Corner::NW) && has(Corner::SE) {
            return Piece::DiagDown;
        }
        let Some(&incoming) = offered.first() else {
            return Piece::DiagUp;
        };
        for (edge, ramp) in ramp_options(incoming) {
            if self.neighbor_offers(at, edge) {
                return ramp;
            }
        }
        match incoming {
            Corner::SW | Corner::NE => Piece::DiagUp,
            Corner::NW | Corner::SE => Piece::DiagDown,
        }
    }

    /// Drop cells that fell outside the grid (after a resize).
    pub fn prune(&mut self) {
        let (cols, rows) = (self.cols, self.rows);
        self.cells.retain(|&(x, y), _| x < cols && y < rows);
    }

    /// Smallest turnout id (from 1) not already used on the plan.
    pub fn next_free_id(&self) -> u32 {
        let used: std::collections::BTreeSet<u32> = self
            .cells
            .values()
            .filter_map(|c| match c {
                Cell::Turnout { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        (1..=MAX_TURNOUT_ID).find(|id| !used.contains(id)).unwrap_or(1)
    }

    pub fn to_json(&self) -> Value {
        let cells: Vec<Value> = self
            .cells
            .iter()
            .map(|(&(x, y), cell)| match cell {
                Cell::Track(p) => json!({"x": x, "y": y, "p": p.code()}),
                Cell::Turnout { id, kind } => {
                    json!({"x": x, "y": y, "p": "T", "id": id, "k": kind.code()})
                }
            })
            .collect();
        json!({"cols": self.cols, "rows": self.rows, "cells": cells})
    }

    /// Rebuild from the config file; anything unusable is dropped cell by
    /// cell rather than losing the whole plan.
    pub fn from_json(v: &Value) -> Layout {
        let mut layout = Layout::new();
        let Some(obj) = v.as_object() else {
            return layout;
        };
        let dim = |key: &str, default: u32| -> u32 {
            obj.get(key)
                .and_then(Value::as_u64)
                .map(|n| (n as u32).clamp(MIN_SIZE, MAX_SIZE))
                .unwrap_or(default)
        };
        layout.cols = dim("cols", DEFAULT_COLS);
        layout.rows = dim("rows", DEFAULT_ROWS);
        let Some(cells) = obj.get("cells").and_then(Value::as_array) else {
            return layout;
        };
        for c in cells {
            let (Some(x), Some(y), Some(p)) = (
                c.get("x").and_then(Value::as_u64),
                c.get("y").and_then(Value::as_u64),
                c.get("p").and_then(Value::as_str),
            ) else {
                continue;
            };
            let key = (x as u32, y as u32);
            let cell = if p == "T" {
                let (Some(id), Some(kind)) = (
                    c.get("id").and_then(Value::as_u64).filter(|&id| id <= MAX_TURNOUT_ID as u64),
                    c.get("k").and_then(Value::as_str).and_then(TKind::from_code),
                ) else {
                    continue;
                };
                Cell::Turnout { id: id as u32, kind }
            } else {
                match Piece::from_code(p) {
                    Some(piece) => Cell::Track(piece),
                    None => continue,
                }
            };
            layout.cells.insert(key, cell);
        }
        layout.prune();
        layout
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut l = Layout::new();
        l.cols = 10;
        l.rows = 6;
        l.cells.insert((0, 2), Cell::Track(Piece::EW));
        l.cells.insert((3, 2), Cell::Turnout { id: 7, kind: TKind::EwNe });
        let back = Layout::from_json(&l.to_json());
        assert_eq!((back.cols, back.rows), (10, 6));
        assert_eq!(back.cells, l.cells);
    }

    #[test]
    fn garbage_falls_back_cell_by_cell() {
        let v: Value = serde_json::from_str(
            r#"{"cols": 9999, "rows": 6, "cells": [
                {"x": 1, "y": 1, "p": "EW"},
                {"x": 2, "y": 1, "p": "nope"},
                {"x": 3, "y": 1, "p": "T", "id": 99999, "k": "EW_NE"},
                {"x": 4, "y": 1, "p": "T", "id": 5, "k": "EW_NE"},
                {"x": 50, "y": 50, "p": "EW"}
            ]}"#,
        )
        .unwrap();
        let l = Layout::from_json(&v);
        assert_eq!(l.cols, MAX_SIZE); // clamped
        // the good cells survive, the bad piece / bad id / out-of-bounds don't
        assert_eq!(l.cells.len(), 2);
        assert!(matches!(l.cells.get(&(4, 1)), Some(Cell::Turnout { id: 5, .. })));
        assert_eq!(Layout::from_json(&Value::Null).cells.len(), 0);
    }

    #[test]
    fn next_free_id_skips_used() {
        let mut l = Layout::new();
        assert_eq!(l.next_free_id(), 1);
        l.cells.insert((0, 0), Cell::Turnout { id: 1, kind: TKind::EwNe });
        l.cells.insert((1, 0), Cell::Turnout { id: 3, kind: TKind::EwNe });
        assert_eq!(l.next_free_id(), 2);
    }

    #[test]
    fn curves_are_named_by_corner_position() {
        // A curve is named for where it sits as the corner of a loop, so
        // its two exits are the edges facing away from that corner:
        // NW (top-left) exits south + east, and so on around.
        let exits = |p: Piece| -> Vec<(f32, f32)> {
            p.segments()
                .iter()
                .flat_map(|&(a, b)| [a, b])
                .filter(|&(x, y)| {
                    x == 0.0 || x == 1.0 || y == 0.0 || y == 1.0
                })
                .collect()
        };
        let south = (0.5, 1.0);
        let north = (0.5, 0.0);
        let east = (1.0, 0.5);
        let west = (0.0, 0.5);
        assert_eq!(exits(Piece::NW), [south, east]);
        assert_eq!(exits(Piece::NE), [south, west]);
        assert_eq!(exits(Piece::SW), [north, east]);
        assert_eq!(exits(Piece::SE), [north, west]);
    }

    #[test]
    fn curve_tool_follows_the_run() {
        let mut l = Layout::new();
        // Painting the top edge eastward, corner cell has track to its west:
        // Right bends the run south (clockwise loop), Left bends it north.
        l.cells.insert((4, 2), Cell::Track(Piece::EW));
        assert_eq!(l.pick_curve((5, 2), false), Piece::NE); // top-right corner
        assert_eq!(l.pick_curve((5, 2), true), Piece::SE); // bottom-right corner
        // Coming down a vertical run, neighbour above:
        l.cells.clear();
        l.cells.insert((5, 1), Cell::Track(Piece::NS));
        assert_eq!(l.pick_curve((5, 2), false), Piece::SE);
        assert_eq!(l.pick_curve((5, 2), true), Piece::SW);
    }

    #[test]
    fn curve_tool_snaps_to_two_perpendicular_neighbours() {
        let mut l = Layout::new();
        // Track approaching from the west and from the south: the corner
        // joining them is the only sensible piece, whatever the hand.
        l.cells.insert((4, 2), Cell::Track(Piece::EW));
        l.cells.insert((5, 3), Cell::Track(Piece::NS));
        assert_eq!(l.pick_curve((5, 2), true), Piece::NE);
        assert_eq!(l.pick_curve((5, 2), false), Piece::NE);
        // A neighbour whose track does not point at us offers nothing:
        // NS to the west has no east exit, so the default (incoming W)
        // applies as if the cell were alone.
        l.cells.clear();
        l.cells.insert((4, 2), Cell::Track(Piece::NS));
        assert_eq!(l.pick_curve((5, 2), false), Piece::NE);
        // A turnout's main route counts; its diagonal branch does not.
        l.cells.clear();
        l.cells.insert((4, 2), Cell::Turnout { id: 1, kind: TKind::EwNe });
        assert!(l.neighbor_offers((5, 2), Dir::W));
        assert!(!l.neighbor_offers((4, 1), Dir::S)); // branch corner, no edge exit
    }

    #[test]
    fn curve_tool_picks_up_a_diverging_leg() {
        let mut l = Layout::new();
        l.cells.insert((8, 9), Cell::Turnout { id: 1, kind: TKind::EwNe });
        // The leg from the SW corner travels north-east: Curve Right
        // levels it off eastward, Curve Left climbs north.
        assert_eq!(l.pick_curve((9, 8), false), Piece::RampSwE);
        assert_eq!(l.pick_curve((9, 8), true), Piece::RampSwN);
        // With the siding straight already alongside, both hands snap to
        // the ramp joining leg and straight.
        l.cells.insert((10, 8), Cell::Track(Piece::EW));
        assert_eq!(l.pick_curve((9, 8), true), Piece::RampSwE);
        assert_eq!(l.pick_curve((9, 8), false), Piece::RampSwE);
        // Same off the end of a plain diagonal.
        let mut l2 = Layout::new();
        l2.cells.insert((5, 5), Cell::Track(Piece::DiagDown));
        assert_eq!(l2.pick_curve((6, 6), false), Piece::RampNwS);
        assert_eq!(l2.pick_curve((6, 6), true), Piece::RampNwE);
    }

    #[test]
    fn diagonal_tool_builds_a_siding_off_a_turnout() {
        let mut l = Layout::new();
        // Turnout on the main line, diverging leg toward the NE corner.
        l.cells.insert((8, 9), Cell::Turnout { id: 1, kind: TKind::EwNe });
        // First click above-right: nothing to level into yet, keep climbing.
        assert_eq!(l.pick_diagonal((9, 8)), Piece::DiagUp);
        // With the siding straight already laid, the same click levels off.
        l.cells.insert((10, 8), Cell::Track(Piece::EW));
        assert_eq!(l.pick_diagonal((9, 8)), Piece::RampSwE);
        l.cells.insert((9, 8), Cell::Track(Piece::RampSwE));
        // Closing end of the siding: turnout throwing NW, straight to the west.
        l.cells.insert((15, 9), Cell::Turnout { id: 2, kind: TKind::EwNw });
        l.cells.insert((13, 8), Cell::Track(Piece::EW));
        assert_eq!(l.pick_diagonal((14, 8)), Piece::RampSeW);
    }

    #[test]
    fn diagonals_chain_corner_to_corner() {
        let mut l = Layout::new();
        l.cells.insert((5, 5), Cell::Track(Piece::DiagUp));
        assert_eq!(l.pick_diagonal((6, 4)), Piece::DiagUp);
        assert_eq!(l.pick_diagonal((4, 6)), Piece::DiagUp);
        l.cells.insert((5, 5), Cell::Track(Piece::DiagDown));
        assert_eq!(l.pick_diagonal((6, 6)), Piece::DiagDown);
        // both ends present pins the orientation outright
        l.cells.insert((7, 7), Cell::Track(Piece::DiagDown));
        assert_eq!(l.pick_diagonal((6, 6)), Piece::DiagDown);
        // with no context at all the tool starts a rising diagonal
        assert_eq!(Layout::new().pick_diagonal((3, 3)), Piece::DiagUp);
    }

    #[test]
    fn ramp_geometry_touches_corner_and_edge() {
        // every ramp starts at its named corner and ends at an edge
        // midpoint matching its exit
        for (piece, corner) in [
            (Piece::RampSwE, Corner::SW),
            (Piece::RampSwN, Corner::SW),
            (Piece::RampNwE, Corner::NW),
            (Piece::RampNwS, Corner::NW),
            (Piece::RampNeW, Corner::NE),
            (Piece::RampNeS, Corner::NE),
            (Piece::RampSeW, Corner::SE),
            (Piece::RampSeN, Corner::SE),
        ] {
            assert_eq!(piece.segments()[0].0, corner.point(), "{piece:?}");
            assert_eq!(piece.corners(), [corner], "{piece:?}");
            assert_eq!(piece.exits().len(), 1, "{piece:?}");
        }
        assert_eq!(Piece::DiagUp.segments(), [((0.0, 1.0), (1.0, 0.0))]);
    }

    #[test]
    fn turnout_geometry_is_consistent() {
        for k in TKind::ALL {
            // the branch always starts at the cell centre
            assert_eq!(k.branch().0, (0.5, 0.5));
            // and ends on a corner
            let (x, y) = k.branch().1;
            assert!((x == 0.0 || x == 1.0) && (y == 0.0 || y == 1.0));
        }
    }
}
