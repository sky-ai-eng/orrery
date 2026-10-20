use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum WorldError {
    #[error("no world at {0} (missing world.db)")]
    NotFound(PathBuf),
    #[error("a world already exists at {0}")]
    AlreadyExists(PathBuf),
    #[error(
        "invalid login {0:?}: 1-39 alphanumeric/hyphen chars, no leading/trailing hyphen, not reserved"
    )]
    InvalidLogin(String),
    #[error("actors of kind 'system' are seeded by the engine, never created through the API")]
    InvalidActorKind,
    #[error("actor {0:?} already exists")]
    ActorExists(String),
    #[error("invalid repo name {0:?}: 1-100 chars of [A-Za-z0-9._-], not '.'/'..', no .git suffix")]
    InvalidRepoName(String),
    #[error("repo {0:?} already exists for this owner")]
    RepoExists(String),
    #[error("no repo with id {0}")]
    NoSuchRepo(i64),
    #[error("the clock only moves forward; got advance of {0} seconds")]
    ClockRegression(i64),
    #[error("an open pull request already exists for head {0:?}")]
    PullExists(String),
    #[error("no pull with id {0}")]
    NoSuchPull(i64),
    #[error("no check run with id {0}")]
    NoSuchCheckRun(i64),
    #[error("invalid label name {0:?}: 1-50 characters, not all whitespace")]
    InvalidLabel(String),
    #[error("pull has no label {0:?}")]
    NoSuchLabel(String),
    #[error("no organization {0:?}")]
    NoSuchOrg(String),
    #[error("{login:?} is not a member of {org:?}")]
    NotOrgMember { org: String, login: String },
    #[error("no team {0:?}")]
    NoSuchTeam(String),
    #[error("a team named {0:?} already exists in this organization")]
    TeamExists(String),
    #[error("invalid team name {0:?}: it must hold at least one letter or digit")]
    InvalidTeamName(String),
    #[error("{0:?} cannot be requested to review this pull")]
    InvalidReviewer(String),
    #[error("a review cannot be requested from the pull's author")]
    ReviewerIsAuthor,
    #[error("invalid state transition: {0}")]
    InvalidTransition(String),
    #[error("no actor {0:?}")]
    NoSuchActor(String),
    #[error("invalid emails: {0}")]
    InvalidEmails(String),
    #[error("invalid OAuth app {field}: {message}")]
    InvalidOAuthApp {
        field: &'static str,
        message: String,
    },
    #[error("the client id and secret do not name an OAuth app")]
    OAuthClientCredentials,
    #[error("the authorization code is unknown, already used, or expired")]
    OAuthBadCode,
    #[error("the redirect_uri does not match the one the code was issued for")]
    OAuthRedirectMismatch,
    #[error("world schema version {found} does not match this binary's {expected}")]
    SchemaVersion { found: i64, expected: i64 },
    #[error("world.db: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("world root io: {0}")]
    Io(#[from] std::io::Error),
}
