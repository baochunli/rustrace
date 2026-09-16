use rustrace::session_fixture::{FixtureAction, maximum_workspace, representative_hour};

#[test]
fn fixtures_are_deterministic_and_reach_all_three_workspace_limits() {
    let files = maximum_workspace(0x36);
    assert_eq!(files, maximum_workspace(0x36));
    assert_ne!(files, maximum_workspace(0x37));
    assert_eq!(files.len(), 256);
    assert_eq!(
        files.values().map(Vec::len).sum::<usize>(),
        10 * 1024 * 1024
    );
    assert_eq!(files.values().map(Vec::len).max(), Some(1024 * 1024));
    assert!(files.values().all(|v| std::str::from_utf8(v).is_ok()));
    let actions = representative_hour(0x36);
    assert_eq!(actions, representative_hour(0x36));
    assert_eq!(actions.last().unwrap().at_millis, 3_600_000);
    for predicate in [
        |a: &FixtureAction| matches!(a, FixtureAction::Paste(_)),
        |a: &FixtureAction| matches!(a, FixtureAction::Undo),
        |a: &FixtureAction| matches!(a, FixtureAction::Redo),
        |a: &FixtureAction| matches!(a, FixtureAction::Save),
        |a: &FixtureAction| matches!(a, FixtureAction::Restart),
        |a: &FixtureAction| matches!(a, FixtureAction::External),
    ] {
        assert!(actions.iter().any(|step| predicate(&step.action)));
    }
    assert!(actions.windows(2).all(|w| w[0].at_millis <= w[1].at_millis));
}
