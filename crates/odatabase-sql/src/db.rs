//! Connections, transactions, statement execution and row decoding.

use crate::values::{enum_from_db, from_sea_string};
use crate::{Dialect, QueryError};
use chrono::TimeDelta;
#[cfg(feature = "postgres")]
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use odatabase_edm::{Model, PrimitiveKind, Value};
#[cfg(feature = "postgres")]
use rust_decimal::Decimal;
use sea_query::{QueryStatementWriter, SelectStatement};
use sea_query_sqlx::SqlxValues;
#[cfg(feature = "postgres")]
use sqlx::Column;
use sqlx::{AssertSqlSafe, Row, TypeInfo, ValueRef};
use std::sync::Arc;
use tokio::sync::Mutex;
#[cfg(feature = "postgres")]
use uuid::Uuid;

/// How to read one result column.
#[derive(Clone, Debug, PartialEq)]
pub enum ColumnDecode {
    /// Converted to this primitive type.
    Primitive(PrimitiveKind),
    /// The qualified name of an enum type, whose member the column stores.
    Enum(String),
    /// The database's own type, for columns only compared with each other.
    Natural,
}

/// A connection pool to one of the supported databases.
#[derive(Clone, Debug)]
pub enum Database {
    /// A PostgreSQL pool.
    #[cfg(feature = "postgres")]
    Postgres(sqlx::PgPool),
    /// A SQLite pool.
    #[cfg(feature = "sqlite")]
    Sqlite(sqlx::SqlitePool),
}

/// Connection settings for [`Database::connect_postgres`].
#[cfg(feature = "postgres")]
#[derive(Clone, Copy, Debug)]
pub struct PostgresOptions {
    /// Size of the pool. 16 by default.
    pub max_connections: u32,
    /// Postgres cancels statements that run longer. `None` lets them run forever.
    pub statement_timeout: Option<std::time::Duration>,
}

#[cfg(feature = "postgres")]
impl Default for PostgresOptions {
    fn default() -> Self {
        Self {
            max_connections: 16,
            statement_timeout: Some(std::time::Duration::from_secs(30)),
        }
    }
}

#[cfg(feature = "postgres")]
impl Database {
    /// Connects with a statement timeout, which is the only limit that stops a query in the
    /// database. A pool built by hand needs the `statement_timeout` connect option for this.
    pub async fn connect_postgres(url: &str, options: PostgresOptions) -> Result<Self, QueryError> {
        use std::str::FromStr;
        let mut connect = sqlx::postgres::PgConnectOptions::from_str(url)?;
        if let Some(timeout) = options.statement_timeout {
            let millis = timeout.as_millis().to_string();
            connect = connect.options([("statement_timeout", millis.as_str())]);
        }
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(options.max_connections)
            .connect_with(connect)
            .await?;
        Ok(Self::Postgres(pool))
    }
}

#[cfg(feature = "postgres")]
impl From<sqlx::PgPool> for Database {
    fn from(pool: sqlx::PgPool) -> Self {
        Self::Postgres(pool)
    }
}

#[cfg(feature = "sqlite")]
impl From<sqlx::SqlitePool> for Database {
    fn from(pool: sqlx::SqlitePool) -> Self {
        Self::Sqlite(pool)
    }
}

impl Database {
    /// The SQL dialect of the pool.
    pub fn dialect(&self) -> Dialect {
        match self {
            #[cfg(feature = "postgres")]
            Self::Postgres(_) => Dialect::Postgres,
            #[cfg(feature = "sqlite")]
            Self::Sqlite(_) => Dialect::Sqlite,
        }
    }

    /// Starts a transaction.
    pub async fn begin(&self) -> Result<Tx, QueryError> {
        Ok(match self {
            #[cfg(feature = "postgres")]
            Self::Postgres(p) => Tx::Postgres(p.begin().await?),
            #[cfg(feature = "sqlite")]
            Self::Sqlite(p) => Tx::Sqlite(p.begin().await?),
        })
    }
}

/// An open transaction.
pub enum Tx {
    /// A PostgreSQL transaction.
    #[cfg(feature = "postgres")]
    Postgres(sqlx::Transaction<'static, sqlx::Postgres>),
    /// A SQLite transaction.
    #[cfg(feature = "sqlite")]
    Sqlite(sqlx::Transaction<'static, sqlx::Sqlite>),
}

impl Tx {
    /// Commits the transaction.
    pub async fn commit(self) -> Result<(), QueryError> {
        match self {
            #[cfg(feature = "postgres")]
            Self::Postgres(t) => t.commit().await?,
            #[cfg(feature = "sqlite")]
            Self::Sqlite(t) => t.commit().await?,
        }
        Ok(())
    }

    /// Rolls the transaction back.
    pub async fn rollback(self) -> Result<(), QueryError> {
        match self {
            #[cfg(feature = "postgres")]
            Self::Postgres(t) => t.rollback().await?,
            #[cfg(feature = "sqlite")]
            Self::Sqlite(t) => t.rollback().await?,
        }
        Ok(())
    }
}

/// Where statements run. Cloneable, so concurrent expands can share a transaction.
#[derive(Clone)]
pub enum Session {
    /// Each statement takes a connection from the pool.
    Pool(Database),
    /// Statements share one transaction, which is gone once committed or rolled back.
    Tx(Dialect, Arc<Mutex<Option<Tx>>>),
}

impl From<Database> for Session {
    fn from(db: Database) -> Self {
        Self::Pool(db)
    }
}

impl Session {
    /// Starts a transactional session.
    pub async fn begin(db: &Database) -> Result<Self, QueryError> {
        Ok(Self::Tx(
            db.dialect(),
            Arc::new(Mutex::new(Some(db.begin().await?))),
        ))
    }

    /// The SQL dialect of the database.
    pub fn dialect(&self) -> Dialect {
        match self {
            Self::Pool(db) => db.dialect(),
            Self::Tx(d, _) => *d,
        }
    }

    /// Commits a transactional session. A pool session has nothing to commit.
    pub async fn commit(&self) -> Result<(), QueryError> {
        if let Self::Tx(_, tx) = self
            && let Some(tx) = tx.lock().await.take()
        {
            tx.commit().await?;
        }
        Ok(())
    }

    /// Rolls back a transactional session.
    pub async fn rollback(&self) -> Result<(), QueryError> {
        if let Self::Tx(_, tx) = self
            && let Some(tx) = tx.lock().await.take()
        {
            tx.rollback().await?;
        }
        Ok(())
    }

    /// Sets a database setting until the transaction ends, such as what row-level security
    /// policies read with `current_setting`. Postgres only.
    pub async fn set_local(&self, name: &str, value: &str) -> Result<(), QueryError> {
        let call = sea_query::Func::cust(sea_query::Alias::new("set_config"))
            .arg(name)
            .arg(value)
            .arg(true);
        let mut q = sea_query::Query::select();
        q.expr(call);
        self.run(&q).await?;
        Ok(())
    }

    /// Runs a query and decodes each row's columns as `columns` says.
    pub async fn fetch(
        &self,
        model: &Model,
        stmt: &SelectStatement,
        columns: &[ColumnDecode],
    ) -> Result<Vec<Vec<Value>>, QueryError> {
        let mut rows = self.run(stmt).await?;
        for row in &mut rows {
            for (value, col) in row.iter_mut().zip(columns) {
                let decoded = match (std::mem::replace(value, Value::Null), col) {
                    (v, ColumnDecode::Natural) => v,
                    (v, ColumnDecode::Primitive(k)) => convert(v, *k)?,
                    (v, ColumnDecode::Enum(name)) => {
                        let e = model
                            .enum_type(name)
                            .ok_or_else(|| QueryError::internal(format!("unknown enum {name}")))?;
                        enum_from_db(e, v)
                    }
                };
                *value = decoded;
            }
        }
        Ok(rows)
    }

    /// Runs a statement, returning each column in its natural database type.
    async fn run<S>(&self, stmt: &S) -> Result<Vec<Vec<Value>>, QueryError>
    where
        S: QueryStatementWriter,
    {
        match self {
            Self::Pool(db) => match db {
                #[cfg(feature = "postgres")]
                Database::Postgres(p) => {
                    let (sql, values) = pg_sql(stmt);
                    let rows = sqlx::query_with(AssertSqlSafe(sql), values)
                        .fetch_all(p)
                        .await?;
                    rows.iter().map(pg_row).collect()
                }
                #[cfg(feature = "sqlite")]
                Database::Sqlite(p) => {
                    let (sql, values) = sqlite_sql(stmt);
                    let rows = sqlx::query_with(AssertSqlSafe(sql), values)
                        .fetch_all(p)
                        .await?;
                    rows.iter().map(sqlite_row).collect()
                }
            },
            Self::Tx(_, tx) => {
                let mut guard = tx.lock().await;
                match guard.as_mut() {
                    None => Err(QueryError::internal("transaction is closed")),
                    #[cfg(feature = "postgres")]
                    Some(Tx::Postgres(t)) => {
                        let (sql, values) = pg_sql(stmt);
                        let rows = sqlx::query_with(AssertSqlSafe(sql), values)
                            .fetch_all(&mut **t)
                            .await?;
                        rows.iter().map(pg_row).collect()
                    }
                    #[cfg(feature = "sqlite")]
                    Some(Tx::Sqlite(t)) => {
                        let (sql, values) = sqlite_sql(stmt);
                        let rows = sqlx::query_with(AssertSqlSafe(sql), values)
                            .fetch_all(&mut **t)
                            .await?;
                        rows.iter().map(sqlite_row).collect()
                    }
                }
            }
        }
    }

    /// Runs an insert, update or delete, returning `RETURNING` rows and the affected count.
    pub async fn execute<S>(
        &self,
        stmt: &S,
        columns: &[ColumnDecode],
    ) -> Result<(u64, Vec<Vec<Value>>), QueryError>
    where
        S: QueryStatementWriter,
    {
        let mut rows = self.run(stmt).await?;
        for row in &mut rows {
            for (value, col) in row.iter_mut().zip(columns) {
                if let ColumnDecode::Primitive(k) = col {
                    *value = convert(std::mem::replace(value, Value::Null), *k)?;
                }
            }
        }
        Ok((rows.len() as u64, rows))
    }

    /// Like [`Session::execute`], for statements without `RETURNING`.
    pub async fn affected<S>(&self, stmt: &S) -> Result<u64, QueryError>
    where
        S: QueryStatementWriter,
    {
        match self {
            Self::Pool(db) => match db {
                #[cfg(feature = "postgres")]
                Database::Postgres(p) => {
                    let (sql, values) = pg_sql(stmt);
                    let r = sqlx::query_with(AssertSqlSafe(sql), values)
                        .execute(p)
                        .await?;
                    Ok(r.rows_affected())
                }
                #[cfg(feature = "sqlite")]
                Database::Sqlite(p) => {
                    let (sql, values) = sqlite_sql(stmt);
                    let r = sqlx::query_with(AssertSqlSafe(sql), values)
                        .execute(p)
                        .await?;
                    Ok(r.rows_affected())
                }
            },
            Self::Tx(_, tx) => {
                let mut guard = tx.lock().await;
                match guard.as_mut() {
                    None => Err(QueryError::internal("transaction is closed")),
                    #[cfg(feature = "postgres")]
                    Some(Tx::Postgres(t)) => {
                        let (sql, values) = pg_sql(stmt);
                        let r = sqlx::query_with(AssertSqlSafe(sql), values)
                            .execute(&mut **t)
                            .await?;
                        Ok(r.rows_affected())
                    }
                    #[cfg(feature = "sqlite")]
                    Some(Tx::Sqlite(t)) => {
                        let (sql, values) = sqlite_sql(stmt);
                        let r = sqlx::query_with(AssertSqlSafe(sql), values)
                            .execute(&mut **t)
                            .await?;
                        Ok(r.rows_affected())
                    }
                }
            }
        }
    }
}

#[cfg(feature = "postgres")]
fn pg_sql<S: QueryStatementWriter>(stmt: &S) -> (String, SqlxValues) {
    let (sql, values) = stmt.build_any(&sea_query::PostgresQueryBuilder);
    (sql, SqlxValues(values))
}

#[cfg(feature = "sqlite")]
fn sqlite_sql<S: QueryStatementWriter>(stmt: &S) -> (String, SqlxValues) {
    let (sql, values) = stmt.build_any(&sea_query::SqliteQueryBuilder);
    (sql, SqlxValues(values))
}

#[cfg(feature = "postgres")]
fn pg_row(row: &sqlx::postgres::PgRow) -> Result<Vec<Value>, QueryError> {
    let mut out = Vec::with_capacity(row.len());
    for i in 0..row.len() {
        let raw = row.try_get_raw(i)?;
        if raw.is_null() {
            out.push(Value::Null);
            continue;
        }
        let name = row.column(i).type_info().name().to_owned();
        out.push(match name.as_str() {
            "BOOL" => Value::Boolean(row.try_get(i)?),
            "INT2" => Value::Int16(row.try_get(i)?),
            "INT4" => Value::Int32(row.try_get(i)?),
            "INT8" => Value::Int64(row.try_get(i)?),
            "FLOAT4" => Value::Single(row.try_get(i)?),
            "FLOAT8" => Value::Double(row.try_get(i)?),
            "NUMERIC" => Value::Decimal(row.try_get::<Decimal, _>(i)?),
            "DATE" => Value::Date(row.try_get::<NaiveDate, _>(i)?),
            "TIME" => Value::TimeOfDay(row.try_get::<NaiveTime, _>(i)?),
            "TIMESTAMPTZ" => {
                Value::DateTimeOffset(row.try_get::<DateTime<Utc>, _>(i)?.fixed_offset())
            }
            "TIMESTAMP" => {
                Value::DateTimeOffset(row.try_get::<NaiveDateTime, _>(i)?.and_utc().fixed_offset())
            }
            "UUID" => Value::Guid(row.try_get::<Uuid, _>(i)?),
            "BYTEA" => Value::Binary(row.try_get(i)?),
            _ => Value::String(row.try_get(i)?),
        });
    }
    Ok(out)
}

#[cfg(feature = "sqlite")]
fn sqlite_row(row: &sqlx::sqlite::SqliteRow) -> Result<Vec<Value>, QueryError> {
    let mut out = Vec::with_capacity(row.len());
    for i in 0..row.len() {
        let raw = row.try_get_raw(i)?;
        if raw.is_null() {
            out.push(Value::Null);
            continue;
        }
        let name = raw.type_info().name().to_owned();
        out.push(match name.as_str() {
            "INTEGER" | "BOOLEAN" | "INT8" => Value::Int64(row.try_get_unchecked(i)?),
            "REAL" => Value::Double(row.try_get_unchecked(i)?),
            "BLOB" => Value::Binary(row.try_get_unchecked(i)?),
            _ => Value::String(row.try_get_unchecked(i)?),
        });
    }
    Ok(out)
}

/// Fits a natural database value to the Edm kind the model declares.
fn convert(value: Value, kind: PrimitiveKind) -> Result<Value, QueryError> {
    use PrimitiveKind as P;
    if value.is_null() || value.kind() == Some(kind) {
        return Ok(value);
    }
    let mismatch =
        |v: &Value| QueryError::internal(format!("cannot read {v:?} as {}", kind.edm_name()));
    match (&value, kind) {
        (Value::Int16(_) | Value::Int32(_) | Value::Int64(_), P::Boolean) => {
            Ok(Value::Boolean(value.as_i64() != Some(0)))
        }
        (Value::Int64(v), P::Duration) => Ok(Value::Duration(TimeDelta::microseconds(*v))),
        (Value::String(s), k) if k.is_geo() => Ok(Value::Geo(s.clone())),
        (Value::String(s), k) => from_sea_string(k, s),
        _ => value.coerce(kind).ok_or_else(|| mismatch(&value)),
    }
}
