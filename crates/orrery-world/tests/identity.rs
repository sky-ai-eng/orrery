//! Actor emails and the OAuth grant: the single-row-write rule (returned row ==
//! point read), validation, and the code's single use and expiry on the
//! virtual clock.

use orrery_world::{
    ActorEmail, ActorKind, Attribution, DEFAULT_T0_UNIX, Door, EventBody, OAUTH_CODE_TTL_SECONDS,
    World, WorldError, WorldSecret,
};

/// A write by `actor_id` as the REST face makes it.
fn by(actor_id: i64) -> Attribution {
    Attribution::new(actor_id, Door::Rest)
}

fn fresh() -> (tempfile::TempDir, World) {
    let dir = tempfile::tempdir().expect("tempdir");
    let world = World::create(
        &dir.path().join("world"),
        DEFAULT_T0_UNIX,
        &WorldSecret::LOCAL,
    )
    .expect("create world");
    (dir, world)
}

fn email(addr: &str, primary: bool, verified: bool) -> ActorEmail {
    ActorEmail {
        email: addr.to_string(),
        primary,
        verified,
    }
}

#[test]
fn a_new_actor_holds_one_derived_address() {
    let (_dir, w) = fresh();
    let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
    assert_eq!(
        w.actor_emails(alice.id).unwrap(),
        vec![email("alice@orrery.invalid", true, true)]
    );
}

#[test]
fn set_actor_emails_returns_the_stored_rows() {
    let (_dir, w) = fresh();
    let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
    let sys = w.actor_by_login("orrery").unwrap().unwrap();
    let returned = w
        .set_actor_emails(
            "alice",
            &[
                email("zed@personal.test", false, false),
                email("alice@corp.test", true, true),
                email("Alice@other.test", false, true),
            ],
            by(sys.id),
        )
        .unwrap();
    assert_eq!(returned, w.actor_emails(alice.id).unwrap());
    assert_eq!(returned[0].email, "alice@corp.test", "primary first");

    let logged = w.log_after(0, 100).unwrap();
    let set = logged
        .iter()
        .find(|e| e.body.type_name() == "actor:emails_set")
        .expect("logged");
    assert_eq!(set.actor.login, "orrery");
    let EventBody::ActorEmailsSet(set) = &set.body else {
        unreachable!()
    };
    assert_eq!(set.subject.login, "alice");
    let emails = set.changes.emails.as_ref().expect("the emails changed");
    assert_eq!(emails.from.len(), 1, "the address an actor starts with");
    assert_eq!(emails.to, returned);
}

#[test]
fn set_actor_emails_validates_the_whole_set() {
    let (_dir, w) = fresh();
    let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
    let sys = w.actor_by_login("orrery").unwrap().unwrap();
    for bad in [
        vec![],
        vec![email("a@x.test", false, true)],
        vec![email("a@x.test", true, true), email("b@x.test", true, true)],
        vec![email("not-an-address", true, true)],
        vec![email("a b@x.test", true, true)],
        vec![
            email("a@x.test", true, true),
            email("A@X.test", false, true),
        ],
    ] {
        assert!(
            matches!(
                w.set_actor_emails("alice", &bad, by(sys.id)),
                Err(WorldError::InvalidEmails(_))
            ),
            "{bad:?} accepted"
        );
    }
    assert_eq!(
        w.actor_emails(alice.id).unwrap(),
        vec![email("alice@orrery.invalid", true, true)],
        "a refused set changes nothing"
    );
    assert!(matches!(
        w.set_actor_emails("ghost", &[email("g@x.test", true, true)], by(sys.id)),
        Err(WorldError::NoSuchActor(_))
    ));
    assert!(matches!(
        w.set_actor_emails("orrery", &[email("o@x.test", true, true)], by(sys.id)),
        Err(WorldError::NoSuchActor(_))
    ));
}

#[test]
fn create_oauth_app_returns_the_stored_row_with_derived_credentials() {
    let (_dir, w) = fresh();
    let sys = w.actor_by_login("orrery").unwrap().unwrap();
    let app = w
        .create_oauth_app("gotrue", "http://auth.test/callback", by(sys.id))
        .unwrap();
    assert_eq!(
        Some(app.clone()),
        w.oauth_app_by_client_id(&app.client_id).unwrap()
    );
    assert!(app.client_id.starts_with("orrfake_client_"));
    assert!(app.client_secret.starts_with("orrfake_secret_"));

    let (_dir2, w2) = fresh();
    let sys2 = w2.actor_by_login("orrery").unwrap().unwrap();
    let again = w2
        .create_oauth_app("gotrue", "http://auth.test/callback", by(sys2.id))
        .unwrap();
    assert_eq!(
        (app.client_id, app.client_secret),
        (again.client_id, again.client_secret),
        "replay issues the same credentials"
    );

    // A name is measured in characters, not bytes: each of these is three.
    let longest = "名".repeat(100);
    let app = w
        .create_oauth_app(&longest, "http://auth.test/cb", by(sys.id))
        .unwrap();
    assert_eq!(app.name, longest);

    let too_long = "名".repeat(101);
    for (name, url, field) in [
        ("", "http://auth.test/cb", "name"),
        (too_long.as_str(), "http://auth.test/cb", "name"),
        ("x", "auth.test/cb", "callback_url"),
        ("x", "ftp://auth.test/cb", "callback_url"),
        ("x", "http:///cb", "callback_url"),
    ] {
        // The message names the case, not the result: an app created by
        // mistake would print its client secret.
        let refused = matches!(
            w.create_oauth_app(name, url, by(sys.id)),
            Err(WorldError::InvalidOAuthApp { field: f, .. }) if f == field
        );
        assert!(refused, "({name:?}, {url:?}) was not refused on {field}");
    }
}

#[test]
fn an_oauth_code_is_spent_once_and_expires_on_the_virtual_clock() {
    let (_dir, w) = fresh();
    let sys = w.actor_by_login("orrery").unwrap().unwrap();
    let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
    let app = w
        .create_oauth_app("gotrue", "http://auth.test/callback", by(sys.id))
        .unwrap();
    let redirect = "http://auth.test/callback";

    let code = w
        .issue_oauth_code(&app, &alice, redirect, "user:email")
        .unwrap();
    assert!(matches!(
        w.exchange_oauth_code(&app.client_id, "wrong", &code, None),
        Err(WorldError::OAuthClientCredentials)
    ));
    assert!(matches!(
        w.exchange_oauth_code(
            &app.client_id,
            &app.client_secret,
            &code,
            Some("http://auth.test/other")
        ),
        Err(WorldError::OAuthRedirectMismatch)
    ));
    let token = w
        .exchange_oauth_code(&app.client_id, &app.client_secret, &code, Some(redirect))
        .unwrap();
    assert_eq!(token.actor, alice);
    assert_eq!(token.scope, "user:email");
    assert_eq!(w.actor_by_token(&token.token).unwrap(), Some(alice.clone()));
    assert!(matches!(
        w.exchange_oauth_code(&app.client_id, &app.client_secret, &code, None),
        Err(WorldError::OAuthBadCode)
    ));

    let late = w.issue_oauth_code(&app, &alice, redirect, "").unwrap();
    w.advance_clock(OAUTH_CODE_TTL_SECONDS + 1, by(sys.id))
        .unwrap();
    assert!(matches!(
        w.exchange_oauth_code(&app.client_id, &app.client_secret, &late, None),
        Err(WorldError::OAuthBadCode)
    ));

    let actions: Vec<(&str, Door, Option<i64>)> = w
        .log_after(0, 100)
        .unwrap()
        .into_iter()
        .filter(|e| e.actor.id == alice.id)
        .map(|e| (e.body.type_name(), e.door, e.app.map(|a| a.id)))
        .collect();
    let through = |t| (t, Door::Web, Some(app.id));
    assert!(actions.contains(&through("oauth_authorization:created")));
    assert!(actions.contains(&through("oauth_authorization:exchanged")));
}

#[test]
fn an_issued_client_secret_exchanges_beside_the_registration_secret() {
    let (_dir, w) = fresh();
    let sys = w.actor_by_login("orrery").unwrap().unwrap();
    let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
    let redirect = "http://auth.test/callback";
    let app = w.create_oauth_app("gotrue", redirect, by(sys.id)).unwrap();
    let other = w.create_oauth_app("other", redirect, by(sys.id)).unwrap();

    let first = w
        .issue_oauth_client_secret(&app.client_id, by(sys.id))
        .unwrap();
    let second = w
        .issue_oauth_client_secret(&app.client_id, by(sys.id))
        .unwrap();
    assert!(first.starts_with("orrfake_secret_"));
    assert_ne!(first, second);
    assert_ne!(first, app.client_secret);
    assert_eq!(
        w.oauth_app_by_client_id(&app.client_id).unwrap(),
        Some(app.clone()),
        "the app keeps the secret it was registered with"
    );

    for secret in [&first, &second, &app.client_secret] {
        let code = w.issue_oauth_code(&app, &alice, redirect, "").unwrap();
        let token = w
            .exchange_oauth_code(&app.client_id, secret, &code, None)
            .unwrap();
        assert_eq!(token.actor, alice);
    }
    // A secret is the app's own: it names no other app.
    let code = w.issue_oauth_code(&other, &alice, redirect, "").unwrap();
    assert!(matches!(
        w.exchange_oauth_code(&other.client_id, &first, &code, None),
        Err(WorldError::OAuthClientCredentials)
    ));

    assert!(matches!(
        w.issue_oauth_client_secret("orrfake_client_nope", by(sys.id)),
        Err(WorldError::NoSuchOAuthApp(_))
    ));

    let log = w.log_after(0, 100).unwrap();
    let issued: Vec<_> = log
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::OAuthAppSecretIssued(i) => {
                Some((e.actor.login.as_str(), i.subject.client_id.as_str(), i.n))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        issued,
        [1, 2].map(|n| ("orrery", app.client_id.as_str(), n))
    );
    let wire = serde_json::to_string(&log).unwrap();
    for (which, secret) in [("first", &first), ("second", &second)] {
        assert!(
            !wire.contains(secret.as_str()),
            "the log leaks the {which} issued secret"
        );
    }
}
