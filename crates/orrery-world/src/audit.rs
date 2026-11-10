//! The checks every write runs in a test build. Each value the write stores
//! is in one of its events (spec/events.md): [`World::write`] reads every
//! table before and after the write, and the write fails the test if a row
//! it added, removed, or changed belongs to no event's subject, or a value it
//! changed is under no field the event names. And each event, as the log
//! serves it, matches the schema its types make ([`check_schema`]).
//!
//! Each table's columns are listed here with the event field that carries
//! them, so a column the schema gains fails the check until it is listed,
//! and the write that stores it is made to carry it.
//!
//! [`World::write`]: crate::World::write

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;
use rusqlite::types::ValueRef;
use serde_json::Value;

use crate::Result;
use crate::events::{Event, EventBody};

/// What a column of a table is to its row's subject.
#[derive(Clone, Copy)]
enum Column {
    /// The value of this field of the subject.
    Field(&'static str),
    /// Part of the subject's identity, which never changes.
    Identity,
    /// A credential, derived from the world secret and numbers the events
    /// carry. Only a re-key changes one.
    Credential,
    /// How many entries the log holds: the log's own length, which no event
    /// carries. [`Audit::end`] checks it against the log instead.
    LogLength,
}

use Column::{Credential, Field, Identity, LogLength};

/// Every table but the log, and what each of its columns is.
const TABLES: &[(&str, &[(&str, Column)])] = &[
    (
        "world",
        &[
            ("id", Identity),
            ("clock_unix", Field("clock_unix")),
            ("secret", Credential),
            ("log_entries", LogLength),
        ],
    ),
    (
        "actors",
        &[
            ("id", Identity),
            ("login", Field("login")),
            ("kind", Field("kind")),
            ("created_clock_unix", Field("created_unix")),
        ],
    ),
    (
        "tokens",
        &[
            ("token", Credential),
            ("actor_id", Identity),
            ("origin", Identity),
            ("created_clock_unix", Identity),
        ],
    ),
    (
        "repos",
        &[
            ("id", Identity),
            ("owner_id", Field("owner")),
            ("name", Field("name")),
            ("description", Field("description")),
            ("private", Field("private")),
            ("default_branch", Field("default_branch")),
            ("created_by", Field("created_by")),
            ("created_clock_unix", Field("created_unix")),
            ("pushed_at_clock_unix", Field("pushed_unix")),
        ],
    ),
    (
        "actor_emails",
        &[
            ("actor_id", Identity),
            ("email", Field("emails")),
            ("is_primary", Field("emails")),
            ("verified", Field("emails")),
        ],
    ),
    (
        "oauth_apps",
        &[
            ("id", Identity),
            ("name", Field("name")),
            ("client_id", Field("client_id")),
            ("client_secret", Credential),
            ("callback_url", Field("callback_url")),
            ("created_clock_unix", Field("created_unix")),
        ],
    ),
    (
        "oauth_app_secrets",
        &[
            ("app_id", Identity),
            ("n", Identity),
            ("secret", Credential),
            ("created_clock_unix", Identity),
        ],
    ),
    (
        "oauth_codes",
        &[
            ("id", Identity),
            ("code", Credential),
            ("app_id", Field("app")),
            ("actor_id", Field("actor")),
            ("redirect_uri", Field("redirect_uri")),
            ("scope", Field("scope")),
            ("created_clock_unix", Field("created_unix")),
            ("exchanged_clock_unix", Field("exchanged_unix")),
        ],
    ),
    (
        "pulls",
        &[
            ("id", Identity),
            ("repo_id", Identity),
            ("number", Field("number")),
            ("title", Field("title")),
            ("body", Field("body")),
            ("author_id", Field("author")),
            ("state", Field("state")),
            ("draft", Field("draft")),
            ("head_ref", Field("head")),
            ("head_sha", Field("head")),
            ("base_ref", Field("base")),
            ("base_sha", Field("base")),
            ("merged", Field("merged")),
            ("merge_commit_sha", Field("merge_commit_sha")),
            ("merged_by_id", Field("merged_by")),
            ("created_clock_unix", Field("created_unix")),
            ("updated_clock_unix", Field("updated_unix")),
            ("closed_clock_unix", Field("closed_unix")),
            ("base_moved", Field("base_moved")),
        ],
    ),
    (
        "pull_labels",
        &[("pull_id", Identity), ("name", Field("labels"))],
    ),
    (
        "pull_timeline",
        &[
            ("id", Identity),
            ("pull_id", Identity),
            ("kind", Field("kind")),
            ("label", Field("label")),
            ("reviewer_id", Field("reviewer")),
            ("team_id", Field("team")),
            ("team_slug", Field("team")),
            ("actor_id", Field("actor")),
            ("clock_unix", Field("created_unix")),
            ("ref_name", Field("ref_name")),
        ],
    ),
    (
        "org_members",
        &[
            ("org_id", Identity),
            ("member_id", Field("member")),
            ("role", Field("role")),
            ("created_clock_unix", Field("created_unix")),
        ],
    ),
    (
        "teams",
        &[
            ("id", Identity),
            ("org_id", Identity),
            ("slug", Field("slug")),
            ("name", Field("name")),
            ("description", Field("description")),
            ("created_clock_unix", Field("created_unix")),
        ],
    ),
    (
        "team_members",
        &[
            ("team_id", Identity),
            ("member_id", Field("member")),
            ("role", Field("role")),
            ("created_clock_unix", Field("created_unix")),
        ],
    ),
    (
        "review_requests",
        &[
            ("id", Identity),
            ("pull_id", Identity),
            ("reviewer_id", Identity),
            ("team_id", Identity),
        ],
    ),
    (
        "reviews",
        &[
            ("id", Identity),
            ("pull_id", Identity),
            ("author_id", Field("author")),
            ("state", Field("state")),
            ("body", Field("body")),
            ("commit_sha", Field("commit_sha")),
            ("submitted_clock_unix", Field("submitted_unix")),
        ],
    ),
    (
        "issue_comments",
        &[
            ("id", Identity),
            ("pull_id", Identity),
            ("author_id", Field("author")),
            ("body", Field("body")),
            ("created_clock_unix", Field("created_unix")),
            ("updated_clock_unix", Field("updated_unix")),
        ],
    ),
    (
        "check_runs",
        &[
            ("id", Identity),
            ("repo_id", Identity),
            ("head_sha", Field("head_sha")),
            ("name", Field("name")),
            ("status", Field("status")),
            ("conclusion", Field("conclusion")),
            ("details_url", Field("details_url")),
            ("external_id", Field("external_id")),
            ("output", Field("output")),
            ("creator_id", Field("creator")),
            ("started_clock_unix", Field("started_unix")),
            ("completed_clock_unix", Field("completed_unix")),
            ("created_clock_unix", Field("created_unix")),
        ],
    ),
    (
        "ssh_signing_keys",
        &[
            ("id", Identity),
            ("actor_id", Identity),
            ("public_key", Field("key")),
            ("title", Field("title")),
            ("created_clock_unix", Field("created_unix")),
        ],
    ),
    (
        "commit_verifications",
        &[
            ("repo_id", Identity),
            ("sha", Identity),
            ("signer_id", Field("signer")),
            ("key_id", Field("signing_key")),
            ("verified_clock_unix", Field("verified_unix")),
        ],
    ),
];

/// Fail the test if an event, as the log serves it, does not match the
/// schema `GET /_orrery/log/schema` serves.
pub(crate) fn check_schema(events: &[Event]) {
    static SCHEMA: std::sync::OnceLock<(boon::Schemas, boon::SchemaIndex)> =
        std::sync::OnceLock::new();
    let (schemas, index) = SCHEMA.get_or_init(|| {
        let mut schemas = boon::Schemas::new();
        let mut compiler = boon::Compiler::new();
        compiler
            .add_resource("orrery:event.schema.json", crate::events::schema())
            .expect("the schema is a resource");
        let index = compiler
            .compile("orrery:event.schema.json", &mut schemas)
            .expect("the schema compiles");
        (schemas, index)
    });
    for event in events {
        let served = serde_json::to_value(event).expect("an event serializes");
        if let Err(e) = schemas.validate(&served, *index) {
            panic!("an event does not match its schema: {e:#}\n{served:#}");
        }
    }
}

type Row = BTreeMap<String, Value>;
/// A table's rows by primary key.
type Table = BTreeMap<String, Row>;

/// A subject, as `(type, key)`.
type Subject = (String, String);

pub(crate) struct Audit {
    before: BTreeMap<String, Table>,
}

impl Audit {
    pub(crate) fn begin(tx: &Connection) -> Result<Audit> {
        Ok(Audit { before: read(tx)? })
    }

    /// Fail the test if the write stored a value none of `events` carries,
    /// or left the log's count other than the number of entries it holds.
    pub(crate) fn end(self, tx: &Connection, events: &[EventBody]) -> Result<()> {
        let (kept, counted): (i64, i64) = tx.query_row(
            "SELECT log_entries, (SELECT COUNT(*) FROM action_log) FROM world WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        assert_eq!(
            kept, counted,
            "the log's count is not the number of entries it holds"
        );
        let after = read(tx)?;
        let carried = Carried::of(events);
        let mut missing = Vec::new();
        for (table, columns) in TABLES {
            let empty = Table::new();
            let before = self.before.get(*table).unwrap_or(&empty);
            let now = after.get(*table).unwrap_or(&empty);
            let keys: BTreeSet<&String> = before.keys().chain(now.keys()).collect();
            for key in keys {
                let (from, to) = (before.get(key), now.get(key));
                if from == to {
                    continue;
                }
                let row = to.or(from).expect("a key comes from one side");
                let (subject, child_field) = subject_of(table, row);
                for (column, kind) in *columns {
                    let changed = from.map(|r| &r[*column]) != to.map(|r| &r[*column]);
                    let added_or_removed = from.is_none() || to.is_none();
                    if !changed && !added_or_removed {
                        continue;
                    }
                    let field = match (kind, &child_field) {
                        (LogLength, _) => continue,
                        (Credential, _) if added_or_removed => continue,
                        (Credential, _) if carried.rekeyed => continue,
                        (Credential, _) => "credential".to_string(),
                        (_, Some(field)) => field.clone(),
                        (Identity, None) if added_or_removed => {
                            if carried.subjects.contains(&subject) {
                                continue;
                            }
                            "the row".to_string()
                        }
                        (Identity, None) => format!("identity column {column}"),
                        (Field(field), None) => field.to_string(),
                    };
                    if !carried.fields.contains(&(subject.clone(), field.clone())) {
                        missing.push(format!("{table}.{column} of {subject:?} under {field:?}"));
                    }
                }
            }
        }
        missing.dedup();
        assert!(
            missing.is_empty(),
            "the write stored values none of its events carries:\n  {}\nits events:\n{}",
            missing.join("\n  "),
            serde_json::to_string_pretty(events).unwrap_or_default(),
        );
        Ok(())
    }
}

/// Every table but the log, by name. A table [`TABLES`] does not list, or
/// a column it does not, fails the test.
fn read(tx: &Connection) -> Result<BTreeMap<String, Table>> {
    let names: Vec<String> = tx
        .prepare(
            "SELECT name FROM sqlite_schema WHERE type = 'table'
             AND name NOT LIKE 'sqlite_%' AND name != 'action_log' ORDER BY name",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    // A schema a test extended with appended migrations has tables and
    // columns no write of this binary stores.
    let extended = crate::schema::test_support::extended();
    let mut tables = BTreeMap::new();
    for name in names {
        let Some((_, columns)) = TABLES.iter().find(|(t, _)| *t == name) else {
            assert!(extended, "table {name} has no audit rule (audit.rs)");
            continue;
        };
        let mut stmt = tx.prepare(&format!("SELECT * FROM {name}"))?;
        let found: Vec<String> = stmt
            .column_names()
            .iter()
            .map(|c| c.to_string())
            .filter(|c| {
                let listed = columns.iter().any(|(listed, _)| listed == c);
                assert!(
                    listed || extended,
                    "column {name}.{c} has no audit rule (audit.rs)"
                );
                listed
            })
            .collect();
        // A token's key is its value, which a re-key changes; what it was
        // derived from is what stays.
        let pk: Vec<String> = match name.as_str() {
            "tokens" => vec!["actor_id".into(), "origin".into()],
            _ => tx
                .prepare(&format!(
                    "SELECT name FROM pragma_table_info('{name}') WHERE pk > 0 ORDER BY pk"
                ))?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?,
        };
        let mut rows = Table::new();
        let mut query = stmt.query([])?;
        while let Some(r) = query.next()? {
            let mut row = Row::new();
            for c in &found {
                row.insert(c.clone(), value(r.get_ref(c.as_str())?));
            }
            let key = pk
                .iter()
                .map(|c| row[c].to_string())
                .collect::<Vec<_>>()
                .join("|");
            rows.insert(key, row);
        }
        tables.insert(name, rows);
    }
    Ok(tables)
}

fn value(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => i.into(),
        ValueRef::Real(f) => f.into(),
        ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned().into(),
        ValueRef::Blob(b) => hex::encode(b).into(),
    }
}

fn int(row: &Row, column: &str) -> String {
    row[column].to_string()
}

fn text<'a>(row: &'a Row, column: &str) -> &'a str {
    row[column].as_str().unwrap_or_default()
}

/// The subject a row belongs to, and, for a row of a child table, the field
/// of that subject the row is part of.
fn subject_of(table: &str, row: &Row) -> (Subject, Option<String>) {
    let s = |kind: &str, key: String| (kind.to_string(), key);
    match table {
        "world" => (s("world", String::new()), None),
        "actors" => (s("actor", int(row, "id")), None),
        "repos" => (s("repo", int(row, "id")), None),
        "oauth_apps" => (s("oauth_app", int(row, "id")), None),
        "oauth_codes" => (s("oauth_authorization", int(row, "id")), None),
        "pulls" => (s("pull", int(row, "id")), None),
        "pull_timeline" => (s("timeline_item", int(row, "id")), None),
        "teams" => (s("team", int(row, "id")), None),
        "reviews" => (s("review", int(row, "id")), None),
        "issue_comments" => (s("comment", int(row, "id")), None),
        "check_runs" => (s("check_run", int(row, "id")), None),
        "ssh_signing_keys" => (s("ssh_signing_key", int(row, "id")), None),
        "commit_verifications" => (
            s(
                "commit",
                format!("{}:{}", int(row, "repo_id"), text(row, "sha")),
            ),
            None,
        ),
        "org_members" => (
            s(
                "org_membership",
                format!("{}:{}", int(row, "org_id"), int(row, "member_id")),
            ),
            None,
        ),
        "team_members" => (
            s(
                "team_membership",
                format!("{}:{}", int(row, "team_id"), int(row, "member_id")),
            ),
            None,
        ),
        "actor_emails" => (s("actor", int(row, "actor_id")), Some("emails".into())),
        "pull_labels" => (s("pull", int(row, "pull_id")), Some("labels".into())),
        "review_requests" => {
            let field = match row["reviewer_id"].is_null() {
                true => "requested_teams",
                false => "requested_reviewers",
            };
            (s("pull", int(row, "pull_id")), Some(field.into()))
        }
        "tokens" => {
            let origin = row["origin"].as_str().unwrap_or_default();
            match origin.strip_prefix("oauth:") {
                Some(code) => (
                    s("oauth_authorization", code.to_string()),
                    Some("token".into()),
                ),
                None => {
                    let n = origin.strip_prefix("actor:").unwrap_or("0");
                    (s("actor", int(row, "actor_id")), Some(format!("token:{n}")))
                }
            }
        }
        "oauth_app_secrets" => (
            s("oauth_app", int(row, "app_id")),
            Some(format!("secret:{}", int(row, "n"))),
        ),
        other => panic!("table {other} has no audit rule (audit.rs)"),
    }
}

/// What a write's events carry: each subject, the fields named under it,
/// and whether the write re-keyed the world.
struct Carried {
    subjects: BTreeSet<Subject>,
    fields: BTreeSet<(Subject, String)>,
    rekeyed: bool,
}

impl Carried {
    fn of(events: &[EventBody]) -> Carried {
        let mut carried = Carried {
            subjects: BTreeSet::new(),
            fields: BTreeSet::new(),
            rekeyed: false,
        };
        for event in events {
            let body = serde_json::to_value(event).expect("an event serializes");
            let Some(subject) = subject_key(&body["subject"]) else {
                continue;
            };
            let mut fields: Vec<String> = ["changes", "object"]
                .iter()
                .filter_map(|k| body[*k].as_object())
                .flat_map(|o| o.keys().cloned())
                .collect();
            match event.type_name() {
                "actor:created" => fields.push("token:0".into()),
                "actor:token_issued" => fields.push(format!("token:{}", body["n"])),
                "oauth_app:secret_issued" => fields.push(format!("secret:{}", body["n"])),
                "oauth_authorization:exchanged" => fields.push("token".into()),
                "world:rekeyed" => carried.rekeyed = true,
                _ => {}
            }
            for field in fields {
                carried.fields.insert((subject.clone(), field));
            }
            carried.subjects.insert(subject);
        }
        carried
    }
}

/// A subject as [`subject_of`] keys it, or `None` for a ref, which lives
/// in git rather than in a table.
fn subject_key(subject: &Value) -> Option<Subject> {
    let kind = subject["type"].as_str()?.to_string();
    let key = match kind.as_str() {
        "world" => String::new(),
        "ref" => return None,
        "org_membership" => format!("{}:{}", subject["owner"]["id"], subject["member"]["id"]),
        "team_membership" => format!("{}:{}", subject["team"]["id"], subject["member"]["id"]),
        "commit" => format!(
            "{}:{}",
            subject["repo"]["id"],
            subject["sha"].as_str().unwrap_or_default()
        ),
        _ => subject["id"].to_string(),
    };
    Some((kind, key))
}
