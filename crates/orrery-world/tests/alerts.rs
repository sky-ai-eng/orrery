//! Dependency alerts: the write rule (what a write returns is what a read
//! answers), which moves each state allows, the order and the bounds of a
//! read, and what the updater did.

use orrery_world::{
    ActorKind, AlertPosition, AlertQuery, AlertScope, AlertSeverity, AlertState, AlertTransition,
    AlertUpdate, Attribution, DEFAULT_T0_UNIX, DismissReason, Door, EventBody, NewAlertUpdate,
    NewDependencyAlert, NewPull, SYSTEM_ACTOR_ID, World, WorldError, WorldSecret, WorldView,
};

const SYSTEM: Attribution = Attribution::new(SYSTEM_ACTOR_ID, Door::Control);
const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn alert<'a>(package: &'a str, severity: AlertSeverity) -> NewDependencyAlert<'a> {
    NewDependencyAlert {
        ecosystem: "npm",
        package,
        manifest_path: "package-lock.json",
        scope: Some(AlertScope::Runtime),
        version_requirement: Some("1.0.0"),
        advisory_id: "GHSA-0000-0000-0000",
        cve_id: None,
        summary: "A vulnerable dependency",
        description: "",
        severity,
        vulnerable_version_range: "< 2.0.0",
        first_patched_version: Some("2.0.0"),
    }
}

/// Alice and her repository `web`, with alerts on: the world, her
/// attribution, and the repository's id.
fn seeded() -> (tempfile::TempDir, World, Attribution, i64) {
    let dir = tempfile::tempdir().expect("tempdir");
    let w = World::create(
        &dir.path().join("world"),
        DEFAULT_T0_UNIX,
        &WorldSecret::LOCAL,
    )
    .expect("create world");
    let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
    let by = Attribution::new(alice.id, Door::Rest);
    let repo = w
        .create_repo(alice.id, "web", None, true, "main", by)
        .unwrap();
    assert!(
        !repo.dependency_alerts,
        "a repository starts with alerts off"
    );
    let on = w.set_dependency_alerts(repo.id, true, by).unwrap();
    assert!(on.dependency_alerts);
    assert_eq!(w.repo_by_id(repo.id).unwrap(), Some(on));
    (dir, w, by, repo.id)
}

fn kinds(w: &World) -> Vec<&'static str> {
    w.log_after(0, 500)
        .unwrap()
        .iter()
        .map(|e| e.body.type_name())
        .filter(|kind| kind.starts_with("dependency_alert:") || kind.starts_with("repo:dep"))
        .collect()
}

#[test]
fn an_alert_moves_only_as_its_state_allows() {
    let (_dir, w, alice, repo) = seeded();
    // What a write returns is what a read answers.
    let made = w
        .create_dependency_alert(repo, &alert("lodash", AlertSeverity::High), SYSTEM)
        .unwrap();
    assert_eq!((made.number, made.state), (1, AlertState::Open));
    assert_eq!(made.update, AlertUpdate::None);
    assert_eq!(w.dependency_alert(repo, 1).unwrap(), Some(made.clone()));
    assert_eq!(
        w.create_dependency_alert(repo, &alert("minimist", AlertSeverity::Low), SYSTEM)
            .unwrap()
            .number,
        2
    );

    let go = |transition: AlertTransition<'_>, by: Attribution| {
        w.move_dependency_alert(repo, 1, &transition, by)
    };
    let dismiss = AlertTransition::Dismiss {
        reason: DismissReason::NotUsed,
        comment: Some("unused"),
    };
    // Open: nothing to reopen or bring back.
    for refused in [AlertTransition::Reopen, AlertTransition::Reintroduce] {
        assert!(matches!(
            go(refused, alice),
            Err(WorldError::DependencyAlertState {
                number: 1,
                state: AlertState::Open
            })
        ));
    }
    w.advance_clock(30, SYSTEM).unwrap();
    let dismissed = go(dismiss, alice).unwrap();
    assert_eq!(dismissed.state, AlertState::Dismissed);
    let held = dismissed.dismissed.clone().unwrap();
    assert_eq!(
        (held.by.login.as_str(), held.reason),
        ("alice", DismissReason::NotUsed)
    );
    assert_eq!(held.comment.as_deref(), Some("unused"));
    assert_eq!(held.clock_unix, dismissed.updated_clock_unix);
    assert_eq!(dismissed.updated_clock_unix, made.created_clock_unix + 30);
    assert_eq!(w.dependency_alert(repo, 1).unwrap(), Some(dismissed));
    assert!(matches!(
        go(dismiss, alice),
        Err(WorldError::DependencyAlertState {
            state: AlertState::Dismissed,
            ..
        })
    ));

    // Reopened, the dismissal is gone.
    let reopened = go(AlertTransition::Reopen, alice).unwrap();
    assert_eq!(
        (reopened.state, reopened.dismissed),
        (AlertState::Open, None)
    );

    // Fixed, from open or dismissed, and then nobody's to dismiss or reopen.
    go(dismiss, alice).unwrap();
    let fixed = go(AlertTransition::Fix, SYSTEM).unwrap();
    assert_eq!((fixed.state, &fixed.dismissed), (AlertState::Fixed, &None));
    assert_eq!(fixed.fixed_clock_unix, Some(fixed.updated_clock_unix));
    for refused in [dismiss, AlertTransition::Reopen, AlertTransition::Fix] {
        assert!(matches!(
            go(refused, alice),
            Err(WorldError::DependencyAlertState {
                state: AlertState::Fixed,
                ..
            })
        ));
    }
    let back = go(AlertTransition::Reintroduce, SYSTEM).unwrap();
    assert_eq!(
        (back.state, back.fixed_clock_unix),
        (AlertState::Open, None)
    );

    // A rule dismisses one, and it comes back by rule or by a person.
    let auto = go(AlertTransition::AutoDismiss, SYSTEM).unwrap();
    assert_eq!(auto.state, AlertState::AutoDismissed);
    assert_eq!(
        auto.auto_dismissed_clock_unix,
        Some(auto.updated_clock_unix)
    );
    go(AlertTransition::Reintroduce, SYSTEM).unwrap();
    go(AlertTransition::AutoDismiss, SYSTEM).unwrap();
    assert_eq!(
        go(AlertTransition::Reopen, alice).unwrap().state,
        AlertState::Open
    );

    assert_eq!(
        kinds(&w),
        [
            "repo:dependency_alerts_set",
            "dependency_alert:created",
            "dependency_alert:created",
            "dependency_alert:dismissed",
            "dependency_alert:reopened",
            "dependency_alert:dismissed",
            "dependency_alert:fixed",
            "dependency_alert:reintroduced",
            "dependency_alert:auto_dismissed",
            "dependency_alert:auto_reopened",
            "dependency_alert:auto_dismissed",
            "dependency_alert:reopened",
        ]
    );
    // A move is the mover's.
    let log = w.log_after(0, 500).unwrap();
    let who = |kind: &str| {
        log.iter()
            .find(|e| e.body.type_name() == kind)
            .map(|e| e.actor.login.clone())
            .unwrap()
    };
    assert_eq!(who("dependency_alert:dismissed"), "alice");
    assert_eq!(who("dependency_alert:fixed"), "orrery");

    assert!(matches!(
        w.move_dependency_alert(repo, 9, &AlertTransition::Fix, SYSTEM),
        Err(WorldError::NoSuchDependencyAlert(9))
    ));
}

#[test]
fn alerts_are_made_and_moved_only_while_they_are_on() {
    let (_dir, w, alice, repo) = seeded();
    w.create_dependency_alert(repo, &alert("lodash", AlertSeverity::High), SYSTEM)
        .unwrap();
    let before = w.view_meta().unwrap().last_seq;
    // Setting what is already so writes nothing.
    w.set_dependency_alerts(repo, true, alice).unwrap();
    assert_eq!(w.view_meta().unwrap().last_seq, before);

    w.set_dependency_alerts(repo, false, alice).unwrap();
    assert!(matches!(
        w.create_dependency_alert(repo, &alert("minimist", AlertSeverity::Low), SYSTEM),
        Err(WorldError::DependencyAlertsOff(id)) if id == repo
    ));
    assert!(matches!(
        w.move_dependency_alert(repo, 1, &AlertTransition::Fix, SYSTEM),
        Err(WorldError::DependencyAlertsOff(_))
    ));
    assert!(matches!(
        w.set_dependency_alert_update(repo, 1, &NewAlertUpdate::Pending, SYSTEM),
        Err(WorldError::DependencyAlertsOff(_))
    ));
    // The alert is kept for when they are on again.
    w.set_dependency_alerts(repo, true, alice).unwrap();
    assert_eq!(
        w.dependency_alert(repo, 1).unwrap().unwrap().state,
        AlertState::Open
    );
}

#[test]
fn the_updater_is_recorded_and_a_read_is_ordered_and_bounded() {
    let (_dir, w, alice, repo) = seeded();
    for (package, severity) in [
        ("a", AlertSeverity::Low),
        ("b", AlertSeverity::High),
        ("c", AlertSeverity::Critical),
        ("d", AlertSeverity::High),
    ] {
        w.create_dependency_alert(repo, &alert(package, severity), SYSTEM)
            .unwrap();
        w.advance_clock(10, SYSTEM).unwrap();
    }
    let pull = w
        .create_pull(
            alice,
            NewPull {
                repo_id: repo,
                title: "Bump b",
                body: None,
                head_ref: "bump",
                head_sha: SHA_B,
                base_ref: "main",
                base_sha: SHA_A,
                draft: false,
            },
        )
        .unwrap();

    // What the updater did, and that recording it again writes nothing.
    let set = |number, update: NewAlertUpdate<'_>| {
        w.set_dependency_alert_update(repo, number, &update, SYSTEM)
    };
    assert_eq!(
        set(1, NewAlertUpdate::Pending).unwrap().update,
        AlertUpdate::Pending
    );
    let opened = set(
        2,
        NewAlertUpdate::PullRequest {
            number: pull.number,
        },
    )
    .unwrap();
    assert_eq!(
        opened.update,
        AlertUpdate::PullRequest {
            pull_id: pull.id,
            number: pull.number
        }
    );
    assert_eq!(w.dependency_alert(repo, 2).unwrap(), Some(opened));
    let failed = set(
        3,
        NewAlertUpdate::Error {
            error_type: "dependency_file_not_found",
            title: "No manifest",
        },
    )
    .unwrap();
    assert!(matches!(failed.update, AlertUpdate::Error { .. }));
    let before = w.view_meta().unwrap().last_seq;
    set(1, NewAlertUpdate::Pending).unwrap();
    assert_eq!(w.view_meta().unwrap().last_seq, before);
    assert_eq!(
        set(3, NewAlertUpdate::None).unwrap().update,
        AlertUpdate::None
    );
    assert!(matches!(
        set(4, NewAlertUpdate::PullRequest { number: 99 }),
        Err(WorldError::NoSuchPullNumber { number: 99, .. })
    ));
    // Recording it moves no state, so an alert's `updated` stays.
    let first = w.dependency_alert(repo, 1).unwrap().unwrap();
    assert_eq!(first.updated_clock_unix, first.created_clock_unix);
    assert!(
        w.log_after(0, 500)
            .unwrap()
            .iter()
            .any(|e| matches!(e.body, EventBody::DependencyAlertUpdateSet(_)))
    );

    // Newest first, unless asked; a page starts past a position, or ends
    // just before one.
    let read = |query: &AlertQuery, limit: usize| -> Vec<i64> {
        w.dependency_alerts(repo, query, limit)
            .unwrap()
            .iter()
            .map(|a| a.number)
            .collect()
    };
    let all = AlertQuery::default();
    assert_eq!(read(&all, 10), [4, 3, 2, 1]);
    assert_eq!(read(&all, 2), [4, 3]);
    let third = w.dependency_alert(repo, 3).unwrap().unwrap();
    let at = all.position(&third);
    assert_eq!(
        at,
        AlertPosition {
            clock_unix: third.created_clock_unix,
            number: 3
        }
    );
    let past = AlertQuery {
        after: Some(at),
        ..AlertQuery::default()
    };
    assert_eq!(read(&past, 10), [2, 1]);
    let ahead = AlertQuery {
        before: Some(all.position(&w.dependency_alert(repo, 1).unwrap().unwrap())),
        ..AlertQuery::default()
    };
    assert_eq!(
        read(&ahead, 2),
        [3, 2],
        "the two nearest, still newest first"
    );
    let oldest = AlertQuery {
        ascending: true,
        ..AlertQuery::default()
    };
    assert_eq!(read(&oldest, 10), [1, 2, 3, 4]);
    // Oldest first, past a position and ahead of one.
    let second = oldest.position(&w.dependency_alert(repo, 2).unwrap().unwrap());
    let past = AlertQuery {
        after: Some(second),
        ..oldest.clone()
    };
    assert_eq!(read(&past, 10), [3, 4]);
    assert_eq!(read(&past, 1), [3]);
    let ahead = AlertQuery {
        before: Some(oldest.position(&w.dependency_alert(repo, 4).unwrap().unwrap())),
        ..oldest.clone()
    };
    assert_eq!(
        read(&ahead, 2),
        [2, 3],
        "the two nearest, still oldest first"
    );
    assert_eq!(read(&ahead, 10), [1, 2, 3]);

    // By when each was last updated: a move brings an alert to the front.
    w.move_dependency_alert(repo, 1, &AlertTransition::Fix, SYSTEM)
        .unwrap();
    let by_updated = AlertQuery {
        by_updated: true,
        ..AlertQuery::default()
    };
    assert_eq!(read(&by_updated, 10), [1, 4, 3, 2]);

    // The filters, and the count they keep wherever a read starts.
    let high = AlertQuery {
        severities: Some(vec![AlertSeverity::High]),
        ..AlertQuery::default()
    };
    assert_eq!(read(&high, 10), [4, 2]);
    let open = AlertQuery {
        states: Some(vec![AlertState::Open]),
        after: Some(at),
        ..AlertQuery::default()
    };
    assert_eq!(read(&open, 10), [2]);
    let none = AlertQuery {
        states: Some(Vec::new()),
        ..AlertQuery::default()
    };
    assert_eq!(read(&none, 10), [] as [i64; 0]);
    let packages = AlertQuery {
        packages: Some(vec!["a".to_string(), "c".to_string()]),
        ecosystems: Some(vec!["NPM".to_string()]),
        manifests: Some(vec!["package-lock.json".to_string()]),
        scopes: Some(vec![AlertScope::Runtime, AlertScope::Development]),
        ..AlertQuery::default()
    };
    assert_eq!(read(&packages, 10), [3, 1]);
}
