//! Actor emails and the OAuth grant: the single-row-write rule (returned row ==
//! point read), validation, and the code's single use and expiry on the
//! virtual clock.

use orrery_world::{
    ActorEmail, ActorKind, DEFAULT_T0_UNIX, OAUTH_CODE_TTL_SECONDS, World, WorldError,
};

fn fresh() -> (tempfile::TempDir, World) {
    let dir = tempfile::tempdir().expect("tempdir");
    let world = World::create(&dir.path().join("world"), DEFAULT_T0_UNIX).expect("create world");
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
    assert!(app.client_id.starts_with("orr_client_"));
    assert!(app.client_secret.starts_with("orr_secret_"));

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

    for (name, url, field) in [
        ("", "http://auth.test/cb", "name"),
        ("x", "auth.test/cb", "callback_url"),
        ("x", "ftp://auth.test/cb", "callback_url"),
        ("x", "http:///cb", "callback_url"),
    ] {
        match w.create_oauth_app(name, url, sys.id) {
            Err(WorldError::InvalidOAuthApp { field: f, .. }) => assert_eq!(f, field),
            other => panic!("({name:?}, {url:?}) gave {other:?}"),
        }
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
