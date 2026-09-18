use go_server::{
    session::{SessionConfig, Sessions},
    worker::WorkerPool,
};
use std::time::Duration;

#[tokio::test]
async fn snapshot_exposes_diagnostics_only_in_profile_build() {
    let sessions = Sessions::new(
        SessionConfig::default(),
        WorkerPool::new(None, Duration::from_secs(1)),
    );
    let handle = sessions.open(None).unwrap();
    let snapshot = handle.attach("profile-test".to_owned()).await.unwrap();
    sessions.shutdown();
    let profile = &snapshot["analysis"]["cpuProfile"];
    if cfg!(feature = "search-profiling") {
        assert!(profile["windowSeconds"].as_f64().unwrap() >= 0.0);
        assert_eq!(profile["activity"]["evaluate"], 0);
        assert_eq!(profile["activity"]["leaf_count"], 0);
        assert!(profile["inclusiveStages"]["next_evaluation"]["nanos"].is_u64());
        assert_eq!(profile["maxNodes"], 100_000_000u64);
    } else {
        assert!(profile.is_null());
    }
}
