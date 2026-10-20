//! Organization membership, teams and review requests: the single-row-write
//! rule (returned row == point read), GitHub's refusals, and the timeline and
//! action-log entries each write owes.

use orrery_world::{
    ActorKind, DEFAULT_T0_UNIX, NewPull, OrgRole, Pull, RequestedReviewer, ReviewState, TeamRole,
    TimelineKind, World, WorldError,
};

const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

struct Fixture {
    _dir: tempfile::TempDir,
    w: World,
    alice: i64,
    bob: i64,
    carol: i64,
    acme: i64,
    repo: i64,
}

/// An `acme` org owning `webapp`, with alice an admin and bob a member;
/// carol is not in the org.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let w = World::create(&dir.path().join("world"), DEFAULT_T0_UNIX).expect("create world");
    let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
    let (bob, _) = w.create_actor("bob", ActorKind::User).unwrap();
    let (carol, _) = w.create_actor("carol", ActorKind::User).unwrap();
    let (acme, _) = w.create_actor("acme", ActorKind::Org).unwrap();
    w.set_org_membership("acme", "alice", OrgRole::Admin, alice.id)
        .unwrap();
    w.set_org_membership("acme", "bob", OrgRole::Member, alice.id)
        .unwrap();
    let repo = w
        .create_repo(acme.id, "webapp", None, false, "main", alice.id)
        .unwrap();
    Fixture {
        _dir: dir,
        w,
        alice: alice.id,
        bob: bob.id,
        carol: carol.id,
        acme: acme.id,
        repo: repo.id,
    }
}

fn open_pull(f: &Fixture) -> Pull {
    f.w.create_pull(NewPull {
        repo_id: f.repo,
        author_id: f.alice,
        title: "Add feature",
        body: None,
        head_ref: "feature",
        head_sha: SHA_B,
        base_ref: "main",
        base_sha: SHA_A,
        draft: false,
    })
    .unwrap()
}

fn actions(w: &World) -> Vec<String> {
    w.log_after(0, 1000)
        .unwrap()
        .into_iter()
        .map(|e| e.action)
        .collect()
}

#[test]
fn org_membership_returns_the_stored_row_and_changes_role_in_place() {
    let f = fixture();
    f.w.create_actor("dave", ActorKind::User).unwrap();
    let set =
        f.w.set_org_membership("acme", "dave", OrgRole::Member, f.alice)
            .unwrap();
    assert_eq!(
        Some(set.clone()),
        f.w.org_membership("acme", "dave").unwrap()
    );
    assert_eq!(set.org_login, "acme");
    assert_eq!(set.member_login, "dave");

    let logged = actions(&f.w).len();
    let same =
        f.w.set_org_membership("ACME", "Dave", OrgRole::Member, f.alice)
            .unwrap();
    assert_eq!(same, set);
    assert_eq!(
        actions(&f.w).len(),
        logged,
        "an unchanged role writes nothing"
    );

    let promoted =
        f.w.set_org_membership("acme", "dave", OrgRole::Admin, f.alice)
            .unwrap();
    assert_eq!(promoted.role, OrgRole::Admin);
    assert_eq!(promoted.created_clock_unix, set.created_clock_unix);
    assert_eq!(Some(promoted), f.w.org_membership("acme", "dave").unwrap());

    assert!(matches!(
        f.w.set_org_membership("alice", "bob", OrgRole::Member, f.alice),
        Err(WorldError::NoSuchOrg(_))
    ));
    assert!(matches!(
        f.w.set_org_membership("acme", "ghost", OrgRole::Member, f.alice),
        Err(WorldError::NoSuchActor(_))
    ));
    f.w.create_actor("ci", ActorKind::Bot).unwrap();
    assert!(matches!(
        f.w.set_org_membership("acme", "ci", OrgRole::Member, f.alice),
        Err(WorldError::NoSuchActor(_))
    ));
}

#[test]
fn a_team_returns_the_stored_row_and_its_creator_maintains_it() {
    let f = fixture();
    let team =
        f.w.create_team("acme", "Platform Team", Some("owns the platform"), f.alice)
            .unwrap();
    assert_eq!(
        Some(team.clone()),
        f.w.team_by_slug("acme", "platform-team").unwrap()
    );
    assert_eq!(team.slug, "platform-team");
    assert_eq!(team.org_id, f.acme);
    assert_eq!(
        f.w.team_members_count(team.id).unwrap(),
        1,
        "the creator joins as a maintainer"
    );
    assert_eq!(
        f.w.team_membership("acme", "platform-team", "alice")
            .unwrap()
            .unwrap()
            .role,
        TeamRole::Maintainer
    );

    let outsider = f.w.create_team("acme", "Docs", None, f.carol).unwrap();
    assert_eq!(
        f.w.team_members_count(outsider.id).unwrap(),
        0,
        "a non-member creator does not join"
    );

    assert!(matches!(
        f.w.create_team("acme", "platform team", None, f.alice),
        Err(WorldError::TeamExists(_))
    ));
    assert!(matches!(
        f.w.create_team("acme", "Platform-Team", None, f.alice),
        Err(WorldError::TeamExists(_)),
    ));
    assert!(matches!(
        f.w.create_team("acme", " !! ", None, f.alice),
        Err(WorldError::InvalidTeamName(_))
    ));
    assert!(matches!(
        f.w.create_team("alice", "Mine", None, f.alice),
        Err(WorldError::NoSuchOrg(_))
    ));
    assert!(actions(&f.w).contains(&"team.create".to_string()));
}

#[test]
fn team_membership_requires_org_membership() {
    let f = fixture();
    f.w.create_team("acme", "Platform", None, f.alice).unwrap();
    let set =
        f.w.set_team_membership("acme", "platform", "bob", TeamRole::Member, f.alice)
            .unwrap();
    assert_eq!(
        Some(set.clone()),
        f.w.team_membership("acme", "platform", "bob").unwrap()
    );
    assert_eq!(set.member_login, "bob");
    assert_eq!(f.w.team_members_count(set.team_id).unwrap(), 2);

    assert!(matches!(
        f.w.set_team_membership("acme", "platform", "carol", TeamRole::Member, f.alice),
        Err(WorldError::NotOrgMember { .. })
    ));
    assert!(matches!(
        f.w.set_team_membership("acme", "nope", "bob", TeamRole::Member, f.alice),
        Err(WorldError::NoSuchTeam(_))
    ));

    f.w.remove_team_membership("acme", "platform", "bob", f.alice)
        .unwrap();
    assert_eq!(
        f.w.team_membership("acme", "platform", "bob").unwrap(),
        None
    );
    let logged = actions(&f.w).len();
    f.w.remove_team_membership("acme", "platform", "bob", f.alice)
        .unwrap();
    assert_eq!(
        actions(&f.w).len(),
        logged,
        "removing someone not on the team succeeds and writes nothing"
    );
    assert!(matches!(
        f.w.remove_team_membership("acme", "platform", "ghost", f.alice),
        Err(WorldError::NoSuchActor(_))
    ));
    let acts = actions(&f.w);
    assert!(acts.contains(&"team.membership.set".to_string()));
    assert!(acts.contains(&"team.membership.remove".to_string()));
}

#[test]
fn listing_teams_by_org_member_and_filter() {
    let f = fixture();
    f.w.create_team("acme", "Platform", None, f.alice).unwrap();
    f.w.create_team("acme", "Web", None, f.carol).unwrap();
    f.w.set_team_membership("acme", "web", "bob", TeamRole::Member, f.alice)
        .unwrap();

    let slugs = |teams: Vec<orrery_world::Team>| -> Vec<String> {
        teams.into_iter().map(|t| t.slug).collect()
    };
    assert_eq!(
        slugs(f.w.list_org_teams(f.acme, None).unwrap()),
        ["platform", "web"]
    );
    assert_eq!(
        slugs(
            f.w.list_org_teams(f.acme, Some(&["BOB".to_string()]))
                .unwrap()
        ),
        ["web"]
    );
    assert_eq!(
        slugs(f.w.list_teams_for_member(f.alice).unwrap()),
        ["platform"]
    );
    assert!(f.w.list_teams_for_member(f.carol).unwrap().is_empty());
}

fn timeline(f: &Fixture, pull_id: i64) -> Vec<TimelineKind> {
    f.w.list_pull_timeline(pull_id)
        .unwrap()
        .into_iter()
        .map(|i| i.kind)
        .collect()
}

#[test]
fn review_requests_land_on_the_pull_and_its_timeline() {
    let f = fixture();
    f.w.create_team("acme", "Platform", None, f.alice).unwrap();
    let pull = open_pull(&f);

    let requested =
        f.w.request_reviewers(pull.id, f.alice, &["bob", "carol"], &["platform"])
            .unwrap();
    assert_eq!(requested, f.w.pull_by_id(pull.id).unwrap().unwrap());
    assert_eq!(requested.requested_reviewers, ["bob", "carol"]);
    assert_eq!(requested.requested_teams, ["platform"]);

    let again =
        f.w.request_reviewers(pull.id, f.alice, &["BOB"], &[])
            .unwrap();
    assert_eq!(
        again, requested,
        "a pending reviewer is skipped without a write"
    );

    let removed =
        f.w.remove_review_requests(pull.id, f.alice, &["carol", "ghost"], &["platform"])
            .unwrap();
    assert_eq!(removed, f.w.pull_by_id(pull.id).unwrap().unwrap());
    assert_eq!(removed.requested_reviewers, ["bob"]);
    assert!(removed.requested_teams.is_empty());

    let user = |l: &str| RequestedReviewer::User(l.to_string());
    let platform = f.w.team_by_slug("acme", "platform").unwrap().unwrap();
    let team = |s: &str| RequestedReviewer::Team {
        id: platform.id,
        slug: s.to_string(),
    };
    assert_eq!(
        timeline(&f, pull.id),
        [
            TimelineKind::ReviewRequested(user("bob")),
            TimelineKind::ReviewRequested(user("carol")),
            TimelineKind::ReviewRequested(team("platform")),
            TimelineKind::ReviewRequestRemoved(user("carol")),
            TimelineKind::ReviewRequestRemoved(team("platform")),
        ]
    );
    let acts = actions(&f.w);
    assert!(acts.contains(&"pull.request_reviewers".to_string()));
    assert!(acts.contains(&"pull.remove_review_requests".to_string()));
}

#[test]
fn an_invalid_reviewer_refuses_the_whole_request() {
    let f = fixture();
    let pull = open_pull(&f);
    f.w.create_actor("ci", ActorKind::Bot).unwrap();
    for (users, teams) in [
        (vec!["bob", "ghost"], vec![]),
        (vec!["bob", "ci"], vec![]),
        (vec!["bob", "acme"], vec![]),
        (vec!["bob"], vec!["no-such-team"]),
    ] {
        assert!(
            matches!(
                f.w.request_reviewers(pull.id, f.alice, &users, &teams),
                Err(WorldError::InvalidReviewer(_))
            ),
            "{users:?} {teams:?} accepted"
        );
    }
    assert!(matches!(
        f.w.request_reviewers(pull.id, f.bob, &["bob", "alice"], &[]),
        Err(WorldError::ReviewerIsAuthor)
    ));
    assert_eq!(
        f.w.pull_by_id(pull.id).unwrap().unwrap(),
        pull,
        "a refused request changes nothing"
    );

    // A repository a user owns has no teams to ask.
    let own =
        f.w.create_repo(f.bob, "dotfiles", None, false, "main", f.bob)
            .unwrap();
    f.w.create_team("acme", "Platform", None, f.alice).unwrap();
    let theirs =
        f.w.create_pull(NewPull {
            repo_id: own.id,
            author_id: f.bob,
            title: "tweak",
            body: None,
            head_ref: "t",
            head_sha: SHA_B,
            base_ref: "main",
            base_sha: SHA_A,
            draft: false,
        })
        .unwrap();
    assert!(matches!(
        f.w.request_reviewers(theirs.id, f.bob, &[], &["platform"]),
        Err(WorldError::InvalidReviewer(_))
    ));
}

#[test]
fn a_review_clears_its_authors_request_but_not_their_teams() {
    let f = fixture();
    f.w.create_team("acme", "Platform", None, f.alice).unwrap();
    f.w.set_team_membership("acme", "platform", "bob", TeamRole::Member, f.alice)
        .unwrap();
    let pull = open_pull(&f);
    f.w.request_reviewers(pull.id, f.alice, &["bob", "carol"], &["platform"])
        .unwrap();
    f.w.create_review(pull.id, f.bob, ReviewState::Approved, None, None)
        .unwrap();
    let after = f.w.pull_by_id(pull.id).unwrap().unwrap();
    assert_eq!(after.requested_reviewers, ["carol"]);
    assert_eq!(after.requested_teams, ["platform"]);
    assert_eq!(
        timeline(&f, pull.id).len(),
        3,
        "a review leaves no request-removed item"
    );
}

#[test]
fn deleting_a_team_drops_its_requests_and_keeps_its_history() {
    let f = fixture();
    let old = f.w.create_team("acme", "Platform", None, f.alice).unwrap();
    f.w.set_team_membership("acme", "platform", "bob", TeamRole::Member, f.alice)
        .unwrap();
    let pull = open_pull(&f);
    f.w.request_reviewers(pull.id, f.alice, &[], &["platform"])
        .unwrap();

    f.w.delete_team("acme", "platform", f.alice).unwrap();
    assert_eq!(f.w.team_by_slug("acme", "platform").unwrap(), None);
    assert!(f.w.list_teams_for_member(f.bob).unwrap().is_empty());
    let after = f.w.pull_by_id(pull.id).unwrap().unwrap();
    assert!(after.requested_teams.is_empty());
    assert_eq!(
        timeline(&f, pull.id),
        [TimelineKind::ReviewRequested(RequestedReviewer::Team {
            id: old.id,
            slug: "platform".to_string(),
        })]
    );
    assert!(matches!(
        f.w.delete_team("acme", "platform", f.alice),
        Err(WorldError::NoSuchTeam(_))
    ));

    let again = f.w.create_team("acme", "Platform", None, f.alice).unwrap();
    assert_ne!(again.id, old.id, "a team id is never reused");
    assert_eq!(f.w.team_by_id(old.id).unwrap(), None);
    assert_eq!(
        f.w.team_members_count(again.id).unwrap(),
        1,
        "a new team starts with its creator only"
    );
    assert!(actions(&f.w).contains(&"team.delete".to_string()));
}
