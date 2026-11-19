//! Dependency alerts: a vulnerable dependency a repository holds, and what
//! has been done about it. They are fabricated state, as check runs are: the
//! vendor makes an alert itself, from a scan a world does not run, so the
//! kernel stores what it is told. Who may move an alert between which states
//! is the callers' to say: a person dismisses and reopens one, and the
//! vendor fixes, reintroduces, and auto-dismisses one.
//!
//! A repository has alerts only while they are turned on for it.

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::events::{self, Attribution, EventBody};
use crate::log::Write;
use crate::types::*;
use crate::{Result, World, WorldError, objects, queries};

/// An alert's columns, in the order [`row_to_alert`] reads them: the
/// dismisser's and the update's pull request's columns come from joins.
const ALERT_SELECT: &str = "SELECT a.id, a.repo_id, a.number, a.state, a.ecosystem, a.package,
        a.manifest_path, a.scope, a.version_requirement, a.advisory_id, a.cve_id, a.summary,
        a.description, a.severity, a.vulnerable_version_range, a.first_patched_version,
        a.created_clock_unix, a.updated_clock_unix, a.dismissed_clock_unix, d.id, d.login,
        d.kind, d.created_clock_unix, a.dismissed_reason, a.dismissed_comment,
        a.fixed_clock_unix, a.auto_dismissed_clock_unix, a.update_state, p.id, p.number,
        a.update_error_type, a.update_error_title
     FROM dependency_alerts a
     LEFT JOIN actors d ON d.id = a.dismissed_by_id
     LEFT JOIN pulls p ON p.id = a.update_pull_id";

/// A new alert, as the vendor would report one.
#[derive(Debug, Clone)]
pub struct NewDependencyAlert<'a> {
    pub ecosystem: &'a str,
    pub package: &'a str,
    pub manifest_path: &'a str,
    pub scope: Option<AlertScope>,
    pub version_requirement: Option<&'a str>,
    pub advisory_id: &'a str,
    pub cve_id: Option<&'a str>,
    pub summary: &'a str,
    pub description: &'a str,
    pub severity: AlertSeverity,
    pub vulnerable_version_range: &'a str,
    pub first_patched_version: Option<&'a str>,
}

/// Which alerts a read holds, in which order, and from where.
#[derive(Debug, Clone, Default)]
pub struct AlertQuery {
    /// Any of these states. `None` is every state, and an empty list is no
    /// alert at all.
    pub states: Option<Vec<AlertState>>,
    pub severities: Option<Vec<AlertSeverity>>,
    pub ecosystems: Option<Vec<String>>,
    pub packages: Option<Vec<String>>,
    pub manifests: Option<Vec<String>>,
    pub scope: Option<AlertScope>,
    /// Ordered by when each was last updated, where the default is by when
    /// each was made. An alert's number breaks a tie either way.
    pub by_updated: bool,
    /// Oldest first, where the default is newest first.
    pub ascending: bool,
    /// Only alerts past this one in the order.
    pub after: Option<AlertPosition>,
    /// Only alerts ahead of this one in the order: the page ends just
    /// before it.
    pub before: Option<AlertPosition>,
}

/// Where an alert stands in an order: the time it is ordered by, and its
/// number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlertPosition {
    pub clock_unix: i64,
    pub number: i64,
}

impl AlertQuery {
    /// Where `alert` stands in this query's order.
    pub fn position(&self, alert: &DependencyAlert) -> AlertPosition {
        AlertPosition {
            clock_unix: match self.by_updated {
                true => alert.updated_clock_unix,
                false => alert.created_clock_unix,
            },
            number: alert.number,
        }
    }

    /// The SQL condition this query's filters make, and their values.
    fn filter(&self) -> (String, Vec<String>) {
        let mut sql = String::new();
        let mut values = Vec::new();
        let mut any_of = |column: &str, wanted: Vec<String>| {
            let marks: Vec<String> = wanted
                .into_iter()
                .map(|value| {
                    values.push(value);
                    format!("?{}", values.len() + 1)
                })
                .collect();
            sql.push_str(&match marks.is_empty() {
                true => " AND 0".to_string(),
                false => format!(" AND a.{column} IN ({})", marks.join(", ")),
            });
        };
        if let Some(states) = &self.states {
            any_of(
                "state",
                states.iter().map(|s| s.as_str().to_string()).collect(),
            );
        }
        if let Some(severities) = &self.severities {
            any_of(
                "severity",
                severities.iter().map(|s| s.as_str().to_string()).collect(),
            );
        }
        if let Some(ecosystems) = &self.ecosystems {
            any_of("ecosystem", ecosystems.clone());
        }
        if let Some(packages) = &self.packages {
            any_of("package", packages.clone());
        }
        if let Some(manifests) = &self.manifests {
            any_of("manifest_path", manifests.clone());
        }
        if let Some(scope) = self.scope {
            any_of("scope", vec![scope.as_str().to_string()]);
        }
        (sql, values)
    }
}

impl World {
    /// Turn a repository's alerts on or off. Its alerts stay as they are
    /// while they are off, and are read again once they are on. Setting what
    /// is already so writes nothing.
    pub fn set_dependency_alerts(&self, repo_id: i64, on: bool, by: Attribution) -> Result<Repo> {
        self.write(by, |w| {
            let tx = w.tx;
            let before = objects::repo_object(tx, repo_id)?;
            let repo = tx.query_row(
                &format!(
                    "UPDATE repos SET dependency_alerts = ?1 WHERE id = ?2 RETURNING {}",
                    queries::REPO_COLUMNS
                ),
                params![on, repo_id],
                queries::row_to_repo,
            )?;
            let changes =
                events::RepoChanges::between(&before, &objects::repo_object(tx, repo_id)?);
            if !changes.is_empty() {
                w.operation(EventBody::RepoDependencyAlertsSet(events::RepoChanged {
                    subject: objects::repo_subject(tx, repo_id)?,
                    changes,
                }));
            }
            Ok(repo)
        })
    }

    /// Make an alert on a repository whose alerts are on. It is numbered
    /// from 1 within its repository, and starts open.
    pub fn create_dependency_alert(
        &self,
        repo_id: i64,
        new: &NewDependencyAlert<'_>,
        by: Attribution,
    ) -> Result<DependencyAlert> {
        self.write(by, |w| {
            let tx = w.tx;
            require_alerts_on(tx, repo_id)?;
            let clock = queries::clock(tx)?;
            let id: i64 = tx.query_row(
                "INSERT INTO dependency_alerts (
                     repo_id, number, state, ecosystem, package, manifest_path, scope,
                     version_requirement, advisory_id, cve_id, summary, description, severity,
                     vulnerable_version_range, first_patched_version, created_clock_unix,
                     updated_clock_unix, update_state)
                 VALUES (?1, (SELECT COALESCE(MAX(number), 0) + 1 FROM dependency_alerts
                              WHERE repo_id = ?1),
                         'open', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14,
                         'none')
                 RETURNING id",
                params![
                    repo_id,
                    new.ecosystem,
                    new.package,
                    new.manifest_path,
                    new.scope.map(AlertScope::as_str),
                    new.version_requirement,
                    new.advisory_id,
                    new.cve_id,
                    new.summary,
                    new.description,
                    new.severity.as_str(),
                    new.vulnerable_version_range,
                    new.first_patched_version,
                    clock
                ],
                |r| r.get(0),
            )?;
            w.operation(EventBody::DependencyAlertCreated(
                events::DependencyAlertCreated {
                    subject: objects::dependency_alert_subject(tx, id)?,
                    object: objects::dependency_alert_object(tx, id)?,
                },
            ));
            alert_by_id(tx, id)
        })
    }

    /// Move an alert to another state, as `transition` names it, when the
    /// state it is in allows that. `by` is who did it: the person, for a
    /// dismissal or a reopening, and whoever stands for the vendor for the
    /// rest. An alert leaves a dismissal behind when it leaves `dismissed`.
    pub fn move_dependency_alert(
        &self,
        repo_id: i64,
        number: i64,
        transition: &AlertTransition<'_>,
        by: Attribution,
    ) -> Result<DependencyAlert> {
        self.write(by, |w| {
            let tx = w.tx;
            require_alerts_on(tx, repo_id)?;
            let alert = require_alert(tx, repo_id, number)?;
            let refused = || WorldError::DependencyAlertState {
                number,
                state: alert.state,
            };
            use AlertState::{AutoDismissed, Dismissed, Fixed, Open};
            let (to, event): (AlertState, fn(events::DependencyAlertChanged) -> EventBody) =
                match (transition, alert.state) {
                    (AlertTransition::Dismiss { .. }, Open) => {
                        (Dismissed, EventBody::DependencyAlertDismissed)
                    }
                    (AlertTransition::Reopen, Dismissed | AutoDismissed) => {
                        (Open, EventBody::DependencyAlertReopened)
                    }
                    (AlertTransition::Fix, Open | Dismissed | AutoDismissed) => {
                        (Fixed, EventBody::DependencyAlertFixed)
                    }
                    (AlertTransition::AutoDismiss, Open) => {
                        (AutoDismissed, EventBody::DependencyAlertAutoDismissed)
                    }
                    (AlertTransition::Reintroduce, Fixed) => {
                        (Open, EventBody::DependencyAlertReintroduced)
                    }
                    (AlertTransition::Reintroduce, AutoDismissed) => {
                        (Open, EventBody::DependencyAlertAutoReopened)
                    }
                    _ => return Err(refused()),
                };
            let clock = queries::clock(tx)?;
            let dismissal = match transition {
                AlertTransition::Dismiss { reason, comment } => {
                    Some((clock, w.by.actor_id, reason.as_str(), *comment))
                }
                _ => None,
            };
            let before = objects::dependency_alert_object(tx, alert.id)?;
            tx.execute(
                "UPDATE dependency_alerts
                 SET state = ?1, updated_clock_unix = ?2,
                     dismissed_clock_unix = ?3, dismissed_by_id = ?4, dismissed_reason = ?5,
                     dismissed_comment = ?6,
                     fixed_clock_unix = CASE ?1 WHEN 'fixed' THEN ?2 END,
                     auto_dismissed_clock_unix = CASE ?1 WHEN 'auto_dismissed' THEN ?2 END
                 WHERE id = ?7",
                params![
                    to.as_str(),
                    clock,
                    dismissal.map(|d| d.0),
                    dismissal.map(|d| d.1),
                    dismissal.map(|d| d.2),
                    dismissal.and_then(|d| d.3),
                    alert.id
                ],
            )?;
            changed(w, alert.id, &before, event)?;
            alert_by_id(tx, alert.id)
        })
    }

    /// Record what the vendor's updater did about an alert: nothing, an
    /// attempt it has started, the pull request it opened, or the error it
    /// reported. Recording what is already so writes nothing.
    pub fn set_dependency_alert_update(
        &self,
        repo_id: i64,
        number: i64,
        update: &NewAlertUpdate<'_>,
        by: Attribution,
    ) -> Result<DependencyAlert> {
        self.write(by, |w| {
            let tx = w.tx;
            require_alerts_on(tx, repo_id)?;
            let alert = require_alert(tx, repo_id, number)?;
            let (state, pull_id, error_type, error_title) = match update {
                NewAlertUpdate::None => ("none", None, None, None),
                NewAlertUpdate::Pending => ("pending", None, None, None),
                NewAlertUpdate::PullRequest { number: pull } => {
                    let id: Option<i64> = tx
                        .query_row(
                            "SELECT id FROM pulls WHERE repo_id = ?1 AND number = ?2",
                            params![repo_id, pull],
                            |r| r.get(0),
                        )
                        .optional()?;
                    let id = id.ok_or(WorldError::NoSuchPullNumber {
                        repo_id,
                        number: *pull,
                    })?;
                    ("pull_request", Some(id), None, None)
                }
                NewAlertUpdate::Error { error_type, title } => {
                    ("error", None, Some(*error_type), Some(*title))
                }
            };
            let before = objects::dependency_alert_object(tx, alert.id)?;
            tx.execute(
                "UPDATE dependency_alerts
                 SET update_state = ?1, update_pull_id = ?2, update_error_type = ?3,
                     update_error_title = ?4
                 WHERE id = ?5",
                params![state, pull_id, error_type, error_title, alert.id],
            )?;
            changed(w, alert.id, &before, EventBody::DependencyAlertUpdateSet)?;
            alert_by_id(tx, alert.id)
        })
    }

    /// A repository's alert by its number.
    pub fn dependency_alert(&self, repo_id: i64, number: i64) -> Result<Option<DependencyAlert>> {
        self.with_conn(|conn| alert_by_number(conn, repo_id, number))
    }

    /// At most `limit` of a repository's alerts that `query` keeps, in its
    /// order. With `before`, they are the ones nearest it, still in order.
    pub fn dependency_alerts(
        &self,
        repo_id: i64,
        query: &AlertQuery,
        limit: usize,
    ) -> Result<Vec<DependencyAlert>> {
        let (filter, values) = query.filter();
        let column = match query.by_updated {
            true => "a.updated_clock_unix",
            false => "a.created_clock_unix",
        };
        // A page that ends before a position is read backwards from it, and
        // turned round.
        let backwards = query.before.is_some();
        let from = query.before.or(query.after);
        let past = match query.ascending != backwards {
            true => ">",
            false => "<",
        };
        let n = values.len();
        let bound = match from {
            Some(_) => format!(" AND ({column}, a.number) {past} (?{}, ?{})", n + 2, n + 3),
            None => String::new(),
        };
        let direction = match query.ascending != backwards {
            true => "ASC",
            false => "DESC",
        };
        let sql = format!(
            "{ALERT_SELECT} WHERE a.repo_id = ?1{filter}{bound}
             ORDER BY {column} {direction}, a.number {direction} LIMIT {}",
            // SQLite counts a limit in 64 signed bits.
            limit.min(i64::MAX as usize)
        );
        let mut bind: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(repo_id)];
        bind.extend(
            values
                .into_iter()
                .map(|v| Box::new(v) as Box<dyn rusqlite::ToSql>),
        );
        if let Some(position) = from {
            bind.push(Box::new(position.clock_unix));
            bind.push(Box::new(position.number));
        }
        self.with_conn(|conn| {
            let mut alerts = conn
                .prepare(&sql)?
                .query_map(rusqlite::params_from_iter(bind.iter()), row_to_alert)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if backwards {
                alerts.reverse();
            }
            Ok(alerts)
        })
    }

    /// How many of a repository's alerts `query`'s filters keep, wherever it
    /// starts.
    pub fn dependency_alert_count(&self, repo_id: i64, query: &AlertQuery) -> Result<i64> {
        let (filter, values) = query.filter();
        let sql = format!("SELECT COUNT(*) FROM dependency_alerts a WHERE a.repo_id = ?1{filter}");
        let mut bind: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(repo_id)];
        bind.extend(
            values
                .into_iter()
                .map(|v| Box::new(v) as Box<dyn rusqlite::ToSql>),
        );
        self.with_conn(|conn| {
            Ok(conn.query_row(&sql, rusqlite::params_from_iter(bind.iter()), |r| r.get(0))?)
        })
    }
}

/// Every alert, in id order: what a view of the whole world reads.
pub(crate) fn list_alerts(conn: &Connection) -> Result<Vec<DependencyAlert>> {
    Ok(conn
        .prepare(&format!("{ALERT_SELECT} ORDER BY a.id"))?
        .query_map([], row_to_alert)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The event for a change to an alert, when anything changed.
fn changed(
    w: &mut Write<'_>,
    id: i64,
    before: &events::DependencyAlertObject,
    event: fn(events::DependencyAlertChanged) -> EventBody,
) -> Result<()> {
    let changes = events::DependencyAlertChanges::between(
        before,
        &objects::dependency_alert_object(w.tx, id)?,
    );
    if !changes.is_empty() {
        w.operation(event(events::DependencyAlertChanged {
            subject: objects::dependency_alert_subject(w.tx, id)?,
            changes,
        }));
    }
    Ok(())
}

fn require_alerts_on(conn: &Connection, repo_id: i64) -> Result<()> {
    let on: Option<bool> = conn
        .query_row(
            "SELECT dependency_alerts FROM repos WHERE id = ?1",
            params![repo_id],
            |r| r.get(0),
        )
        .optional()?;
    match on {
        Some(true) => Ok(()),
        Some(false) => Err(WorldError::DependencyAlertsOff(repo_id)),
        None => Err(WorldError::NoSuchRepo(repo_id)),
    }
}

fn require_alert(conn: &Connection, repo_id: i64, number: i64) -> Result<DependencyAlert> {
    alert_by_number(conn, repo_id, number)?.ok_or(WorldError::NoSuchDependencyAlert(number))
}

fn alert_by_number(
    conn: &Connection,
    repo_id: i64,
    number: i64,
) -> Result<Option<DependencyAlert>> {
    Ok(conn
        .query_row(
            &format!("{ALERT_SELECT} WHERE a.repo_id = ?1 AND a.number = ?2"),
            params![repo_id, number],
            row_to_alert,
        )
        .optional()?)
}

pub(crate) fn alert_by_id(conn: &Connection, id: i64) -> Result<DependencyAlert> {
    Ok(conn.query_row(
        &format!("{ALERT_SELECT} WHERE a.id = ?1"),
        params![id],
        row_to_alert,
    )?)
}

fn row_to_alert(r: &Row<'_>) -> rusqlite::Result<DependencyAlert> {
    let dismissed = match r.get::<_, Option<i64>>(18)? {
        Some(clock_unix) => Some(AlertDismissal {
            clock_unix,
            by: Actor {
                id: r.get(19)?,
                login: r.get(20)?,
                kind: ActorKind::from_db(&r.get::<_, String>(21)?),
                created_clock_unix: r.get(22)?,
            },
            reason: DismissReason::from_db(&r.get::<_, String>(23)?),
            comment: r.get(24)?,
        }),
        None => None,
    };
    let update = match AlertUpdateState::from_db(&r.get::<_, String>(27)?) {
        AlertUpdateState::None => AlertUpdate::None,
        AlertUpdateState::Pending => AlertUpdate::Pending,
        AlertUpdateState::PullRequest => AlertUpdate::PullRequest {
            pull_id: r.get(28)?,
            number: r.get(29)?,
        },
        AlertUpdateState::Error => AlertUpdate::Error {
            error_type: r.get(30)?,
            title: r.get(31)?,
        },
    };
    Ok(DependencyAlert {
        id: r.get(0)?,
        repo_id: r.get(1)?,
        number: r.get(2)?,
        state: AlertState::from_db(&r.get::<_, String>(3)?),
        ecosystem: r.get(4)?,
        package: r.get(5)?,
        manifest_path: r.get(6)?,
        scope: r
            .get::<_, Option<String>>(7)?
            .as_deref()
            .map(AlertScope::from_db),
        version_requirement: r.get(8)?,
        advisory_id: r.get(9)?,
        cve_id: r.get(10)?,
        summary: r.get(11)?,
        description: r.get(12)?,
        severity: AlertSeverity::from_db(&r.get::<_, String>(13)?),
        vulnerable_version_range: r.get(14)?,
        first_patched_version: r.get(15)?,
        created_clock_unix: r.get(16)?,
        updated_clock_unix: r.get(17)?,
        dismissed,
        fixed_clock_unix: r.get(25)?,
        auto_dismissed_clock_unix: r.get(26)?,
        update,
    })
}
