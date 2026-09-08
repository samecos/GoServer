use go_core::*;

fn evaluation(p: &Position) -> Evaluation {
    let area = p.board().len();
    let legal = p.legal_search_moves();
    let mut policy = vec![-1.0; area + 1];
    for m in &legal {
        policy[m.point.map_or(area, usize::from)] = 1.0 / legal.len() as f64;
    }
    Evaluation {
        policy,
        white_win_prob: 0.6,
        white_loss_prob: 0.4,
        white_no_result_prob: 0.0,
        white_score_mean: 2.0,
        white_score_mean_sq: 13.0,
        white_lead: 2.0,
        has_shortterm_error: true,
        shortterm_winloss_error: 0.1,
        shortterm_score_error: 1.0,
        ownership: vec![0.0; area],
    }
}
fn task(s: &mut Search) -> EvaluationRequest {
    for _ in 0..10000 {
        match s.next_evaluation().unwrap() {
            SearchStep::Evaluate(r) => return r,
            SearchStep::Advanced => {}
            other => panic!("expected work, got {other:?}"),
        }
    }
    panic!("no evaluation");
}
fn search() -> Search {
    Search::new(Position::new(9, 7.5).unwrap(), SearchConfig::default()).unwrap()
}

#[test]
fn single_initializer_duplicate_late_and_retry() {
    let mut s = search();
    let r = task(&mut s);
    assert!(matches!(s.next_evaluation().unwrap(), SearchStep::Waiting));
    let e = evaluation(&r.position);
    assert_eq!(s.fail(r.token), Completion::Applied);
    let retry = task(&mut s);
    assert_ne!(r.token, retry.token);
    assert_eq!(s.complete(r.token, e.clone()).unwrap(), Completion::Stale);
    assert_eq!(s.root_visits(), 0);
    assert_eq!(
        s.complete(retry.token, e.clone()).unwrap(),
        Completion::Applied
    );
    assert_eq!(s.root_visits(), 1);
    let snapshot = s.snapshot(10, 10);
    assert_eq!(s.complete(retry.token, e).unwrap(), Completion::Stale);
    assert_eq!(snapshot.version, s.snapshot(10, 10).version);
    assert_eq!(s.in_flight(), 0);
}

#[test]
fn many_async_paths_no_visits_until_commit_and_balanced_cancel() {
    let mut s = search();
    let root = task(&mut s);
    s.complete(root.token, evaluation(&root.position)).unwrap();
    let mut tasks = Vec::new();
    for _ in 0..16 {
        tasks.push(task(&mut s));
    }
    assert_eq!(s.root_visits(), 1);
    assert_eq!(s.in_flight(), 16);
    let mut unique = std::collections::HashSet::new();
    for r in &tasks {
        assert!(unique.insert(r.position.graph_key()));
    }
    for r in tasks.iter().rev().take(8) {
        s.complete(r.token, evaluation(&r.position)).unwrap();
    }
    assert_eq!(s.root_visits(), 9);
    assert_eq!(
        s.snapshot(100, 1)
            .candidates
            .iter()
            .map(|c| c.visits)
            .sum::<u64>(),
        8
    );
    let canceled = s.cancel_all();
    assert_eq!(canceled.len(), 8);
    assert_eq!(s.in_flight(), 0);
    assert!(s
        .snapshot(100, 1)
        .candidates
        .iter()
        .all(|c| c.in_flight == 0));
    for r in &tasks {
        assert_eq!(
            s.complete(r.token, evaluation(&r.position)).unwrap(),
            Completion::Stale
        );
    }
    assert_eq!(s.root_visits(), 9);
}

#[test]
fn invalid_outputs_do_not_release_or_corrupt_task() {
    let mut s = search();
    let r = task(&mut s);
    let mut e = evaluation(&r.position);
    e.white_score_mean = f64::NAN;
    assert!(s.complete(r.token, e).is_err());
    assert_eq!(s.in_flight(), 1);
    assert_eq!(s.root_visits(), 0);
    let mut e = evaluation(&r.position);
    e.policy.fill(0.0);
    assert!(s.complete(r.token, e).is_err());
    s.complete(r.token, evaluation(&r.position)).unwrap();
    assert_eq!(s.root_visits(), 1);
}

#[test]
fn root_change_invalidates_tasks_reclaims_and_reuses_child() {
    let mut s = search();
    let root = task(&mut s);
    s.complete(root.token, evaluation(&root.position)).unwrap();
    let child = task(&mut s);
    s.complete(child.token, evaluation(&child.position))
        .unwrap();
    let outstanding = task(&mut s);
    let old = s.snapshot(100, 5);
    let tokens = s.set_root(child.position.clone()).unwrap();
    assert_eq!(tokens, vec![outstanding.token]);
    assert!(s.generation() > old.generation);
    assert_eq!(s.root_visits(), 1);
    assert!(s.snapshot(100, 5).nodes < old.nodes);
    assert_eq!(s.in_flight(), 0);
    assert_eq!(
        s.complete(outstanding.token, evaluation(&outstanding.position))
            .unwrap(),
        Completion::Stale
    );
    let r = task(&mut s);
    assert!(r.position.moves().starts_with(child.position.moves()));
}

#[test]
fn memory_and_inflight_budgets_apply_backpressure() {
    let mut s = Search::new(
        Position::new(9, 7.5).unwrap(),
        SearchConfig {
            max_nodes: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let r = task(&mut s);
    s.complete(r.token, evaluation(&r.position)).unwrap();
    assert!(matches!(
        s.next_evaluation().unwrap(),
        SearchStep::MemoryLimited
    ));
    assert_eq!(s.root_visits(), 1);
    let mut s = Search::new(
        Position::new(9, 7.5).unwrap(),
        SearchConfig {
            max_in_flight: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let _ = task(&mut s);
    assert!(matches!(s.next_evaluation().unwrap(), SearchStep::Waiting));
    assert!(Search::new(
        Position::new(19, 7.5).unwrap(),
        SearchConfig {
            max_memory_bytes: 1,
            ..Default::default()
        }
    )
    .is_err());
}

#[test]
fn terminal_never_requests_network_eval_and_tie_moments_are_gridded() {
    let mut p = Position::new(5, 0.0).unwrap();
    p.play(Move {
        color: Color::Black,
        point: None,
    })
    .unwrap();
    p.play(Move {
        color: Color::White,
        point: None,
    })
    .unwrap();
    let mut s = Search::new(p, SearchConfig::default()).unwrap();
    assert!(matches!(s.next_evaluation().unwrap(), SearchStep::Terminal));
    let snap = s.snapshot(10, 5);
    assert_eq!(snap.root.visits, 1);
    assert_eq!(snap.root.white_win_prob(), 0.5);
    assert_eq!(snap.root.score_mean_sq, 0.25);
    assert_eq!(snap.evaluations_completed, 0);
    assert!(snap.candidates.is_empty());
}

#[test]
fn deterministic_fixed_eval_search_preserves_moment_bounds() {
    fn run() -> SearchSnapshot {
        let mut s = search();
        for _ in 0..80 {
            let r = task(&mut s);
            s.complete(r.token, evaluation(&r.position)).unwrap();
        }
        s.snapshot(100, 12)
    }
    let a = run();
    let b = run();
    assert_eq!(a.root, b.root);
    assert_eq!(a.evaluations_completed, 80);
    assert_eq!(
        a.root.visits,
        1 + a.candidates.iter().map(|c| c.visits).sum::<u64>()
    );
    assert!(a.root.weight_sum > 0.0);
    assert!(a.root.weight_sq_sum > 0.0);
    assert!(a.root.utility_sq + 1e-10 >= a.root.utility.powi(2));
    assert_eq!(
        a.candidates.iter().map(|c| c.mv).collect::<Vec<_>>(),
        b.candidates.iter().map(|c| c.mv).collect::<Vec<_>>()
    );
}

#[test]
fn real_go_transposition_has_one_initializer_and_catches_up_other_edge() {
    fn focused(p: &Position) -> Evaluation {
        let mut e = evaluation(p);
        e.policy
            .iter_mut()
            .filter(|v| **v >= 0.0)
            .for_each(|v| *v = 0.0);
        let choices: Vec<usize> = if p.moves().is_empty() {
            vec![0, 2]
        } else if p.to_move() == Color::White {
            vec![360]
        } else if p.board()[0] == 0 {
            vec![0]
        } else {
            vec![2]
        };
        for point in &choices {
            e.policy[*point] = 1.0 / choices.len() as f64;
        }
        e
    }
    let mut s = Search::new(Position::new(19, 7.5).unwrap(), SearchConfig::default()).unwrap();
    let root = task(&mut s);
    s.complete(root.token, focused(&root.position)).unwrap();
    let a = task(&mut s);
    let b = task(&mut s);
    assert_eq!(a.position.moves().len(), 1);
    assert_eq!(b.position.moves().len(), 1);
    s.complete(a.token, focused(&a.position)).unwrap();
    s.complete(b.token, focused(&b.position)).unwrap();
    let a2 = task(&mut s);
    let b2 = task(&mut s);
    assert_eq!(a2.position.moves().len(), 2);
    assert_eq!(b2.position.moves().len(), 2);
    s.complete(a2.token, focused(&a2.position)).unwrap();
    s.complete(b2.token, focused(&b2.position)).unwrap();
    let shared = task(&mut s);
    assert_eq!(shared.position.moves().len(), 3);
    let another = task(&mut s);
    assert_ne!(shared.position.graph_key(), another.position.graph_key());
    assert!(s.snapshot(10, 5).transposition_hits > 0);
    let e = evaluation(&shared.position);
    s.complete(shared.token, e).unwrap();
    s.fail(another.token);
    for _ in 0..20 {
        if s.snapshot(10, 5).catch_up_visits > 0 {
            break;
        }
        match s.next_evaluation().unwrap() {
            SearchStep::Evaluate(r) => {
                s.complete(r.token, evaluation(&r.position)).unwrap();
            }
            SearchStep::Advanced => {}
            other => panic!("{other:?}"),
        }
    }
    assert!(s.snapshot(10, 5).catch_up_visits > 0);
    s.cancel_all();
    assert_eq!(s.in_flight(), 0);
}

#[test]
fn friendly_second_pass_is_evaluated_and_only_search_can_resume() {
    let mut p = Position::new(5, 7.5).unwrap();
    p.play(Move {
        color: Color::Black,
        point: None,
    })
    .unwrap();
    let mut s = Search::new(p, SearchConfig::default()).unwrap();
    let root = task(&mut s);
    let mut e = evaluation(&root.position);
    e.policy.fill(0.0);
    e.policy[25] = 1.0;
    s.complete(root.token, e).unwrap();
    let friendly = task(&mut s);
    assert_eq!(friendly.position.moves().len(), 2);
    assert!(friendly.position.terminal().is_some());
    assert!(friendly.force_non_terminal);
    assert!(!friendly.allow_terminal_search_history);
    let mut external = friendly.position.clone();
    assert!(external
        .play(Move {
            color: Color::Black,
            point: Some(0)
        })
        .is_err());
    let mut e = evaluation(&friendly.position);
    e.policy.fill(0.0);
    e.policy[0] = 1.0;
    s.complete(friendly.token, e).unwrap();
    let continued = task(&mut s);
    assert_eq!(continued.position.moves().len(), 3);
    assert_eq!(continued.position.board()[0], 1);
    assert!(continued.allow_terminal_search_history);
    assert!(!continued.force_non_terminal);
    assert!(continued.position.terminal().is_none());
    // Re-rooting an actually completed external game must not reuse the NN-only
    // forced node as if it were the final rule score.
    let final_game = Position::replay(5, 7.5, friendly.position.moves()).unwrap();
    s.set_root(final_game).unwrap();
    assert_eq!(s.root_visits(), 0);
    assert!(matches!(s.next_evaluation().unwrap(), SearchStep::Terminal));
    assert_eq!(s.snapshot(0, 0).root.white_win_prob(), 1.0);
}

#[test]
fn third_pass_is_true_terminal_not_another_forced_nn_node() {
    let mut p = Position::new(5, 7.5).unwrap();
    p.play(Move {
        color: Color::Black,
        point: None,
    })
    .unwrap();
    let mut s = Search::new(p, SearchConfig::default()).unwrap();
    for _ in 0..2 {
        let r = task(&mut s);
        let mut e = evaluation(&r.position);
        e.policy.fill(0.0);
        e.policy[25] = 1.0;
        s.complete(r.token, e).unwrap();
    }
    assert!(matches!(s.next_evaluation().unwrap(), SearchStep::Advanced));
    assert_eq!(s.snapshot(1, 3).evaluations_completed, 2);
    assert_eq!(s.in_flight(), 0);
}

#[test]
fn older_models_keep_error_sentinels_and_fall_back_to_unit_weight() {
    for use_uncertainty in [false, true] {
        let mut s = Search::new(
            Position::new(9, 7.5).unwrap(),
            SearchConfig {
                use_uncertainty,
                ..Default::default()
            },
        )
        .unwrap();
        let r = task(&mut s);
        let mut e = evaluation(&r.position);
        e.has_shortterm_error = false;
        e.shortterm_winloss_error = -1.0;
        e.shortterm_score_error = -1.0;
        s.complete(r.token, e).unwrap();
        assert_eq!(s.snapshot(0, 0).root.weight_sum, 1.0);
        let r = task(&mut s);
        let mut e = evaluation(&r.position);
        assert!(s.complete(r.token, e.clone()).is_err());
        e.has_shortterm_error = false;
        e.shortterm_winloss_error = -1.0;
        e.shortterm_score_error = -1.0;
        s.complete(r.token, e).unwrap();
        assert_eq!(s.snapshot(0, 0).root.weight_sum, 2.0);
    }
}

#[test]
fn real_uncertainty_uses_error_values_and_rejects_sentinels() {
    let mut s = Search::new(
        Position::new(9, 7.5).unwrap(),
        SearchConfig {
            use_uncertainty: true,
            static_score_utility_factor: 0.0,
            ..Default::default()
        },
    )
    .unwrap();
    let r = task(&mut s);
    let mut e = evaluation(&r.position);
    e.shortterm_winloss_error = -1.0;
    assert!(s.complete(r.token, e.clone()).is_err());
    e.shortterm_winloss_error = 0.1;
    s.complete(r.token, e).unwrap();
    assert!((s.snapshot(0, 0).root.weight_sum - 1.6).abs() < 1e-12);
}
