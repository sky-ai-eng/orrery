//! Domain types the kernel stores and serves. Vocabulary is vendor-neutral
//! (rule 9): faces translate these into GitHub (or any other vendor) wire
//! shapes; nothing here knows what a wire shape is.

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    User,
    Bot,
    Org,
    System,
}

impl ActorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ActorKind::User => "user",
            ActorKind::Bot => "bot",
            ActorKind::Org => "org",
            ActorKind::System => "system",
        }
    }

    pub(crate) fn from_db(s: &str) -> ActorKind {
        match s {
            "user" => ActorKind::User,
            "bot" => ActorKind::Bot,
            "org" => ActorKind::Org,
            _ => ActorKind::System,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Actor {
    pub id: i64,
    pub login: String,
    pub kind: ActorKind,
    pub created_clock_unix: i64,
}

/// Who a request's token acts as, and the OAuth app it was issued to, if
/// an authorization code was traded for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub actor: Actor,
    pub app_id: Option<i64>,
}

impl Caller {
    /// The attribution of a write this caller makes through `door`.
    pub fn via(&self, door: crate::Door) -> crate::Attribution {
        crate::Attribution::new(self.actor.id, door).with_app(self.app_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Repo {
    pub id: i64,
    pub owner_login: String,
    pub name: String,
    pub description: Option<String>,
    pub private: bool,
    pub default_branch: String,
    pub created_clock_unix: i64,
    pub pushed_at_clock_unix: Option<i64>,
}

impl Repo {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner_login, self.name)
    }
}

/// The order a read of the log returns its events in, by `seq`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogOrder {
    OldestFirst,
    NewestFirst,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PullState {
    Open,
    Closed,
}

impl PullState {
    pub fn as_str(self) -> &'static str {
        match self {
            PullState::Open => "open",
            PullState::Closed => "closed",
        }
    }

    pub(crate) fn from_db(s: &str) -> PullState {
        match s {
            "open" => PullState::Open,
            _ => PullState::Closed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Pull {
    pub id: i64,
    pub repo_id: i64,
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub author_login: String,
    pub state: PullState,
    pub draft: bool,
    pub head_ref: String,
    /// Tracks the head branch: push observation re-pins it (GitHub's
    /// "synchronize"), so it is always the tip the PR currently proposes.
    pub head_sha: String,
    pub base_ref: String,
    /// Pinned at creation (and on base retarget), never tracking the base
    /// branch — GitHub's `base.sha` semantics.
    pub base_sha: String,
    pub merged: bool,
    pub merge_commit_sha: Option<String>,
    pub merged_by_login: Option<String>,
    pub created_clock_unix: i64,
    pub updated_clock_unix: i64,
    pub closed_clock_unix: Option<i64>,
    /// The pull's labels, ordered by name without regard to case.
    pub labels: Vec<String>,
    /// Logins of the users whose review is pending, in request order.
    pub requested_reviewers: Vec<String>,
    /// Slugs of the teams, in the repository's organization, whose review is
    /// pending, in request order.
    pub requested_teams: Vec<String>,
    /// Whether the base branch has moved since the head or base was last
    /// pinned: at creation, on a re-pin of the head, on a reopen, or on a
    /// retarget.
    pub base_moved: bool,
    /// Whether the head or the base branch has moved since the pull's last
    /// test merge started. Opening, reopening, and retargeting a pull start
    /// one; a move of either branch starts none, so the next read of the
    /// pull's mergeability does ([`crate::World::start_test_merge`]).
    pub test_merge_stale: bool,
}

/// One item on a pull's timeline (see `pull_timeline` in the schema).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TimelineItem {
    pub id: i64,
    pub pull_id: i64,
    pub kind: TimelineKind,
    pub actor_login: String,
    pub clock_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum TimelineKind {
    Labeled(String),
    Unlabeled(String),
    ReadyForReview,
    ReviewRequested(RequestedReviewer),
    ReviewRequestRemoved(RequestedReviewer),
    /// Recorded by a merge as well as a plain close: GitHub logs a `closed`
    /// event beside `merged`, by the same actor at the same time.
    Closed,
    /// A push deleted the pull's head branch, by the name it had.
    HeadRefDeleted(String),
    /// A push deleted the pull's base branch, by the name it had, while the
    /// pull was open.
    BaseRefDeleted(String),
    /// A push created the pull's head branch again after it was deleted.
    HeadRefRestored,
}

/// Whose review a request asks for: a user by login, or a team by its id,
/// with the slug it had in the pull's organization.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestedReviewer {
    User(String),
    Team { id: i64, slug: String },
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum OrgRole {
    Admin,
    Member,
}

impl OrgRole {
    pub fn as_str(self) -> &'static str {
        match self {
            OrgRole::Admin => "admin",
            OrgRole::Member => "member",
        }
    }

    pub(crate) fn from_db(s: &str) -> OrgRole {
        match s {
            "admin" => OrgRole::Admin,
            _ => OrgRole::Member,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OrgMembership {
    pub org_login: String,
    pub member_login: String,
    pub role: OrgRole,
    pub created_clock_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Team {
    pub id: i64,
    pub org_id: i64,
    pub org_login: String,
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub created_clock_unix: i64,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TeamRole {
    Member,
    Maintainer,
}

impl TeamRole {
    pub fn as_str(self) -> &'static str {
        match self {
            TeamRole::Member => "member",
            TeamRole::Maintainer => "maintainer",
        }
    }

    pub(crate) fn from_db(s: &str) -> TeamRole {
        match s {
            "maintainer" => TeamRole::Maintainer,
            _ => TeamRole::Member,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TeamMembership {
    pub team_id: i64,
    pub member_login: String,
    pub role: TeamRole,
    pub created_clock_unix: i64,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
}

impl ReviewState {
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewState::Approved => "approved",
            ReviewState::ChangesRequested => "changes_requested",
            ReviewState::Commented => "commented",
        }
    }

    pub(crate) fn from_db(s: &str) -> ReviewState {
        match s {
            "approved" => ReviewState::Approved,
            "changes_requested" => ReviewState::ChangesRequested,
            _ => ReviewState::Commented,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Review {
    pub id: i64,
    pub pull_id: i64,
    pub author_login: String,
    pub state: ReviewState,
    pub body: Option<String>,
    /// The pull's head at submission time — what the reviewer actually judged.
    pub commit_sha: String,
    pub submitted_clock_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct IssueComment {
    pub id: i64,
    pub pull_id: i64,
    pub author_login: String,
    pub body: String,
    pub created_clock_unix: i64,
    pub updated_clock_unix: i64,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Queued,
    InProgress,
    Completed,
}

impl CheckStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            CheckStatus::Queued => "queued",
            CheckStatus::InProgress => "in_progress",
            CheckStatus::Completed => "completed",
        }
    }

    pub(crate) fn from_db(s: &str) -> CheckStatus {
        match s {
            "queued" => CheckStatus::Queued,
            "in_progress" => CheckStatus::InProgress,
            _ => CheckStatus::Completed,
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CheckConclusion {
    Success,
    Failure,
    Neutral,
    Cancelled,
    Skipped,
    Stale,
    TimedOut,
    ActionRequired,
}

impl CheckConclusion {
    pub fn as_str(self) -> &'static str {
        match self {
            CheckConclusion::Success => "success",
            CheckConclusion::Failure => "failure",
            CheckConclusion::Neutral => "neutral",
            CheckConclusion::Cancelled => "cancelled",
            CheckConclusion::Skipped => "skipped",
            CheckConclusion::Stale => "stale",
            CheckConclusion::TimedOut => "timed_out",
            CheckConclusion::ActionRequired => "action_required",
        }
    }

    pub(crate) fn from_db(s: &str) -> CheckConclusion {
        match s {
            "success" => CheckConclusion::Success,
            "failure" => CheckConclusion::Failure,
            "neutral" => CheckConclusion::Neutral,
            "cancelled" => CheckConclusion::Cancelled,
            "skipped" => CheckConclusion::Skipped,
            "stale" => CheckConclusion::Stale,
            "timed_out" => CheckConclusion::TimedOut,
            _ => CheckConclusion::ActionRequired,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CheckRun {
    pub id: i64,
    pub repo_id: i64,
    pub head_sha: String,
    pub name: String,
    pub status: CheckStatus,
    pub conclusion: Option<CheckConclusion>,
    pub details_url: Option<String>,
    pub external_id: Option<String>,
    /// Stored generously as whatever JSON the creator sent (rule 1: fields
    /// liberal); the kernel takes no view on its shape.
    pub output: Option<serde_json::Value>,
    pub creator_login: String,
    pub started_clock_unix: Option<i64>,
    pub completed_clock_unix: Option<i64>,
    pub created_clock_unix: i64,
}

/// One of an actor's email addresses. Every actor but the system actor holds
/// exactly one primary address.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
#[schemars(transform = may_grow)]
pub struct ActorEmail {
    pub email: String,
    pub primary: bool,
    pub verified: bool,
}

/// The control plane refuses an address with a field it does not know. An
/// event's addresses may gain fields (spec/events.md, rule 7), so the events'
/// schema does not refuse them.
fn may_grow(schema: &mut schemars::Schema) {
    schema.remove("additionalProperties");
}

/// An OAuth application actors can authorize. Its client secret is fake by
/// construction (rule 12), like every credential in a world.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OAuthApp {
    pub id: i64,
    pub name: String,
    pub client_id: String,
    pub client_secret: String,
    pub callback_url: String,
    pub created_clock_unix: i64,
}

/// An actor as a list of actors names it, with its addresses.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ListedActor {
    pub login: String,
    pub kind: ActorKind,
    pub emails: Vec<ActorEmail>,
}

/// An OAuth app as a list of apps names it. There is no secret field, so a
/// list cannot return one.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ListedOAuthApp {
    pub name: String,
    pub client_id: String,
    pub callback_url: String,
}

/// The result of trading an authorization code: a token acting as `actor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthToken {
    pub actor: Actor,
    pub token: String,
    pub scope: String,
}

/// One of an actor's addresses, keyed by the actor that holds it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ActorAddress {
    pub actor_id: i64,
    pub email: ActorEmail,
}

/// An actor's grant of an OAuth app, without the code itself: issued when
/// the actor authorizes the app, exchanged when the app trades it for a
/// token.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OAuthAuthorization {
    pub id: i64,
    pub client_id: String,
    pub app_name: String,
    pub actor_login: String,
    pub scope: String,
    pub created_clock_unix: i64,
    pub exchanged_clock_unix: Option<i64>,
}

/// A pending review request on a pull. A team is named by its id and its
/// current slug.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ReviewRequest {
    pub pull_id: i64,
    pub reviewer: RequestedReviewer,
}

/// An SSH key an account holds for signing commits.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SigningKey {
    pub id: i64,
    pub owner_id: i64,
    /// The key's type and base64 blob, as OpenSSH writes a public key,
    /// without a comment.
    pub key: String,
    pub title: Option<String>,
    pub created_clock_unix: i64,
}

/// What a commit's signature is, read from the commit alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitSignature {
    Unsigned,
    /// An SSH signature: the key that made it, spelled as
    /// [`SigningKey::key`] spells one, and whether the signature verifies
    /// over the commit with that key.
    Ssh {
        key: String,
        verifies: bool,
    },
    /// An SSH signature that does not parse.
    MalformedSsh,
    /// A PGP or X.509 signature. No account in a world holds a key of
    /// either kind, since nothing registers one.
    Other,
}

/// A commit as the verdict on its signature reads it: what the commit
/// itself says, which no write changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitFacts {
    pub sha: String,
    pub committer_email: String,
    pub signature: CommitSignature,
}

/// The verdict on a commit's signature ([`World::commit_verification`]).
///
/// [`World::commit_verification`]: crate::World::commit_verification
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Unsigned,
    /// A PGP or X.509 signature, or an SSH signature whose key no account
    /// holds as a signing key.
    UnknownKey,
    /// An SSH signature that does not parse.
    Malformed,
    /// The signature does not verify with the key that made it.
    BadSignature,
    /// The key's account holds the committer address, and has verified it.
    Verified,
    /// The key's account holds the committer address, unverified.
    UnverifiedAddress,
    /// Other accounts hold the committer address, and the key's does not.
    OtherAccountsAddress,
    /// No account holds the committer address.
    NoAccount,
}

/// A verdict, and when a push stored it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitVerification {
    pub verdict: Verdict,
    /// When the push that brought the commit in stored its verdict, or
    /// `None` for a verdict reached on this read.
    pub stored_clock_unix: Option<i64>,
}
