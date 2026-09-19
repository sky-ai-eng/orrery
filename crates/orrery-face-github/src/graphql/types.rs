//! The GitHub GraphQL schema subset, grown strictly on demand (spec/README.md
//! rule 1): every type and field exists because a consumer query in
//! spec/surface-github.md §3 selects it. Fields the world genuinely has are
//! resolved from it; fields whose *write surface* doesn't exist yet (labels,
//! review requests, timeline events, inline review comments) resolve
//! truthfully empty — a world with no label writes has no labels. A field
//! outside this subset fails validation, and the execution layer marks that
//! error as a coverage gap (`graphql::mark_unmocked_errors`) so it can never
//! read as "GitHub doesn't have this".

use std::sync::OnceLock;

use async_graphql::{
    Context, Enum, Error, ID, InputObject, Interface, Object, SimpleObject, Union,
};
use orrery_face_git::store;
use orrery_world as world;

use crate::GithubFace;
use crate::nodeid::{self, NodeRef};
use crate::render::rfc3339;

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
    ReadyForReviewEvent,
}

// ---- shared plumbing ------------------------------------------------------

#[derive(SimpleObject, Default)]
pub struct PageInfo {
    pub has_next_page: bool,
    pub end_cursor: Option<String>,
}

fn face<'a>(ctx: &Context<'a>) -> &'a GithubFace {
    ctx.data_unchecked::<GithubFace>()
}

fn internal(e: impl std::fmt::Display) -> Error {
    Error::new(format!("internal: {e}"))
}

fn unresolved(id: &str) -> Error {
    Error::new(format!(
        "Could not resolve to a node with the global id of '{id}'"
    ))
}

/// Truncate `items` to GitHub's `first:` argument, reporting the cut on
/// `pageInfo.hasNextPage` — consumers watchdog that flag, so a silent
/// truncation here would defeat their logging.
fn take_first<T>(mut items: Vec<T>, first: Option<i32>) -> (Vec<T>, PageInfo) {
    let cap = first.map(|f| f.max(0) as usize).unwrap_or(items.len());
    let truncated = items.len() > cap;
    items.truncate(cap);
    (
        items,
        PageInfo {
            has_next_page: truncated,
            end_cursor: None,
        },
    )
}

// ---- actors ---------------------------------------------------------------

#[derive(SimpleObject)]
pub struct User {
    pub login: String,
}

#[derive(Interface)]
#[graphql(name = "Actor", field(name = "login", ty = "String"))]
pub enum GqlActor {
    User(User),
}

fn author(login: &str) -> GqlActor {
    GqlActor::User(User {
        login: login.to_string(),
    })
}

// ---- teams / organizations ------------------------------------------------

#[derive(SimpleObject)]
pub struct TeamMemberConnection {
    pub total_count: i64,
}

pub struct Organization {
    pub login: String,
}

#[Object]
impl Organization {
    async fn login(&self) -> &str {
        &self.login
    }

    /// Teams are unmodeled in the kernel, so every organization truthfully
    /// has none — the same answer GitHub gives for an org that never created
    /// teams, and the answer TF's team listing treats as "user is in no
    /// teams" rather than an error.
    async fn teams(
        &self,
        _first: Option<i32>,
        _after: Option<String>,
        _user_logins: Option<Vec<String>>,
    ) -> TeamConnection {
        TeamConnection {
            page_info: PageInfo::default(),
            nodes: Vec::new(),
        }
    }
}

#[derive(SimpleObject)]
pub struct Team {
    pub slug: String,
    pub name: String,
    pub organization: Organization,
    pub members: TeamMemberConnection,
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
        let oid = store::rev_parse(&bare, &qualified_name).map_err(internal)?;
        Ok(oid.map(|_| Ref {
            id: ID(format!("O_ref_{}_{}", self.repo.id, qualified_name)),
        }))
    }
}

#[derive(SimpleObject)]
pub struct Ref {
    pub id: ID,
}

// ---- labels / comments / timeline (write surfaces not yet mocked) ---------

#[derive(SimpleObject)]
pub struct Label {
    pub name: String,
}

#[derive(SimpleObject)]
pub struct LabelConnection {
    pub page_info: PageInfo,
    pub nodes: Vec<Label>,
}

#[derive(SimpleObject)]
pub struct IssueCommentConnection {
    pub page_info: PageInfo,
    pub total_count: i64,
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
pub struct ReadyForReviewEvent {
    pub created_at: String,
}

// Variant names mirror GitHub's type names; the postfix is theirs.
#[allow(clippy::enum_variant_names)]
#[derive(Union)]
pub enum PullRequestTimelineItems {
    LabeledEvent(LabeledEvent),
    UnlabeledEvent(UnlabeledEvent),
    ReviewRequestedEvent(ReviewRequestedEvent),
    ReadyForReviewEvent(ReadyForReviewEvent),
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

    async fn author(&self) -> Option<GqlActor> {
        Some(author(&self.review.author_login))
    }

    async fn state(&self) -> PullRequestReviewState {
        match self.review.state {
            world::ReviewState::Approved => PullRequestReviewState::Approved,
            world::ReviewState::ChangesRequested => PullRequestReviewState::ChangesRequested,
            world::ReviewState::Commented => PullRequestReviewState::Commented,
        }
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
            nodes: Vec::new(),
        }
    }
}

#[derive(SimpleObject)]
pub struct PullRequestReviewComment {
    pub id: ID,
    pub path: String,
    pub line: Option<i64>,
    pub start_line: Option<i64>,
    pub body: String,
}

#[derive(SimpleObject)]
pub struct PullRequestReviewCommentConnection {
    pub page_info: PageInfo,
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
    async fn contexts(&self, first: Option<i32>) -> StatusCheckRollupContextConnection {
        let nodes: Vec<StatusCheckRollupContext> = self
            .runs
            .iter()
            .map(|r| StatusCheckRollupContext::CheckRun(GqlCheckRun { run: r.clone() }))
            .collect();
        let (nodes, page_info) = take_first(nodes, first);
        StatusCheckRollupContextConnection { page_info, nodes }
    }
}

// ---- commits --------------------------------------------------------------

pub struct Commit {
    pub repo: world::Repo,
    pub sha: String,
}

#[Object]
impl Commit {
    async fn oid(&self) -> &str {
        &self.sha
    }

    async fn committed_date(&self, ctx: &Context<'_>) -> Result<String, Error> {
        let face = face(ctx);
        let bare = store::bare_dir(
            &face.world.repos_dir(),
            &self.repo.owner_login,
            &self.repo.name,
        );
        let unix = store::commit_date_unix(&bare, &self.sha)
            .map_err(internal)?
            .ok_or_else(|| {
                internal(format!(
                    "commit {} not found in {}",
                    self.sha,
                    self.repo.full_name()
                ))
            })?;
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
    /// Lazily computed diff totals, shared by the additions / deletions /
    /// changedFiles resolvers so one selection costs one `git diff`.
    diff: OnceLock<Result<store::DiffStats, String>>,
}

impl PullRequest {
    pub fn new(pull: world::Pull, repo: world::Repo) -> PullRequest {
        PullRequest {
            pull,
            repo,
            diff: OnceLock::new(),
        }
    }

    fn bare(&self, face: &GithubFace) -> std::path::PathBuf {
        store::bare_dir(
            &face.world.repos_dir(),
            &self.repo.owner_login,
            &self.repo.name,
        )
    }

    /// The comparison base for diff stats and mergeability: the base
    /// branch's current tip while the pull is open (GitHub recomputes
    /// against the moving base), the pinned base sha once it is terminal
    /// (GitHub freezes the numbers).
    fn diff_base(&self, face: &GithubFace) -> Result<String, Error> {
        if self.pull.state == world::PullState::Open {
            let tip = store::rev_parse(
                &self.bare(face),
                &format!("refs/heads/{}", self.pull.base_ref),
            )
            .map_err(internal)?;
            if let Some(tip) = tip {
                return Ok(tip);
            }
        }
        Ok(self.pull.base_sha.clone())
    }

    fn diff_stats(&self, face: &GithubFace) -> Result<store::DiffStats, Error> {
        self.diff
            .get_or_init(|| {
                let base = self.diff_base(face).map_err(|e| e.message.clone())?;
                store::diff_stats(&self.bare(face), &base, &self.pull.head_sha)
                    .map_err(|e| e.to_string())
            })
            .clone()
            .map_err(Error::new)
    }

    fn load_reviews(&self, face: &GithubFace) -> Result<Vec<world::Review>, Error> {
        face.world
            .list_reviews_for_pull(self.pull.id)
            .map_err(internal)
    }
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

    async fn author(&self) -> Option<GqlActor> {
        Some(author(&self.pull.author_login))
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

    async fn mergeable(&self, ctx: &Context<'_>) -> Result<MergeableState, Error> {
        if self.pull.state != world::PullState::Open {
            return Ok(MergeableState::Unknown);
        }
        let face = face(ctx);
        let bare = self.bare(face);
        let Some(base_tip) = store::rev_parse(&bare, &format!("refs/heads/{}", self.pull.base_ref))
            .map_err(internal)?
        else {
            return Ok(MergeableState::Unknown);
        };
        let merged = store::merge_tree(&bare, &base_tip, &self.pull.head_sha).map_err(internal)?;
        Ok(match merged {
            Some(_) => MergeableState::Mergeable,
            None => MergeableState::Conflicting,
        })
    }

    async fn head_ref_name(&self) -> &str {
        &self.pull.head_ref
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
        Ok(self.diff_stats(face(ctx))?.additions)
    }

    async fn deletions(&self, ctx: &Context<'_>) -> Result<i64, Error> {
        Ok(self.diff_stats(face(ctx))?.deletions)
    }

    async fn changed_files(&self, ctx: &Context<'_>) -> Result<i64, Error> {
        Ok(self.diff_stats(face(ctx))?.changed_files)
    }

    /// Review requests have no write surface yet, so every pull truthfully
    /// has none pending.
    async fn review_requests(&self, _first: Option<i32>) -> ReviewRequestConnection {
        ReviewRequestConnection {
            page_info: PageInfo::default(),
            nodes: Vec::new(),
        }
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
        let (nodes, page_info) = take_first(nodes, first);
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
    ) -> Result<PullRequestReviewConnection, Error> {
        let all = self.load_reviews(face(ctx))?;
        let total = all.len() as i64;
        let (nodes, page_info) = take_first(all, first);
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
                },
            }],
        }
    }

    /// Labels have no write surface yet, so every pull truthfully has none.
    async fn labels(&self, _first: Option<i32>) -> Option<LabelConnection> {
        Some(LabelConnection {
            page_info: PageInfo::default(),
            nodes: Vec::new(),
        })
    }

    async fn comments(&self, ctx: &Context<'_>) -> Result<IssueCommentConnection, Error> {
        let comments = face(ctx)
            .world
            .list_issue_comments_for_pull(self.pull.id)
            .map_err(internal)?;
        Ok(IssueCommentConnection {
            page_info: PageInfo::default(),
            total_count: comments.len() as i64,
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

    /// No timeline events exist because none of the actions that would mint
    /// them (labeling, review requests, ready-for-review before this commit's
    /// log format) have write surfaces that record them yet.
    async fn timeline_items(
        &self,
        _last: Option<i32>,
        _item_types: Option<Vec<PullRequestTimelineItemsItemType>>,
    ) -> PullRequestTimelineItemsConnection {
        PullRequestTimelineItemsConnection {
            page_info: PageInfo::default(),
            nodes: Vec::new(),
        }
    }
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

fn resolve_node(face: &GithubFace, id: &str) -> Result<Option<Node>, Error> {
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
            Ok(Some(Node::PullRequest(PullRequest::new(pull, repo))))
        }
        Some(NodeRef::Review(review_id)) => {
            let Some(review) = face.world.review_by_id(review_id).map_err(internal)? else {
                return Ok(None);
            };
            Ok(Some(Node::PullRequestReview(PullRequestReview { review })))
        }
        None => Ok(None),
    }
}

pub struct QueryRoot;

#[Object]
impl QueryRoot {
    /// Batch node lookup. An id that doesn't resolve yields a null entry —
    /// GitHub's shape, which consumers skip (TF's refresh drops null nodes).
    async fn nodes(&self, ctx: &Context<'_>, ids: Vec<ID>) -> Result<Vec<Option<Node>>, Error> {
        let face = face(ctx);
        ids.iter().map(|id| resolve_node(face, id)).collect()
    }

    async fn node(&self, ctx: &Context<'_>, id: ID) -> Result<Option<Node>, Error> {
        resolve_node(face(ctx), &id)
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
        let (matches, page_info) = take_first(matches, first.or(Some(30)));
        Ok(SearchResultItemConnection {
            page_info,
            nodes: matches
                .into_iter()
                .map(|(pull, repo)| SearchResultItem::PullRequest(PullRequest::new(pull, repo)))
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
            .map(|a| Organization { login: a.login }))
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
    let actor = ctx.data::<world::Actor>()?;
    let Some(NodeRef::Pull(pull_id)) = nodeid::parse(id) else {
        return Err(unresolved(id));
    };
    let pull = face
        .world
        .set_pull_draft(pull_id, actor.id, draft)
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
