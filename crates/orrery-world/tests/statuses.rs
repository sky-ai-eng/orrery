//! Commit statuses: each is kept, the newest of a context is what counts,
//! and a context holds no more than GitHub keeps.

use orrery_world::{
    ActorKind, Attribution, CommitStatus, Door, MAX_STATUSES_PER_CONTEXT, NewCommitStatus,
    StatusState, World, WorldError, WorldSecret, combined_state, newest_of_each_context,
};

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[test]
fn a_commit_keeps_every_status_and_comes_to_the_newest_of_each_context() {
    let dir = tempfile::tempdir().unwrap();
    let w = World::create(dir.path(), 1_700_000_000, &WorldSecret::LOCAL).unwrap();
    let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
    let me = Attribution::new(alice.id, Door::Rest);
    let repo = w
        .create_repo(alice.id, "web", None, false, "main", me)
        .unwrap();
    let give = |context: &str, state: StatusState| -> Result<CommitStatus, WorldError> {
        w.create_commit_status(
            me,
            NewCommitStatus {
                repo_id: repo.id,
                sha: SHA,
                context,
                state,
                description: Some("why"),
                target_url: None,
            },
        )
    };
    let came_to = || {
        let statuses = w.list_commit_statuses(repo.id, SHA).unwrap();
        let newest = newest_of_each_context(&statuses);
        let contexts: Vec<String> = newest.iter().map(|s| s.context.clone()).collect();
        (combined_state(newest), contexts)
    };
    assert_eq!(came_to(), (StatusState::Pending, Vec::new()));

    let first = give("build", StatusState::Pending).unwrap();
    assert_eq!(first.creator_login, "alice");
    assert_eq!(first.description.as_deref(), Some("why"));
    // The write returns the row a read finds.
    assert_eq!(
        w.list_commit_statuses(repo.id, SHA).unwrap(),
        std::slice::from_ref(&first)
    );
    give("lint", StatusState::Success).unwrap();
    assert_eq!(came_to().0, StatusState::Pending);
    // An error counts as a failure, and a failure outweighs what is pending.
    give("lint", StatusState::Error).unwrap();
    assert_eq!(
        came_to(),
        (
            StatusState::Failure,
            vec!["build".to_string(), "lint".to_string()]
        )
    );
    // A context's next status is a new one, and moves it to the end.
    let second = give("build", StatusState::Success).unwrap();
    assert_ne!(second.id, first.id);
    give("lint", StatusState::Success).unwrap();
    assert_eq!(
        came_to(),
        (
            StatusState::Success,
            vec!["build".to_string(), "lint".to_string()]
        )
    );
    assert_eq!(w.list_commit_statuses(repo.id, SHA).unwrap().len(), 5);

    // A context holds as many statuses as GitHub keeps, and no more.
    for _ in 2..MAX_STATUSES_PER_CONTEXT {
        give("build", StatusState::Pending).unwrap();
    }
    assert!(matches!(
        give("build", StatusState::Success),
        Err(WorldError::TooManyStatuses { .. })
    ));
    give("lint", StatusState::Success).unwrap();
}
