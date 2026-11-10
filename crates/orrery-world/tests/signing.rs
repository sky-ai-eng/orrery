//! Signing keys and the verdict on a commit's signature: the single-row
//! write rule, the order the verdict's checks apply in, and which verdicts a
//! push stores.

use orrery_world::{
    ActorEmail, ActorKind, Attribution, CommitFacts, CommitSignature, CommitVerification,
    DEFAULT_T0_UNIX, Door, EventBody, RefChange, RefMove, SYSTEM_ACTOR_ID, Verdict, World,
    WorldError, WorldSecret,
};

const KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIF1ZZZ9YNYg8yl9pzBjnLU8mjxuB7NQXuUV2YGVC2wv3";
const OTHER_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICXNiiZ0NcLB2s0gYXpNwyFmtzd28Zxng4mIlVSzlRIv";
const ZERO: &str = "0000000000000000000000000000000000000000";
const TIP: &str = "1111111111111111111111111111111111111111";

fn by(actor_id: i64) -> Attribution {
    Attribution::new(actor_id, Door::Rest)
}

struct Fixture {
    w: World,
    alice: i64,
    bob: i64,
    repo: i64,
    _dir: tempfile::TempDir,
}

/// alice holds `alice@corp.test`, verified, and `alice@old.test`, not; bob
/// holds `bob@corp.test`. alice owns `alice/webapp`.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let w = World::create(
        &dir.path().join("world"),
        DEFAULT_T0_UNIX,
        &WorldSecret::LOCAL,
    )
    .unwrap();
    let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
    let (bob, _) = w.create_actor("bob", ActorKind::User).unwrap();
    let email = |email: &str, primary, verified| ActorEmail {
        email: email.to_string(),
        primary,
        verified,
    };
    let system = by(SYSTEM_ACTOR_ID);
    w.set_actor_emails(
        "alice",
        &[
            email("alice@corp.test", true, true),
            email("alice@old.test", false, false),
        ],
        system,
    )
    .unwrap();
    w.set_actor_emails("bob", &[email("bob@corp.test", true, true)], system)
        .unwrap();
    let repo = w
        .create_repo(alice.id, "webapp", None, false, "main", by(alice.id))
        .unwrap();
    Fixture {
        w,
        alice: alice.id,
        bob: bob.id,
        repo: repo.id,
        _dir: dir,
    }
}

fn ssh(key: &str, verifies: bool, email: &str) -> CommitFacts {
    CommitFacts {
        sha: TIP.to_string(),
        committer_email: email.to_string(),
        signature: CommitSignature::Ssh {
            key: key.to_string(),
            verifies,
        },
    }
}

fn verdict(f: &Fixture, facts: &CommitFacts) -> Verdict {
    f.w.commit_verification(f.repo, facts).unwrap().verdict
}

/// A push of `main` to [`TIP`], with `signed` as what its commit says.
fn push(f: &Fixture, signed: &[CommitFacts]) {
    let first = RefMove {
        name: "refs/heads/main",
        old: ZERO,
        new: TIP,
        repins: Vec::new(),
        change: RefChange::Created,
        forced: false,
        commits: vec![TIP.to_string()],
        commit_count: 1,
    };
    f.w.record_push(f.repo, by(f.alice), &[first], None, signed)
        .unwrap();
}

fn types(f: &Fixture) -> Vec<&'static str> {
    f.w.log_after(0, 1000)
        .unwrap()
        .into_iter()
        .map(|e| e.body.type_name())
        .collect()
}

#[test]
fn a_key_is_stored_without_its_comment_and_returns_its_row() {
    let f = fixture();
    let added =
        f.w.add_signing_key(by(f.alice), &format!("{KEY} alice@laptop"), Some("laptop"))
            .unwrap();
    assert_eq!(added.key, KEY);
    assert_eq!(added.title.as_deref(), Some("laptop"));
    assert_eq!(f.w.signing_keys(f.alice).unwrap(), vec![added.clone()]);
    let last = f.w.log_after(0, 1000).unwrap().pop().unwrap();
    match last.body {
        EventBody::SshSigningKeyCreated(e) => {
            assert_eq!((e.subject.id, e.subject.owner.id), (added.id, f.alice));
            assert_eq!(e.object.key, KEY);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_key_one_account_holds_is_refused_to_every_account() {
    let f = fixture();
    f.w.add_signing_key(by(f.alice), KEY, None).unwrap();
    for who in [f.alice, f.bob] {
        assert!(matches!(
            f.w.add_signing_key(by(who), &format!("{KEY} another comment"), None),
            Err(WorldError::SigningKeyExists)
        ));
    }
    assert!(matches!(
        f.w.add_signing_key(by(f.bob), "ssh-ed25519 AAAA", None),
        Err(WorldError::InvalidSigningKey(_))
    ));
    assert!(f.w.signing_keys(f.bob).unwrap().is_empty());
}

#[test]
fn only_its_holder_deletes_a_key_and_its_id_is_never_reused() {
    let f = fixture();
    let key = f.w.add_signing_key(by(f.alice), KEY, None).unwrap();
    assert!(matches!(
        f.w.delete_signing_key(by(f.bob), key.id),
        Err(WorldError::NoSuchSigningKey(_))
    ));
    f.w.delete_signing_key(by(f.alice), key.id).unwrap();
    assert!(matches!(
        f.w.delete_signing_key(by(f.alice), key.id),
        Err(WorldError::NoSuchSigningKey(_))
    ));
    let again = f.w.add_signing_key(by(f.alice), KEY, None).unwrap();
    assert!(again.id > key.id);
    assert_eq!(
        types(&f)[types(&f).len() - 3..],
        [
            "ssh_signing_key:created",
            "ssh_signing_key:deleted",
            "ssh_signing_key:created"
        ]
    );
}

#[test]
fn the_verdict_takes_the_first_fault_that_applies() {
    let f = fixture();
    f.w.add_signing_key(by(f.alice), KEY, None).unwrap();
    let facts = |signature| CommitFacts {
        sha: TIP.to_string(),
        committer_email: "alice@corp.test".to_string(),
        signature,
    };
    let cases = [
        (facts(CommitSignature::Unsigned), Verdict::Unsigned),
        (facts(CommitSignature::Other), Verdict::UnknownKey),
        (facts(CommitSignature::MalformedSsh), Verdict::Malformed),
        // An unregistered key, whether or not the signature verifies.
        (
            ssh(OTHER_KEY, false, "alice@corp.test"),
            Verdict::UnknownKey,
        ),
        (ssh(OTHER_KEY, true, "alice@corp.test"), Verdict::UnknownKey),
        // A registered key, before any address is looked at.
        (ssh(KEY, false, "nobody@else.test"), Verdict::BadSignature),
        (ssh(KEY, true, "alice@corp.test"), Verdict::Verified),
        (ssh(KEY, true, "ALICE@Corp.test"), Verdict::Verified),
        (ssh(KEY, true, "alice@old.test"), Verdict::UnverifiedAddress),
        (
            ssh(KEY, true, "bob@corp.test"),
            Verdict::OtherAccountsAddress,
        ),
        (ssh(KEY, true, "nobody@else.test"), Verdict::NoAccount),
    ];
    for (facts, expected) in cases {
        assert_eq!(verdict(&f, &facts), expected, "{facts:?}");
    }
}

#[test]
fn a_push_stores_a_passing_verdict_that_outlives_its_key() {
    let f = fixture();
    let key = f.w.add_signing_key(by(f.alice), KEY, None).unwrap();
    f.w.advance_clock(60, by(SYSTEM_ACTOR_ID)).unwrap();
    let signed = ssh(KEY, true, "alice@corp.test");
    push(&f, std::slice::from_ref(&signed));

    let stored = CommitVerification {
        verdict: Verdict::Verified,
        stored_clock_unix: Some(DEFAULT_T0_UNIX + 60),
    };
    assert_eq!(f.w.commit_verification(f.repo, &signed).unwrap(), stored);
    assert_eq!(
        f.w.stored_commit_verification(f.repo, TIP).unwrap(),
        Some(DEFAULT_T0_UNIX + 60)
    );
    let verified = f.w.log_after(0, 1000).unwrap().pop().unwrap();
    match verified.body {
        EventBody::CommitVerified(e) => {
            assert_eq!((e.subject.sha.as_str(), e.subject.repo.id), (TIP, f.repo));
            assert_eq!(
                (
                    e.object.signer.id,
                    e.object.signing_key.id,
                    e.object.verified_unix
                ),
                (f.alice, key.id, DEFAULT_T0_UNIX + 60)
            );
        }
        other => panic!("{other:?}"),
    }

    // The key is gone, but the verdict the push stored stands.
    f.w.delete_signing_key(by(f.alice), key.id).unwrap();
    assert_eq!(f.w.commit_verification(f.repo, &signed).unwrap(), stored);
}

#[test]
fn a_push_stores_no_failing_verdict_so_a_read_reaches_it_again() {
    let f = fixture();
    f.w.add_signing_key(by(f.alice), KEY, None).unwrap();
    let signed = ssh(KEY, true, "alice@old.test");
    push(&f, std::slice::from_ref(&signed));
    assert_eq!(*types(&f).last().unwrap(), "ref:created");
    assert_eq!(verdict(&f, &signed), Verdict::UnverifiedAddress);

    // Verifying the address fixes the fault, and the next read says so.
    f.w.set_actor_emails(
        "alice",
        &[ActorEmail {
            email: "alice@old.test".to_string(),
            primary: true,
            verified: true,
        }],
        by(SYSTEM_ACTOR_ID),
    )
    .unwrap();
    assert_eq!(
        f.w.commit_verification(f.repo, &signed).unwrap(),
        CommitVerification {
            verdict: Verdict::Verified,
            stored_clock_unix: None,
        }
    );
}

#[test]
fn a_commit_a_push_lists_again_keeps_the_verdict_it_has() {
    let f = fixture();
    f.w.add_signing_key(by(f.alice), KEY, None).unwrap();
    let signed = ssh(KEY, true, "alice@corp.test");
    push(&f, std::slice::from_ref(&signed));
    f.w.advance_clock(60, by(SYSTEM_ACTOR_ID)).unwrap();
    let again = RefMove {
        name: "refs/heads/copy",
        old: ZERO,
        new: TIP,
        repins: Vec::new(),
        change: RefChange::Created,
        forced: false,
        commits: vec![TIP.to_string()],
        commit_count: 1,
    };
    f.w.record_push(f.repo, by(f.alice), &[again], None, &[signed])
        .unwrap();
    assert_eq!(*types(&f).last().unwrap(), "ref:created");
    assert_eq!(
        f.w.stored_commit_verification(f.repo, TIP).unwrap(),
        Some(DEFAULT_T0_UNIX)
    );
}
