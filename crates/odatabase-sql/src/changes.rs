//! The change log behind delta links.
//!
//! Each row names an entity that changed. A delta response re-reads those entities, so a row seen
//! twice only sends the current state again. That lets positions err on the early side: on
//! Postgres, where transactions commit out of order, a position is a transaction id and a client
//! resumes from the oldest transaction that was still running when it read.

use crate::db::{ColumnDecode, Database, Session};
use crate::{Dialect, QueryError};
use odatabase_edm::{PrimitiveKind, Value};
use sea_query::{Alias, Expr, ExprTrait, Order, Query};
use std::time::Duration;

const TABLE: &str = "odata_changes";
/// `entity_set` of the row that remembers how far the log was pruned.
const PRUNED: &str = "";

const POSTGRES_TABLE: &str = "
CREATE TABLE IF NOT EXISTS odata_changes (
  id BIGSERIAL PRIMARY KEY,
  stamp BIGINT NOT NULL DEFAULT (pg_current_xact_id()::text::bigint),
  entity_set TEXT NOT NULL,
  entity_key TEXT NOT NULL,
  kind CHAR(1) NOT NULL CHECK (kind IN ('C', 'U', 'D', 'P')),
  changed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  tenant TEXT
);
ALTER TABLE odata_changes ADD COLUMN IF NOT EXISTS tenant TEXT;
CREATE INDEX IF NOT EXISTS odata_changes_read ON odata_changes (entity_set, stamp, id);
";

const SQLITE_TABLE: &str = "
CREATE TABLE IF NOT EXISTS odata_changes (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  entity_set TEXT NOT NULL,
  entity_key TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('C', 'U', 'D', 'P')),
  changed_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  tenant TEXT
);
CREATE INDEX IF NOT EXISTS odata_changes_read ON odata_changes (entity_set, id);
";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Created,
    Updated,
    Deleted,
}

impl ChangeKind {
    fn code(self) -> &'static str {
        match self {
            Self::Created => "C",
            Self::Updated => "U",
            Self::Deleted => "D",
        }
    }
}

/// One row of the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoggedChange {
    pub id: i64,
    /// The key predicate, such as `('a')` or `(Code=1,Id='a')`.
    pub key: String,
    pub kind: ChangeKind,
}

/// The column that orders the log. SQLite commits one writer at a time, so its ids are in order.
fn stamp(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::Postgres => "stamp",
        Dialect::Sqlite => "id",
    }
}

fn column(name: &str) -> Expr {
    Expr::col(Alias::new(name))
}

fn int(row: &[Value], i: usize) -> Result<i64, QueryError> {
    row.get(i)
        .and_then(Value::as_i64)
        .ok_or_else(|| QueryError::internal("the change log returned no number"))
}

/// Adds a row to the log, in the transaction of the change.
pub async fn record_change(
    session: &Session,
    set: &str,
    tenant: Option<&str>,
    key: &str,
    kind: ChangeKind,
) -> Result<(), QueryError> {
    let mut q = Query::insert();
    q.into_table(Alias::new(TABLE))
        .columns([
            Alias::new("entity_set"),
            Alias::new("tenant"),
            Alias::new("entity_key"),
            Alias::new("kind"),
        ])
        .values_panic([
            set.into(),
            tenant.map(str::to_owned).into(),
            key.into(),
            kind.code().into(),
        ]);
    session.affected(&q).await?;
    Ok(())
}

/// The position a client resumes from, to be read before the data it covers.
pub async fn change_position(session: &Session) -> Result<i64, QueryError> {
    let position = match session.dialect() {
        Dialect::Postgres => "pg_snapshot_xmin(pg_current_snapshot())::text::bigint",
        Dialect::Sqlite => "(SELECT COALESCE(MAX(id), 0) + 1 FROM odata_changes)",
    };
    let mut q = Query::select();
    q.expr(Expr::cust(position));
    let decode = [ColumnDecode::Primitive(PrimitiveKind::Int64)];
    let (_, rows) = session.execute(&q, &decode).await?;
    int(rows.first().map_or(&[], Vec::as_slice), 0)
}

/// The first position the log still covers.
pub async fn change_floor(session: &Session) -> Result<i64, QueryError> {
    let mut q = Query::select();
    q.column(Alias::new("entity_key"))
        .from(Alias::new(TABLE))
        .and_where(column("entity_set").eq(PRUNED))
        .order_by(Alias::new("id"), Order::Desc)
        .limit(1);
    let (_, rows) = session.execute(&q, &[ColumnDecode::Natural]).await?;
    let text = rows.first().and_then(|r| r.first());
    Ok(match text {
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    })
}

/// The changes of `set` from position `since`, after row `after`, oldest first. With `tenants`,
/// only theirs: a row of another tenant would otherwise come back as a removed key.
pub async fn read_changes(
    session: &Session,
    set: &str,
    tenants: Option<&[String]>,
    since: i64,
    after: i64,
    limit: u64,
) -> Result<Vec<LoggedChange>, QueryError> {
    let mut q = Query::select();
    q.columns([
        Alias::new("id"),
        Alias::new("entity_key"),
        Alias::new("kind"),
    ])
    .from(Alias::new(TABLE))
    .and_where(column("entity_set").eq(set))
    .and_where(column(stamp(session.dialect())).gte(since))
    .and_where(column("id").gt(after))
    .order_by(Alias::new("id"), Order::Asc)
    .limit(limit);
    if let Some(tenants) = tenants {
        q.and_where(column("tenant").is_in(tenants.iter().map(String::as_str)));
    }
    let decode = [
        ColumnDecode::Primitive(PrimitiveKind::Int64),
        ColumnDecode::Natural,
        ColumnDecode::Natural,
    ];
    let (_, rows) = session.execute(&q, &decode).await?;
    rows.iter()
        .map(|r| {
            let text = |i: usize| match r.get(i) {
                Some(Value::String(s)) => Ok(s.trim().to_owned()),
                _ => Err(QueryError::internal("the change log returned no text")),
            };
            let kind = match text(2)?.as_str() {
                "C" => ChangeKind::Created,
                "D" => ChangeKind::Deleted,
                _ => ChangeKind::Updated,
            };
            Ok(LoggedChange {
                id: int(r, 0)?,
                key: text(1)?,
                kind,
            })
        })
        .collect()
}

impl Database {
    /// Creates the table that entities with `#[odata(track_changes)]` write to, if it is missing.
    pub async fn create_change_log(&self) -> Result<(), QueryError> {
        match self {
            #[cfg(feature = "postgres")]
            Self::Postgres(p) => {
                sqlx::raw_sql(POSTGRES_TABLE).execute(p).await?;
            }
            #[cfg(feature = "sqlite")]
            Self::Sqlite(p) => {
                sqlx::raw_sql(SQLITE_TABLE).execute(p).await?;
                // SQLite has no `ADD COLUMN IF NOT EXISTS`, for a log from before tenants.
                let added = sqlx::raw_sql("ALTER TABLE odata_changes ADD COLUMN tenant TEXT")
                    .execute(p)
                    .await;
                match added {
                    Err(e) if !e.to_string().contains("duplicate column") => Err(e)?,
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// Removes changes older than `age`, returning how many went. Clients holding a delta link
    /// from before them get a 410 and start over.
    pub async fn prune_change_log(&self, age: Duration) -> Result<u64, QueryError> {
        let dialect = self.dialect();
        let seconds = age.as_secs();
        let older = match dialect {
            Dialect::Postgres => format!("changed_at < now() - interval '{seconds} seconds'"),
            Dialect::Sqlite => format!("changed_at < datetime('now', '-{seconds} seconds')"),
        };
        let order = stamp(dialect);
        let session = Session::begin(self).await?;
        let outcome = async {
            let mut q = Query::select();
            q.expr(column(order).max())
                .from(Alias::new(TABLE))
                .and_where(column("entity_set").ne(PRUNED))
                .and_where(Expr::cust(older));
            let decode = [ColumnDecode::Primitive(PrimitiveKind::Int64)];
            let (_, rows) = session.execute(&q, &decode).await?;
            let Some(last) = rows.first().and_then(|r| r.first()).and_then(Value::as_i64) else {
                return Ok(0);
            };
            let mut q = Query::delete();
            q.from_table(Alias::new(TABLE))
                .and_where(column(order).lte(last));
            let removed = session.affected(&q).await?;
            let mut q = Query::insert();
            q.into_table(Alias::new(TABLE))
                .columns([
                    Alias::new("entity_set"),
                    Alias::new("entity_key"),
                    Alias::new("kind"),
                ])
                .values_panic([PRUNED.into(), (last + 1).to_string().into(), "P".into()]);
            session.affected(&q).await?;
            Ok::<_, QueryError>(removed)
        }
        .await;
        match outcome {
            Ok(n) => {
                session.commit().await?;
                Ok(n)
            }
            Err(e) => {
                session.rollback().await?;
                Err(e)
            }
        }
    }
}
