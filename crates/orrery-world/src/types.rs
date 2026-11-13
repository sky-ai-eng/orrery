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

/// How a world's clock moves (spec/usage.md, "Time").
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ClockMode {
    /// Only an advance moves it.
    #[default]
    Manual,
    /// It follows the host's time.
    Real,
}

impl ClockMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ClockMode::Manual => "manual",
            ClockMode::Real => "real",
        }
    }

    pub(crate) fn from_db(s: &str) -> ClockMode {
        match s {
            "real" => ClockMode::Real,
            _ => ClockMode::Manual,
        }
    }
}

/// How a new world's clock starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewClock {
    /// `manual`, at `t0_unix`.
    Manual { t0_unix: i64 },
    /// `real`, at the host's time.
    Real,
}

/// A world's clock as a read answers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockReading {
    /// What time the world holds it is, in Unix seconds. In a `real` world,
    /// the later of the host's time and the stored clock.
    pub clock_unix: i64,
    pub mode: ClockMode,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Actor {
    pub id: i64,
    pub login: String,
    pub kind: ActorKind,
    pub created_clock_unix: i64,
}

/// Who a request's token acts as, and the app it was issued to: the OAuth
/// app whose authorization code was traded for it, or the app whose
/// installation it is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub actor: Actor,
    pub app_id: Option<i64>,
    /// What an installation token may reach. `None` for any other token,
    /// which reaches every repository.
    pub installation: Option<InstallationGrant>,
}

impl Caller {
    /// The attribution of a write this caller makes through `door`.
    pub fn via(&self, door: crate::Door) -> crate::Attribution {
        crate::Attribution::new(self.actor.id, door).with_app(self.app_id)
    }

    /// Whether the caller's token reaches `repo`.
    pub fn reaches(&self, repo: &Repo) -> bool {
        self.installation
            .as_ref()
            .is_none_or(|grant| grant.covers(repo))
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

/// How far a permission reaches. Each level includes the ones before it.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum AccessLevel {
    Read,
    Write,
    Admin,
}

/// What an app, an installation, or an installation token may do, by
/// permission name. The world records and echoes them; nothing checks a
/// call against them.
pub type Permissions = std::collections::BTreeMap<String, AccessLevel>;

/// Which of its account's repositories an installation covers.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RepositorySelection {
    /// Every repository the account owns, those created later included.
    All,
    /// The repositories the installation lists.
    Selected,
}

impl RepositorySelection {
    pub fn as_str(self) -> &'static str {
        match self {
            RepositorySelection::All => "all",
            RepositorySelection::Selected => "selected",
        }
    }

    pub(crate) fn from_db(s: &str) -> RepositorySelection {
        match s {
            "selected" => RepositorySelection::Selected,
            _ => RepositorySelection::All,
        }
    }
}

/// An app: an OAuth app with a bot, which accounts install. Its `id` is its
/// OAuth app's.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct App {
    pub id: i64,
    /// The bot's login.
    pub slug: String,
    pub name: String,
    pub client_id: String,
    pub owner: Actor,
    pub bot: Actor,
    /// At least one. The first is the one an authorization redirects to when
    /// its request names none.
    pub callback_urls: Vec<String>,
    pub permissions: Permissions,
    pub events: Vec<String>,
    pub created_clock_unix: i64,
}

/// What registering an app answers: the app, and the client secret issued
/// with it, which no read returns again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredApp {
    pub app: App,
    pub client_secret: String,
}

/// An app to register.
#[derive(Debug, Clone, Copy)]
pub struct NewApp<'a> {
    pub slug: &'a str,
    pub name: &'a str,
    /// The login of the user or organization that owns it.
    pub owner: &'a str,
    pub callback_urls: &'a [String],
    pub permissions: &'a Permissions,
    pub events: &'a [String],
}

/// A public key an app's signatures are checked against.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AppKey {
    pub id: i64,
    /// The key as the base64 of its PKCS#1 `RSAPublicKey` DER.
    pub public_key: String,
    pub created_clock_unix: i64,
}

/// An app installed on an account.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Installation {
    pub id: i64,
    pub app_id: i64,
    pub app_slug: String,
    pub client_id: String,
    pub account: Actor,
    pub repository_selection: RepositorySelection,
    /// The repositories a `selected` installation lists, by name, in id
    /// order. Empty for `all`.
    pub repositories: Vec<String>,
    pub permissions: Permissions,
    pub events: Vec<String>,
    pub created_clock_unix: i64,
}

/// A token issued for an installation, as its issue answers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallationToken {
    pub token: String,
    pub installation: Installation,
    pub permissions: Permissions,
    /// The repositories the token was narrowed to, in id order, or `None`
    /// for one that reaches everything its installation covers.
    pub repositories: Option<Vec<Repo>>,
    pub expires_clock_unix: i64,
}

/// What an installation token may reach: the repositories its installation
/// covers, or the ones it was narrowed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallationGrant {
    pub installation_id: i64,
    /// The login of the account the app is installed on.
    pub account: String,
    /// The repositories by id, or `None` for every repository the account
    /// owns.
    pub repos: Option<Vec<i64>>,
}

impl InstallationGrant {
    pub fn covers(&self, repo: &Repo) -> bool {
        match &self.repos {
            Some(ids) => ids.contains(&repo.id),
            None => repo.owner_login.eq_ignore_ascii_case(&self.account),
        }
    }
}
