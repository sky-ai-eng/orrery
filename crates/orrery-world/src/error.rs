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
    #[error("invalid state transition: {0}")]
    InvalidTransition(String),
    #[error("world schema version {found} does not match this binary's {expected}")]
    SchemaVersion { found: i64, expected: i64 },
    #[error("world.db: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("world root io: {0}")]
    Io(#[from] std::io::Error),
}
