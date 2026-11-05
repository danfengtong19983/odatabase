//! Inserts, updates and deletes. Statements are built from model columns only.

use crate::db::ColumnDecode;
use crate::filter::Translator;
use crate::query::source;
use crate::values::to_sea;
use crate::{Dialect, QueryContext, QueryError, Session};
use odatabase_edm::{Model, PrimitiveKind, TypeKind, Value};
use odatabase_uri::Segment;
use sea_query::{Alias, Expr, ExprTrait, Query};

/// A column assignment or condition.
pub type ColumnValue = (String, Value);

/// The bound value of a column. A null keeps its type, as Postgres requires.
fn bind(
    model: &Model,
    dialect: Dialect,
    value: &Value,
    kind: Option<&TypeKind>,
) -> Result<sea_query::Value, QueryError> {
    if value.is_null() {
        return Ok(typed_null(model, dialect, kind));
    }
    to_sea(model, dialect, value)
}

fn typed_null(model: &Model, dialect: Dialect, kind: Option<&TypeKind>) -> sea_query::Value {
    use PrimitiveKind as P;
    use sea_query::Value as S;
    let primitive = match kind {
        Some(TypeKind::Primitive(k)) => *k,
        Some(TypeKind::Enum(n)) => {
            if model.enum_type(n).is_some_and(|e| e.stored_as_string) {
                P::String
            } else {
                P::Int64
            }
        }
        _ => P::String,
    };
    let text_stored = dialect == Dialect::Sqlite;
    match primitive {
        P::Boolean => S::Bool(None),
        P::Byte | P::SByte | P::Int16 => S::SmallInt(None),
        P::Int32 => S::Int(None),
        P::Int64 | P::Duration => S::BigInt(None),
        P::Single => S::Float(None),
        P::Double => S::Double(None),
        P::Decimal if text_stored => S::Double(None),
        P::Decimal => S::Decimal(None),
        P::Date if !text_stored => S::ChronoDate(None),
        P::TimeOfDay if !text_stored => S::ChronoTime(None),
        P::DateTimeOffset if !text_stored => S::ChronoDateTimeUtc(None),
        P::Guid if !text_stored => S::Uuid(None),
        P::Binary => S::Bytes(None),
        _ => S::String(None),
    }
}

/// One column write, with the declared type of its property.
#[derive(Clone, Debug)]
pub struct ColumnWrite {
    /// The database column.
    pub column: String,
    /// The value to store.
    pub value: Value,
    /// The property's type, which says how enums are stored. `None` for primitives.
    pub kind: Option<TypeKind>,
}

impl ColumnWrite {
    /// A column write.
    pub fn new(column: impl Into<String>, value: Value, kind: Option<TypeKind>) -> Self {
        Self {
            column: column.into(),
            value,
            kind,
        }
    }
}

/// `INSERT`, returning `returning` columns in their natural types.
pub async fn insert(
    model: &Model,
    session: &Session,
    table: &str,
    columns: &[ColumnWrite],
    returning: &[String],
) -> Result<Vec<Value>, QueryError> {
    let dialect = session.dialect();
    let mut stmt = Query::insert();
    stmt.into_table(Alias::new(table));
    if columns.is_empty() {
        stmt.or_default_values();
    } else {
        stmt.columns(columns.iter().map(|c| Alias::new(c.column.as_str())));
        let mut values = Vec::with_capacity(columns.len());
        for c in columns {
            values.push(Expr::value(bind(
                model,
                dialect,
                &c.value,
                c.kind.as_ref(),
            )?));
        }
        stmt.values(values)
            .map_err(|e| QueryError::internal(e.to_string()))?;
    }
    if returning.is_empty() {
        session.affected(&stmt).await?;
        return Ok(Vec::new());
    }
    stmt.returning(Query::returning().columns(returning.iter().map(|c| Alias::new(c.as_str()))));
    let decoders = vec![ColumnDecode::Natural; returning.len()];
    let (_, rows) = session.execute(&stmt, &decoders).await?;
    rows.into_iter()
        .next()
        .ok_or_else(|| QueryError::internal("insert returned no row"))
}

/// `UPDATE ... WHERE`, returning the affected row count.
pub async fn update(
    model: &Model,
    session: &Session,
    table: &str,
    sets: &[ColumnWrite],
    version_bump: Option<&str>,
    conditions: &[ColumnValue],
) -> Result<u64, QueryError> {
    let dialect = session.dialect();
    let mut stmt = Query::update();
    stmt.table(Alias::new(table));
    if sets.is_empty() && version_bump.is_none() {
        return Ok(1);
    }
    for c in sets {
        let v = bind(model, dialect, &c.value, c.kind.as_ref())?;
        stmt.value(Alias::new(c.column.as_str()), Expr::value(v));
    }
    if let Some(column) = version_bump {
        stmt.value(Alias::new(column), Expr::col(Alias::new(column)).add(1));
    }
    for (column, value) in conditions {
        stmt.and_where(
            Expr::col(Alias::new(column.as_str())).eq(Expr::value(to_sea(model, dialect, value)?)),
        );
    }
    session.affected(&stmt).await
}

/// `DELETE ... WHERE`, returning the affected row count.
pub async fn delete(
    model: &Model,
    session: &Session,
    table: &str,
    conditions: &[ColumnValue],
) -> Result<u64, QueryError> {
    let dialect = session.dialect();
    let mut stmt = Query::delete();
    stmt.from_table(Alias::new(table));
    for (column, value) in conditions {
        stmt.and_where(
            Expr::col(Alias::new(column.as_str())).eq(Expr::value(to_sea(model, dialect, value)?)),
        );
    }
    session.affected(&stmt).await
}

/// The order values of a collection's rows, ascending.
pub async fn collection_positions(
    model: &Model,
    session: &Session,
    table: &str,
    owner: &[ColumnValue],
    order: &str,
) -> Result<Vec<i64>, QueryError> {
    let dialect = session.dialect();
    let mut stmt = Query::select();
    stmt.column(Alias::new(order)).from(Alias::new(table));
    for (column, value) in owner {
        stmt.and_where(
            Expr::col(Alias::new(column.as_str())).eq(Expr::value(to_sea(model, dialect, value)?)),
        );
    }
    stmt.order_by(Alias::new(order), sea_query::Order::Asc);
    let decode = [crate::ColumnDecode::Primitive(PrimitiveKind::Int64)];
    let (_, rows) = session.execute(&stmt, &decode).await?;
    Ok(rows
        .iter()
        .filter_map(|r| r.first().and_then(Value::as_i64))
        .collect())
}

/// Frees order value `from` by moving it and those after it up by one. Two steps, because a
/// unique key is checked row by row: the rows pass through negative values first.
pub async fn open_position(
    model: &Model,
    session: &Session,
    table: &str,
    owner: &[ColumnValue],
    order: &str,
    from: i64,
) -> Result<(), QueryError> {
    let dialect = session.dialect();
    let column = || Expr::col(Alias::new(order));
    let step = |value: Expr, condition: Expr| -> Result<_, QueryError> {
        let mut stmt = Query::update();
        stmt.table(Alias::new(table))
            .value(Alias::new(order), value)
            .and_where(condition);
        for (name, v) in owner {
            stmt.and_where(
                Expr::col(Alias::new(name.as_str())).eq(Expr::value(to_sea(model, dialect, v)?)),
            );
        }
        Ok(stmt)
    };
    let up = step(column().add(1).mul(-1), column().gte(from))?;
    session.affected(&up).await?;
    let back = step(column().mul(-1), column().lt(0))?;
    session.affected(&back).await?;
    Ok(())
}

/// Locks the row at `segments` until the transaction ends, so that a check made after this
/// call stays true. SQLite has no row locks, and its writers already run one at a time.
pub async fn lock_row(
    ctx: &QueryContext<'_>,
    session: &Session,
    segments: &[Segment],
    type_name: &str,
) -> Result<(), QueryError> {
    if ctx.dialect != Dialect::Postgres || !matches!(session, Session::Tx(..)) {
        return Ok(());
    }
    let mut tr = Translator::new(ctx.clone());
    let mut sel = Query::select();
    source(&mut tr, &mut sel, segments, type_name)?;
    sel.expr(Expr::val(1));
    sel.lock(sea_query::LockType::Update);
    session
        .fetch(ctx.model, &sel, &[ColumnDecode::Natural])
        .await?;
    Ok(())
}

/// Reads raw columns of the single entity at `segments`, under all row restrictions.
pub async fn read_columns(
    ctx: &QueryContext<'_>,
    session: &Session,
    segments: &[Segment],
    type_name: &str,
    columns: &[String],
) -> Result<Option<Vec<Value>>, QueryError> {
    let mut tr = Translator::new(ctx.clone());
    let mut sel = Query::select();
    let frame = source(&mut tr, &mut sel, segments, type_name)?;
    for c in columns {
        sel.expr(crate::filter::col(&frame.alias, c));
    }
    if columns.is_empty() {
        sel.expr(Expr::val(1));
    }
    sel.limit(1);
    let decoders = vec![ColumnDecode::Natural; columns.len().max(1)];
    let rows = session.fetch(ctx.model, &sel, &decoders).await?;
    Ok(rows.into_iter().next())
}
