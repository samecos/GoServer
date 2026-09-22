use super::*;

// Frozen pre-change collector, used only as a semantic/performance reference.
fn baseline(s: &mut Search) {
    let mut reachable = HashSet::new();
    let mut stack = vec![s.root];
    while let Some(n) = stack.pop() {
        if reachable.insert(n) {
            stack.extend(s.nodes[n].edges.iter().filter_map(|e| e.child));
        }
    }
    let mut remap = HashMap::new();
    let mut nodes = Vec::with_capacity(reachable.len());
    for (old, n) in std::mem::take(&mut s.nodes).into_iter().enumerate() {
        if reachable.contains(&old) {
            remap.insert(old, nodes.len());
            nodes.push(n);
        }
    }
    for n in &mut nodes {
        for e in &mut n.edges {
            e.child = e.child.and_then(|c| remap.get(&c).copied());
        }
        {
            n.linked_edges = n
                .edges
                .iter()
                .enumerate()
                .filter_map(|(i, e)| e.child.map(|_| i as u16))
                .collect();
        }
        n.parents.clear();
    }
    for n in 0..nodes.len() {
        let children: Vec<_> = nodes[n].edges.iter().filter_map(|e| e.child).collect();
        for c in children {
            nodes[c].parents.insert(n);
        }
    }
    s.root = remap[&s.root];
    s.index = nodes.iter().enumerate().map(|(i, n)| (n.key, i)).collect();
    s.nodes = nodes;
    s.recount_payload();
}

fn fixture(count: usize, keep_pct: usize, slots: usize, permute: bool) -> Search {
    let position = Position::new(19, 7.5).unwrap();
    let mut s = Search::new(
        position.clone(),
        SearchConfig {
            // One million synthetic 256-edge nodes exceeds the default 32 GiB
            // conservative charge. This fixture uses an explicit 64 GiB budget.
            max_memory_bytes: 64 * 1024 * 1024 * 1024,
            ..SearchConfig::default()
        },
    )
    .unwrap();
    s.nodes.clear();
    s.index.clear();
    let root = count - count * keep_pct / 100;
    let id = |i: usize| if permute { (i * 37) % count } else { i };
    s.root = id(root);
    for i in 0..count {
        let mut n = Node::new(&position);
        n.key[..8].copy_from_slice(&(i as u64).to_le_bytes());
        n.state = State::Expanded;
        n.stats.visits = (count - i) as u64;
        n.stats.weight_sum = n.stats.visits as f64;
        n.stats.utility = i as f64 * 0.000001;
        n.raw = Some(n.stats);
        n.ownership = vec![0.125; 361];
        n.edges = (0..slots)
            .map(|j| Edge {
                point: Some(j as u16),
                prior: 1.0 / slots as f64,
                child: None,
                visits: 0,
                in_flight: 0,
            })
            .collect();
        s.index.insert(n.key, i);
        s.nodes.push(n);
    }
    for i in 0..count {
        let start = if i < root { 0 } else { root };
        let end = if i < root { root } else { count };
        let mut children = Vec::new();
        for k in 1..=4 {
            let child = start + (i - start) * 4 + k;
            if child < end {
                children.push(child);
            }
        }
        if i + 2 < end {
            children.push(i + 2);
        }
        if i % 97 == 0 && i > start {
            children.push(start);
        }
        if i + 1 == root {
            children.push(root);
        }
        // Shared children and distinct edges pointing to the same child.
        if i + 1 < end {
            children.push(i + 1);
        }
        for (e, child) in children.into_iter().enumerate() {
            s.nodes[id(i)].set_child(e, id(child));
            s.nodes[id(i)].edges[e].visits = 1;
            s.nodes[id(child)].parents.insert(id(i));
        }
    }
    // A retained historical reverse link without a corresponding forward edge.
    s.nodes[id(count - 1)].parents.insert(id(root));
    s.recount_payload();
    s
}

fn signature(s: &Search) -> String {
    let mut h = Sha256::new();
    h.update(s.root.to_le_bytes());
    h.update(s.nodes.len().to_le_bytes());
    assert_eq!(s.nodes.len(), s.index.len());
    let mut expected_parents = vec![HashSet::new(); s.nodes.len()];
    for (parent, n) in s.nodes.iter().enumerate() {
        for c in n.edges.iter().filter_map(|e| e.child) {
            assert!(c < s.nodes.len());
            expected_parents[c].insert(parent);
        }
    }
    for (i, n) in s.nodes.iter().enumerate() {
        assert_eq!(s.index[&n.key], i);
        assert_eq!(n.parents, expected_parents[i]);
        assert_eq!(n.in_flight, 0);
        h.update(n.key);
        h.update(serde_json::to_vec(&n.stats).unwrap());
        h.update(serde_json::to_vec(&n.raw).unwrap());
        h.update(format!(
            "{:?}{:?}{}",
            n.state, n.color, n.force_non_terminal
        ));
        for x in &n.ownership {
            h.update(x.to_bits().to_le_bytes());
        }
        for e in &n.edges {
            h.update(e.point.unwrap_or(u16::MAX).to_le_bytes());
            h.update(e.prior.to_bits().to_le_bytes());
            h.update(e.visits.to_le_bytes());
            h.update(e.child.unwrap_or(usize::MAX).to_le_bytes());
            h.update(e.in_flight.to_le_bytes());
        }
        assert_eq!(
            n.linked_edges,
            n.edges
                .iter()
                .enumerate()
                .filter_map(|(i, e)| e.child.map(|_| i as u16))
                .collect::<Vec<_>>()
        );
    }
    format!("{:x}", h.finalize())
}

#[test]
fn collection_matches_reference_for_shared_cycles_holes_and_stale_parents() {
    for count in [100, 4096] {
        for keep in [1, 10, 50, 90, 100] {
            for permute in [false, true] {
                let mut reference = fixture(count, keep, 16, permute);
                baseline(&mut reference);
                let expected = signature(&reference);
                let mut s = fixture(count, keep, 16, permute);
                s.collect_unreachable();
                assert_eq!(signature(&s), expected);
                s.reclaimer.drain();
                assert_eq!(s.reclaimer.bytes(), 0);
                assert_eq!(s.reclaimer.snapshot().pending_nodes, 0);
                assert_eq!(s.reclaimer.snapshot().pending_batches, 0);
            }
        }
    }
}

#[test]
fn repeated_resets_drain_on_shutdown_and_respect_budget() {
    let mut s = fixture(12000, 10, 64, true);
    for komi in [6.5, 7.5, 8.5, 5.5, 7.5] {
        s.set_root(Position::new(19, komi).unwrap()).unwrap();
        assert_eq!(s.nodes.len(), 1);
        assert!(s.memory_bytes() <= s.config.max_memory_bytes);
        let state = s.reclaimer.snapshot();
        assert!(state.pending_batches <= 2);
        assert!(state.pending_nodes <= s.config.max_nodes);
        assert!(state.pending_bytes <= s.config.max_memory_bytes);
    }
    s.reclaimer.drain();
    assert_eq!(s.reclaimer.snapshot().reclaimed_nodes, 12004);
    assert_eq!(s.reclaimer.snapshot().pending_bytes, 0);

    let mut tight = fixture(4096, 10, 16, true);
    tight.config.max_memory_bytes = tight.node_charge() + tight.position_charge(&tight.position);
    tight.set_root(Position::new(19, 6.5).unwrap()).unwrap();
    assert_eq!(tight.reclaimer.bytes(), 0);
    assert!(tight.memory_bytes() <= tight.config.max_memory_bytes);
    assert!(tight.reclaimer.snapshot().synchronous_batches > 0);
}

#[test]
fn late_result_and_virtual_reservations_are_cleared_before_retirement() {
    let mut s = fixture(4096, 10, 16, true);
    let position = Position::new(19, 7.5).unwrap();
    let token = EvalToken {
        generation: s.generation,
        id: 3,
    };
    let parent = s.root;
    let child = s.nodes[parent].edges[0].child.unwrap();
    s.nodes[child].state = State::Evaluating(token);
    s.recount_payload();
    s.reserve_path(child, &[(parent, 0)], true);
    s.pending_memory_bytes = s.position_charge(&position) * 2 + 32;
    s.pending.insert(
        token,
        Pending {
            node: child,
            path: vec![(parent, 0)],
            position: position.clone(),
        },
    );
    // Root key must refer to an actual position for the public root-change API.
    s.index.remove(&s.nodes[parent].key);
    s.nodes[parent].key = position.graph_key();
    s.index.insert(position.graph_key(), parent);
    let cancelled = s.set_root(position).unwrap();
    assert_eq!(cancelled, vec![token]);
    assert_eq!(s.fail(token), Completion::Stale);
    assert_eq!(s.pending_memory_bytes, 0);
    assert!(s
        .nodes
        .iter()
        .all(|n| n.in_flight == 0 && n.edges.iter().all(|e| e.in_flight == 0)));
    assert!(s
        .nodes
        .iter()
        .all(|n| !matches!(n.state, State::Evaluating(_))));
    signature(&s);
}

fn numbers(name: &str, fallback: &str) -> Vec<usize> {
    std::env::var(name)
        .unwrap_or(fallback.into())
        .split(',')
        .map(|n| n.parse().unwrap())
        .collect()
}

#[test]
#[ignore = "explicit release ABBA benchmark; allocates large graphs"]
fn abba_collection() {
    let slots = numbers("MOVE_PROBE_EDGES", "256")[0];
    let rounds = numbers("MOVE_PROBE_ROUNDS", "2")[0];
    assert!((8..=362).contains(&slots));
    for count in numbers("MOVE_PROBE_NODES", "10000,100000,300000") {
        for keep in numbers("MOVE_PROBE_KEEP", "10,90") {
            let mut reference = None;
            for round in 0..rounds {
                for (order, candidate) in [false, true, true, false].into_iter().enumerate() {
                    let mut s = fixture(count, keep, slots, false);
                    s.root_change = Some(RootChangeMetrics::default());
                    let started = Instant::now();
                    if candidate {
                        s.collect_unreachable();
                    } else {
                        baseline(&mut s);
                    }
                    let ms = started.elapsed().as_secs_f64() * 1000.0;
                    let pending = s.reclaimer.snapshot();
                    let charged = s.memory_bytes();
                    s.reclaimer.drain();
                    let drained_ms = started.elapsed().as_secs_f64() * 1000.0;
                    let sig = signature(&s);
                    if let Some(ref expected) = reference {
                        assert_eq!(&sig, expected);
                    } else {
                        reference = Some(sig.clone());
                    }
                    println!(
                        "MOVE_PROBE {}",
                        serde_json::json!({
                            "nodes":count,"keep_pct":keep,"edge_slots":slots,
                            "round":round,"order":order,"variant":if candidate {"candidate"} else {"baseline"},
                            "elapsed_ms":ms,"drained_ms":drained_ms,"remaining":s.nodes.len(),"signature":sig,
                            "charged_bytes":charged,"pending":pending,"reclamation":s.reclaimer.snapshot(),"stages":s.root_change,
                        })
                    );
                }
            }
        }
    }
}

#[test]
#[ignore = "explicit release ABBA for concurrent destruction and new search"]
fn abba_reset_and_search() {
    let count = numbers("MOVE_PROBE_NODES", "300000")[0];
    let evaluations = numbers("MOVE_SEARCH_EVALS", "3000")[0];
    let mut reference = None;
    for round in 0..numbers("MOVE_PROBE_ROUNDS", "2")[0] {
        for (order, candidate) in [false, true, true, false].into_iter().enumerate() {
            let mut s = fixture(count, 10, 256, false);
            s.config.background_reclamation = candidate;
            let start = Instant::now();
            s.set_root(Position::new(19, 6.5).unwrap()).unwrap();
            let root_ms = start.elapsed().as_secs_f64() * 1000.0;
            let searching = Instant::now();
            let mut trace = Sha256::new();
            let mut completed = 0;
            while completed < evaluations {
                match s.next_evaluation().unwrap() {
                    SearchStep::Evaluate(request) => {
                        trace.update(request.input_hash);
                        let q = (request.input_hash[8] as f64 / 255.0 - 0.5) * 0.6;
                        s.complete(
                            request.token,
                            Evaluation {
                                policy: vec![1.0 / 362.0; 362],
                                white_win_prob: (1.0 + q) * 0.5,
                                white_loss_prob: (1.0 - q) * 0.5,
                                white_no_result_prob: 0.0,
                                white_score_mean: q * 12.0,
                                white_score_mean_sq: (q * 12.0).powi(2) + 16.0,
                                white_lead: q * 12.0,
                                has_shortterm_error: false,
                                shortterm_winloss_error: -1.0,
                                shortterm_score_error: -1.0,
                                ownership: vec![q; 361],
                            },
                        )
                        .unwrap();
                        completed += 1;
                    }
                    SearchStep::Advanced => {}
                    SearchStep::Waiting => std::thread::yield_now(),
                    other => panic!("unexpected new search state: {other:?}"),
                }
                assert!(
                    searching.elapsed().as_secs() < 60,
                    "search stuck after retirement"
                );
            }
            let search_ms = searching.elapsed().as_secs_f64() * 1000.0;
            let root_and_search_ms = start.elapsed().as_secs_f64() * 1000.0;
            s.reclaimer.drain();
            let mut snapshot = serde_json::to_value(s.snapshot(362, 30)).unwrap();
            for field in ["root_change", "reclamation"] {
                snapshot.as_object_mut().unwrap().remove(field);
            }
            trace.update(serde_json::to_vec(&snapshot).unwrap());
            let signature = format!("{:x}", trace.finalize());
            if let Some(ref expected) = reference {
                assert_eq!(&signature, expected);
            } else {
                reference = Some(signature.clone());
            }
            println!(
                "MOVE_SEARCH {}",
                serde_json::json!({
                    "nodes":count,"evaluations":evaluations,"round":round,"order":order,
                    "variant":if candidate {"candidate"} else {"baseline"},
                    "root_ms":root_ms,"search_ms":search_ms,"root_and_search_ms":root_and_search_ms,
                    "signature":signature,"reclamation":s.reclaimer.snapshot(),"root_change":s.root_change,
                })
            );
        }
    }
}
