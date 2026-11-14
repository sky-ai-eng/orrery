//! The world's events: the record of every change to a world, in the order
//! it was made (spec/events.md). Each write appends its operation's event
//! first, then one event for each consequence, and every value a write
//! stores is in one of its events. [`Event`] is the one definition of the
//! format: the log stores it, `log/list` and `log/stream` serve it, and
//! [`schema`] generates its JSON Schema from it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::types::{
    ActorEmail, ActorKind, CheckConclusion, CheckStatus, ClockMode, DeliveryFailure, DeliveryState,
    OrgRole, Permissions, PullState, RepositorySelection, ReviewState, TeamRole,
};

/// How a write reached the world.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Door {
    /// The GitHub face's REST API.
    Rest,
    /// The GitHub face's GraphQL API.
    Graphql,
    /// Git smart-HTTP: a push.
    Git,
    /// GitHub's OAuth web flow.
    Web,
    /// The `/_orrery/` control plane.
    Control,
    /// The `orrery` command, creating or forking a world.
    Cli,
    /// The engine itself, recording what came of a delivery it sent.
    Engine,
}

impl Door {
    pub fn as_str(self) -> &'static str {
        match self {
            Door::Rest => "rest",
            Door::Graphql => "graphql",
            Door::Git => "git",
            Door::Web => "web",
            Door::Control => "control",
            Door::Cli => "cli",
            Door::Engine => "engine",
        }
    }

    pub(crate) fn from_db(s: &str) -> Option<Door> {
        Some(match s {
            "rest" => Door::Rest,
            "graphql" => Door::Graphql,
            "git" => Door::Git,
            "web" => Door::Web,
            "control" => Door::Control,
            "cli" => Door::Cli,
            "engine" => Door::Engine,
            _ => return None,
        })
    }
}

/// Who makes a write, through which door, and with which app: what every
/// event of the write records as its `actor`, `door`, and `app`. Nothing in
/// the world reads them back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attribution {
    pub actor_id: i64,
    pub door: Door,
    /// The app the call came through: the OAuth app whose token
    /// authenticated it, or whose authorize page or client credentials it
    /// used, or the app whose installation token or own signature
    /// authenticated it.
    pub app_id: Option<i64>,
}

impl Attribution {
    pub const fn new(actor_id: i64, door: Door) -> Attribution {
        Attribution {
            actor_id,
            door,
            app_id: None,
        }
    }

    pub const fn with_app(self, app_id: Option<i64>) -> Attribution {
        Attribution { app_id, ..self }
    }
}

/// A change to a world, as its log records it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "Orrery event")]
pub struct Event {
    /// The event's position in the log. Positions only grow, and a restore
    /// leaves a gap where the events it discarded were.
    pub seq: i64,
    /// The write the event belongs to.
    pub write: WriteSpan,
    /// The world clock when the write was made, in Unix seconds.
    pub clock_unix: i64,
    /// The real time the write was appended at, in Unix milliseconds, for a
    /// reader showing when something happened. Nothing in the world reads it.
    pub wall_unix_ms: i64,
    /// Who made the change.
    pub actor: ActorRef,
    /// How the change reached the world.
    pub door: Door,
    /// The app the call came through, or null.
    pub app: Option<AppRef>,
    #[serde(flatten)]
    pub body: EventBody,
}

/// Where an event's write starts and how long it is. A write's events are
/// appended together, so a reader holding `events` events from `seq` holds
/// the whole write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WriteSpan {
    /// The `seq` of the write's first event, its operation's.
    pub seq: i64,
    /// How many events the write appended.
    pub events: i64,
}

/// A field's value before and after a change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(inline)]
pub struct Change<T> {
    pub from: T,
    pub to: T,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ActorRef {
    pub id: i64,
    pub login: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AppRef {
    pub id: i64,
    pub client_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepoRef {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PullRef {
    pub id: i64,
    pub number: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TeamRef {
    pub id: i64,
    pub slug: String,
}

/// A pull's head or base: a branch, and the commit the pull holds for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Branch {
    #[serde(rename = "ref")]
    pub name: String,
    pub sha: String,
}

// ---- subjects ----

/// A subject's `type`: one constant string per subject type.
macro_rules! subject_type {
    ($($tag:ident = $value:literal),* $(,)?) => {$(
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
        pub enum $tag {
            #[default]
            #[serde(rename = $value)]
            Tag,
        }
    )*};
}

subject_type! {
    WorldType = "world",
    ActorType = "actor",
    OAuthAppType = "oauth_app",
    OAuthAuthorizationType = "oauth_authorization",
    AppType = "app",
    AppKeyType = "app_key",
    InstallationType = "installation",
    InstallationTokenType = "installation_token",
    DeliveryType = "delivery",
    RepoType = "repo",
    RefType = "ref",
    PullType = "pull",
    TimelineItemType = "timeline_item",
    ReviewType = "review",
    CommentType = "comment",
    CheckRunType = "check_run",
    OrgMembershipType = "org_membership",
    TeamType = "team",
    TeamMembershipType = "team_membership",
    SshSigningKeyType = "ssh_signing_key",
    CommitType = "commit",
}

/// The world itself, which a world has one of.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorldSubject {
    #[serde(rename = "type")]
    pub kind: WorldType,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ActorSubject {
    #[serde(rename = "type")]
    pub kind: ActorType,
    pub id: i64,
    pub login: String,
    #[serde(rename = "kind")]
    pub actor_kind: ActorKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OAuthAppSubject {
    #[serde(rename = "type")]
    pub kind: OAuthAppType,
    pub id: i64,
    pub client_id: String,
    pub name: String,
}

/// An actor's grant of an OAuth app: issued when the actor authorizes the
/// app, exchanged when the app trades it for a token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OAuthAuthorizationSubject {
    #[serde(rename = "type")]
    pub kind: OAuthAuthorizationType,
    pub id: i64,
    pub app: AppRef,
    /// The actor who granted it.
    pub actor: ActorRef,
}

/// An app: an OAuth app with a bot, which accounts install. Its `id` is its
/// OAuth app's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AppSubject {
    #[serde(rename = "type")]
    pub kind: AppType,
    pub id: i64,
    /// The login of the app's bot.
    pub slug: String,
    pub client_id: String,
}

/// A public key an app's signatures are checked against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AppKeySubject {
    #[serde(rename = "type")]
    pub kind: AppKeyType,
    pub id: i64,
    pub app: AppRef,
}

/// An app installed on an account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InstallationSubject {
    #[serde(rename = "type")]
    pub kind: InstallationType,
    pub id: i64,
    pub app: AppRef,
    /// The account the app is installed on.
    pub account: ActorRef,
}

/// A token issued for an installation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InstallationTokenSubject {
    #[serde(rename = "type")]
    pub kind: InstallationTokenType,
    pub id: i64,
    pub installation: InstallationRef,
    pub app: AppRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InstallationRef {
    pub id: i64,
}

/// One POST a world owes an app's webhook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DeliverySubject {
    #[serde(rename = "type")]
    pub kind: DeliveryType,
    pub id: i64,
    pub app: AppRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepoSubject {
    #[serde(rename = "type")]
    pub kind: RepoType,
    pub id: i64,
    pub name: String,
    pub owner: ActorRef,
}

/// A git ref. Refs have no id: a ref is its name in its repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RefSubject {
    #[serde(rename = "type")]
    pub kind: RefType,
    /// The full name, such as `refs/heads/main`.
    pub name: String,
    pub repo: RepoRef,
    pub owner: ActorRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PullSubject {
    #[serde(rename = "type")]
    pub kind: PullType,
    pub id: i64,
    pub number: i64,
    pub repo: RepoRef,
    pub owner: ActorRef,
}

/// One item on a pull's timeline: what happened to it that its current
/// state no longer shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TimelineItemSubject {
    #[serde(rename = "type")]
    pub kind: TimelineItemType,
    pub id: i64,
    pub pull: PullRef,
    pub repo: RepoRef,
    pub owner: ActorRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReviewSubject {
    #[serde(rename = "type")]
    pub kind: ReviewType,
    pub id: i64,
    pub pull: PullRef,
    pub repo: RepoRef,
    pub owner: ActorRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CommentSubject {
    #[serde(rename = "type")]
    pub kind: CommentType,
    pub id: i64,
    pub pull: PullRef,
    pub repo: RepoRef,
    pub owner: ActorRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CheckRunSubject {
    #[serde(rename = "type")]
    pub kind: CheckRunType,
    pub id: i64,
    pub name: String,
    pub head_sha: String,
    pub repo: RepoRef,
    pub owner: ActorRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OrgMembershipSubject {
    #[serde(rename = "type")]
    pub kind: OrgMembershipType,
    pub member: ActorRef,
    /// The organization.
    pub owner: ActorRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TeamSubject {
    #[serde(rename = "type")]
    pub kind: TeamType,
    pub id: i64,
    pub slug: String,
    /// The organization.
    pub owner: ActorRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TeamMembershipSubject {
    #[serde(rename = "type")]
    pub kind: TeamMembershipType,
    pub member: ActorRef,
    pub team: TeamRef,
    /// The organization.
    pub owner: ActorRef,
}

/// An SSH key an account holds for signing commits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SshSigningKeySubject {
    #[serde(rename = "type")]
    pub kind: SshSigningKeyType,
    pub id: i64,
    /// The account that holds it.
    pub owner: ActorRef,
}

/// A commit in a repository. A commit has no id: it is its sha in its
/// repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CommitSubject {
    #[serde(rename = "type")]
    pub kind: CommitType,
    pub sha: String,
    pub repo: RepoRef,
    pub owner: ActorRef,
}

// ---- objects ----

/// An object type, every one of its stored fields, and the `changes` an
/// event carries for it: each field that changed, as a [`Change`].
macro_rules! object {
    (
        $(#[$meta:meta])*
        $object:ident, $changes:ident {
            $( $(#[$field_meta:meta])* $field:ident : $ty:ty ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
        pub struct $object {
            $( $(#[$field_meta])* pub $field: $ty, )*
        }

        /// The fields of the subject that changed, each as a [`Change`].
        #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
        pub struct $changes {
            $(
                #[serde(default, skip_serializing_if = "Option::is_none")]
                pub $field: Option<Change<$ty>>,
            )*
        }

        impl $changes {
            /// The fields that differ between `from` and `to`.
            pub fn between(from: &$object, to: &$object) -> $changes {
                $changes {
                    $(
                        $field: (from.$field != to.$field).then(|| Change {
                            from: from.$field.clone(),
                            to: to.$field.clone(),
                        }),
                    )*
                }
            }

            pub fn is_empty(&self) -> bool {
                true $( && self.$field.is_none() )*
            }
        }
    };
}

object! {
    WorldObject, WorldChanges {
        /// The world clock, in Unix seconds.
        clock_unix: i64,
        /// How the clock moves. Events appended before the world stored it
        /// carry none, and every world was `manual` then.
        #[serde(default)]
        clock_mode: ClockMode,
    }
}

object! {
    ActorObject, ActorChanges {
        id: i64,
        login: String,
        kind: ActorKind,
        created_unix: i64,
        /// Primary first, then by address.
        emails: Vec<ActorEmail>,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OAuthAppObject {
    pub id: i64,
    pub name: String,
    pub client_id: String,
    pub callback_url: String,
    pub created_unix: i64,
}

object! {
    AppObject, AppChanges {
        id: i64,
        slug: String,
        name: String,
        client_id: String,
        /// The user or organization that owns the app.
        owner: ActorRef,
        bot: ActorRef,
        /// The first is the one an authorization redirects to when its request
        /// names none.
        callback_urls: Vec<String>,
        permissions: Permissions,
        events: Vec<String>,
        created_unix: i64,
        /// Where the app's deliveries go, or null for an app with no
        /// webhook. Events appended before a world held webhooks carry
        /// none of the three hook fields.
        #[serde(default)]
        hook_url: Option<String>,
        /// Whether the hook makes deliveries.
        #[serde(default)]
        hook_active: bool,
        /// How many secrets the hook has been issued. Deliveries are signed
        /// with the newest, which is derived and never in an event.
        #[serde(default)]
        hook_secrets: i64,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AppKeyObject {
    pub id: i64,
    /// The base64 of the key's PKCS#1 `RSAPublicKey` DER.
    pub public_key: String,
    pub created_unix: i64,
}

object! {
    InstallationObject, InstallationChanges {
        id: i64,
        account: ActorRef,
        repository_selection: RepositorySelection,
        /// The repositories a `selected` installation lists, in id order. Empty
        /// for `all`.
        repositories: Vec<RepoRef>,
        permissions: Permissions,
        events: Vec<String>,
        created_unix: i64,
        /// When the installation was suspended, or null while it is not.
        /// Events appended before a world could suspend one carry neither
        /// suspension field.
        #[serde(default)]
        suspended_unix: Option<i64>,
        /// Who suspended it.
        #[serde(default)]
        suspended_by: Option<ActorRef>,
    }
}

object! {
    DeliveryObject, DeliveryChanges {
        id: i64,
        /// The same for a delivery and every redelivery of it.
        guid: String,
        /// The event's name, in the words of the face that made the
        /// delivery.
        event: String,
        action: Option<String>,
        installation: Option<InstallationRef>,
        /// The id of the repository the event is about.
        repo: Option<i64>,
        /// The hook's URL when the delivery was made.
        url: String,
        /// The request body, exactly as it is sent.
        body: String,
        /// The delivery this one sends again.
        redelivery_of: Option<i64>,
        created_unix: i64,
        state: DeliveryState,
        /// The world's clock when the attempt's result was recorded.
        attempted_unix: Option<i64>,
        /// The status the receiver answered with.
        status_code: Option<i64>,
        /// Which step failed, for an attempt that got no answer.
        failure: Option<DeliveryFailure>,
        failure_detail: Option<String>,
        /// How long the attempt took, in real milliseconds.
        duration_ms: Option<i64>,
        /// The answer's headers, each a name and a value.
        response_headers: Option<Vec<(String, String)>>,
        /// The first of the answer's body, as text.
        response_body: Option<String>,
        /// Whether the answer's body went on past what was kept.
        response_truncated: Option<bool>,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InstallationTokenObject {
    pub id: i64,
    /// The repositories the token was narrowed to, in id order. Empty for
    /// one that reaches everything its installation covers.
    pub repositories: Vec<RepoRef>,
    pub permissions: Permissions,
    pub created_unix: i64,
    pub expires_unix: i64,
}

object! {
    OAuthAuthorizationObject, OAuthAuthorizationChanges {
        id: i64,
        app: AppRef,
        actor: ActorRef,
        /// The redirect URI the code was issued for.
        redirect_uri: String,
        scope: String,
        created_unix: i64,
        /// When the app traded the code for a token, or null while it has
        /// not.
        exchanged_unix: Option<i64>,
    }
}

object! {
    RepoObject, RepoChanges {
        id: i64,
        name: String,
        owner: ActorRef,
        description: Option<String>,
        private: bool,
        default_branch: String,
        created_by: ActorRef,
        created_unix: i64,
        /// When a push or a merge last moved a ref, or null before the first.
        pushed_unix: Option<i64>,
    }
}

object! {
    RefObject, RefChanges {
        /// The full name, such as `refs/heads/main`.
        name: String,
        sha: String,
    }
}

object! {
    PullObject, PullChanges {
        id: i64,
        number: i64,
        title: String,
        body: Option<String>,
        author: ActorRef,
        state: PullState,
        draft: bool,
        /// The head branch, and the commit the pull proposes: a push to the
        /// branch moves it while the pull is open.
        head: Branch,
        /// The base branch, and the commit it was at when the pull was
        /// opened or retargeted.
        base: Branch,
        merged: bool,
        merge_commit_sha: Option<String>,
        merged_by: Option<ActorRef>,
        created_unix: i64,
        updated_unix: i64,
        closed_unix: Option<i64>,
        /// Ordered by name without regard to case.
        labels: Vec<String>,
        /// The users whose review is pending, in request order.
        requested_reviewers: Vec<ActorRef>,
        /// The teams whose review is pending, in request order.
        requested_teams: Vec<TeamRef>,
        /// Whether the base branch has moved since the head or base was last
        /// pinned: at opening, on a move of the head, on a reopen, or on a
        /// retarget.
        base_moved: bool,
        /// Whether the head or the base branch has moved since the pull's
        /// last test merge started. Events appended before the world stored
        /// it carry none. When the world began to store it, each pull's was
        /// its `base_moved`, and no event records that.
        #[serde(default)]
        test_merge_stale: bool,
    }
}

/// What a timeline item records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TimelineItemKind {
    Labeled,
    Unlabeled,
    ReadyForReview,
    ReviewRequested,
    ReviewRequestRemoved,
    /// A close, a merge's included.
    Closed,
    /// A push deleted the pull's head branch.
    HeadRefDeleted,
    /// A push deleted the pull's base branch while the pull was open.
    BaseRefDeleted,
    /// A push created the pull's head branch again after it was deleted.
    HeadRefRestored,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TimelineItemObject {
    pub id: i64,
    pub kind: TimelineItemKind,
    /// The label, for the label kinds; otherwise null.
    pub label: Option<String>,
    /// The branch deleted, by the name it had, for the deletion kinds;
    /// otherwise null.
    pub ref_name: Option<String>,
    /// The user asked for or no longer asked for a review, for the
    /// review-request kinds when they name a user; otherwise null.
    pub reviewer: Option<ActorRef>,
    /// The team, with the slug it had, for the review-request kinds when
    /// they name a team; otherwise null.
    pub team: Option<TeamRef>,
    pub actor: ActorRef,
    pub created_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReviewObject {
    pub id: i64,
    pub author: ActorRef,
    pub state: ReviewState,
    pub body: Option<String>,
    /// The pull's head the review judged.
    pub commit_sha: String,
    pub submitted_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CommentObject {
    pub id: i64,
    pub author: ActorRef,
    pub body: String,
    pub created_unix: i64,
    pub updated_unix: i64,
}

object! {
    CheckRunObject, CheckRunChanges {
        id: i64,
        head_sha: String,
        name: String,
        status: CheckStatus,
        conclusion: Option<CheckConclusion>,
        details_url: Option<String>,
        external_id: Option<String>,
        /// Whatever JSON the creator sent.
        output: Option<serde_json::Value>,
        creator: ActorRef,
        /// As its creator stated it, in Unix seconds.
        started_unix: Option<i64>,
        /// As its creator stated it, in Unix seconds.
        completed_unix: Option<i64>,
        created_unix: i64,
    }
}

object! {
    OrgMembershipObject, OrgMembershipChanges {
        member: ActorRef,
        role: OrgRole,
        created_unix: i64,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TeamObject {
    pub id: i64,
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub created_unix: i64,
}

object! {
    TeamMembershipObject, TeamMembershipChanges {
        member: ActorRef,
        role: TeamRole,
        created_unix: i64,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SshSigningKeyObject {
    pub id: i64,
    /// The key's type and base64 blob, as OpenSSH writes a public key,
    /// without a comment.
    pub key: String,
    pub title: Option<String>,
    pub created_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SshSigningKeyRef {
    pub id: i64,
}

/// A commit's signature verified when a push brought the commit in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CommitVerificationObject {
    /// The account whose signing key made the signature, and whose
    /// verified address the commit's committer address is.
    pub signer: ActorRef,
    /// The key that made the signature.
    pub signing_key: SshSigningKeyRef,
    pub verified_unix: i64,
}

// ---- bodies ----

/// What `world:rekeyed` derived again under the world's new secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Rekeyed {
    pub tokens: i64,
    /// The client secret each app was registered with.
    pub oauth_apps: i64,
    /// The client secrets issued for apps since they were registered.
    pub oauth_app_secrets: i64,
    /// The authorization codes not yet exchanged.
    pub oauth_codes: i64,
    /// The installation tokens. Events appended before a world held any
    /// carry none.
    #[serde(default)]
    pub installation_tokens: i64,
    /// The hook secrets. Events appended before a world held any carry
    /// none.
    #[serde(default)]
    pub hook_secrets: i64,
}

/// A body type with a subject and one more field per entry.
macro_rules! bodies {
    ($(
        $(#[$meta:meta])*
        $name:ident { $( $(#[$field_meta:meta])* $field:ident : $ty:ty ),* $(,)? }
    )*) => {$(
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
        pub struct $name {
            $( $(#[$field_meta])* pub $field: $ty, )*
        }
    )*};
}

bodies! {
    WorldCreated { subject: WorldSubject, object: WorldObject }
    WorldChanged { subject: WorldSubject, changes: WorldChanges }
    WorldRekeyed { subject: WorldSubject, rekeyed: Rekeyed }
    ActorCreated { subject: ActorSubject, object: ActorObject }
    ActorChanged { subject: ActorSubject, changes: ActorChanges }
    /// A token issued for an actor, numbered from 1 per actor. The token
    /// its creation minted is its 0th.
    ActorTokenIssued { subject: ActorSubject, n: i64 }
    OAuthAppCreated { subject: OAuthAppSubject, object: OAuthAppObject }
    /// A client secret issued for an app, numbered from 1 per app. The one
    /// its registration issued is its 0th.
    OAuthAppSecretIssued { subject: OAuthAppSubject, n: i64 }
    OAuthAuthorizationCreated {
        subject: OAuthAuthorizationSubject,
        object: OAuthAuthorizationObject,
    }
    OAuthAuthorizationChanged {
        subject: OAuthAuthorizationSubject,
        changes: OAuthAuthorizationChanges,
    }
    AppCreated { subject: AppSubject, object: AppObject }
    AppChanged { subject: AppSubject, changes: AppChanges }
    AppKeyCreated { subject: AppKeySubject, object: AppKeyObject }
    InstallationCreated { subject: InstallationSubject, object: InstallationObject }
    InstallationChanged { subject: InstallationSubject, changes: InstallationChanges }
    /// The installation as it was.
    InstallationDeleted { subject: InstallationSubject, object: InstallationObject }
    DeliveryCreated { subject: DeliverySubject, object: DeliveryObject }
    DeliveryChanged { subject: DeliverySubject, changes: DeliveryChanges }
    InstallationTokenCreated {
        subject: InstallationTokenSubject,
        object: InstallationTokenObject,
    }
    RepoCreated { subject: RepoSubject, object: RepoObject }
    RepoChanged { subject: RepoSubject, changes: RepoChanges }
    RefCreated {
        subject: RefSubject,
        object: RefObject,
        /// The commits the ref brought into the repository: those reachable
        /// from it that no ref reached before the write. Oldest first, and at
        /// most 2048, the newest.
        commits: Vec<String>,
        /// How many commits the ref brought in, all of them.
        commit_count: i64,
    }
    RefUpdated {
        subject: RefSubject,
        changes: RefChanges,
        /// Whether `sha.from` is not an ancestor of `sha.to`.
        forced: bool,
        /// The commits reachable from `sha.to` and not from `sha.from`.
        /// Oldest first, and at most 2048, the newest.
        commits: Vec<String>,
        /// How many commits the move added, all of them.
        commit_count: i64,
    }
    RefDeleted { subject: RefSubject, object: RefObject }
    PullOpened { subject: PullSubject, object: PullObject }
    PullChanged { subject: PullSubject, changes: PullChanges }
    TimelineItemCreated { subject: TimelineItemSubject, object: TimelineItemObject }
    ReviewSubmitted { subject: ReviewSubject, object: ReviewObject }
    CommentCreated { subject: CommentSubject, object: CommentObject }
    CheckRunCreated { subject: CheckRunSubject, object: CheckRunObject }
    CheckRunChanged { subject: CheckRunSubject, changes: CheckRunChanges }
    OrgMembershipCreated { subject: OrgMembershipSubject, object: OrgMembershipObject }
    OrgMembershipChanged { subject: OrgMembershipSubject, changes: OrgMembershipChanges }
    TeamCreated { subject: TeamSubject, object: TeamObject }
    /// The team as it was.
    TeamDeleted { subject: TeamSubject, object: TeamObject }
    TeamMembershipCreated { subject: TeamMembershipSubject, object: TeamMembershipObject }
    TeamMembershipChanged { subject: TeamMembershipSubject, changes: TeamMembershipChanges }
    /// The membership as it was.
    TeamMembershipDeleted { subject: TeamMembershipSubject, object: TeamMembershipObject }
    SshSigningKeyCreated { subject: SshSigningKeySubject, object: SshSigningKeyObject }
    /// The key as it was.
    SshSigningKeyDeleted { subject: SshSigningKeySubject, object: SshSigningKeyObject }
    CommitVerified { subject: CommitSubject, object: CommitVerificationObject }
}

macro_rules! event_types {
    ($( $(#[$meta:meta])* $variant:ident($body:ty) = $name:literal, )*) => {
        /// An event's type and the body its type has. A type is
        /// `object:happening`: the subject's type, then what happened to it.
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "type")]
        pub enum EventBody {
            $( $(#[$meta])* #[serde(rename = $name)] $variant($body), )*
        }

        impl EventBody {
            /// Every type, in the order the schema lists them.
            pub const TYPES: &'static [&'static str] = &[$($name),*];

            /// The event's type, as its `type` field spells it.
            pub fn type_name(&self) -> &'static str {
                match self {
                    $( EventBody::$variant(_) => $name, )*
                }
            }
        }
    };
}

event_types! {
    /// A world was created, with its system actor.
    WorldCreated(WorldCreated) = "world:created",
    /// The clock moved forward: by an advance in a `manual` world, or to the
    /// host's time, as a write of its own, before a `real` world's write or a
    /// change of mode.
    WorldClockAdvanced(WorldChanged) = "world:clock_advanced",
    /// The clock's mode changed.
    WorldClockModeSet(WorldChanged) = "world:clock_mode_set",
    /// A forked world took its own secret, and every credential it inherited
    /// was derived again under it.
    WorldRekeyed(WorldRekeyed) = "world:rekeyed",

    /// An actor was created, with its token and its address.
    ActorCreated(ActorCreated) = "actor:created",
    ActorEmailsSet(ActorChanged) = "actor:emails_set",
    ActorTokenIssued(ActorTokenIssued) = "actor:token_issued",

    /// An OAuth app was registered, with its client secret.
    OAuthAppCreated(OAuthAppCreated) = "oauth_app:created",
    OAuthAppSecretIssued(OAuthAppSecretIssued) = "oauth_app:secret_issued",
    /// An actor authorized an OAuth app, which issued an authorization code.
    OAuthAuthorizationCreated(OAuthAuthorizationCreated) = "oauth_authorization:created",
    /// The app traded the authorization's code for a token acting as the
    /// actor who granted it.
    OAuthAuthorizationExchanged(OAuthAuthorizationChanged) = "oauth_authorization:exchanged",

    /// An app was registered, with its OAuth app and, unless a bot already
    /// held its slug, its bot.
    AppCreated(AppCreated) = "app:created",
    /// An app's webhook was pointed at a URL, taken off one, or turned on
    /// or off.
    AppHookSet(AppChanged) = "app:hook_set",
    /// A secret was issued for an app's webhook, replacing the one before.
    AppHookSecretIssued(AppChanged) = "app:hook_secret_issued",
    AppKeyCreated(AppKeyCreated) = "app_key:created",
    /// An app was installed on an account.
    InstallationCreated(InstallationCreated) = "installation:created",
    InstallationSuspended(InstallationChanged) = "installation:suspended",
    InstallationUnsuspended(InstallationChanged) = "installation:unsuspended",
    /// An installation was removed. Its tokens stop working with it.
    InstallationDeleted(InstallationDeleted) = "installation:deleted",
    /// An app issued a token for one of its installations.
    InstallationTokenCreated(InstallationTokenCreated) = "installation_token:created",

    /// The world owes a receiver a POST: a face made a delivery for a write
    /// that had just landed, or an app asked for one to be sent again.
    DeliveryCreated(DeliveryCreated) = "delivery:created",
    /// The engine made a delivery's one attempt, and this is what came of
    /// it.
    DeliveryAttempted(DeliveryChanged) = "delivery:attempted",
    /// A pending delivery will not be attempted: its world was restored or
    /// copied.
    DeliveryDropped(DeliveryChanged) = "delivery:dropped",

    RepoCreated(RepoCreated) = "repo:created",
    /// A push, or a merge, moved refs in the repository. On a push it is the
    /// operation, and a ref event for each ref it moved follows.
    RepoPushed(RepoChanged) = "repo:pushed",

    RefCreated(RefCreated) = "ref:created",
    RefUpdated(RefUpdated) = "ref:updated",
    RefDeleted(RefDeleted) = "ref:deleted",

    PullOpened(PullOpened) = "pull:opened",
    PullEdited(PullChanged) = "pull:edited",
    /// A close, or a push's deletion of the pull's head or base branch.
    PullClosed(PullChanged) = "pull:closed",
    PullReopened(PullChanged) = "pull:reopened",
    PullMerged(PullChanged) = "pull:merged",
    PullReadyForReview(PullChanged) = "pull:ready_for_review",
    PullConvertedToDraft(PullChanged) = "pull:converted_to_draft",
    PullLabeled(PullChanged) = "pull:labeled",
    PullUnlabeled(PullChanged) = "pull:unlabeled",
    PullReviewRequested(PullChanged) = "pull:review_requested",
    /// A request withdrawn, a review by a user asked for one, or the
    /// deletion of a team asked for one.
    PullReviewRequestRemoved(PullChanged) = "pull:review_request_removed",
    /// A push or a merge moved the pull's head branch, and the pull with it.
    PullHeadMoved(PullChanged) = "pull:head_moved",
    /// A read of the pull's mergeability found it stale and started a test
    /// merge.
    PullTestMergeStarted(PullChanged) = "pull:test_merge_started",
    /// Any other change a write makes to a pull it is not about.
    PullUpdated(PullChanged) = "pull:updated",

    TimelineItemCreated(TimelineItemCreated) = "timeline_item:created",

    ReviewSubmitted(ReviewSubmitted) = "review:submitted",
    CommentCreated(CommentCreated) = "comment:created",

    CheckRunCreated(CheckRunCreated) = "check_run:created",
    CheckRunUpdated(CheckRunChanged) = "check_run:updated",

    OrgMembershipCreated(OrgMembershipCreated) = "org_membership:created",
    OrgMembershipUpdated(OrgMembershipChanged) = "org_membership:updated",

    /// A team was created. A creator who is a member of the organization
    /// becomes its first maintainer.
    TeamCreated(TeamCreated) = "team:created",
    /// A team was deleted, with its memberships and pending review requests.
    TeamDeleted(TeamDeleted) = "team:deleted",

    TeamMembershipCreated(TeamMembershipCreated) = "team_membership:created",
    TeamMembershipUpdated(TeamMembershipChanged) = "team_membership:updated",
    TeamMembershipDeleted(TeamMembershipDeleted) = "team_membership:deleted",

    /// An account registered an SSH key for signing commits.
    SshSigningKeyCreated(SshSigningKeyCreated) = "ssh_signing_key:created",
    SshSigningKeyDeleted(SshSigningKeyDeleted) = "ssh_signing_key:deleted",

    /// A push brought in a commit whose signature verifies: an account's
    /// signing key made it, and its committer address is a verified address
    /// of that account.
    CommitVerified(CommitVerified) = "commit:verified",
}

/// The JSON Schema of an [`Event`], as `GET /_orrery/log/schema` serves it
/// and `spec/event.schema.json` holds it.
pub fn schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(Event)).expect("a schema serializes")
}

/// [`schema`] as `spec/event.schema.json` holds it: pretty-printed, with a
/// final newline.
pub fn schema_file() -> String {
    let mut text = serde_json::to_string_pretty(&schema()).expect("a schema serializes");
    text.push('\n');
    text
}

/// The SHA-256 of [`EventBody`]'s JSON Schema, as lowercase hex: the shape
/// of what a world stores of an event, its type and its body. The rest of
/// the envelope is read from columns, which the tables' migrations cover.
/// `tests/fixtures/event-schemas` records it for each schema version.
///
/// It is taken over a normal form that keeps only what a stored body's shape
/// depends on. Doc comments become `description` and `title`, and a
/// documented value splits a string enum into a `oneOf` of its values, so
/// both are undone. `enum`, `oneOf`, and `required` are sets, so their order,
/// which follows the order of the declarations, is dropped too.
pub fn schema_digest() -> String {
    fn string_values(schema: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
        let object = schema.as_object()?;
        if object.len() != 2 || object.get("type")? != "string" {
            return None;
        }
        object.get("enum")?.as_array()
    }
    fn normalize(schema: &mut schemars::Schema) {
        schemars::transform::transform_subschemas(&mut normalize, schema);
        let Some(object) = schema.as_object_mut() else {
            return;
        };
        object.remove("description");
        object.remove("title");
        if let Some(value) = object.remove("const") {
            object.insert("enum".into(), vec![value].into());
        }
        let values: Option<Vec<serde_json::Value>> = object
            .get("oneOf")
            .and_then(serde_json::Value::as_array)
            .and_then(|members| {
                members
                    .iter()
                    .map(string_values)
                    .collect::<Option<Vec<_>>>()
            })
            .map(|lists| lists.into_iter().flatten().cloned().collect());
        if let Some(values) = values {
            object.remove("oneOf");
            object.insert("type".into(), "string".into());
            object.insert("enum".into(), values.into());
        }
        for set in ["enum", "oneOf", "required"] {
            if let Some(serde_json::Value::Array(members)) = object.get_mut(set) {
                members.sort_by_cached_key(serde_json::Value::to_string);
            }
        }
    }
    let mut schema = schemars::schema_for!(EventBody);
    normalize(&mut schema);
    let text = serde_json::to_string(&schema).expect("a schema serializes");
    hex::encode(Sha256::digest(text))
}
