//! `$crossjoin`: rows of several entity sets, combined.

use crate::aggregate::decode_for;
use crate::db::ColumnDecode;
use crate::filter::{Frame, Translator};
use crate::query::{Page, sort_direction};
use crate::{QueryContext, QueryError, Session};
use odatabase_edm::{PrimitiveKind, TypeKind, Value};
use odatabase_uri::{ExprType, QueryOptions};
use sea_query::{Asterisk, Expr, ExprTrait, Query, SelectStatement};

/// The keys of one combined row, one key per set.
pub type CrossJoinRow = Vec<Vec<(String, Value)>>;

fn statement(
    tr: &mut Translator<'_>,
    sets: &[(String, String)],
    options: &QueryOptions,
) -> Result<(SelectStatement, Vec<Frame>), QueryError> {
    let mut sel = Query::select();
    let mut frames = Vec::with_capacity(sets.len());
    // Each `enter` adds to `FROM` with its restrictions, which makes the product.
    for (set, type_name) in sets {
        let frame = tr.enter(&mut sel, type_name)?;
        tr.bind(set, frame.clone());
        frames.push(frame);
    }
    if let Some(f) = &options.filter {
        let condition = tr.condition(&frames[0], f)?;
        sel.and_where(condition);
    }
    Ok((sel, frames))
}

/// One page of the combined rows, as the keys of their parts.
pub async fn read_crossjoin(
    ctx: &QueryContext<'_>,
    session: &Session,
    sets: &[(String, String)],
    options: &QueryOptions,
    page: Page,
) -> Result<Vec<CrossJoinRow>, QueryError> {
    let model = ctx.model;
    let mut tr = Translator::new(ctx.clone());
    let (mut sel, frames) = statement(&mut tr, sets, options)?;
    for item in &options.orderby {
        let (direction, nulls) = sort_direction(item.descending);
        let e = tr.expr(&frames[0], &item.expr)?;
        sel.order_by_expr_with_nulls(e, direction, nulls);
    }
    let mut columns = Vec::new();
    let mut decoders = Vec::new();
    for ((_, type_name), frame) in sets.iter().zip(&frames) {
        for k in model.key_of(type_name) {
            let p = model
                .property(type_name, k)
                .ok_or_else(|| QueryError::internal(format!("unknown key {k}")))?;
            let e = tr.scalar(frame, p)?;
            sel.expr(e.clone());
            let (direction, nulls) = sort_direction(false);
            sel.order_by_expr_with_nulls(e, direction, nulls);
            decoders.push(match &p.kind {
                TypeKind::Primitive(k) => decode_for(&ExprType::Primitive(*k)),
                TypeKind::Enum(n) => decode_for(&ExprType::Enum(n.clone())),
                TypeKind::Complex(_) => ColumnDecode::Natural,
            });
            columns.push(k.clone());
        }
    }
    if let Some(top) = page.top {
        sel.limit(top);
    }
    if let Some(skip) = page.skip {
        if page.top.is_none() {
            sel.limit(i64::MAX as u64);
        }
        sel.offset(skip);
    }
    let rows = session.fetch(model, &sel, &decoders).await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let mut values = columns.iter().cloned().zip(row);
            sets.iter()
                .map(|(_, t)| values.by_ref().take(model.key_of(t).len()).collect())
                .collect()
        })
        .collect())
}

/// The number of combined rows.
pub async fn count_crossjoin(
    ctx: &QueryContext<'_>,
    session: &Session,
    sets: &[(String, String)],
    options: &QueryOptions,
) -> Result<i64, QueryError> {
    let mut tr = Translator::new(ctx.clone());
    let (mut sel, _) = statement(&mut tr, sets, options)?;
    sel.expr(Expr::col(Asterisk).count());
    let decode = [ColumnDecode::Primitive(PrimitiveKind::Int64)];
    let rows = session.fetch(ctx.model, &sel, &decode).await?;
    Ok(rows
        .first()
        .and_then(|r| r.first())
        .and_then(Value::as_i64)
        .unwrap_or(0))
}
