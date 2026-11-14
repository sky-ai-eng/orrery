//! Apps, their keys, their installations, and installation tokens: the
//! write rule (what a write returns is what a read answers), each refusal,
//! and what a token reaches and for how long.

use orrery_world::{
    AccessLevel, ActorKind, Attribution, DEFAULT_T0_UNIX, Door, EventBody,
    INSTALLATION_TOKEN_TTL_SECONDS, NewApp, Permissions, RepositorySelection, SYSTEM_ACTOR_ID,
    World, WorldError, WorldSecret,
};

const SYSTEM: Attribution = Attribution::new(SYSTEM_ACTOR_ID, Door::Control);

const KEY: &str = "-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAyQDurX+SGDbR9X9O5rwY
XND5rbR9L5MHPsh5fy9PCvnjEtf9vd+gbv+MKb5isMFXGXS2Jwc/zZ4mitvK/TUB
FZD5K4Q53vIVpt54aWggRB9a3KaePpA8hMrLDklc2JaVcUtOqR2U8okAqQhagx+V
Hkjnfr2FKrRaIcA/vRvx/o+9yqSP/6TP3Kp5kMpxMBuCIPs3kF2WzxnvSrw+91Qp
CljNv66agiv3iPnNWyeaOe5C/0YFg3PRLVJq03K/LT4cHaUGM/tSFJLGj9BwyxHT
67koh9CtImSoshN5v/41kPYAuxUYzKXmUr9JGENNAIaUkrl7Gr3CzFnUIFfuCpd9
uQIDAQAB
-----END PUBLIC KEY-----
";

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

fn permissions(pairs: &[(&str, AccessLevel)]) -> Permissions {
    pairs
        .iter()
        .map(|(name, level)| (name.to_string(), *level))
        .collect()
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| v.to_string()).collect()
}

/// Alice and the organization acme, which owns `web` and `api`, and the app
/// `helper`, owned by acme.
fn seeded() -> (tempfile::TempDir, World, i64) {
    let (dir, w) = fresh();
    let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
    let (acme, _) = w.create_actor("acme", ActorKind::Org).unwrap();
    let by = Attribution::new(alice.id, Door::Rest);
    for name in ["web", "api"] {
        w.create_repo(acme.id, name, None, true, "main", by)
            .unwrap();
    }
    w.create_repo(alice.id, "own", None, true, "main", by)
        .unwrap();
    let app = w
        .create_app(
            &NewApp {
                slug: "helper",
                name: "Helper",
                owner: "acme",
                callback_urls: &strings(&["https://helper.test/a", "https://helper.test/b"]),
                permissions: &permissions(&[
                    ("contents", AccessLevel::Write),
                    ("metadata", AccessLevel::Read),
                ]),
                events: &strings(&["push"]),
                hook_url: None,
                hook_active: false,
            },
            SYSTEM,
        )
        .unwrap();
    (dir, w, app.app.id)
}

fn last_event(w: &World) -> orrery_world::Event {
    w.log_after(0, 1000).unwrap().pop().unwrap()
}

#[test]
fn create_app_returns_the_app_a_read_answers() {
    let (dir, w) = fresh();
    let _ = dir;
    w.create_actor("acme", ActorKind::Org).unwrap();
    let urls = strings(&["https://helper.test/a", "https://helper.test/b"]);
    let registered = w
        .create_app(
            &NewApp {
                slug: "helper",
                name: "Helper",
                owner: "acme",
                callback_urls: &urls,
                permissions: &permissions(&[("contents", AccessLevel::Write)]),
                events: &strings(&["push", "pull_request"]),
                hook_url: None,
                hook_active: false,
            },
            SYSTEM,
        )
        .unwrap();
    let app = registered.app;
    assert_eq!(app.slug, "helper");
    assert_eq!(app.owner.login, "acme");
    assert_eq!(app.bot.login, "helper");
    assert_eq!(app.bot.kind, ActorKind::Bot);
    assert_eq!(app.callback_urls, urls);
    assert_eq!(app.events, ["push", "pull_request"]);
    assert_eq!(app.created_clock_unix, DEFAULT_T0_UNIX);
    assert!(registered.client_secret.starts_with("orrfake_secret_"));

    assert_eq!(w.app_by_slug("HELPER").unwrap().as_ref(), Some(&app));
    assert_eq!(w.app_by_id(app.id).unwrap().as_ref(), Some(&app));
    assert_eq!(
        w.app_by_client_id(&app.client_id).unwrap().as_ref(),
        Some(&app)
    );
    assert_eq!(w.apps_page(None, 10).unwrap(), (vec![app.clone()], 1));
    assert_eq!(w.app_by_slug("nobody").unwrap(), None);

    // The app is its OAuth app, under one id, with one of its callbacks.
    let oauth = w.oauth_app_by_client_id(&app.client_id).unwrap().unwrap();
    assert_eq!((oauth.id, oauth.name.as_str()), (app.id, "Helper"));
    assert_eq!(oauth.client_secret, registered.client_secret);
    assert_eq!(w.oauth_callback_urls(app.id).unwrap(), urls);
    assert!(
        w.oauth_app_holds_secret(app.id, &registered.client_secret)
            .unwrap()
    );
    assert!(!w.oauth_app_holds_secret(app.id, "wrong").unwrap());

    // One write: the app, then its bot and its OAuth app.
    let log = w.log_after(0, 100).unwrap();
    let types: Vec<_> = log.iter().map(|e| e.body.type_name()).collect();
    assert_eq!(
        types[types.len() - 3..],
        ["app:created", "actor:created", "oauth_app:created"]
    );
    let write = log[log.len() - 3].write;
    assert_eq!(write.events, 3);
    assert!(log[log.len() - 3..].iter().all(|e| e.write == write));
}

#[test]
fn an_app_takes_a_bot_that_has_none_and_no_other_login() {
    let (_dir, w) = fresh();
    w.create_actor("alice", ActorKind::User).unwrap();
    let (ci, _) = w.create_actor("ci", ActorKind::Bot).unwrap();
    let urls = strings(&["https://ci.test/cb"]);
    let new = |slug: &'static str, owner: &'static str| {
        w.create_app(
            &NewApp {
                slug,
                name: "CI",
                owner,
                callback_urls: &urls,
                permissions: &Permissions::new(),
                events: &[],
                hook_url: None,
                hook_active: false,
            },
            SYSTEM,
        )
    };
    let app = new("ci", "alice").unwrap().app;
    assert_eq!(app.bot, ci);
    assert_eq!(last_event(&w).body.type_name(), "oauth_app:created");
    assert!(matches!(new("ci", "alice"), Err(WorldError::AppExists(s)) if s == "ci"));
    assert!(matches!(new("alice", "alice"), Err(WorldError::AppSlugTaken(s)) if s == "alice"));
    for owner in ["nobody", "ci", "orrery"] {
        assert!(
            matches!(
                new("other", owner),
                Err(WorldError::InvalidApp { field: "owner", .. })
            ),
            "{owner}"
        );
    }
    // A refused registration leaves nothing behind.
    assert_eq!(w.actor_by_login("other").unwrap(), None);
    assert_eq!(w.apps_page(None, 10).unwrap().1, 1);
}

#[test]
fn an_app_value_is_refused_under_its_own_field() {
    let (_dir, w) = fresh();
    w.create_actor("alice", ActorKind::User).unwrap();
    let good_urls = strings(&["https://ci.test/cb"]);
    let refused = |new: NewApp<'_>| match w.create_app(&new, SYSTEM) {
        Err(WorldError::InvalidApp { field, .. }) => field,
        other => panic!("expected a refusal, got {other:?}"),
    };
    let base = NewApp {
        slug: "ci",
        name: "CI",
        owner: "alice",
        callback_urls: &good_urls,
        permissions: &Permissions::new(),
        events: &[],
        hook_url: None,
        hook_active: false,
    };
    assert_eq!(
        refused(NewApp {
            slug: "-ci",
            ..base
        }),
        "slug"
    );
    assert_eq!(
        refused(NewApp {
            slug: "orrery",
            ..base
        }),
        "slug"
    );
    assert_eq!(refused(NewApp { name: " ", ..base }), "name");
    for urls in [
        strings(&[]),
        strings(&["ftp://ci.test/cb"]),
        strings(&["https://ci.test/cb", "https://ci.test/cb"]),
        vec!["https://ci.test/cb".to_string(); 11],
    ] {
        assert_eq!(
            refused(NewApp {
                callback_urls: &urls,
                ..base
            }),
            "callback_urls"
        );
    }
    assert_eq!(
        refused(NewApp {
            permissions: &permissions(&[("Contents", AccessLevel::Read)]),
            ..base
        }),
        "permissions"
    );
    for events in [
        strings(&["Push"]),
        strings(&["push", "push"]),
        strings(&[""]),
    ] {
        assert_eq!(
            refused(NewApp {
                events: &events,
                ..base
            }),
            "events"
        );
    }
}

#[test]
fn add_app_key_returns_the_key_a_read_answers() {
    let (_dir, w, app_id) = seeded();
    let key = w.add_app_key("helper", KEY, SYSTEM).unwrap();
    assert_eq!(key.created_clock_unix, DEFAULT_T0_UNIX);
    assert_eq!(w.app_keys(app_id).unwrap(), vec![key.clone()]);
    assert_eq!(last_event(&w).body.type_name(), "app_key:created");
    assert!(matches!(
        w.add_app_key("helper", KEY, SYSTEM),
        Err(WorldError::AppKeyExists)
    ));
    assert!(matches!(
        w.add_app_key("nobody", KEY, SYSTEM),
        Err(WorldError::NoSuchApp(_))
    ));
    assert!(matches!(
        w.add_app_key("helper", "not a key", SYSTEM),
        Err(WorldError::InvalidAppKey(_))
    ));
    assert_eq!(w.app_keys(app_id).unwrap(), vec![key]);
}

#[test]
fn create_installation_returns_the_installation_a_read_answers() {
    let (_dir, w, app_id) = seeded();
    let all = w
        .create_installation("helper", "acme", None, SYSTEM)
        .unwrap();
    assert_eq!(all.app_id, app_id);
    assert_eq!(all.app_slug, "helper");
    assert_eq!(all.account.login, "acme");
    assert_eq!(all.repository_selection, RepositorySelection::All);
    assert!(all.repositories.is_empty());
    assert_eq!(all.permissions["contents"], AccessLevel::Write);
    assert_eq!(all.events, ["push"]);
    assert_eq!(w.installation_by_id(all.id).unwrap().as_ref(), Some(&all));
    assert_eq!(last_event(&w).body.type_name(), "installation:created");

    let chosen = w
        .create_installation("helper", "alice", Some(&strings(&["own"])), SYSTEM)
        .unwrap();
    assert_eq!(chosen.repository_selection, RepositorySelection::Selected);
    assert_eq!(chosen.repositories, ["own"]);
    assert_eq!(
        w.installation_by_id(chosen.id).unwrap().as_ref(),
        Some(&chosen)
    );

    let both = vec![all.clone(), chosen.clone()];
    assert_eq!(
        w.installations_page(app_id, None, 10).unwrap(),
        (both.clone(), 2)
    );
    assert_eq!(
        w.installations_page(app_id, Some(all.id), 10).unwrap(),
        (vec![chosen.clone()], 2)
    );
    // The face's list is newest first.
    assert_eq!(
        w.installations_slice(app_id, 0, 1).unwrap(),
        (vec![chosen], 2)
    );
    assert_eq!(
        w.installations_slice(app_id, 1, 10).unwrap(),
        (vec![all], 2)
    );
    assert_eq!(w.installations_slice(app_id, 0, 0).unwrap(), (vec![], 2));
    assert_eq!(w.installation_by_id(999).unwrap(), None);

    let refused = |account: &str, repositories: Option<&[String]>| {
        w.create_installation("helper", account, repositories, SYSTEM)
    };
    assert!(matches!(
        refused("acme", None),
        Err(WorldError::InstallationExists { .. })
    ));
    for account in ["nobody", "helper", "orrery"] {
        assert!(
            matches!(
                refused(account, None),
                Err(WorldError::InvalidInstallation {
                    field: "account",
                    ..
                })
            ),
            "{account}"
        );
    }
    w.create_actor("bob", ActorKind::User).unwrap();
    for repositories in [strings(&[]), strings(&["web"]), strings(&["nothing"])] {
        assert!(
            matches!(
                refused("bob", Some(&repositories)),
                Err(WorldError::InvalidInstallation {
                    field: "repositories",
                    ..
                })
            ),
            "{repositories:?}"
        );
    }
    assert!(matches!(
        w.create_installation("nobody", "bob", None, SYSTEM),
        Err(WorldError::NoSuchApp(_))
    ));
    assert_eq!(w.installations_page(app_id, None, 10).unwrap().1, 2);
}

#[test]
fn an_installation_token_acts_as_the_bot_until_it_expires() {
    let (_dir, w, app_id) = seeded();
    let installation = w
        .create_installation("helper", "acme", None, SYSTEM)
        .unwrap();
    let issued = w
        .create_installation_token(app_id, installation.id, None, None)
        .unwrap();
    assert!(issued.token.starts_with("orrfake_"));
    assert_eq!(issued.repositories, None);
    assert_eq!(issued.permissions, installation.permissions);
    assert_eq!(
        issued.expires_clock_unix,
        DEFAULT_T0_UNIX + INSTALLATION_TOKEN_TTL_SECONDS
    );

    // The issue is the bot's write, through the app.
    let event = last_event(&w);
    assert_eq!(event.actor.login, "helper");
    assert_eq!(event.door, Door::Rest);
    assert_eq!(event.app.as_ref().map(|a| a.id), Some(app_id));
    let EventBody::InstallationTokenCreated(created) = &event.body else {
        panic!("expected a token event, got {}", event.body.type_name());
    };
    assert_eq!(created.subject.installation.id, installation.id);
    assert_eq!(created.object.expires_unix, issued.expires_clock_unix);

    let caller = w.caller_by_token(&issued.token).unwrap().unwrap();
    assert_eq!(caller.actor.login, "helper");
    assert_eq!(caller.app_id, Some(app_id));
    let grant = caller.installation.clone().unwrap();
    assert_eq!(grant.installation_id, installation.id);
    assert_eq!(grant.repos, None);
    let repo = |owner: &str, name: &str| w.repo_by_owner_name(owner, name).unwrap().unwrap();
    assert!(caller.reaches(&repo("acme", "web")));
    assert!(!caller.reaches(&repo("alice", "own")));
    let names: Vec<_> = w
        .granted_repos(&grant)
        .unwrap()
        .into_iter()
        .map(|r| r.name)
        .collect();
    assert_eq!(names, ["web", "api"]);

    // A repository the account gets later is covered too.
    let acme = w.actor_by_login("acme").unwrap().unwrap();
    w.create_repo(acme.id, "late", None, true, "main", SYSTEM)
        .unwrap();
    assert!(caller.reaches(&repo("acme", "late")));

    // A second token is another token.
    let second = w
        .create_installation_token(app_id, installation.id, None, None)
        .unwrap();
    assert_ne!(second.token, issued.token);

    w.advance_clock(INSTALLATION_TOKEN_TTL_SECONDS - 1, SYSTEM)
        .unwrap();
    assert!(w.caller_by_token(&issued.token).unwrap().is_some());
    w.advance_clock(1, SYSTEM).unwrap();
    assert_eq!(w.caller_by_token(&issued.token).unwrap(), None);
}

#[test]
fn a_token_is_narrowed_to_repositories_and_permissions_the_installation_holds() {
    let (_dir, w, app_id) = seeded();
    let all = w
        .create_installation("helper", "acme", None, SYSTEM)
        .unwrap();
    let web_id = w.repo_by_owner_name("acme", "web").unwrap().unwrap().id;

    let issued = w
        .create_installation_token(
            app_id,
            all.id,
            Some(&strings(&["web", "WEB"])),
            Some(&permissions(&[("contents", AccessLevel::Read)])),
        )
        .unwrap();
    let narrowed: Vec<_> = issued
        .repositories
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(narrowed, [web_id]);
    assert_eq!(
        issued.permissions,
        permissions(&[("contents", AccessLevel::Read)])
    );
    let grant = w
        .caller_by_token(&issued.token)
        .unwrap()
        .unwrap()
        .installation
        .unwrap();
    assert_eq!(grant.repos, Some(vec![web_id]));

    // An empty list, or no permissions, narrows nothing.
    let whole = w
        .create_installation_token(app_id, all.id, Some(&[]), Some(&Permissions::new()))
        .unwrap();
    assert_eq!(whole.repositories, None);
    assert_eq!(whole.permissions, all.permissions);

    let refused = |repositories: Option<&[String]>, asked: Option<&Permissions>| {
        w.create_installation_token(app_id, all.id, repositories, asked)
    };
    for name in ["own", "nothing"] {
        assert!(matches!(
            refused(Some(&strings(&[name])), None),
            Err(WorldError::InstallationRepository(n)) if n == name
        ));
    }
    assert!(matches!(
        refused(None, Some(&permissions(&[("issues", AccessLevel::Read)]))),
        Err(WorldError::InstallationPermission(name)) if name == "issues"
    ));
    assert!(matches!(
        refused(None, Some(&permissions(&[("contents", AccessLevel::Admin)]))),
        Err(WorldError::InstallationPermissionLevel(name)) if name == "contents"
    ));

    // A selected installation's token reaches its list, and narrows within
    // it.
    let chosen = w
        .create_installation("helper", "alice", Some(&strings(&["own"])), SYSTEM)
        .unwrap();
    let own_id = w.repo_by_owner_name("alice", "own").unwrap().unwrap().id;
    let token = w
        .create_installation_token(app_id, chosen.id, None, None)
        .unwrap();
    let grant = w
        .caller_by_token(&token.token)
        .unwrap()
        .unwrap()
        .installation
        .unwrap();
    assert_eq!(grant.repos, Some(vec![own_id]));
    let alice = w.actor_by_login("alice").unwrap().unwrap();
    w.create_repo(alice.id, "other", None, true, "main", SYSTEM)
        .unwrap();
    assert!(matches!(
        w.create_installation_token(app_id, chosen.id, Some(&strings(&["other"])), None),
        Err(WorldError::InstallationRepository(_))
    ));

    // Another app's installation is not this app's to issue for.
    let other = w
        .create_app(
            &NewApp {
                slug: "other-app",
                name: "Other",
                owner: "alice",
                callback_urls: &strings(&["https://other.test/cb"]),
                permissions: &Permissions::new(),
                events: &[],
                hook_url: None,
                hook_active: false,
            },
            SYSTEM,
        )
        .unwrap();
    for (app, installation) in [(other.app.id, all.id), (app_id, 999)] {
        assert!(matches!(
            w.create_installation_token(app, installation, None, None),
            Err(WorldError::NoSuchInstallation(_))
        ));
    }
}

#[test]
fn a_real_worlds_token_expires_by_the_hosts_time() {
    let (_dir, w, app_id) = seeded();
    let installation = w
        .create_installation("helper", "acme", None, SYSTEM)
        .unwrap();
    let host = DEFAULT_T0_UNIX + 10_000;
    w.set_host_time(host);
    w.set_clock_mode(orrery_world::ClockMode::Real, SYSTEM)
        .unwrap();
    let issued = w
        .create_installation_token(app_id, installation.id, None, None)
        .unwrap();
    assert_eq!(
        issued.expires_clock_unix,
        host + INSTALLATION_TOKEN_TTL_SECONDS
    );
    // No write moves the stored clock: the read alone sees the time pass.
    w.set_host_time(host + INSTALLATION_TOKEN_TTL_SECONDS - 1);
    assert!(w.caller_by_token(&issued.token).unwrap().is_some());
    w.set_host_time(host + INSTALLATION_TOKEN_TTL_SECONDS);
    assert_eq!(w.caller_by_token(&issued.token).unwrap(), None);
}
