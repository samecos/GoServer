use go_core::{Color, Move, Position, PositionError, Terminal};

fn play(p: &mut Position, point: Option<u16>) {
    p.play(Move {
        color: p.to_move(),
        point,
    })
    .unwrap();
}

#[test]
fn captures_suicide_and_turn_validation() {
    let mut p = Position::new(5, 7.5).unwrap();
    // White A5 eventually has no liberties after Black A4.
    for point in [Some(1), Some(0), Some(5)] {
        play(&mut p, point);
    }
    assert_eq!(p.board()[0], 0);
    assert_eq!(p.board()[1], 1);
    assert_eq!(p.board()[5], 1);
    let hash = p.input_hash();
    assert_eq!(
        p.play(Move {
            color: Color::White,
            point: Some(0)
        }),
        Err(PositionError::Suicide)
    );
    assert_eq!(p.input_hash(), hash);
    assert_eq!(
        p.play(Move {
            color: Color::Black,
            point: Some(2)
        }),
        Err(PositionError::WrongPlayer)
    );
    assert_eq!(
        p.play(Move {
            color: Color::White,
            point: Some(1)
        }),
        Err(PositionError::Occupied)
    );
    assert_eq!(
        p.play(Move {
            color: Color::White,
            point: Some(25)
        }),
        Err(PositionError::OutOfBounds)
    );
}

#[test]
fn simple_ko_lifts_after_threat_and_answer() {
    let mut p = Position::new(5, 7.5).unwrap();
    //  . B W . .
    //  B W . W .  Black C4 captures White B4, only liberty B4.
    //  . B W . .
    for point in [1, 2, 5, 6, 11, 12, 24, 8, 7] {
        play(&mut p, Some(point));
    }
    assert_eq!(p.ko_point(), Some(6));
    assert_eq!(
        p.play(Move {
            color: Color::White,
            point: Some(6)
        }),
        Err(PositionError::Ko)
    );
    play(&mut p, Some(20));
    play(&mut p, Some(22));
    play(&mut p, Some(6));
    assert_eq!(p.board()[7], 0);
    assert_eq!(p.board()[6], 2);
    assert_eq!(p.ko_point(), Some(7));
}

#[test]
fn pass_undo_terminal_and_halfpoint_komi() {
    assert_eq!(Position::new(19, 7.3).unwrap_err(), PositionError::Komi);
    let mut p = Position::new(5, 0.0).unwrap();
    play(&mut p, Some(0));
    play(&mut p, Some(24));
    let original = p.input_hash();
    play(&mut p, None);
    assert!(p.terminal().is_none());
    play(&mut p, None);
    assert_eq!(
        p.terminal(),
        Some(Terminal::Score {
            white_minus_black: 0.0
        })
    );
    assert_eq!(
        p.play(Move {
            color: p.to_move(),
            point: Some(4)
        }),
        Err(PositionError::GameOver)
    );
    p.undo().unwrap();
    p.undo().unwrap();
    assert_eq!(p.input_hash(), original);
    assert!(p.terminal().is_none());
}

#[test]
fn graph_transpositions_share_but_nn_history_does_not() {
    let mut a = Position::new(19, 7.5).unwrap();
    let mut b = a.clone();
    for p in [0, 360, 2] {
        play(&mut a, Some(p));
    }
    for p in [2, 360, 0] {
        play(&mut b, Some(p));
    }
    assert_eq!(a.board(), b.board());
    assert_eq!(a.graph_key(), b.graph_key());
    assert_ne!(a.input_hash(), b.input_hash());
    let mut c = a.clone();
    play(&mut c, None);
    assert_ne!(c.graph_key(), a.graph_key());
    let d = Position::replay(19, 6.5, a.moves()).unwrap();
    assert_ne!(d.graph_key(), a.graph_key());
}

#[test]
fn bounded_reversible_history_does_not_collapse_small_cycles() {
    let mut a = Position::new(3, 0.5).unwrap();
    let mut b = a.clone();
    for p in [0, 8, 2] {
        play(&mut a, Some(p));
    }
    for p in [2, 8, 0] {
        play(&mut b, Some(p));
    }
    assert_eq!(a.board(), b.board());
    assert_ne!(a.graph_key(), b.graph_key());
}

#[test]
fn score_simple_territory_and_dame() {
    let mut p = Position::new(5, 7.5).unwrap();
    play(&mut p, Some(0));
    assert_eq!(p.area_score(), -17.5);
    play(&mut p, Some(24));
    assert_eq!(p.area_score(), 7.5);
    let area = p.area_ownership();
    assert_eq!(area[0], 1);
    assert_eq!(area[24], 2);
    assert_eq!(area[12], 0);
}

#[test]
fn all_replayed_positions_have_same_legal_set_and_identity() {
    let mut p = Position::new(9, 7.5).unwrap();
    let mut seed = 42u64;
    for _ in 0..150 {
        if p.terminal().is_some() {
            break;
        }
        let legal = p.legal_moves();
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let m = legal[(seed as usize) % legal.len()];
        p.play(m).unwrap();
        let replay = Position::replay(9, 7.5, p.moves()).unwrap();
        assert_eq!(p.board(), replay.board());
        assert_eq!(p.graph_key(), replay.graph_key());
        assert_eq!(p.legal_moves(), replay.legal_moves());
    }
}

#[test]
fn triple_ko_third_situational_occurrence_is_no_result() {
    let mut p = Position::new(19, 7.5).unwrap();
    let mut black = Vec::new();
    let mut white = Vec::new();
    for (offset, reverse) in [(0, false), (6, true), (12, false)] {
        let b = [offset + 1, 19 + offset, 38 + offset + 1];
        let w = [
            offset + 2,
            19 + offset + 1,
            38 + offset + 2,
            19 + offset + 3,
        ];
        if reverse {
            black.extend(w);
            white.extend(b);
        } else {
            black.extend(b);
            white.extend(w);
        }
    }
    black.push(360);
    assert_eq!(black.len(), white.len());
    for (b, w) in black.into_iter().zip(white) {
        play(&mut p, Some(b));
        play(&mut p, Some(w));
    }
    let initial = p.board().to_vec();
    for point in [21, 27, 33, 20, 26, 32] {
        play(&mut p, Some(point));
    }
    assert_eq!(p.board(), initial);
    assert!(p.terminal().is_none());
    for point in [21, 27, 33, 20, 26, 32] {
        play(&mut p, Some(point));
    }
    assert_eq!(p.board(), initial);
    assert_eq!(p.terminal(), Some(Terminal::NoResult));
}
