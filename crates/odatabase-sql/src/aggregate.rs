//! `$apply` pipelines. Each stage after an aggregation is a derived table of named columns.

use crate::db::ColumnDecode;
use crate::filter::{Frame, FrameKind, Translator, col};
use crate::query::{Page, sort_direction, source};
use crate::record::Record;
use crate::search::search_condition;
use crate::{QueryContext, QueryError, Session};
use odatabase_edm::{PrimitiveKind, Value};
use odatabase_uri::{
    AggregateMethod, Aggregation, Expr as UExpr, ExprType, GroupKey, OrderItem, QueryOptions,
    Segment, SelectItem, Transformation,
};
use sea_query::{Alias, Asterisk, Expr, ExprTrait, Query, SelectStatement};

#[derive(Clone, Debug)]
struct Column {
    path: Vec<String>,
    alias: String,
    ty: ExprType,
}

/// The statement of a pipeline so far.
struct Stage {
    sel: SelectStatement,
    frame: Frame,
    /// `None` while the rows are still entities.
    columns: Option<Vec<Column>>,
    /// Offset and limit from `skip` and `top`, which act on the current statement so that
    /// its order decides the rows they keep.
    window: Option<(u64, Option<u64>)>,
    /// The latest `orderby`, which also orders the final result.
    order: Vec<OrderItem>,
}

/// How a column of type `ty` is read.
pub fn decode_for(ty: &ExprType) -> ColumnDecode {
    match ty {
        ExprType::Primitive(k) => ColumnDecode::Primitive(*k),
        ExprType::Enum(n) => ColumnDecode::Enum(n.clone()),
        _ => ColumnDecode::Natural,
    }
}

/// Wraps a statement as a derived table, so later conditions see its results.
fn wrap(
    tr: &mut Translator<'_>,
    mut sel: SelectStatement,
    columns: &[Column],
) -> (SelectStatement, Frame) {
    let alias = tr.alias();
    let mut outer = Query::select();
    for c in columns {
        outer.expr_as(col(&alias, &c.alias), Alias::new(c.alias.as_str()));
    }
    outer.from_subquery(sel.take(), Alias::new(alias.as_str()));
    let frame = Frame {
        alias,
        kind: FrameKind::Aggregated {
            columns: columns
                .iter()
                .map(|c| (c.path.join("/"), c.alias.clone()))
                .collect(),
        },
        joined: Vec::new(),
    };
    (outer, frame)
}

impl Stage {
    /// Starts a new derived table when the rows are already aggregated.
    fn open(&mut self, tr: &mut Translator<'_>) {
        if let Some(columns) = &self.columns {
            let (sel, frame) = wrap(tr, self.sel.take(), columns);
            self.sel = sel;
            self.frame = frame;
            self.window = None;
        }
    }

    /// Entities cannot be wrapped, so steps that must follow `top` or `skip` are refused.
    fn refuse_after_window(&self, step: &str) -> Result<(), QueryError> {
        if self.columns.is_none() && self.window.is_some() {
            return Err(QueryError::not_implemented(format!(
                "{step} after top or skip on entities"
            )));
        }
        Ok(())
    }

    fn narrow(&mut self, skip: u64, top: Option<u64>) {
        let (offset, limit) = self.window.unwrap_or((0, None));
        let limit = limit.map(|l| l.saturating_sub(skip));
        let limit = match (limit, top) {
            (Some(l), Some(t)) => Some(Ord::min(l, t)),
            (l, t) => l.or(t),
        };
        let offset = offset.saturating_add(skip);
        self.window = Some((offset, limit));
        self.sel
            .limit(Ord::min(limit.unwrap_or(u64::MAX), i64::MAX as u64));
        self.sel.offset(Ord::min(offset, i64::MAX as u64));
    }

    /// A value to group or aggregate, which is computed once per input row.
    fn input(&mut self, tr: &mut Translator<'_>, e: &UExpr) -> Result<Expr, QueryError> {
        let frame = self.frame.clone();
        match tr.left_join_path(&mut self.sel, &frame, e)? {
            Some(value) => Ok(value),
            None => tr.expr(&frame, e),
        }
    }

    fn aggregate(
        &mut self,
        tr: &mut Translator<'_>,
        keys: &[GroupKey],
        aggregates: &[Aggregation],
    ) -> Result<(), QueryError> {
        self.open(tr);
        self.sel.clear_selects();
        let mut inputs = Vec::new();
        for key in keys {
            let alias = tr.alias();
            let value = self.input(tr, &key.expr)?;
            self.sel.expr_as(value, Alias::new(alias.as_str()));
            inputs.push(alias);
        }
        let mut measures = Vec::new();
        for item in aggregates {
            let input = match &item.expr {
                Some(e) => {
                    let alias = tr.alias();
                    let value = self.input(tr, e)?;
                    self.sel.expr_as(value, Alias::new(alias.as_str()));
                    Some(alias)
                }
                None => None,
            };
            measures.push(input);
        }
        if inputs.is_empty() && measures.iter().all(Option::is_none) {
            self.sel.expr_as(Expr::cust("1"), Alias::new("one"));
        }
        let derived = tr.alias();
        let mut outer = Query::select();
        outer.from_subquery(self.sel.take(), Alias::new(derived.as_str()));
        let mut columns = Vec::new();
        for (key, input) in keys.iter().zip(&inputs) {
            let alias = tr.alias();
            outer.expr_as(col(&derived, input), Alias::new(alias.as_str()));
            outer.add_group_by([col(&derived, input)]);
            columns.push(Column {
                path: key.path.clone(),
                alias,
                ty: key.ty.clone(),
            });
        }
        for (item, input) in aggregates.iter().zip(&measures) {
            let alias = tr.alias();
            let e = match (item.method, input) {
                (AggregateMethod::Count, _) | (_, None) => Expr::col(Asterisk).count(),
                (AggregateMethod::Sum, Some(i)) => col(&derived, i).sum(),
                (AggregateMethod::Min, Some(i)) => col(&derived, i).min(),
                (AggregateMethod::Max, Some(i)) => col(&derived, i).max(),
                (AggregateMethod::Average, Some(i)) => col(&derived, i).avg(),
                (AggregateMethod::CountDistinct, Some(i)) => col(&derived, i).count_distinct(),
            };
            outer.expr_as(e, Alias::new(alias.as_str()));
            columns.push(Column {
                path: vec![item.alias.clone()],
                alias,
                ty: item.ty.clone(),
            });
        }
        self.sel = outer;
        self.columns = Some(columns);
        self.window = None;
        self.order.clear();
        Ok(())
    }
}

fn order(
    tr: &mut Translator<'_>,
    sel: &mut SelectStatement,
    frame: &Frame,
    items: &[OrderItem],
) -> Result<(), QueryError> {
    for item in items {
        let (direction, nulls) = sort_direction(item.descending);
        sel.order_by_expr_with_nulls(tr.expr(frame, &item.expr)?, direction, nulls);
    }
    Ok(())
}

struct Pipeline {
    sel: SelectStatement,
    columns: Vec<Column>,
    order: Vec<OrderItem>,
}

fn pipeline(
    tr: &mut Translator<'_>,
    segments: &[Segment],
    type_name: &str,
    options: &QueryOptions,
) -> Result<Pipeline, QueryError> {
    let mut sel = Query::select();
    let frame = source(tr, &mut sel, segments, type_name)?;
    let mut stage = Stage {
        sel,
        frame,
        columns: None,
        window: None,
        order: Vec::new(),
    };
    for t in &options.apply {
        match t {
            Transformation::Identity => {}
            Transformation::Filter(e) => {
                stage.refuse_after_window("filter")?;
                stage.open(tr);
                let condition = tr.condition(&stage.frame, e)?;
                stage.sel.and_where(condition);
            }
            Transformation::Search(s) => {
                stage.refuse_after_window("search")?;
                let condition = search_condition(tr, &stage.frame, type_name, s)?;
                stage.sel.and_where(condition);
            }
            Transformation::Compute(items) => {
                if stage.columns.is_none() {
                    for item in items {
                        let e = tr.expr(&stage.frame, &item.expr)?;
                        tr.computed.push((item.alias.clone(), e));
                    }
                    continue;
                }
                stage.open(tr);
                let columns = stage.columns.as_mut().expect("checked above");
                for item in items {
                    let alias = tr.alias();
                    let e = tr.expr(&stage.frame, &item.expr)?;
                    stage.sel.expr_as(e, Alias::new(alias.as_str()));
                    columns.push(Column {
                        path: vec![item.alias.clone()],
                        alias,
                        ty: item.ty.clone(),
                    });
                }
            }
            Transformation::Aggregate(items) => stage.aggregate(tr, &[], items)?,
            Transformation::GroupBy { keys, aggregates } => {
                stage.aggregate(tr, keys, aggregates)?
            }
            Transformation::OrderBy(items) => {
                stage.refuse_after_window("orderby")?;
                stage.open(tr);
                let frame = stage.frame.clone();
                order(tr, &mut stage.sel, &frame, items)?;
                stage.order = items.clone();
            }
            Transformation::Top(n) => stage.narrow(0, Some(*n)),
            Transformation::Skip(n) => stage.narrow(*n, None),
        }
    }
    let columns = stage
        .columns
        .ok_or_else(|| QueryError::internal("the pipeline does not aggregate"))?;
    Ok(Pipeline {
        sel: stage.sel,
        columns,
        order: stage.order,
    })
}

struct ResultQuery {
    sel: SelectStatement,
    frame: Frame,
    columns: Vec<Column>,
    order: Vec<OrderItem>,
}

/// The final statement: the pipeline, then `$filter` over its result.
fn result_query(
    tr: &mut Translator<'_>,
    segments: &[Segment],
    type_name: &str,
    options: &QueryOptions,
) -> Result<ResultQuery, QueryError> {
    let Pipeline {
        sel,
        columns,
        order,
    } = pipeline(tr, segments, type_name, options)?;
    let (mut outer, frame) = wrap(tr, sel, &columns);
    if let Some(f) = &options.filter {
        let condition = tr.condition(&frame, f)?;
        outer.and_where(condition);
    }
    Ok(ResultQuery {
        sel: outer,
        frame,
        columns,
        order,
    })
}

/// Runs an aggregating `$apply`, with the options that follow it.
pub async fn read_aggregated(
    ctx: &QueryContext<'_>,
    session: &Session,
    segments: &[Segment],
    type_name: &str,
    options: &QueryOptions,
    page: Page,
) -> Result<Vec<Record>, QueryError> {
    let mut tr = Translator::new(ctx.clone());
    let ResultQuery {
        mut sel,
        frame,
        columns,
        order: inner_order,
    } = result_query(&mut tr, segments, type_name, options)?;
    let requested = if options.orderby.is_empty() {
        &inner_order
    } else {
        &options.orderby
    };
    order(&mut tr, &mut sel, &frame, requested)?;
    // Every column together identifies a row, which keeps paging stable.
    for c in &columns {
        let (direction, nulls) = sort_direction(false);
        sel.order_by_expr_with_nulls(col(&frame.alias, &c.alias), direction, nulls);
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
    let decoders: Vec<ColumnDecode> = columns.iter().map(|c| decode_for(&c.ty)).collect();
    let rows = session.fetch(ctx.model, &sel, &decoders).await?;
    let selected = |c: &Column| match &options.select {
        None => true,
        Some(items) => items.iter().any(|i| match i {
            SelectItem::Star => true,
            SelectItem::Path(p) => c.path.starts_with(p),
        }),
    };
    Ok(rows
        .into_iter()
        .map(|row: Vec<Value>| {
            let mut rec = Record::default();
            for (c, v) in columns.iter().zip(row) {
                if selected(c) {
                    rec.set_path(&c.path, v);
                }
            }
            rec.collapse_empty_complex();
            rec
        })
        .collect())
}

/// The number of rows an aggregating `$apply` produces.
pub async fn count_aggregated(
    ctx: &QueryContext<'_>,
    session: &Session,
    segments: &[Segment],
    type_name: &str,
    options: &QueryOptions,
) -> Result<i64, QueryError> {
    let mut tr = Translator::new(ctx.clone());
    let mut sel = result_query(&mut tr, segments, type_name, options)?.sel;
    let alias = tr.alias();
    let mut outer = Query::select();
    outer.expr(Expr::col(Asterisk).count());
    outer.from_subquery(sel.take(), Alias::new(alias.as_str()));
    let decode = [ColumnDecode::Primitive(PrimitiveKind::Int64)];
    let rows = session.fetch(ctx.model, &outer, &decode).await?;
    Ok(rows
        .first()
        .and_then(|r| r.first())
        .and_then(Value::as_i64)
        .unwrap_or(0))
}
