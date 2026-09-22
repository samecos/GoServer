use super::*;

fn evaluation(area: usize) -> Evaluation {
    Evaluation {
        policy: vec![1.0; area + 1],
        white_win_prob: 0.5,
        white_loss_prob: 0.5,
        white_no_result_prob: 0.0,
        white_score_mean: 0.0,
        white_score_mean_sq: 1.0,
        white_lead: 0.0,
        has_shortterm_error: false,
        shortterm_winloss_error: -1.0,
        shortterm_score_error: -1.0,
        ownership: vec![0.0; area],
    }
}

fn check(s: &Search) {
    assert_eq!(
        s.graph_payload_bytes,
        s.nodes
            .iter()
            .map(|n| n.heap_charge(s.position.board().len()))
            .sum::<usize>()
    );
    assert!(s.memory_bytes() <= s.config.max_memory_bytes);
}

#[test]
fn fixed_budget_search_exceeds_old_full_board_charge_and_stops_safely() {
    let budget = 8 * 1024 * 1024;
    let mut s = Search::new(
        Position::new(19, 7.5).unwrap(),
        SearchConfig {
            max_memory_bytes: budget,
            max_nodes: 1_000_000_000,
            ..Default::default()
        },
    )
    .unwrap();
    for step in 0..10_000 {
        match s.next_evaluation().unwrap() {
            SearchStep::Evaluate(r) => {
                check(&s);
                s.complete(r.token, evaluation(361)).unwrap();
            }
            SearchStep::Advanced => {}
            SearchStep::MemoryLimited => break,
            other => panic!("unexpected {other:?}"),
        }
        check(&s);
        assert!(step < 9_999, "must still enforce the memory budget");
    }
    check(&s);
    assert!(s.nodes.len() > (budget / 38_920) * 3 / 2);
    assert_eq!(s.in_flight(), 0);
    let before = s.snapshot(1, 1);
    assert!(matches!(
        s.next_evaluation().unwrap(),
        SearchStep::MemoryLimited
    ));
    assert_eq!(s.snapshot(1, 1).root, before.root);
    let mut moved = s.position.clone();
    moved.play(before.candidates[0].mv).unwrap();
    s.set_root(moved).unwrap();
    s.reclaimer.drain();
    check(&s);
    assert!(s.memory_bytes() < before.memory_bytes);
    assert!(!s.graph_budget_exhausted());
    s.set_tuning(SearchTuning {
        playout_doubling_advantage: 0.5,
        wide_root_noise: 0.04,
        ..Default::default()
    })
    .unwrap();
    s.reclaimer.drain();
    check(&s);
    assert_eq!(s.nodes.len(), 1);
}

#[test]
fn completion_reservation_covers_ownership_and_releases_unused_slots() {
    let mut s = Search::new(Position::new(19, 7.5).unwrap(), SearchConfig::default()).unwrap();
    for i in 0..128 {
        let SearchStep::Evaluate(r) = s.next_evaluation().unwrap() else {
            continue;
        };
        let reserved = s.memory_bytes();
        let mut value = evaluation(361);
        if i % 2 == 0 {
            value.ownership.clear();
        }
        value.ownership.reserve(5000);
        s.complete(r.token, value).unwrap();
        check(&s);
        assert!(s.memory_bytes() <= reserved);
    }
}

#[test]
fn reverse_link_growth_is_reserved_for_shared_children_and_cycles() {
    let mut s = Search::new(Position::new(19, 7.5).unwrap(), SearchConfig::default()).unwrap();
    for _ in 0..128 {
        s.nodes.push(Node::new(&s.position));
    }
    for n in &mut s.nodes {
        n.state = State::Expanded;
        n.edges.push(Edge {
            point: None,
            prior: 1.0,
            child: None,
            visits: 0,
            in_flight: 0,
        });
    }
    s.recount_payload();
    for parent in 0..s.nodes.len() {
        let before = s.memory_bytes();
        let reserve = s.link_growth_charge(parent, 0, Some(0));
        s.attach_child(parent, 0, 0);
        check(&s);
        assert!(s.memory_bytes() <= before + reserve);
        assert_eq!(s.link_growth_charge(parent, 0, Some(0)), 0);
        let before = s.memory_bytes();
        s.attach_child(parent, 0, 0);
        assert_eq!(s.memory_bytes(), before);
    }
    s.collect_unreachable();
    s.reclaimer.drain();
    check(&s);
}

#[test]
fn retaining_an_over_budget_root_frees_capacities_and_can_initialize_again() {
    let mut s = Search::new(Position::new(19, 7.5).unwrap(), SearchConfig::default()).unwrap();
    for _ in 0..100 {
        if let SearchStep::Evaluate(r) = s.next_evaluation().unwrap() {
            s.complete(r.token, evaluation(361)).unwrap();
        }
    }
    s.config.max_memory_bytes = 64 * 1024;
    s.set_root(s.position.clone()).unwrap();
    s.reclaimer.drain();
    check(&s);
    assert_eq!(s.nodes.len(), 1);
    assert_eq!(s.nodes[0].edges.capacity(), 0);
    assert_eq!(s.nodes[0].parents.capacity(), 0);
    let SearchStep::Evaluate(r) = s.next_evaluation().unwrap() else {
        panic!("root must reinitialize")
    };
    s.complete(r.token, evaluation(361)).unwrap();
    check(&s);
    assert!(!s.snapshot(1, 1).candidates.is_empty());
}
