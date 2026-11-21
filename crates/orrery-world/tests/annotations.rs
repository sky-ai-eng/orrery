//! A check run's annotations: each is kept after those before it, and the
//! check run answers how many it has.

use orrery_world::{
    ActorKind, Annotation, Attribution, CheckRunPatch, CheckStatus, Door, NewCheckRun, World,
    WorldSecret,
};

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn note(line: i64, message: &str) -> Annotation {
    Annotation {
        path: "src/lib.rs".to_string(),
        start_line: line,
        end_line: line,
        start_column: None,
        end_column: None,
        level: "warning".to_string(),
        message: message.to_string(),
        title: None,
        raw_details: None,
    }
}

#[test]
fn a_check_run_keeps_each_annotation_after_those_it_has() {
    let dir = tempfile::tempdir().unwrap();
    let w = World::create(dir.path(), 1_700_000_000, &WorldSecret::LOCAL).unwrap();
    let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
    let (ci, _) = w.create_actor("ci", ActorKind::Bot).unwrap();
    let repo = w
        .create_repo(
            alice.id,
            "web",
            None,
            false,
            "main",
            Attribution::new(alice.id, Door::Rest),
        )
        .unwrap();
    let by = Attribution::new(ci.id, Door::Rest);
    let logged = |after: i64| -> Vec<&'static str> {
        w.log_after(after, 100)
            .unwrap()
            .iter()
            .map(|event| event.body.type_name())
            .collect()
    };
    let seq = || w.log_after(0, 10_000).unwrap().last().unwrap().seq;
    let clock = w.clock().unwrap();

    // A new check run stores what it is given, and the row it returns
    // counts them, as a read does.
    let first = [note(1, "one"), note(2, "two")];
    let before = seq();
    let run = w
        .create_check_run(
            by,
            NewCheckRun {
                repo_id: repo.id,
                head_sha: SHA,
                name: "lint",
                status: CheckStatus::InProgress,
                conclusion: None,
                details_url: None,
                external_id: None,
                output: None,
                started_clock_unix: None,
                completed_clock_unix: None,
                annotations: &first,
            },
            clock,
        )
        .unwrap();
    assert_eq!(run.annotations_count, 2);
    assert_eq!(w.check_run_by_id(run.id).unwrap().unwrap(), run);
    assert_eq!(w.list_check_run_annotations(run.id).unwrap(), first);
    // The check run is the write's operation, and its annotations a
    // consequence of it.
    let events = logged(before);
    assert_eq!(events[0], "check_run:created");
    assert!(events[1..].contains(&"check_run:annotated"), "{events:?}");

    // An update that changes a field and gives more: the change is the
    // write's operation, and the annotations follow it.
    let more = [Annotation {
        start_column: Some(2),
        end_column: Some(9),
        title: Some("Unused".to_string()),
        raw_details: Some("details".to_string()),
        level: "failure".to_string(),
        ..note(3, "three")
    }];
    let before = seq();
    let updated = w
        .update_check_run(
            run.id,
            by,
            CheckRunPatch {
                name: Some("lint again"),
                annotations: &more,
                ..Default::default()
            },
            clock,
        )
        .unwrap();
    assert_eq!(updated.annotations_count, 3);
    assert_eq!(w.check_run_by_id(run.id).unwrap().unwrap(), updated);
    assert_eq!(logged(before), ["check_run:updated", "check_run:annotated"]);
    let all = w.list_check_run_annotations(run.id).unwrap();
    assert_eq!(all[..2], first);
    assert_eq!(all[2], more[0]);

    // An update that only gives more is a write of the annotations alone,
    // and one that gives none and changes nothing writes nothing.
    let before = seq();
    let last = [note(4, "four")];
    let updated = w
        .update_check_run(
            run.id,
            by,
            CheckRunPatch {
                annotations: &last,
                ..Default::default()
            },
            clock,
        )
        .unwrap();
    assert_eq!(updated.annotations_count, 4);
    assert_eq!(logged(before), ["check_run:annotated"]);
    let before = seq();
    w.update_check_run(run.id, by, CheckRunPatch::default(), clock)
        .unwrap();
    assert!(logged(before).is_empty());

    // Another check run has none of them.
    let other = w
        .create_check_run(
            by,
            NewCheckRun {
                repo_id: repo.id,
                head_sha: SHA,
                name: "build",
                status: CheckStatus::Queued,
                conclusion: None,
                details_url: None,
                external_id: None,
                output: None,
                started_clock_unix: None,
                completed_clock_unix: None,
                annotations: &[],
            },
            clock,
        )
        .unwrap();
    assert_eq!(other.annotations_count, 0);
    assert!(w.list_check_run_annotations(other.id).unwrap().is_empty());
}
