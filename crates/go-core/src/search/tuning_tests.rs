use super::*;

#[test]
fn played_moves_retain_evaluated_subtrees_for_compatible_pda_and_wide_root() {
    for (pda, reference, wide) in [
        (0.0, PdaPlayer::Root, 0.0),
        (0.0, PdaPlayer::Root, 0.04),
        (0.5, PdaPlayer::Black, 0.0),
        (0.5, PdaPlayer::White, 0.0),
        (0.5, PdaPlayer::Black, 0.04),
        (0.5, PdaPlayer::White, 0.04),
        (0.5, PdaPlayer::Root, 0.04),
    ] {
        let mut s = Search::new(
            Position::new(19, 7.5).unwrap(),
            SearchConfig {
                tuning: SearchTuning {
                    playout_doubling_advantage: pda,
                    playout_doubling_advantage_pla: reference,
                    wide_root_noise: wide,
                },
                ..Default::default()
            },
        )
        .unwrap();
        for turn in 0..3 {
            let target = s.evaluations_completed + 80;
            while s.evaluations_completed < target {
                match s.next_evaluation().unwrap() {
                    SearchStep::Evaluate(r) => {
                        let mut policy = vec![0.0; 362];
                        let mv = r.position.legal_search_moves()[0];
                        policy[mv.point.map_or(361, usize::from)] = 1.0;
                        s.complete(
                            r.token,
                            Evaluation {
                                policy,
                                white_win_prob: 0.5,
                                white_loss_prob: 0.5,
                                white_no_result_prob: 0.0,
                                white_score_mean: 0.0,
                                white_score_mean_sq: 1.0,
                                white_lead: 0.0,
                                has_shortterm_error: false,
                                shortterm_winloss_error: -1.0,
                                shortterm_score_error: -1.0,
                                ownership: vec![0.0; 361],
                            },
                        )
                        .unwrap();
                    }
                    SearchStep::Advanced => {}
                    other => panic!("unexpected {other:?}"),
                }
            }
            let edge = s.nodes[s.root]
                .connected_edges()
                .max_by_key(|e| s.nodes[e.child.unwrap()].stats.visits)
                .unwrap();
            let expected = s.nodes[edge.child.unwrap()].stats;
            assert!(expected.visits > 1);
            let mut moves = s.position.moves().to_vec();
            moves.push(Move {
                color: s.position.to_move(),
                point: edge.point,
            });
            // Match the frontend play/seek endpoint's reconstruction, not just
            // set_root(request.position), which could hide a replay hash bug.
            let next = Position::replay(19, s.position.komi(), &moves).unwrap();
            s.set_root(next).unwrap();
            let after = s.snapshot(1, 1);
            if pda != 0.0 && reference == PdaPlayer::Root {
                assert_eq!(after.root.visits, 0);
                assert_eq!(after.root_change.as_ref().unwrap().retained_nodes, 0);
                assert_eq!(
                    after.root_change.as_ref().unwrap().reuse_reason,
                    "pda_reference_changed"
                );
            } else {
                assert_eq!(
                    after.root, expected,
                    "PDA {pda} {reference:?}, wide {wide}, turn {turn}"
                );
                assert!(after.root_change.as_ref().unwrap().retained_nodes > 1);
                assert_eq!(after.root_change.as_ref().unwrap().reuse_reason, "reused");
                assert_eq!(
                    after.root_change.as_ref().unwrap().retained_visits,
                    expected.visits
                );
            }
            println!("reuse pda={pda} reference={reference:?} wide={wide} turn={turn}: before_child={} after_root={} retained={}",
                expected.visits, after.root.visits, after.root_change.unwrap().retained_nodes);
        }
    }
}

#[test]
fn wide_noise_only_changes_root_selection_not_values_priors_or_pv() {
    for color in [Color::Black, Color::White] {
        let mut s = Search::new(Position::new(19, 7.5).unwrap(), SearchConfig::default()).unwrap();
        for _ in 0..2 {
            s.nodes.push(Node::new(&s.position));
        }
        for n in &mut s.nodes {
            n.color = color;
            n.stats.utility = 0.2;
            n.edges = (0..32)
                .map(|i| Edge {
                    point: Some(i),
                    prior: (i + 1) as f64 / 528.0,
                    child: None,
                    visits: 0,
                    in_flight: 0,
                })
                .collect();
        }
        s.nodes[2].stats.weight_sum = 7.0;
        s.nodes[2].stats.visits = 7;
        s.nodes[2].in_flight = 2;
        for id in [0, 1] {
            s.nodes[id].set_child(2, 2);
            s.nodes[id].edges[2].visits = 3;
        }
        let scores = |order: EdgeOrder| order.remaining.iter().map(|e| e.score).collect::<Vec<_>>();
        let before = scores(s.ordered_edges(0));
        let deep_before = scores(s.ordered_edges(1));
        let snapshot_before = serde_json::to_value(s.snapshot(32, 4)).unwrap();
        s.config.tuning.wide_root_noise = 0.2;
        let after = scores(s.ordered_edges(0));
        assert!(after.iter().zip(&before).all(|(a, b)| a > b));
        assert_eq!(scores(s.ordered_edges(1)), deep_before);
        assert_ne!(
            scores(s.ordered_edges(0)),
            after,
            "sample fresh exploration bonuses"
        );
        assert_eq!(
            serde_json::to_value(s.snapshot(32, 4)).unwrap(),
            snapshot_before
        );
        s.config.tuning.wide_root_noise = 0.0;
        assert_eq!(scores(s.ordered_edges(0)), before);
    }
}

#[test]
fn wide_root_scores_use_flattened_prior_and_bonus_after_virtual_loss() {
    let mut s = Search::new(Position::new(19, 7.5).unwrap(), SearchConfig::default()).unwrap();
    let mut child = Node::new(&s.position);
    child.stats.visits = 10;
    child.stats.weight_sum = 10.0;
    child.stats.utility = 0.3;
    child.in_flight = 2;
    s.nodes.push(child);
    s.nodes[0].edges.push(Edge {
        point: Some(0),
        prior: 0.04,
        child: Some(1),
        visits: 5,
        in_flight: 2,
    });
    s.config.tuning.wide_root_noise = 0.1;
    let weight = 5.0;
    let virtual_weight = 6.0;
    let utility = 0.3 + (1.3 - 0.3) * virtual_weight / (virtual_weight + weight);
    let scale = (weight + 0.01_f64).sqrt();
    let expected_base =
        -utility + scale * 0.04_f64.powf(1.0 / 1.4) / (1.0 + weight + virtual_weight);
    let mut found_zero_bonus = false;
    for _ in 0..50 {
        let score = s.ordered_edges(0).remaining[0].score;
        assert!(score >= expected_base - 1e-14);
        if (score - expected_base).abs() < 1e-14 {
            found_zero_bonus = true;
        }
    }
    assert!(found_zero_bonus, "bonus should be absent half the time");
}
