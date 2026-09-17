use crate::profiling::{Span, Stage};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use thiserror::Error;

pub(crate) type Key = [u8; 32];
pub(crate) const REP_BOUND: usize = 11;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_legality_matches_full_move_simulation_through_random_games() {
        let mut seed = 0x96208f735a123u64;
        for size in [3, 5, 9, 19] {
            for _ in 0..3 {
                let mut position = Position::new(size, 7.5).unwrap();
                for turn in 0..160 {
                    assert_eq!(position.current_ko_hash(), position.ko_hash());
                    let mut legal = Vec::new();
                    for point in (0..size as u16 * size as u16).map(Some).chain([None]) {
                        let simulated = position.board_after(point).is_ok();
                        assert_eq!(
                            position.legal_point(point),
                            simulated,
                            "size={size}, turn={turn}, point={point:?}, moves={:?}",
                            position.moves()
                        );
                        if simulated {
                            legal.push(point);
                        }
                    }
                    assert!(!position.legal_point(Some(u16::MAX)));
                    if position.terminal().is_some() {
                        break;
                    }
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    let point = legal[seed as usize % legal.len()];
                    position
                        .play(Move {
                            color: position.to_move(),
                            point,
                        })
                        .unwrap();
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Color {
    Black,
    White,
}
impl Color {
    pub fn opposite(self) -> Self {
        match self {
            Self::Black => Self::White,
            Self::White => Self::Black,
        }
    }
    pub fn white_sign(self) -> f64 {
        if self == Self::White {
            1.0
        } else {
            -1.0
        }
    }
    pub fn stone(self) -> u8 {
        if self == Self::Black {
            1
        } else {
            2
        }
    }
}

/// Top-left row-major index; `None` means pass. No setup stones/handicap in v1.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Move {
    pub color: Color,
    pub point: Option<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Terminal {
    Score { white_minus_black: f64 },
    NoResult,
}

#[derive(Debug, Error, PartialEq)]
pub enum PositionError {
    #[error("board size must be in 2..=19")]
    BoardSize,
    #[error("komi must be finite, a multiple of 0.5, and in -150..=150")]
    Komi,
    #[error("wrong player to move")]
    WrongPlayer,
    #[error("point is outside the board")]
    OutOfBounds,
    #[error("intersection is occupied")]
    Occupied,
    #[error("suicide is prohibited")]
    Suicide,
    #[error("immediate ko recapture is prohibited")]
    Ko,
    #[error("game has ended")]
    GameOver,
    #[error("there is no move to undo")]
    NoMove,
}

/// Strict alternating play from an empty board, using KataGo's `chinese` preset:
/// area, simple ko, no suicide, no tax/button, friendly pass enabled.
/// Board history uses situational hashes for threefold no-result, cleared on pass.
#[derive(Clone, Debug)]
pub struct Position {
    size: u8,
    komi: f64,
    board: Vec<u8>,
    next: Color,
    moves: Vec<Move>,
    ko: Option<u16>,
    passes: u32,
    terminal: Option<Terminal>,
    last_friendly_end: bool,
    post_terminal_play: bool,
    ko_history: Vec<Key>,
    before_black_pass: Vec<Key>,
    before_white_pass: Vec<Key>,
    graph_key: Key,
}

impl Position {
    pub fn new(size: u8, komi: f64) -> Result<Self, PositionError> {
        if !(2..=19).contains(&size) {
            return Err(PositionError::BoardSize);
        }
        if !komi.is_finite() || komi.abs() > 150.0 || (komi * 2.0).fract() != 0.0 {
            return Err(PositionError::Komi);
        }
        let mut p = Self {
            size,
            komi: if komi == 0.0 { 0.0 } else { komi },
            board: vec![0; size as usize * size as usize],
            next: Color::Black,
            moves: Vec::new(),
            ko: None,
            passes: 0,
            terminal: None,
            last_friendly_end: false,
            post_terminal_play: false,
            ko_history: Vec::new(),
            before_black_pass: Vec::new(),
            before_white_pass: Vec::new(),
            graph_key: [0; 32],
        };
        p.ko_history.push(p.ko_hash());
        p.graph_key = p.state_hash();
        Ok(p)
    }
    pub fn replay(size: u8, komi: f64, moves: &[Move]) -> Result<Self, PositionError> {
        let mut p = Self::new(size, komi)?;
        for m in moves {
            p.play(*m)?;
        }
        Ok(p)
    }
    pub fn board_size(&self) -> u8 {
        self.size
    }
    pub fn komi(&self) -> f64 {
        self.komi
    }
    /// 0 = empty, 1 = black, 2 = white.
    pub fn board(&self) -> &[u8] {
        &self.board
    }
    pub fn to_move(&self) -> Color {
        self.next
    }
    pub fn moves(&self) -> &[Move] {
        &self.moves
    }
    pub fn terminal(&self) -> Option<Terminal> {
        self.terminal
    }
    pub fn has_post_terminal_play(&self) -> bool {
        self.post_terminal_play
    }
    pub(crate) fn friendly_pass_would_force_non_terminal(&self) -> bool {
        self.passes == 1 && !self.pass_history().contains(&self.current_ko_hash())
    }
    pub fn ko_point(&self) -> Option<u16> {
        self.ko
    }
    pub fn graph_key(&self) -> Key {
        self.graph_key
    }
    /// Exact replay identity, intentionally distinct from transposition identity.
    pub fn input_hash(&self) -> Key {
        let _timer = Span::new(Stage::InputHash);
        let mut h = Sha256::new();
        h.update(b"go-input-chinese-v1");
        h.update([self.size]);
        h.update(self.komi.to_le_bytes());
        for m in &self.moves {
            h.update([m.color.stone()]);
            h.update(m.point.unwrap_or(u16::MAX).to_le_bytes());
        }
        h.finalize().into()
    }
    pub fn undo(&mut self) -> Result<(), PositionError> {
        if self.moves.is_empty() {
            return Err(PositionError::NoMove);
        }
        *self = Self::replay(self.size, self.komi, &self.moves[..self.moves.len() - 1])?;
        Ok(())
    }
    pub fn is_legal(&self, point: Option<u16>) -> bool {
        if self.terminal.is_some() {
            return false;
        }
        self.legal_point(point)
    }
    pub fn legal_moves(&self) -> Vec<Move> {
        if self.terminal.is_some() {
            return Vec::new();
        }
        self.legal_search_moves()
    }
    /// Evaluator legal policy domain for a search-only friendly-pass node.
    /// Does not authorize continuing an externally completed game.
    pub fn legal_search_moves(&self) -> Vec<Move> {
        let _timer = Span::new(Stage::LegalMoves);
        (0..self.board.len())
            .map(|p| Some(p as u16))
            .chain(std::iter::once(None))
            .filter(|p| self.legal_point(*p))
            .map(|point| Move {
                color: self.next,
                point,
            })
            .collect()
    }
    pub fn play(&mut self, m: Move) -> Result<(), PositionError> {
        if self.terminal.is_some() {
            return Err(PositionError::GameOver);
        }
        if m.color != self.next {
            return Err(PositionError::WrongPlayer);
        }
        let (board, ko) = self.board_after(m.point)?;
        let before = self.current_ko_hash();
        let can_force_friendly = m.point.is_none() && self.friendly_pass_would_force_non_terminal();
        let spight_end = m.point.is_none() && self.pass_history().contains(&before);
        self.board = board;
        self.ko = ko;
        if m.point.is_none() {
            self.passes += 1;
            self.ko_history.clear();
            if m.color == Color::Black {
                self.before_black_pass.push(before);
            } else {
                self.before_white_pass.push(before);
            }
        } else {
            self.passes = 0;
        }
        self.next = self.next.opposite();
        self.moves.push(m);
        let hash = self.ko_hash();
        self.ko_history.push(hash);
        if self.passes >= 2 || spight_end {
            self.terminal = Some(Terminal::Score {
                white_minus_black: self.area_score(),
            });
        }
        if m.point.is_some() && self.ko_history.iter().filter(|k| **k == hash).count() >= 3 {
            self.terminal = Some(Terminal::NoResult);
        }
        self.last_friendly_end = can_force_friendly && self.terminal.is_some();
        let state = self.state_hash();
        // KataGo game/graphhash.cpp resets history when repeating this move
        // provably requires more than repBound plays; otherwise history is chained.
        if m.point
            .is_some_and(|p| self.simple_repetition_bound_gt(p as usize, REP_BOUND))
        {
            self.graph_key = state;
        } else {
            let mut h = Sha256::new();
            h.update(b"go-graph-history-v1");
            h.update(self.graph_key);
            h.update(state);
            self.graph_key = h.finalize().into();
        }
        Ok(())
    }
    pub(crate) fn play_search(&mut self, m: Move) -> Result<(), PositionError> {
        if self.terminal.is_some() {
            if !self.last_friendly_end {
                return Err(PositionError::GameOver);
            }
            // This is the sole permitted continuation of a finished search
            // history. Ordinary play() remains strict and cannot reach this path.
            let mut continued = self.clone();
            continued.terminal = None;
            continued.play(m)?;
            continued.post_terminal_play = true;
            *self = continued;
            Ok(())
        } else {
            self.play(m)
        }
    }
    fn pass_history(&self) -> &[Key] {
        if self.next == Color::Black {
            &self.before_black_pass
        } else {
            &self.before_white_pass
        }
    }
    fn ko_hash(&self) -> Key {
        let mut h = Sha256::new();
        h.update([self.size, self.next.stone()]);
        h.update(&self.board);
        h.finalize().into()
    }
    fn current_ko_hash(&self) -> Key {
        // new/play append the current situational hash, including after a pass
        // clears the earlier history. No additional cache or invalidation state.
        *self.ko_history.last().expect("current situational hash")
    }
    fn state_hash(&self) -> Key {
        let mut h = Sha256::new();
        h.update(b"go-chinese-simple-area-v1");
        h.update(self.current_ko_hash());
        h.update(self.komi.to_le_bytes());
        h.update(self.ko.unwrap_or(u16::MAX).to_le_bytes());
        h.update(self.passes.to_le_bytes());
        h.update([
            u8::from(self.passes >= 1 || self.pass_history().contains(&self.current_ko_hash())),
            u8::from(self.terminal.is_some()),
            u8::from(self.terminal == Some(Terminal::NoResult)),
        ]);
        h.finalize().into()
    }
    fn neighbors(&self, p: usize) -> impl Iterator<Item = usize> {
        let n = self.size as usize;
        [
            if p >= n { Some(p - n) } else { None },
            if p + n < n * n { Some(p + n) } else { None },
            if !p.is_multiple_of(n) {
                Some(p - 1)
            } else {
                None
            },
            if p % n + 1 < n { Some(p + 1) } else { None },
        ]
        .into_iter()
        .flatten()
    }
    fn group(&self, board: &[u8], p: usize) -> (Vec<usize>, Vec<usize>) {
        let mut stones = vec![p];
        let mut seen = vec![false; board.len()];
        seen[p] = true;
        let mut liberties = Vec::new();
        let mut i = 0;
        while i < stones.len() {
            let q = stones[i];
            i += 1;
            for a in self.neighbors(q) {
                if seen[a] {
                    continue;
                }
                if board[a] == 0 {
                    seen[a] = true;
                    liberties.push(a);
                } else if board[a] == board[p] {
                    seen[a] = true;
                    stones.push(a);
                }
            }
        }
        (stones, liberties)
    }
    // Under the supported simple-ko/no-suicide rules, a move is legal if it
    // has a direct liberty, connects to a friendly group with another liberty,
    // or captures an adjacent enemy group. No successor board is needed here.
    // Full play still uses board_after to apply captures and determine the ko.
    fn legal_point(&self, point: Option<u16>) -> bool {
        let Some(point) = point else {
            return true;
        };
        let p = point as usize;
        if p >= self.board.len() || self.board[p] != 0 || self.ko == Some(point) {
            return false;
        }
        if self.neighbors(p).any(|a| self.board[a] == 0) {
            return true;
        }
        self.neighbors(p).any(|a| {
            let (_, liberties) = self.group(&self.board, a);
            if self.board[a] == self.next.stone() {
                liberties.len() > 1
            } else {
                liberties.len() == 1
            }
        })
    }

    fn board_after(&self, point: Option<u16>) -> Result<(Vec<u8>, Option<u16>), PositionError> {
        let Some(point) = point else {
            return Ok((self.board.clone(), None));
        };
        let p = point as usize;
        if p >= self.board.len() {
            return Err(PositionError::OutOfBounds);
        }
        if self.board[p] != 0 {
            return Err(PositionError::Occupied);
        }
        if self.ko == Some(point) {
            return Err(PositionError::Ko);
        }
        let mut board = self.board.clone();
        board[p] = self.next.stone();
        let mut captured = Vec::new();
        for a in self.neighbors(p) {
            if board[a] == self.next.opposite().stone() {
                let (g, libs) = self.group(&board, a);
                if libs.is_empty() {
                    for q in g {
                        board[q] = 0;
                        captured.push(q);
                    }
                }
            }
        }
        let (g, libs) = self.group(&board, p);
        if libs.is_empty() {
            return Err(PositionError::Suicide);
        }
        let ko = if captured.len() == 1 && g.len() == 1 && libs.len() == 1 {
            Some(captured[0] as u16)
        } else {
            None
        };
        Ok((board, ko))
    }
    fn simple_repetition_bound_gt(&self, p: usize, bound: usize) -> bool {
        let (g, libs) = self.group(&self.board, p);
        let mut count = g.len();
        if count + libs.len() > bound {
            return true;
        }
        let mut seen = vec![false; self.board.len()];
        let mut stack = libs;
        while let Some(q) = stack.pop() {
            if seen[q] {
                continue;
            }
            seen[q] = true;
            count += 1;
            if count > bound {
                return true;
            }
            for a in self.neighbors(q) {
                if self.board[a] == 0 && !seen[a] {
                    stack.push(a);
                }
            }
        }
        false
    }
    /// KataGo's area scorer uses Benson pass-alive regions, including provably
    /// dead opposing stones, before adding surviving stones/unsafe territories.
    pub fn area_ownership(&self) -> Vec<u8> {
        let n = self.board.len();
        let mut result = vec![0; n];
        for color in [1u8, 2u8] {
            let mut group_of = vec![usize::MAX; n];
            let mut groups = Vec::<Vec<usize>>::new();
            for p in 0..n {
                if self.board[p] == color && group_of[p] == usize::MAX {
                    let (g, _) = self.group(&self.board, p);
                    let id = groups.len();
                    for q in &g {
                        group_of[*q] = id;
                    }
                    groups.push(g);
                }
            }
            struct Region {
                points: Vec<usize>,
                borders: HashSet<usize>,
                vital: HashSet<usize>,
                internal: usize,
                opp: bool,
            }
            let mut regions = Vec::<Region>::new();
            let mut seen = vec![false; n];
            for p in 0..n {
                if self.board[p] != 0 || seen[p] {
                    continue;
                }
                let mut points = vec![p];
                seen[p] = true;
                let mut i = 0;
                let mut borders = HashSet::new();
                let mut vital: Option<HashSet<usize>> = None;
                let mut internal = 0;
                let mut opp = false;
                while i < points.len() {
                    let q = points[i];
                    i += 1;
                    let adjacent: HashSet<usize> = self
                        .neighbors(q)
                        .filter(|a| self.board[*a] == color)
                        .map(|a| group_of[a])
                        .collect();
                    if adjacent.is_empty() {
                        internal += 1;
                    }
                    borders.extend(&adjacent);
                    opp |= self.board[q] != 0;
                    // Fixed BoardHistoryModes(alwaysPassAliveSuicide=false).
                    if self.board[q] == 0 {
                        if let Some(v) = &mut vital {
                            v.retain(|x| adjacent.contains(x));
                        } else {
                            vital = Some(adjacent);
                        }
                    }
                    for a in self.neighbors(q) {
                        if self.board[a] != color && !seen[a] {
                            seen[a] = true;
                            points.push(a);
                        }
                    }
                }
                regions.push(Region {
                    points,
                    borders,
                    vital: vital.unwrap_or_default(),
                    internal,
                    opp,
                });
            }
            let mut alive = vec![true; groups.len()];
            loop {
                let mut kill = Vec::new();
                for (g, is_alive) in alive.iter().enumerate() {
                    if *is_alive
                        && regions
                            .iter()
                            .filter(|r| r.vital.contains(&g) && r.borders.iter().all(|x| alive[*x]))
                            .count()
                            < 2
                    {
                        kill.push(g);
                    }
                }
                if kill.is_empty() {
                    break;
                }
                for g in kill {
                    alive[g] = false;
                }
            }
            for (g, stones) in groups.iter().enumerate() {
                if alive[g] {
                    for p in stones {
                        result[*p] = color;
                    }
                }
            }
            for r in regions {
                let safe = !groups.is_empty() && r.borders.iter().all(|g| alive[*g]);
                if safe && (r.internal <= 1 || !r.opp) {
                    for p in r.points {
                        result[p] = color;
                    }
                } else if !r.opp && !groups.is_empty() {
                    for p in r.points {
                        if result[p] == 0 {
                            result[p] = color;
                        }
                    }
                }
            }
        }
        for (p, stone) in self.board.iter().enumerate() {
            if result[p] == 0 {
                result[p] = *stone;
            }
        }
        result
    }
    pub fn area_score(&self) -> f64 {
        self.komi
            + self
                .area_ownership()
                .iter()
                .map(|x| match x {
                    1 => -1.0,
                    2 => 1.0,
                    _ => 0.0,
                })
                .sum::<f64>()
    }
}
