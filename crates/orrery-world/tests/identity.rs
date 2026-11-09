//! Actor emails and the OAuth grant: the single-row-write rule (returned row ==
//! point read), validation, and the code's single use and expiry on the
//! virtual clock.

use orrery_world::{
    ActorEmail, ActorKind, DEFAULT_T0_UNIX, OAUTH_CODE_TTL_SECONDS, World, WorldError, WorldSecret,
};

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
            sys.id,
        )
        .unwrap();
    assert_eq!(returned, w.actor_emails(alice.id).unwrap());
    assert_eq!(returned[0].email, "alice@corp.test", "primary first");

    let logged = w.log_after(0, 100).unwrap();
    let set = logged
        .iter()
        .find(|e| e.action == "actor.emails.set")
        .expect("logged");
    assert_eq!(set.actor_login, "orrery");
    assert_eq!(set.details["login"], "alice");
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
                w.set_actor_emails("alice", &bad, sys.id),
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
        w.set_actor_emails("ghost", &[email("g@x.test", true, true)], sys.id),
        Err(WorldError::NoSuchActor(_))
    ));
    assert!(matches!(
        w.set_actor_emails("orrery", &[email("o@x.test", true, true)], sys.id),
        Err(WorldError::NoSuchActor(_))
    ));
}

#[test]
fn create_oauth_app_returns_the_stored_row_with_derived_credentials() {
    let (_dir, w) = fresh();
    let sys = w.actor_by_login("orrery").unwrap().unwrap();
    let app = w
        .create_oauth_app("gotrue", "http://auth.test/callback", sys.id)
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
        .create_oauth_app("gotrue", "http://auth.test/callback", sys2.id)
        .unwrap();
    assert_eq!(
        (app.client_id, app.client_secret),
        (again.client_id, again.client_secret),
        "replay issues the same credentials"
    );

    // A name is measured in characters, not bytes: each of these is three.
    let longest = "名".repeat(100);
    let app = w
        .create_oauth_app(&longest, "http://auth.test/cb", sys.id)
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
            w.create_oauth_app(name, url, sys.id),
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
        .create_oauth_app("gotrue", "http://auth.test/callback", sys.id)
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
    w.advance_clock(OAUTH_CODE_TTL_SECONDS + 1, sys.id).unwrap();
    assert!(matches!(
        w.exchange_oauth_code(&app.client_id, &app.client_secret, &late, None),
        Err(WorldError::OAuthBadCode)
    ));

    let actions: Vec<String> = w
        .log_after(0, 100)
        .unwrap()
        .into_iter()
        .map(|e| e.action)
        .collect();
    assert!(actions.contains(&"oauth.authorize".to_string()));
    assert!(actions.contains(&"oauth.token".to_string()));
}

#[test]
fn an_issued_client_secret_exchanges_beside_the_registration_secret() {
    let (_dir, w) = fresh();
    let sys = w.actor_by_login("orrery").unwrap().unwrap();
    let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
    let redirect = "http://auth.test/callback";
    let app = w.create_oauth_app("gotrue", redirect, sys.id).unwrap();
    let other = w.create_oauth_app("other", redirect, sys.id).unwrap();

    let first = w.issue_oauth_client_secret(&app.client_id, sys.id).unwrap();
    let second = w.issue_oauth_client_secret(&app.client_id, sys.id).unwrap();
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
        w.issue_oauth_client_secret("orrfake_client_nope", sys.id),
        Err(WorldError::NoSuchOAuthApp(_))
    ));

    let log = w.log_after(0, 100).unwrap();
    let issued: Vec<_> = log
        .iter()
        .filter(|e| e.action == "oauth_app.secret.issue")
        .map(|e| (e.actor_login.as_str(), e.details.clone()))
        .collect();
    assert_eq!(
        issued,
        [1, 2].map(|n| (
            "orrery",
            serde_json::json!({ "client_id": app.client_id, "n": n })
        ))
    );
    let wire = serde_json::to_string(&log).unwrap();
    for (which, secret) in [("first", &first), ("second", &second)] {
        assert!(
            !wire.contains(secret.as_str()),
            "the log leaks the {which} issued secret"
        );
    }
}
