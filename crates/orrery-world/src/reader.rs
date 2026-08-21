//! Read-only views over a world's state, for the snapshot diff engine.
//!
//! `WorldView` is what a diff side is: full listings of every object table
//! plus the meta pair (clock, last log seq). The live `World` implements it
//! through its own connection; `WorldReader` opens a quiesced copy — a
//! snapshot — read-only, without migrating or otherwise touching it.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::types::*;
use crate::{Result, World, WorldError, queries, schema};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct ViewMeta {
    pub clock_unix: i64,
    pub last_seq: i64,
}

pub trait WorldView {
    fn view_meta(&self) -> Result<ViewMeta>;
    fn list_actors(&self) -> Result<Vec<Actor>>;
    fn list_repos(&self) -> Result<Vec<Repo>>;
    fn list_pulls(&self) -> Result<Vec<Pull>>;
    fn list_reviews(&self) -> Result<Vec<Review>>;
    fn list_issue_comments(&self) -> Result<Vec<IssueComment>>;
    fn list_check_runs(&self) -> Result<Vec<CheckRun>>;
}

impl WorldView for World {
    fn view_meta(&self) -> Result<ViewMeta> {
        self.with_conn(|conn| {
            Ok(ViewMeta {
                clock_unix: queries::clock(conn)?,
                last_seq: queries::last_seq(conn)?,
            })
        })
    }

    fn list_actors(&self) -> Result<Vec<Actor>> {
        self.with_conn(queries::list_actors)
    }

    fn list_repos(&self) -> Result<Vec<Repo>> {
        self.with_conn(queries::list_repos)
    }

    fn list_pulls(&self) -> Result<Vec<Pull>> {
        self.with_conn(queries::list_pulls)
    }

    fn list_reviews(&self) -> Result<Vec<Review>> {
        self.with_conn(queries::list_reviews)
    }

    fn list_issue_comments(&self) -> Result<Vec<IssueComment>> {
        self.with_conn(queries::list_issue_comments)
    }

    fn list_check_runs(&self) -> Result<Vec<CheckRun>> {
        self.with_conn(queries::list_check_runs)
    }
}

/// A read-only connection to a world directory that is not being served —
/// a snapshot. Requires the current schema version: a snapshot made by an
/// older binary is refused loudly (restore it and let migration run) rather
/// than half-read.
pub struct WorldReader {
    conn: Connection,
}

impl WorldReader {
    pub fn open(root: &Path) -> Result<WorldReader> {
        let db = root.join("world.db");
        if !db.exists() {
            return Err(WorldError::NotFound(root.to_path_buf()));
        }
        let conn = Connection::open_with_flags(
            db,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let version = schema::schema_version(&conn)?;
        if version != schema::CURRENT_VERSION {
            return Err(WorldError::SchemaVersion {
                found: version,
                expected: schema::CURRENT_VERSION,
            });
        }
        Ok(WorldReader { conn })
    }
}

impl WorldView for WorldReader {
    fn view_meta(&self) -> Result<ViewMeta> {
        Ok(ViewMeta {
            clock_unix: queries::clock(&self.conn)?,
            last_seq: queries::last_seq(&self.conn)?,
        })
    }

    fn list_actors(&self) -> Result<Vec<Actor>> {
        queries::list_actors(&self.conn)
    }

    fn list_repos(&self) -> Result<Vec<Repo>> {
        queries::list_repos(&self.conn)
    }

    fn list_pulls(&self) -> Result<Vec<Pull>> {
        queries::list_pulls(&self.conn)
    }

    fn list_reviews(&self) -> Result<Vec<Review>> {
        queries::list_reviews(&self.conn)
    }

    fn list_issue_comments(&self) -> Result<Vec<IssueComment>> {
        queries::list_issue_comments(&self.conn)
    }

    fn list_check_runs(&self) -> Result<Vec<CheckRun>> {
        queries::list_check_runs(&self.conn)
    }
}
