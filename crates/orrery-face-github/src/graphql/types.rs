//! The GitHub GraphQL schema subset, grown strictly on demand (spec/README.md
//! rule 1): every type and field exists because a consumer's query selects
//! it, and is listed in spec/surface-github.md §3. Fields the world genuinely has are
//! resolved from it; fields whose *write surface* doesn't exist yet (inline
//! review comments) resolve truthfully empty — a world with no way to write
//! one has none. A field
//! outside this subset fails validation, and the execution layer marks that
//! error as a coverage gap (`graphql::mark_unmocked_errors`) so it can never
//! read as "GitHub doesn't have this".

use std::collections::BTreeMap;
use std::sync::Arc;

use async_graphql::{
    Context, Enum, Error, ID, InputObject, InputValueError, InputValueResult, Interface, Object,
    Scalar, ScalarType, SimpleObject, Union, Value,
};
use base64::Engine as _;
use futures_util::future::join_all;
use orrery_face_git::{GateHold, GitFace, GitFaceError, store};
use orrery_world as world;

use crate::GithubFace;
use crate::nodeid::{self, NodeRef};
use crate::render::{
    MergeCheck, MergeRead, MergeRefs, Mergeability, merge_check, merge_read, merged_cleanly,
    rfc3339,
};

use super::search;

// ---- enums ----------------------------------------------------------------

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum SearchType {
    Issue,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum PullRequestState {
    Open,
    Closed,
    Merged,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum MergeableState {
    Mergeable,
    Conflicting,
    Unknown,
}

/// How a pull request changes a file.
#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum PatchStatus {
    Added,
    Deleted,
    Renamed,
    Copied,
    Modified,
    Changed,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum PullRequestReviewState {
    Pending,
    Commented,
    Approved,
    ChangesRequested,
    Dismissed,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum CheckStatusState {
    Queued,
    InProgress,
    Completed,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum CheckConclusionState {
    ActionRequired,
    Cancelled,
    Failure,
    Neutral,
    Skipped,
    Stale,
    Success,
    TimedOut,
}

// Variant names mirror GitHub's enum values; the postfix is theirs.
#[allow(clippy::enum_variant_names)]
#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum PullRequestTimelineItemsItemType {
    LabeledEvent,
    UnlabeledEvent,
    ReviewRequestedEvent,
    ReviewRequestRemovedEvent,
    ReadyForReviewEvent,
    ClosedEvent,
    HeadRefDeletedEvent,
    BaseRefDeletedEvent,
    HeadRefRestoredEvent,
}

// ---- shared plumbing ------------------------------------------------------

#[derive(SimpleObject, Default)]
pub struct PageInfo {
    pub has_next_page: bool,
    pub has_previous_page: bool,
    pub start_cursor: Option<String>,
    pub end_cursor: Option<String>,
}

/// GitHub's `BigInt`: a database id too wide for GraphQL's `Int`, serialized
/// as a string.
pub struct BigInt(pub i64);

#[Scalar(name = "BigInt")]
impl ScalarType for BigInt {
    fn parse(value: Value) -> InputValueResult<Self> {
        match &value {
            Value::String(s) => s
                .parse()
                .map(BigInt)
                .map_err(|_| InputValueError::expected_type(value)),
            Value::Number(n) => n
                .as_i64()
                .map(BigInt)
                .ok_or_else(|| InputValueError::expected_type(value)),
            _ => Err(InputValueError::expected_type(value)),
        }
    }

    fn to_value(&self) -> Value {
        Value::String(self.0.to_string())
    }
}

fn face<'a>(ctx: &Context<'a>) -> &'a GithubFace {
    ctx.data_unchecked::<GithubFace>()
}

/// The query's hold on the world write gate, which work it hands the
/// blocking pool keeps until that work ends.
fn gate<'a>(ctx: &Context<'a>) -> &'a GateHold {
    ctx.data_unchecked::<GateHold>()
}

fn internal(e: impl std::fmt::Display) -> Error {
    Error::new(format!("internal: {e}"))
}

fn unresolved(id: &str) -> Error {
    Error::new(format!(
        "Could not resolve to a node with the global id of '{id}'"
    ))
}

/// A connection cursor: opaque to consumers, naming the row an item came from.
fn cursor(id: i64) -> String {
    base64::engine::general_purpose::STANDARD.encode(format!("orrery:{id}"))
}

fn cursor_id(c: &str) -> Option<i64> {
    let raw = base64::engine::general_purpose::STANDARD.decode(c).ok()?;
    String::from_utf8(raw)
        .ok()?
        .strip_prefix("orrery:")?
        .parse()
        .ok()
}

/// GitHub's connection paging over `items` in ascending id order: `first`
/// from the front, or `last` from the back of whatever lies `before` a cursor.
/// Asking for both ends is refused, as GitHub refuses it.
fn page<T>(
    connection: &str,
    items: Vec<T>,
    id: impl Fn(&T) -> i64,
    first: Option<i32>,
    last: Option<i32>,
    before: Option<String>,
) -> Result<(Vec<T>, PageInfo), Error> {
    page_size(connection, first, last)?;
    let mut items = items;
    if let Some(b) = before {
        let bound = cursor_id(&b)
            .ok_or_else(|| Error::new(format!("`{b}` does not appear to be a valid cursor.")))?;
        items.retain(|item| id(item) < bound);
    }
    let (items, mut info) = match last {
        Some(n) => {
            let n = n.max(0) as usize;
            let cut = items.len().saturating_sub(n);
            let kept = items.split_off(cut);
            let info = PageInfo {
                has_previous_page: cut > 0,
                ..PageInfo::default()
            };
            (kept, info)
        }
        None => take_first(connection, items, first)?,
    };
    info.start_cursor = items.first().map(|i| cursor(id(i)));
    info.end_cursor = items.last().map(|i| cursor(id(i)));
    Ok((items, info))
}

/// GitHub's bounds on a connection's page: exactly one of `first` and
/// `last`, neither negative nor above 100.
fn page_size(connection: &str, first: Option<i32>, last: Option<i32>) -> Result<(), Error> {
    let (arg, n) = match (first, last) {
        (Some(_), Some(_)) => {
            return Err(Error::new(format!(
                "Passing both `first` and `last` to paginate the `{connection}` connection is not supported."
            )));
        }
        (None, None) => {
            return Err(Error::new(format!(
                "You must provide a `first` or `last` value to properly paginate the `{connection}` connection."
            )));
        }
        (Some(n), None) => ("first", n),
        (None, Some(n)) => ("last", n),
    };
    if n < 0 {
        return Err(Error::new(format!(
            "`{arg}` on the `{connection}` connection cannot be less than zero."
        )));
    }
    if n > 100 {
        return Err(Error::new(format!(
            "Requesting {n} records on the `{connection}` connection exceeds the `{arg}` limit of 100 records."
        )));
    }
    Ok(())
}

/// Truncate `items` to GitHub's `first:` argument, reporting the cut on
/// `pageInfo.hasNextPage` — consumers watchdog that flag, so a silent
/// truncation here would defeat their logging.
fn take_first<T>(
    connection: &str,
    mut items: Vec<T>,
    first: Option<i32>,
) -> Result<(Vec<T>, PageInfo), Error> {
    page_size(connection, first, None)?;
    let cap = first.unwrap_or_default() as usize;
    let truncated = items.len() > cap;
    items.truncate(cap);
    Ok((
        items,
        PageInfo {
            has_next_page: truncated,
            ..PageInfo::default()
        },
    ))
}

// ---- actors ---------------------------------------------------------------

/// `databaseId` here and on `Bot`, `Team` and `Organization` is GitHub's
/// `Int`: the same id REST answers as `id`.
#[derive(SimpleObject)]
pub struct User {
    pub login: String,
    pub database_id: Option<i64>,
}

/// A GitHub App's bot. GraphQL spells its login as the bare slug, without
/// the `[bot]` suffix REST adds.
#[derive(SimpleObject)]
pub struct Bot {
    pub login: String,
    pub database_id: Option<i64>,
}

#[derive(Interface)]
#[graphql(name = "Actor", field(name = "login", ty = "String"))]
pub enum GqlActor {
    User(User),
    Bot(Bot),
}

/// The actor a stored login names (an author, a merger, a closer), typed by
/// its kind. Actors are never deleted, so a lookup miss is an anomaly; it
/// degrades to a User with no id rather than failing the whole query.
fn author(face: &GithubFace, login: &str) -> GqlActor {
    match face.world.actor_by_login(login).ok().flatten() {
        Some(a) if a.kind == world::ActorKind::Bot => GqlActor::Bot(Bot {
            login: a.login,
            database_id: Some(a.id),
        }),
        found => GqlActor::User(User {
            login: login.to_string(),
            database_id: found.map(|a| a.id),
        }),
    }
}

/// The user a stored login names, degrading to no id on a lookup miss as
/// [`author`] does.
fn user(face: &GithubFace, login: &str) -> User {
    User {
        login: login.to_string(),
        database_id: face
            .world
            .actor_by_login(login)
            .ok()
            .flatten()
            .map(|a| a.id),
    }
}

// ---- teams / organizations ------------------------------------------------

#[derive(SimpleObject)]
pub struct TeamMemberConnection {
    pub total_count: i64,
}

pub struct Organization {
    pub id: i64,
    pub login: String,
}

#[Object]
impl Organization {
    async fn database_id(&self) -> Option<i64> {
        Some(self.id)
    }

    async fn login(&self) -> &str {
        &self.login
    }

    /// The organization's teams, oldest first; `userLogins` keeps the teams
    /// with at least one of those users on them.
    async fn teams(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
        user_logins: Option<Vec<String>>,
    ) -> Result<TeamConnection, Error> {
        let mut teams = face(ctx)
            .world
            .list_org_teams(self.id, user_logins.as_deref())
            .map_err(internal)?;
        if let Some(a) = after {
            let bound = cursor_id(&a).ok_or_else(|| {
                Error::new(format!("`{a}` does not appear to be a valid cursor."))
            })?;
            teams.retain(|t| t.id > bound);
        }
        let (teams, mut page_info) = take_first("teams", teams, first)?;
        page_info.start_cursor = teams.first().map(|t| cursor(t.id));
        page_info.end_cursor = teams.last().map(|t| cursor(t.id));
        Ok(TeamConnection {
            page_info,
            nodes: teams.into_iter().map(|team| Team { team }).collect(),
        })
    }
}

pub struct Team {
    pub team: world::Team,
}

#[Object]
impl Team {
    async fn database_id(&self) -> Option<i64> {
        Some(self.team.id)
    }

    async fn slug(&self) -> &str {
        &self.team.slug
    }

    async fn name(&self) -> &str {
        &self.team.name
    }

    async fn organization(&self) -> Organization {
        Organization {
            id: self.team.org_id,
            login: self.team.org_login.clone(),
        }
    }

    async fn members(&self, ctx: &Context<'_>) -> Result<TeamMemberConnection, Error> {
        Ok(TeamMemberConnection {
            total_count: face(ctx)
                .world
                .team_members_count(self.team.id)
                .map_err(internal)?,
        })
    }
}

#[derive(SimpleObject)]
pub struct TeamConnection {
    pub page_info: PageInfo,
    pub nodes: Vec<Team>,
}

#[derive(Union)]
pub enum RequestedReviewer {
    User(User),
    Team(Team),
}

/// A timeline item's reviewer. A team that has since been deleted resolves
/// to null, as GitHub renders a request whose reviewer is gone, even when a
/// later team took its slug.
fn requested_reviewer(
    face: &GithubFace,
    reviewer: &world::RequestedReviewer,
) -> Result<Option<RequestedReviewer>, Error> {
    Ok(match reviewer {
        world::RequestedReviewer::User(login) => Some(RequestedReviewer::User(user(face, login))),
        world::RequestedReviewer::Team { id, .. } => face
            .world
            .team_by_id(*id)
            .map_err(internal)?
            .map(|team| RequestedReviewer::Team(Team { team })),
    })
}

#[derive(SimpleObject)]
pub struct ReviewRequest {
    pub requested_reviewer: Option<RequestedReviewer>,
}

#[derive(SimpleObject)]
pub struct ReviewRequestConnection {
    pub page_info: PageInfo,
    pub nodes: Vec<ReviewRequest>,
}

// ---- repositories ---------------------------------------------------------

pub struct Repository {
    pub repo: world::Repo,
}

#[Object]
impl Repository {
    async fn name_with_owner(&self) -> String {
        self.repo.full_name()
    }

    #[graphql(name = "ref")]
    async fn git_ref(
        &self,
        ctx: &Context<'_>,
        qualified_name: String,
    ) -> Result<Option<Ref>, Error> {
        let face = face(ctx);
        let bare = store::bare_dir(
            &face.world.repos_dir(),
            &self.repo.owner_login,
            &self.repo.name,
        );
        let oid = {
            let _refs = face.git.read_lock(self.repo.id).await;
            store::rev_parse(&bare, &qualified_name).map_err(internal)?
        };
        Ok(oid.map(|_| Ref {
            id: ID(format!("O_ref_{}_{}", self.repo.id, qualified_name)),
        }))
    }
}

#[derive(SimpleObject)]
pub struct Ref {
    pub id: ID,
}

// ---- labels / comments / timeline -----------------------------------------

#[derive(SimpleObject)]
pub struct Label {
    pub name: String,
}

#[derive(SimpleObject)]
pub struct LabelConnection {
    pub page_info: PageInfo,
    pub nodes: Vec<Label>,
}

pub struct IssueComment {
    pub comment: world::IssueComment,
}

#[Object]
impl IssueComment {
    async fn full_database_id(&self) -> BigInt {
        BigInt(self.comment.id)
    }

    async fn author(&self, ctx: &Context<'_>) -> Option<GqlActor> {
        Some(author(face(ctx), &self.comment.author_login))
    }

    async fn body(&self) -> &str {
        &self.comment.body
    }

    async fn created_at(&self) -> String {
        rfc3339(self.comment.created_clock_unix)
    }
}

#[derive(SimpleObject)]
pub struct IssueCommentConnection {
    pub page_info: PageInfo,
    pub total_count: i64,
    pub nodes: Vec<IssueComment>,
}

/// GitHub's root `rateLimit` object. Orrery presents as a GHES with rate
/// limiting off, whose `rateLimit` is null, so this type is never returned;
/// it exists so consumer queries that select it validate.
#[derive(SimpleObject)]
pub struct RateLimit {
    pub cost: i64,
}

#[derive(SimpleObject)]
pub struct LabeledEvent {
    pub created_at: String,
    pub label: Label,
}

#[derive(SimpleObject)]
pub struct UnlabeledEvent {
    pub created_at: String,
    pub label: Label,
}

#[derive(SimpleObject)]
pub struct ReviewRequestedEvent {
    pub created_at: String,
    pub requested_reviewer: Option<RequestedReviewer>,
}

#[derive(SimpleObject)]
pub struct ReviewRequestRemovedEvent {
    pub created_at: String,
    pub requested_reviewer: Option<RequestedReviewer>,
}

#[derive(SimpleObject)]
pub struct ReadyForReviewEvent {
    pub created_at: String,
}

/// `PullRequest` has no closer field: who closed a pull is the actor of its
/// newest close.
#[derive(SimpleObject)]
pub struct ClosedEvent {
    pub actor: Option<GqlActor>,
}

#[derive(SimpleObject)]
pub struct HeadRefDeletedEvent {
    pub actor: Option<GqlActor>,
    pub head_ref_name: String,
}

#[derive(SimpleObject)]
pub struct BaseRefDeletedEvent {
    pub actor: Option<GqlActor>,
    pub base_ref_name: Option<String>,
}

#[derive(SimpleObject)]
pub struct HeadRefRestoredEvent {
    pub actor: Option<GqlActor>,
}

// Variant names mirror GitHub's type names; the postfix is theirs.
#[allow(clippy::enum_variant_names)]
#[derive(Union)]
pub enum PullRequestTimelineItems {
    LabeledEvent(LabeledEvent),
    UnlabeledEvent(UnlabeledEvent),
    ReviewRequestedEvent(ReviewRequestedEvent),
    ReviewRequestRemovedEvent(ReviewRequestRemovedEvent),
    ReadyForReviewEvent(ReadyForReviewEvent),
    ClosedEvent(ClosedEvent),
    HeadRefDeletedEvent(HeadRefDeletedEvent),
    BaseRefDeletedEvent(BaseRefDeletedEvent),
    HeadRefRestoredEvent(HeadRefRestoredEvent),
}

#[derive(SimpleObject)]
pub struct PullRequestTimelineItemsConnection {
    pub page_info: PageInfo,
    pub nodes: Vec<PullRequestTimelineItems>,
}

// ---- reviews --------------------------------------------------------------

pub struct PullRequestReview {
    pub review: world::Review,
}

#[Object]
impl PullRequestReview {
    async fn id(&self) -> ID {
        ID(nodeid::review(self.review.id))
    }

    async fn full_database_id(&self) -> BigInt {
        BigInt(self.review.id)
    }

    async fn author(&self, ctx: &Context<'_>) -> Option<GqlActor> {
        Some(author(face(ctx), &self.review.author_login))
    }

    async fn state(&self) -> PullRequestReviewState {
        review_state(self.review.state)
    }

    async fn submitted_at(&self) -> Option<String> {
        Some(rfc3339(self.review.submitted_clock_unix))
    }

    async fn body(&self) -> String {
        self.review.body.clone().unwrap_or_default()
    }

    /// Inline review comments have no write surface (the REST face refuses
    /// `comments[]` on review creation), so every review truthfully has none.
    async fn comments(&self, _first: Option<i32>) -> PullRequestReviewCommentConnection {
        PullRequestReviewCommentConnection {
            page_info: PageInfo::default(),
            total_count: 0,
            nodes: Vec::new(),
        }
    }
}

fn review_state(s: world::ReviewState) -> PullRequestReviewState {
    match s {
        world::ReviewState::Approved => PullRequestReviewState::Approved,
        world::ReviewState::ChangesRequested => PullRequestReviewState::ChangesRequested,
        world::ReviewState::Commented => PullRequestReviewState::Commented,
    }
}

#[derive(SimpleObject)]
pub struct PullRequestReviewComment {
    pub id: ID,
    pub full_database_id: Option<BigInt>,
    pub path: String,
    pub line: Option<i64>,
    pub start_line: Option<i64>,
    pub body: String,
}

#[derive(SimpleObject)]
pub struct PullRequestReviewCommentConnection {
    pub page_info: PageInfo,
    pub total_count: i64,
    pub nodes: Vec<PullRequestReviewComment>,
}

#[derive(SimpleObject)]
pub struct PullRequestReviewConnection {
    pub page_info: PageInfo,
    pub total_count: i64,
    pub nodes: Vec<PullRequestReview>,
}

// ---- check runs -----------------------------------------------------------

#[derive(SimpleObject)]
#[graphql(name = "WorkflowRun")]
pub struct GqlWorkflowRun {
    pub database_id: Option<i64>,
}

#[derive(SimpleObject)]
#[graphql(name = "CheckSuite")]
pub struct GqlCheckSuite {
    pub workflow_run: Option<GqlWorkflowRun>,
}

pub struct GqlCheckRun {
    pub run: world::CheckRun,
}

#[Object(name = "CheckRun")]
impl GqlCheckRun {
    async fn database_id(&self) -> Option<i64> {
        Some(self.run.id)
    }

    async fn name(&self) -> &str {
        &self.run.name
    }

    async fn status(&self) -> CheckStatusState {
        match self.run.status {
            world::CheckStatus::Queued => CheckStatusState::Queued,
            world::CheckStatus::InProgress => CheckStatusState::InProgress,
            world::CheckStatus::Completed => CheckStatusState::Completed,
        }
    }

    async fn conclusion(&self) -> Option<CheckConclusionState> {
        self.run.conclusion.map(|c| match c {
            world::CheckConclusion::Success => CheckConclusionState::Success,
            world::CheckConclusion::Failure => CheckConclusionState::Failure,
            world::CheckConclusion::Neutral => CheckConclusionState::Neutral,
            world::CheckConclusion::Cancelled => CheckConclusionState::Cancelled,
            world::CheckConclusion::Skipped => CheckConclusionState::Skipped,
            world::CheckConclusion::Stale => CheckConclusionState::Stale,
            world::CheckConclusion::TimedOut => CheckConclusionState::TimedOut,
            world::CheckConclusion::ActionRequired => CheckConclusionState::ActionRequired,
        })
    }

    async fn completed_at(&self) -> Option<String> {
        self.run.completed_clock_unix.map(rfc3339)
    }

    async fn details_url(&self) -> Option<String> {
        self.run.details_url.clone()
    }

    /// The kernel has no check-suite / workflow model, so Actions linkage is
    /// truthfully absent — the same shape third-party CI produces on GitHub.
    async fn check_suite(&self) -> Option<GqlCheckSuite> {
        None
    }
}

#[derive(Union)]
pub enum StatusCheckRollupContext {
    CheckRun(GqlCheckRun),
}

#[derive(SimpleObject)]
pub struct StatusCheckRollupContextConnection {
    pub page_info: PageInfo,
    pub nodes: Vec<StatusCheckRollupContext>,
}

pub struct StatusCheckRollup {
    pub runs: Vec<world::CheckRun>,
}

#[Object]
impl StatusCheckRollup {
    async fn contexts(
        &self,
        first: Option<i32>,
    ) -> Result<StatusCheckRollupContextConnection, Error> {
        let nodes: Vec<StatusCheckRollupContext> = self
            .runs
            .iter()
            .map(|r| StatusCheckRollupContext::CheckRun(GqlCheckRun { run: r.clone() }))
            .collect();
        let (nodes, page_info) = take_first("contexts", nodes, first)?;
        Ok(StatusCheckRollupContextConnection { page_info, nodes })
    }
}

// ---- commits --------------------------------------------------------------

pub struct Commit {
    pub repo: world::Repo,
    pub sha: String,
    /// The batch of the pull request this commit heads, which works out its
    /// date with the other members' heads.
    batch: Arc<PullBatch>,
}

#[Object]
impl Commit {
    async fn oid(&self) -> &str {
        &self.sha
    }

    async fn committed_date(&self, ctx: &Context<'_>) -> Result<String, Error> {
        let unix = self.batch.commit_date(ctx, &self.repo, &self.sha).await?;
        Ok(rfc3339(unix))
    }

    /// `null` when the commit has no check runs at all — GitHub's shape, and
    /// the one consumers read as "no CI here" rather than "CI unknown".
    async fn status_check_rollup(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Option<StatusCheckRollup>, Error> {
        let face = face(ctx);
        let runs = face
            .world
            .list_check_runs_for_sha(self.repo.id, &self.sha)
            .map_err(internal)?;
        Ok((!runs.is_empty()).then_some(StatusCheckRollup { runs }))
    }
}

#[derive(SimpleObject)]
pub struct PullRequestChangedFile {
    pub path: String,
    pub change_type: PatchStatus,
}

#[derive(SimpleObject)]
pub struct PullRequestChangedFileConnection {
    pub page_info: PageInfo,
    pub nodes: Vec<PullRequestChangedFile>,
}

#[derive(SimpleObject)]
pub struct PullRequestCommit {
    pub commit: Commit,
}

#[derive(SimpleObject)]
pub struct PullRequestCommitConnection {
    pub page_info: PageInfo,
    pub nodes: Vec<PullRequestCommit>,
}

// ---- pull requests --------------------------------------------------------

pub struct PullRequest {
    pub pull: world::Pull,
    pub repo: world::Repo,
    batch: Arc<PullBatch>,
}

impl PullRequest {
    pub fn new(pull: world::Pull, repo: world::Repo) -> PullRequest {
        PullRequest::batch(vec![(pull, repo)])
            .pop()
            .expect("one pull request for one pull")
    }

    /// The pull requests of one list, sharing one [`PullBatch`].
    pub fn batch(members: Vec<(world::Pull, world::Repo)>) -> Vec<PullRequest> {
        let batch = Arc::new(PullBatch::new(members));
        batch
            .members
            .iter()
            .map(|(pull, repo)| PullRequest {
                pull: pull.clone(),
                repo: repo.clone(),
                batch: batch.clone(),
            })
            .collect()
    }

    fn load_reviews(&self, face: &GithubFace) -> Result<Vec<world::Review>, Error> {
        face.world
            .list_reviews_for_pull(self.pull.id)
            .map_err(internal)
    }
}

/// The pull requests one field resolves to, such as the nodes of one `nodes`
/// or `search`. GraphQL asks each of them for the same fields, so the first
/// to resolve a field that git answers works it out for all of them, with a
/// few git processes for each repository among them rather than a few for
/// each pull request. Each repository's share of a question is one unit of
/// work on the blocking pool, and as many units run at once as the git face
/// has slots ([`GitFace::batch_read`]). Each answer is kept by pull id, so a
/// pull listed twice is worked out once, and the order units finish in
/// changes no answer.
pub struct PullBatch {
    members: Vec<(world::Pull, world::Repo)>,
    current: tokio::sync::OnceCell<Answers<i64, (world::Pull, MergeRefs)>>,
    mergeability: tokio::sync::OnceCell<Answers<i64, Mergeability>>,
    diff_stats: tokio::sync::OnceCell<Answers<i64, store::DiffStats>>,
    files: tokio::sync::OnceCell<Answers<i64, Arc<[store::ChangedFile]>>>,
    commit_dates: tokio::sync::OnceCell<Answers<(i64, String), i64>>,
}

/// A batch's answers to one question, each an answer or the error message
/// its field reports.
type Answers<K, T> = BTreeMap<K, Result<T, String>>;

impl PullBatch {
    fn new(members: Vec<(world::Pull, world::Repo)>) -> PullBatch {
        PullBatch {
            members,
            current: Default::default(),
            mergeability: Default::default(),
            diff_stats: Default::default(),
            files: Default::default(),
            commit_dates: Default::default(),
        }
    }

    /// The members by repository, each pull once.
    fn by_repo(&self) -> BTreeMap<i64, (&world::Repo, Vec<&world::Pull>)> {
        let mut by_repo: BTreeMap<i64, (&world::Repo, Vec<&world::Pull>)> = BTreeMap::new();
        for (pull, repo) in &self.members {
            let (_, pulls) = by_repo.entry(repo.id).or_insert((repo, Vec::new()));
            if !pulls.iter().any(|p| p.id == pull.id) {
                pulls.push(pull);
            }
        }
        by_repo
    }

    /// Each pull as it is now, and the branch tips its diff and mergeability
    /// are computed from, read together under its repository's lock. The row
    /// a member was built from can be older than the refs: an open row read
    /// before a merge, with the base's tip read after it, would compare the
    /// head against the commit that merged it.
    async fn current(&self, ctx: &Context<'_>) -> &Answers<i64, (world::Pull, MergeRefs)> {
        self.current
            .get_or_init(|| async {
                let face = face(ctx);
                let units = self.by_repo().into_values().map(|(repo, pulls)| {
                    let world = face.world.clone();
                    let bare = bare(face, repo);
                    let repo_id = repo.id;
                    let asked: Vec<(i64, i64)> = pulls.iter().map(|p| (p.id, p.number)).collect();
                    let ids: Vec<i64> = pulls.iter().map(|p| p.id).collect();
                    async move {
                        let read = face
                            .git
                            .batch_read(gate(ctx), repo_id, move || {
                                read_current(&world, &bare, repo_id, &asked)
                            })
                            .await;
                        spread(ids, read)
                    }
                });
                join_all(units).await.into_iter().flatten().collect()
            })
            .await
    }

    async fn mergeability(&self, ctx: &Context<'_>, pull_id: i64) -> Result<Mergeability, Error> {
        let verdicts = self
            .mergeability
            .get_or_init(|| async {
                let current = self.current(ctx).await;
                let face = face(ctx);
                let by = ctx
                    .data_unchecked::<world::Caller>()
                    .via(world::Door::Graphql);
                let mut verdicts = BTreeMap::new();
                let mut merges = Vec::new();
                for (repo, pulls) in self.by_repo().into_values() {
                    let mut asked: Vec<(i64, (String, String))> = Vec::new();
                    let mut stale: Vec<(i64, i64)> = Vec::new();
                    for pull in pulls {
                        match &current[&pull.id] {
                            Err(e) => {
                                verdicts.insert(pull.id, Err(e.clone()));
                            }
                            Ok((now, refs)) => match merge_check(now, refs) {
                                MergeCheck::Known(verdict) => {
                                    verdicts.insert(pull.id, Ok(verdict));
                                }
                                MergeCheck::Stale => stale.push((pull.id, pull.number)),
                                MergeCheck::TestMerge { base, head } => {
                                    asked.push((pull.id, (base.to_string(), head.to_string())));
                                }
                            },
                        }
                    }
                    merges.push((repo, asked));
                    if stale.is_empty() {
                        continue;
                    }
                    // One repository after another, in id order, and each
                    // one's pulls in the order asked, so the batch's writes
                    // land in the log in one order however its other units
                    // are scheduled.
                    let (world, bare) = (face.world.clone(), bare(face, repo));
                    let (repo_id, ids) = (repo.id, stale.iter().map(|s| s.0).collect());
                    let started = face
                        .git
                        .batch_read(gate(ctx), repo_id, move || {
                            start_stale(&world, &bare, repo_id, &stale, by)
                        })
                        .await;
                    verdicts.extend(spread(ids, started));
                }
                verdicts.extend(
                    ask_each_repo(ctx, merges, |git, repo, pairs| {
                        git.merges_clean(repo, &borrowed(pairs))
                            .into_iter()
                            .map(|clean| clean.map(merged_cleanly).map_err(|e| internal(e).message))
                            .collect()
                    })
                    .await,
                );
                verdicts
            })
            .await;
        answer(verdicts, &pull_id)
    }

    /// For each member, the answer `compare` gives for its diff's ends: the
    /// base it compares its head against ([`MergeRefs::diff_base`]), and its
    /// head. `compare` is asked once for each repository, with every member
    /// of it whose ends were read.
    async fn compare_each<T, F>(&self, ctx: &Context<'_>, compare: F) -> Answers<i64, T>
    where
        T: Send + 'static,
        F: Fn(&GitFace, &world::Repo, &[(&str, &str)]) -> Vec<Result<T, String>>
            + Copy
            + Send
            + 'static,
    {
        let current = self.current(ctx).await;
        let mut answers = BTreeMap::new();
        let mut comparisons = Vec::new();
        for (repo, pulls) in self.by_repo().into_values() {
            let mut asked: Vec<(i64, (String, String))> = Vec::new();
            for pull in pulls {
                match &current[&pull.id] {
                    Ok((now, refs)) => {
                        asked.push((pull.id, (refs.diff_base(now), now.head_sha.clone())))
                    }
                    Err(e) => {
                        answers.insert(pull.id, Err(e.clone()));
                    }
                }
            }
            comparisons.push((repo, asked));
        }
        answers.extend(
            ask_each_repo(ctx, comparisons, move |git, repo, pairs| {
                compare(git, repo, &borrowed(pairs))
            })
            .await,
        );
        answers
    }

    async fn diff_stats(&self, ctx: &Context<'_>, pull_id: i64) -> Result<store::DiffStats, Error> {
        let stats = self
            .diff_stats
            .get_or_init(|| async {
                self.compare_each(ctx, |git, repo, pairs| {
                    git.diff_stats(repo, pairs)
                        .into_iter()
                        .map(|stats| stats.map_err(|e| e.to_string()))
                        .collect()
                })
                .await
            })
            .await;
        answer(stats, &pull_id)
    }

    async fn files(
        &self,
        ctx: &Context<'_>,
        pull_id: i64,
    ) -> Result<Arc<[store::ChangedFile]>, Error> {
        let files = self
            .files
            .get_or_init(|| async {
                self.compare_each(ctx, |git, repo, pairs| {
                    git.changed_files(repo, pairs)
                        .into_iter()
                        .map(|files| files.map_err(|e| internal(e).message))
                        .collect()
                })
                .await
            })
            .await;
        answer(files, &pull_id)
    }

    /// The committer date of `sha`, which heads the commits of a member in
    /// `repo`. The dates of every member's head are worked out together.
    async fn commit_date(
        &self,
        ctx: &Context<'_>,
        repo: &world::Repo,
        sha: &str,
    ) -> Result<i64, Error> {
        let dates = self
            .commit_dates
            .get_or_init(|| async {
                let heads = self
                    .by_repo()
                    .into_values()
                    .map(|(repo, pulls)| {
                        let asked = pulls
                            .iter()
                            .map(|p| ((repo.id, p.head_sha.clone()), p.head_sha.clone()))
                            .collect();
                        (repo, asked)
                    })
                    .collect();
                ask_each_repo(ctx, heads, |git, repo, shas: &[String]| {
                    let shas: Vec<&str> = shas.iter().map(String::as_str).collect();
                    shas.iter()
                        .zip(git.commit_dates(repo, &shas))
                        .map(|(sha, date)| match date {
                            Ok(Some(date)) => Ok(date),
                            Ok(None) => Err(internal(format!(
                                "commit {sha} not found in {}",
                                repo.full_name()
                            ))
                            .message),
                            Err(e) => Err(internal(e).message),
                        })
                        .collect()
                })
                .await
                .into_iter()
                .collect()
            })
            .await;
        answer(dates, &(repo.id, sha.to_string()))
    }
}

/// The answer each pull of `stale`, by id and number, gives a read by `by`
/// that found its test merge stale, which starts one ([`merge_read`]). The
/// caller holds the repository's lock for read, and each pull is read again
/// under it, so a write that landed since the batch read the pull, such as a
/// push that closed it and deleted its head, decides what this does. A pull
/// that another read started a test merge for in between still has no
/// answer.
fn start_stale(
    world: &world::World,
    bare: &std::path::Path,
    repo_id: i64,
    stale: &[(i64, i64)],
    by: world::Attribution,
) -> Vec<Result<Mergeability, String>> {
    read_current(world, bare, repo_id, stale)
        .into_iter()
        .map(|read| {
            let (now, refs) = read?;
            match merge_read(world, &now, &refs, by) {
                Ok(MergeRead::Known(verdict)) => Ok(verdict),
                Ok(MergeRead::TestMerge { .. }) => Ok(Mergeability::Unknown),
                Err(e) => Err(internal(e).message),
            }
        })
        .collect()
}

/// Each pull of `asked`, by id and number, as it is now in the repository,
/// with the branch tips read with it. The caller holds the repository's lock
/// for read.
fn read_current(
    world: &world::World,
    bare: &std::path::Path,
    repo_id: i64,
    asked: &[(i64, i64)],
) -> Vec<Result<(world::Pull, MergeRefs), String>> {
    let rows: Vec<Result<world::Pull, String>> = asked
        .iter()
        .map(
            |&(id, number)| match world.pull_by_number(repo_id, number) {
                Ok(Some(now)) => Ok(now),
                Ok(None) => Err(internal(format!("pull request {id} is gone")).message),
                Err(e) => Err(internal(e).message),
            },
        )
        .collect();
    let found: Vec<&world::Pull> = rows.iter().filter_map(|row| row.as_ref().ok()).collect();
    let tips: Vec<Result<MergeRefs, String>> = match MergeRefs::read_many(bare, &found) {
        Ok(tips) => tips.into_iter().map(Ok).collect(),
        Err(e) => {
            let e = internal(e).message;
            found.iter().map(|_| Err(e.clone())).collect()
        }
    };
    let mut tips = tips.into_iter();
    rows.into_iter()
        .map(|row| {
            let now = row?;
            let refs = tips.next().expect("tips for each pull found")?;
            Ok((now, refs))
        })
        .collect()
}

/// Each repository's questions, as `(key, question)`, asked of `ask` in one
/// unit of work for each repository with any ([`GitFace::batch_work`]), and
/// answered by key. The questions are about commits named by their full ids,
/// so no unit holds its repository's lock.
async fn ask_each_repo<K, Q, T, F>(
    ctx: &Context<'_>,
    asked: Vec<(&world::Repo, Vec<(K, Q)>)>,
    ask: F,
) -> Vec<(K, Result<T, String>)>
where
    Q: Send + 'static,
    T: Send + 'static,
    F: Fn(&GitFace, &world::Repo, &[Q]) -> Vec<Result<T, String>> + Copy + Send + 'static,
{
    let face = face(ctx);
    let units = asked
        .into_iter()
        .filter(|(_, questions)| !questions.is_empty())
        .map(|(repo, questions)| {
            let (keys, questions): (Vec<K>, Vec<Q>) = questions.into_iter().unzip();
            let git = face.git.clone();
            let repo = repo.clone();
            async move {
                let worked = face
                    .git
                    .batch_work(gate(ctx), move || ask(&git, &repo, &questions))
                    .await;
                spread(keys, worked)
            }
        });
    join_all(units).await.into_iter().flatten().collect()
}

/// A unit's answers by the keys it was asked about, in order, or, when the
/// unit failed, its failure for each of them.
fn spread<K, T>(
    keys: Vec<K>,
    worked: Result<Vec<Result<T, String>>, GitFaceError>,
) -> Vec<(K, Result<T, String>)> {
    match worked {
        Ok(answers) => keys.into_iter().zip(answers).collect(),
        Err(e) => {
            let e = internal(e).message;
            keys.into_iter().map(|key| (key, Err(e.clone()))).collect()
        }
    }
}

fn borrowed(pairs: &[(String, String)]) -> Vec<(&str, &str)> {
    pairs
        .iter()
        .map(|(base, head)| (base.as_str(), head.as_str()))
        .collect()
}

/// The answer a batch keeps for `key`.
fn answer<K: Ord, T: Clone>(answers: &Answers<K, T>, key: &K) -> Result<T, Error> {
    match answers.get(key) {
        Some(Ok(value)) => Ok(value.clone()),
        Some(Err(e)) => Err(Error::new(e.clone())),
        None => Err(internal("the batch worked out no answer for this")),
    }
}

fn bare(face: &GithubFace, repo: &world::Repo) -> std::path::PathBuf {
    store::bare_dir(&face.world.repos_dir(), &repo.owner_login, &repo.name)
}

#[Object]
impl PullRequest {
    async fn id(&self) -> ID {
        ID(nodeid::pull(self.pull.id))
    }

    async fn number(&self) -> i64 {
        self.pull.number
    }

    async fn title(&self) -> &str {
        &self.pull.title
    }

    /// GitHub's GraphQL `body` is non-null: an unset body reads as the empty
    /// string here, while the REST face renders it as `null`.
    async fn body(&self) -> &str {
        self.pull.body.as_deref().unwrap_or("")
    }

    async fn author(&self, ctx: &Context<'_>) -> Option<GqlActor> {
        Some(author(face(ctx), &self.pull.author_login))
    }

    async fn state(&self) -> PullRequestState {
        if self.pull.merged {
            PullRequestState::Merged
        } else if self.pull.state == world::PullState::Open {
            PullRequestState::Open
        } else {
            PullRequestState::Closed
        }
    }

    async fn is_draft(&self) -> bool {
        self.pull.draft
    }

    async fn merged(&self) -> bool {
        self.pull.merged
    }

    async fn merged_by(&self, ctx: &Context<'_>) -> Option<GqlActor> {
        let login = self.pull.merged_by_login.as_deref()?;
        Some(author(face(ctx), login))
    }

    async fn mergeable(&self, ctx: &Context<'_>) -> Result<MergeableState, Error> {
        Ok(match self.batch.mergeability(ctx, self.pull.id).await? {
            Mergeability::Clean => MergeableState::Mergeable,
            Mergeability::Conflicting => MergeableState::Conflicting,
            Mergeability::Unknown => MergeableState::Unknown,
        })
    }

    async fn head_ref_name(&self) -> &str {
        &self.pull.head_ref
    }

    async fn head_ref_oid(&self) -> &str {
        &self.pull.head_sha
    }

    async fn base_ref_name(&self) -> &str {
        &self.pull.base_ref
    }

    async fn url(&self, ctx: &Context<'_>) -> String {
        let base = face(ctx).base_url.trim_end_matches('/');
        format!(
            "{}/{}/pull/{}",
            base,
            self.repo.full_name(),
            self.pull.number
        )
    }

    async fn repository(&self) -> Repository {
        Repository {
            repo: self.repo.clone(),
        }
    }

    /// Forks are unmodeled: the head always lives in the base repository.
    async fn head_repository(&self) -> Option<Repository> {
        Some(Repository {
            repo: self.repo.clone(),
        })
    }

    async fn additions(&self, ctx: &Context<'_>) -> Result<i64, Error> {
        Ok(self.batch.diff_stats(ctx, self.pull.id).await?.additions)
    }

    async fn deletions(&self, ctx: &Context<'_>) -> Result<i64, Error> {
        Ok(self.batch.diff_stats(ctx, self.pull.id).await?.deletions)
    }

    async fn changed_files(&self, ctx: &Context<'_>) -> Result<i64, Error> {
        Ok(self
            .batch
            .diff_stats(ctx, self.pull.id)
            .await?
            .changed_files)
    }

    /// The files the pull changes, in path order, paged by `first`/`after`.
    /// A cursor names the count of files before it, so it is stable while
    /// the pull's head and base do not move.
    async fn files(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
    ) -> Result<PullRequestChangedFileConnection, Error> {
        let mut skip = 0;
        if let Some(a) = after {
            let consumed = cursor_id(&a).ok_or_else(|| {
                Error::new(format!("`{a}` does not appear to be a valid cursor."))
            })?;
            skip = consumed.max(0) as usize;
        }
        let files = self.batch.files(ctx, self.pull.id).await?;
        let rest: Vec<PullRequestChangedFile> = files
            .iter()
            .skip(skip)
            .map(|f| PullRequestChangedFile {
                path: f.path.clone(),
                change_type: match f.status {
                    store::FileStatus::Added => PatchStatus::Added,
                    store::FileStatus::Removed => PatchStatus::Deleted,
                    store::FileStatus::Renamed => PatchStatus::Renamed,
                    store::FileStatus::Copied => PatchStatus::Copied,
                    store::FileStatus::Modified => PatchStatus::Modified,
                    store::FileStatus::Changed => PatchStatus::Changed,
                },
            })
            .collect();
        let (nodes, mut page_info) = take_first("files", rest, first)?;
        page_info.end_cursor = (!nodes.is_empty()).then(|| cursor((skip + nodes.len()) as i64));
        Ok(PullRequestChangedFileConnection { page_info, nodes })
    }

    /// The pull's pending requests: users, then teams, each in request order.
    async fn review_requests(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
    ) -> Result<ReviewRequestConnection, Error> {
        let face = face(ctx);
        let mut reviewers: Vec<ReviewRequest> = self
            .pull
            .requested_reviewers
            .iter()
            .map(|login| ReviewRequest {
                requested_reviewer: Some(RequestedReviewer::User(user(face, login))),
            })
            .collect();
        for slug in &self.pull.requested_teams {
            let team = face
                .world
                .team_by_slug(&self.repo.owner_login, slug)
                .map_err(internal)?;
            reviewers.push(ReviewRequest {
                requested_reviewer: team.map(|team| RequestedReviewer::Team(Team { team })),
            });
        }
        let (nodes, page_info) = take_first("reviewRequests", reviewers, first)?;
        Ok(ReviewRequestConnection { page_info, nodes })
    }

    /// GitHub's latest-review-per-author connection.
    async fn latest_reviews(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
    ) -> Result<PullRequestReviewConnection, Error> {
        let all = self.load_reviews(face(ctx))?;
        let mut latest: std::collections::BTreeMap<String, world::Review> = Default::default();
        for r in all {
            latest.insert(r.author_login.clone(), r);
        }
        let mut nodes: Vec<world::Review> = latest.into_values().collect();
        nodes.sort_by_key(|r| r.id);
        let total = nodes.len() as i64;
        let (nodes, page_info) = take_first("latestReviews", nodes, first)?;
        Ok(PullRequestReviewConnection {
            page_info,
            total_count: total,
            nodes: nodes
                .into_iter()
                .map(|review| PullRequestReview { review })
                .collect(),
        })
    }

    async fn reviews(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        last: Option<i32>,
        before: Option<String>,
        states: Option<Vec<PullRequestReviewState>>,
    ) -> Result<PullRequestReviewConnection, Error> {
        let mut all = self.load_reviews(face(ctx))?;
        if let Some(states) = states {
            all.retain(|r| states.contains(&review_state(r.state)));
        }
        let total = all.len() as i64;
        let (nodes, page_info) = page("reviews", all, |r| r.id, first, last, before)?;
        Ok(PullRequestReviewConnection {
            page_info,
            total_count: total,
            nodes: nodes
                .into_iter()
                .map(|review| PullRequestReview { review })
                .collect(),
        })
    }

    async fn commits(&self, _last: Option<i32>) -> PullRequestCommitConnection {
        PullRequestCommitConnection {
            page_info: PageInfo::default(),
            nodes: vec![PullRequestCommit {
                commit: Commit {
                    repo: self.repo.clone(),
                    sha: self.pull.head_sha.clone(),
                    batch: self.batch.clone(),
                },
            }],
        }
    }

    async fn labels(&self, first: Option<i32>) -> Result<Option<LabelConnection>, Error> {
        let names = self.pull.labels.clone();
        let (names, page_info) = take_first("labels", names, first)?;
        Ok(Some(LabelConnection {
            page_info,
            nodes: names.into_iter().map(|name| Label { name }).collect(),
        }))
    }

    async fn comments(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        last: Option<i32>,
        before: Option<String>,
    ) -> Result<IssueCommentConnection, Error> {
        let comments = face(ctx)
            .world
            .list_issue_comments_for_pull(self.pull.id)
            .map_err(internal)?;
        let total = comments.len() as i64;
        let (nodes, page_info) = page("comments", comments, |c| c.id, first, last, before)?;
        Ok(IssueCommentConnection {
            page_info,
            total_count: total,
            nodes: nodes
                .into_iter()
                .map(|comment| IssueComment { comment })
                .collect(),
        })
    }

    async fn created_at(&self) -> String {
        rfc3339(self.pull.created_clock_unix)
    }

    async fn updated_at(&self) -> String {
        rfc3339(self.pull.updated_clock_unix)
    }

    async fn merged_at(&self) -> Option<String> {
        self.pull
            .merged
            .then_some(self.pull.closed_clock_unix)
            .flatten()
            .map(rfc3339)
    }

    async fn closed_at(&self) -> Option<String> {
        self.pull.closed_clock_unix.map(rfc3339)
    }

    async fn timeline_items(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        last: Option<i32>,
        before: Option<String>,
        item_types: Option<Vec<PullRequestTimelineItemsItemType>>,
    ) -> Result<PullRequestTimelineItemsConnection, Error> {
        let mut items = face(ctx)
            .world
            .list_pull_timeline(self.pull.id)
            .map_err(internal)?;
        if let Some(types) = item_types {
            items.retain(|i| types.contains(&timeline_item_type(&i.kind)));
        }
        let (items, page_info) = page("timelineItems", items, |i| i.id, first, last, before)?;
        let face = face(ctx);
        Ok(PullRequestTimelineItemsConnection {
            page_info,
            nodes: items
                .into_iter()
                .map(|i| timeline_item(face, i))
                .collect::<Result<_, Error>>()?,
        })
    }
}

fn timeline_item_type(kind: &world::TimelineKind) -> PullRequestTimelineItemsItemType {
    match kind {
        world::TimelineKind::Labeled(_) => PullRequestTimelineItemsItemType::LabeledEvent,
        world::TimelineKind::Unlabeled(_) => PullRequestTimelineItemsItemType::UnlabeledEvent,
        world::TimelineKind::ReadyForReview => {
            PullRequestTimelineItemsItemType::ReadyForReviewEvent
        }
        world::TimelineKind::ReviewRequested(_) => {
            PullRequestTimelineItemsItemType::ReviewRequestedEvent
        }
        world::TimelineKind::ReviewRequestRemoved(_) => {
            PullRequestTimelineItemsItemType::ReviewRequestRemovedEvent
        }
        world::TimelineKind::Closed => PullRequestTimelineItemsItemType::ClosedEvent,
        world::TimelineKind::HeadRefDeleted(_) => {
            PullRequestTimelineItemsItemType::HeadRefDeletedEvent
        }
        world::TimelineKind::BaseRefDeleted(_) => {
            PullRequestTimelineItemsItemType::BaseRefDeletedEvent
        }
        world::TimelineKind::HeadRefRestored => {
            PullRequestTimelineItemsItemType::HeadRefRestoredEvent
        }
    }
}

fn timeline_item(
    face: &GithubFace,
    item: world::TimelineItem,
) -> Result<PullRequestTimelineItems, Error> {
    let created_at = rfc3339(item.clock_unix);
    Ok(match item.kind {
        world::TimelineKind::Labeled(name) => {
            PullRequestTimelineItems::LabeledEvent(LabeledEvent {
                created_at,
                label: Label { name },
            })
        }
        world::TimelineKind::Unlabeled(name) => {
            PullRequestTimelineItems::UnlabeledEvent(UnlabeledEvent {
                created_at,
                label: Label { name },
            })
        }
        world::TimelineKind::ReadyForReview => {
            PullRequestTimelineItems::ReadyForReviewEvent(ReadyForReviewEvent { created_at })
        }
        world::TimelineKind::ReviewRequested(rv) => {
            PullRequestTimelineItems::ReviewRequestedEvent(ReviewRequestedEvent {
                created_at,
                requested_reviewer: requested_reviewer(face, &rv)?,
            })
        }
        world::TimelineKind::ReviewRequestRemoved(rv) => {
            PullRequestTimelineItems::ReviewRequestRemovedEvent(ReviewRequestRemovedEvent {
                created_at,
                requested_reviewer: requested_reviewer(face, &rv)?,
            })
        }
        world::TimelineKind::Closed => PullRequestTimelineItems::ClosedEvent(ClosedEvent {
            actor: Some(author(face, &item.actor_login)),
        }),
        world::TimelineKind::HeadRefDeleted(head_ref_name) => {
            PullRequestTimelineItems::HeadRefDeletedEvent(HeadRefDeletedEvent {
                actor: Some(author(face, &item.actor_login)),
                head_ref_name,
            })
        }
        world::TimelineKind::BaseRefDeleted(base_ref_name) => {
            PullRequestTimelineItems::BaseRefDeletedEvent(BaseRefDeletedEvent {
                actor: Some(author(face, &item.actor_login)),
                base_ref_name: Some(base_ref_name),
            })
        }
        world::TimelineKind::HeadRefRestored => {
            PullRequestTimelineItems::HeadRefRestoredEvent(HeadRefRestoredEvent {
                actor: Some(author(face, &item.actor_login)),
            })
        }
    })
}

// ---- node / search roots --------------------------------------------------

// A Node exists once per resolved id (a refresh batch is ~20), so the size
// gap between a PullRequest and a review is noise, not a footprint.
#[allow(clippy::large_enum_variant)]
#[derive(Interface)]
#[graphql(name = "Node", field(name = "id", ty = "ID"))]
pub enum Node {
    PullRequest(PullRequest),
    PullRequestReview(PullRequestReview),
}

#[derive(Union)]
pub enum SearchResultItem {
    PullRequest(PullRequest),
}

#[derive(SimpleObject)]
pub struct SearchResultItemConnection {
    pub page_info: PageInfo,
    pub nodes: Vec<SearchResultItem>,
}

/// What a node id names. One exists for each id of one `nodes` call, only
/// until its node is built.
#[allow(clippy::large_enum_variant)]
enum Found {
    Pull(world::Pull, world::Repo),
    Review(world::Review),
}

fn find_node(face: &GithubFace, id: &str) -> Result<Option<Found>, Error> {
    match nodeid::parse(id) {
        Some(NodeRef::Pull(pull_id)) => {
            let Some(pull) = face.world.pull_by_id(pull_id).map_err(internal)? else {
                return Ok(None);
            };
            let repo = face
                .world
                .repo_by_id(pull.repo_id)
                .map_err(internal)?
                .ok_or_else(|| internal("pull's repo row missing"))?;
            Ok(Some(Found::Pull(pull, repo)))
        }
        Some(NodeRef::Review(review_id)) => {
            let Some(review) = face.world.review_by_id(review_id).map_err(internal)? else {
                return Ok(None);
            };
            Ok(Some(Found::Review(review)))
        }
        None => Ok(None),
    }
}

/// The nodes `ids` name, in order, their pull requests sharing one batch.
fn resolve_nodes(face: &GithubFace, ids: &[ID]) -> Result<Vec<Option<Node>>, Error> {
    let found: Vec<Option<Found>> = ids
        .iter()
        .map(|id| find_node(face, id))
        .collect::<Result<_, Error>>()?;
    let pulls: Vec<(world::Pull, world::Repo)> = found
        .iter()
        .filter_map(|f| match f {
            Some(Found::Pull(pull, repo)) => Some((pull.clone(), repo.clone())),
            _ => None,
        })
        .collect();
    let mut pulls = PullRequest::batch(pulls).into_iter();
    Ok(found
        .into_iter()
        .map(|f| {
            f.map(|f| match f {
                Found::Pull(..) => Node::PullRequest(pulls.next().expect("one per pull found")),
                Found::Review(review) => Node::PullRequestReview(PullRequestReview { review }),
            })
        })
        .collect())
}

pub struct QueryRoot;

#[Object]
impl QueryRoot {
    /// Batch node lookup. An id that doesn't resolve yields a null entry —
    /// GitHub's shape, which a consumer refreshing a batch of ids skips.
    async fn nodes(&self, ctx: &Context<'_>, ids: Vec<ID>) -> Result<Vec<Option<Node>>, Error> {
        resolve_nodes(face(ctx), &ids)
    }

    async fn node(&self, ctx: &Context<'_>, id: ID) -> Result<Option<Node>, Error> {
        Ok(resolve_nodes(face(ctx), &[id])?.pop().flatten())
    }

    /// Null: rate limiting is off, as on a GHES configured without it.
    async fn rate_limit(&self) -> Option<RateLimit> {
        None
    }

    async fn search(
        &self,
        ctx: &Context<'_>,
        query: String,
        #[graphql(name = "type")] search_type: SearchType,
        first: Option<i32>,
    ) -> Result<SearchResultItemConnection, Error> {
        let SearchType::Issue = search_type;
        let parsed = search::parse(&query).map_err(|term| {
            Error::new(format!(
                "orrery: search qualifier {term:?} is not mocked (query: {query:?})"
            ))
        })?;
        let matches = search::eval(face(ctx), &parsed).map_err(internal)?;
        let (matches, page_info) = take_first("search", matches, first)?;
        Ok(SearchResultItemConnection {
            page_info,
            nodes: PullRequest::batch(matches)
                .into_iter()
                .map(SearchResultItem::PullRequest)
                .collect(),
        })
    }

    /// `null` for a login that isn't an org-kind actor — GitHub's answer for
    /// user accounts, which consumers read as "no teams here" (never an
    /// error).
    async fn organization(
        &self,
        ctx: &Context<'_>,
        login: String,
    ) -> Result<Option<Organization>, Error> {
        let actor = face(ctx).world.actor_by_login(&login).map_err(internal)?;
        Ok(actor
            .filter(|a| a.kind == world::ActorKind::Org)
            .map(|a| Organization {
                id: a.id,
                login: a.login,
            }))
    }

    async fn repository(
        &self,
        ctx: &Context<'_>,
        owner: String,
        name: String,
    ) -> Result<Option<Repository>, Error> {
        let repo = face(ctx)
            .world
            .repo_by_owner_name(&owner, &name)
            .map_err(internal)?;
        Ok(repo.map(|repo| Repository { repo }))
    }
}

// ---- mutations ------------------------------------------------------------

#[derive(InputObject)]
pub struct MarkPullRequestReadyForReviewInput {
    pub pull_request_id: ID,
    pub client_mutation_id: Option<String>,
}

#[derive(SimpleObject)]
pub struct MarkPullRequestReadyForReviewPayload {
    pub pull_request: Option<PullRequest>,
    pub client_mutation_id: Option<String>,
}

#[derive(InputObject)]
pub struct ConvertPullRequestToDraftInput {
    pub pull_request_id: ID,
    pub client_mutation_id: Option<String>,
}

#[derive(SimpleObject)]
pub struct ConvertPullRequestToDraftPayload {
    pub pull_request: Option<PullRequest>,
    pub client_mutation_id: Option<String>,
}

fn set_draft(ctx: &Context<'_>, id: &ID, draft: bool) -> Result<PullRequest, Error> {
    let face = face(ctx);
    let caller = ctx.data::<world::Caller>()?;
    let Some(NodeRef::Pull(pull_id)) = nodeid::parse(id) else {
        return Err(unresolved(id));
    };
    let pull = face
        .world
        .set_pull_draft(pull_id, caller.via(world::Door::Graphql), draft)
        .map_err(|e| match e {
            world::WorldError::NoSuchPull(_) => unresolved(id),
            world::WorldError::InvalidTransition(msg) => Error::new(msg),
            other => internal(other),
        })?;
    let repo = face
        .world
        .repo_by_id(pull.repo_id)
        .map_err(internal)?
        .ok_or_else(|| internal("pull's repo row missing"))?;
    Ok(PullRequest::new(pull, repo))
}

pub struct MutationRoot;

#[Object]
impl MutationRoot {
    async fn mark_pull_request_ready_for_review(
        &self,
        ctx: &Context<'_>,
        input: MarkPullRequestReadyForReviewInput,
    ) -> Result<MarkPullRequestReadyForReviewPayload, Error> {
        let pull_request = set_draft(ctx, &input.pull_request_id, false)?;
        Ok(MarkPullRequestReadyForReviewPayload {
            pull_request: Some(pull_request),
            client_mutation_id: input.client_mutation_id,
        })
    }

    async fn convert_pull_request_to_draft(
        &self,
        ctx: &Context<'_>,
        input: ConvertPullRequestToDraftInput,
    ) -> Result<ConvertPullRequestToDraftPayload, Error> {
        let pull_request = set_draft(ctx, &input.pull_request_id, true)?;
        Ok(ConvertPullRequestToDraftPayload {
            pull_request: Some(pull_request),
            client_mutation_id: input.client_mutation_id,
        })
    }
}
