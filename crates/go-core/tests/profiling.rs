#![cfg(feature = "search-profiling")]

use go_core::{profiling, Position, Search, SearchConfig, SearchStep};

#[test]
fn profile_windows_count_real_requests_and_waits_without_counting_root_as_depth_one() {
    profiling::take_activity();
    let mut search = Search::new(
        Position::new(19, 7.5).unwrap(),
        SearchConfig {
            max_in_flight: 1,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(matches!(
        search.next_evaluation().unwrap(),
        SearchStep::Evaluate(_)
    ));
    assert!(matches!(
        search.next_evaluation().unwrap(),
        SearchStep::Waiting
    ));
    let a = profiling::take_activity();
    assert_eq!((a.evaluate, a.waiting, a.leaf_count), (1, 1, 1));
    assert_eq!((a.leaf_depth_sum, a.leaf_depth_max), (0, 0));
    assert_eq!(profiling::take_activity().evaluate, 0);

    std::thread::spawn(|| {
        let mut other =
            Search::new(Position::new(19, 7.5).unwrap(), SearchConfig::default()).unwrap();
        other.next_evaluation().unwrap();
        assert_eq!(profiling::take_activity().evaluate, 1);
    })
    .join()
    .unwrap();
    assert_eq!(
        profiling::take_activity().evaluate,
        0,
        "sessions must not mix thread counters"
    );
}
