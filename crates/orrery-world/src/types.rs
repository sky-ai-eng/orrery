//! Domain types the kernel stores and serves. Vocabulary is vendor-neutral
//! (rule 9): faces translate these into GitHub (or any other vendor) wire
//! shapes; nothing here knows what a wire shape is.

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LogEntry {
    pub seq: i64,
    pub clock_unix: i64,
    pub actor_login: String,
    pub action: String,
    pub details: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActorEmail {
    pub email: String,
    pub primary: bool,
    pub verified: bool,
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

/// The result of trading an authorization code: a token acting as `actor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthToken {
    pub actor: Actor,
    pub token: String,
    pub scope: String,
}
