//! Deliveries: when a write's are made, what becomes of a write whose
//! notifier fails, and which deliveries can be recorded and asked for again.

use std::sync::{Arc, Mutex};

use orrery_world::{
    ActorKind, Attribution, DEFAULT_T0_UNIX, DeliveryOutcome, DeliveryState, Door, EventBody,
    NewApp, NewDelivery, Notifier, Permissions, SYSTEM_ACTOR_ID, World, WorldError, WorldSecret,
};

const SYSTEM: Attribution = Attribution::new(SYSTEM_ACTOR_ID, Door::Control);

/// A world whose one app, `helper`, has an active webhook, and the app's id.
fn hooked() -> (tempfile::TempDir, Arc<World>, i64) {
    let dir = tempfile::tempdir().expect("tempdir");
    let world = World::create(
        &dir.path().join("world"),
        DEFAULT_T0_UNIX,
        &WorldSecret::LOCAL,
    )
    .expect("create world");
    world.create_actor("alice", ActorKind::User).unwrap();
    let app = world
        .create_app(
            &NewApp {
                slug: "helper",
                name: "Helper",
                owner: "alice",
                callback_urls: &["https://helper.test/callback".to_string()],
                permissions: &Permissions::new(),
                events: &[],
                hook_url: Some("https://helper.test/hook"),
                hook_active: true,
            },
            SYSTEM,
        )
        .unwrap()
        .app;
    (dir, Arc::new(world), app.id)
}

/// Owes `app` one delivery for each actor a write creates.
struct ForEachActor {
    app: i64,
}

impl Notifier for ForEachActor {
    fn deliveries(&self, _: &World, _: Attribution, events: &[EventBody]) -> Vec<NewDelivery> {
        events
            .iter()
            .filter_map(|event| match event {
                EventBody::ActorCreated(created) => Some(NewDelivery {
                    app_id: self.app,
                    installation_id: None,
                    repo_id: None,
                    event: "actor".to_string(),
                    action: Some("created".to_string()),
                    body: format!("{{\"login\":{:?}}}", created.object.login),
                }),
                _ => None,
            })
            .collect()
    }
}

fn answered(status_code: i64) -> DeliveryOutcome {
    DeliveryOutcome::Answered {
        status_code,
        headers: vec![("content-type".to_string(), "text/plain".to_string())],
        body: "ok".to_string(),
        truncated: false,
    }
}

#[test]
fn the_newest_write_gets_the_deliveries_it_is_missing_once() {
    let (_dir, world, app) = hooked();
    // A write that landed with nothing to make its deliveries, as when a
    // process stops between the write and them.
    world.create_actor("bob", ActorKind::User).unwrap();
    assert!(world.pending_deliveries(10).unwrap().is_empty());

    world.set_notifier(Arc::new(ForEachActor { app }));
    world.finish_last_write().unwrap();
    let pending = world.pending_deliveries(10).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].body, "{\"login\":\"bob\"}");

    // The newest write is now the deliveries' own, which makes none.
    world.finish_last_write().unwrap();
    assert_eq!(world.pending_deliveries(10).unwrap().len(), 1);

    // A write whose deliveries were made as it landed is owed no more.
    world.create_actor("carol", ActorKind::User).unwrap();
    world.finish_last_write().unwrap();
    assert_eq!(world.pending_deliveries(10).unwrap().len(), 2);
}

/// Writes to the world it is asked to read, and keeps what it was told.
struct Writes {
    refused: Mutex<Option<WorldError>>,
}

impl Notifier for Writes {
    fn deliveries(&self, world: &World, _: Attribution, _: &[EventBody]) -> Vec<NewDelivery> {
        if let Err(e) = world.create_actor("made-while-notifying", ActorKind::User) {
            *self.refused.lock().unwrap() = Some(e);
        }
        Vec::new()
    }
}

#[test]
fn a_notifier_cannot_write_to_the_world() {
    let (_dir, world, _) = hooked();
    let notifier = Arc::new(Writes {
        refused: Mutex::new(None),
    });
    world.set_notifier(notifier.clone());
    world.create_actor("bob", ActorKind::User).unwrap();
    assert!(matches!(
        notifier.refused.lock().unwrap().take(),
        Some(WorldError::WriteWhileNotifying)
    ));
    assert!(
        world
            .actor_by_login("made-while-notifying")
            .unwrap()
            .is_none()
    );
}

/// Panics on the first write it is told of, and keeps what it was told it
/// lost.
struct Panics {
    app: i64,
    calls: Mutex<usize>,
    lost: Mutex<Vec<String>>,
}

impl Notifier for Panics {
    fn deliveries(&self, world: &World, by: Attribution, events: &[EventBody]) -> Vec<NewDelivery> {
        let call = {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            *calls
        };
        assert!(call > 1, "the first write's deliveries cannot be made");
        ForEachActor { app: self.app }.deliveries(world, by, events)
    }

    fn lost(&self, error: &WorldError) {
        self.lost.lock().unwrap().push(error.to_string());
    }
}

#[test]
fn a_write_stands_when_its_deliveries_cannot_be_made() {
    let (_dir, world, app) = hooked();
    let notifier = Arc::new(Panics {
        app,
        calls: Mutex::new(0),
        lost: Mutex::new(Vec::new()),
    });
    world.set_notifier(notifier.clone());

    // The write is in the log, so its caller is told it landed.
    let (bob, _) = world.create_actor("bob", ActorKind::User).unwrap();
    assert_eq!(
        world.actor_by_login("bob").unwrap().map(|a| a.id),
        Some(bob.id)
    );
    assert!(world.pending_deliveries(10).unwrap().is_empty());
    assert_eq!(
        *notifier.lost.lock().unwrap(),
        [WorldError::NotifierPanicked.to_string()]
    );

    // And the world takes its next write, deliveries and all.
    world.create_actor("carol", ActorKind::User).unwrap();
    assert_eq!(world.pending_deliveries(10).unwrap().len(), 1);
    assert_eq!(notifier.lost.lock().unwrap().len(), 1);
}

#[test]
fn only_an_attempted_delivery_is_asked_for_again() {
    let (_dir, world, app) = hooked();
    world.set_notifier(Arc::new(ForEachActor { app }));
    world.create_actor("bob", ActorKind::User).unwrap();
    world.create_actor("carol", ActorKind::User).unwrap();
    let pending = world.pending_deliveries(10).unwrap();
    let (first, second) = (pending[0].id, pending[1].id);

    // One still owed is not there to ask for.
    assert!(matches!(
        world.redeliver(app, first),
        Err(WorldError::NoSuchDelivery(id)) if id == first
    ));

    // What recording an attempt returns is what a read answers.
    let recorded = world
        .record_delivery_attempt(first, 12, &answered(204))
        .unwrap();
    assert_eq!(recorded.state, DeliveryState::Answered);
    assert_eq!(world.delivery_by_id(first).unwrap(), Some(recorded.clone()));
    assert!(matches!(
        world.record_delivery_attempt(first, 12, &answered(204)),
        Err(WorldError::DeliveryNotPending(id)) if id == first
    ));

    // And so is what asking again returns: a new delivery, with the GUID
    // of the first.
    let again = world.redeliver(app, first).unwrap();
    assert_eq!(world.delivery_by_id(again.id).unwrap(), Some(again.clone()));
    assert_eq!(again.guid, recorded.guid);
    assert_eq!(again.redelivery_of, Some(first));
    assert_eq!(again.state, DeliveryState::Pending);

    // The second is still owed, whether or not the webhook is on.
    world
        .set_app_hook("helper", None, Some(false), SYSTEM)
        .unwrap();
    assert!(matches!(
        world.redeliver(app, second),
        Err(WorldError::NoSuchDelivery(id)) if id == second
    ));
    // A webhook that is off sends nothing, an attempted delivery included.
    assert!(matches!(
        world.redeliver(app, first),
        Err(WorldError::HookInactive)
    ));
}
