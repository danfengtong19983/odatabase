//! `$apply` pipelines. Each stage after an aggregation is a derived table of named columns.

use crate::db::ColumnDecode;
use crate::filter::{Frame, FrameKind, Translator, col};
use crate::query::{Page, sort_direction, source};
use crate::record::Record;
use crate::search::search_condition;
use crate::{Dialect, QueryContext, QueryError, Session};
use odatabase_edm::{PrimitiveKind, Value};
use odatabase_uri::{
    AggregateMethod, Aggregation, ApplyColumn, BinaryOp, ComputeItem, Expr as UExpr, ExprType,
    Func, GroupKey, OrderItem, Path, PathRoot, QueryOptions, RankKind, SearchExpr, Segment,
    SelectItem, Step, Transformation,
};
use sea_query::{
    Alias, Asterisk, Expr, ExprTrait, Order, Query, SelectStatement, UnionType, WindowStatement,
};

#[derive(Clone, Debug)]
struct Column {
    path: Vec<String>,
    alias: String,
    ty: ExprType,
    /// Ordering information of `concat`, kept out of the result.
    hidden: bool,
}

/// The fixed inputs of one pipeline.
#[derive(Clone, Copy)]
struct Input<'r> {
    segments: &'r [Segment],
    type_name: &'r str,
}

/// The statement of a pipeline so far.
#[derive(Clone)]
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
    /// `compute` steps on entities, which are renewed when the frame changes.
    computed: Vec<ComputeItem>,
    /// `join` was used, so the rows are no longer identified by the entity key.
    joined: bool,
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

fn key_exprs(tr: &Translator<'_>, frame: &Frame, type_name: &str) -> Result<Vec<Expr>, QueryError> {
    let model = tr.ctx.model;
    model
        .key_of(type_name)
        .iter()
        .map(|k| {
            let p = model
                .property(type_name, k)
                .ok_or_else(|| QueryError::internal(format!("unknown key {k}")))?;
            tr.scalar(frame, p)
        })
        .collect()
}

impl Stage {
    fn start(tr: &mut Translator<'_>, input: Input<'_>) -> Result<Self, QueryError> {
        let mut sel = Query::select();
        let frame = source(tr, &mut sel, input.segments, input.type_name)?;
        Ok(Self {
            sel,
            frame,
            columns: None,
            window: None,
            order: Vec::new(),
            computed: Vec::new(),
            joined: false,
        })
    }

    /// Starts a new derived table when the rows are already aggregated.
    fn open(&mut self, tr: &mut Translator<'_>) {
        if let Some(columns) = &self.columns {
            let (sel, frame) = wrap(tr, self.sel.take(), columns);
            self.sel = sel;
            self.frame = frame;
            self.window = None;
        }
    }

    /// Replaces the statement with a fresh one over the rows whose keys `keys` selects.
    fn restart_with_keys(
        &mut self,
        tr: &mut Translator<'_>,
        input: Input<'_>,
        keys: SelectStatement,
    ) -> Result<(), QueryError> {
        let mut sel = Query::select();
        let frame = source(tr, &mut sel, input.segments, input.type_name)?;
        let own = key_exprs(tr, &frame, input.type_name)?;
        sel.and_where(Expr::tuple(own).in_subquery(keys));
        self.sel = sel;
        self.frame = frame;
        self.window = None;
        let items = self.computed.clone();
        let names: Vec<&str> = items.iter().map(|i| i.alias.as_str()).collect();
        tr.computed.retain(|(n, _)| !names.contains(&n.as_str()));
        for item in &items {
            let e = tr.expr(&self.frame, &item.expr)?;
            tr.computed.push((item.alias.clone(), e));
        }
        Ok(())
    }

    /// Lets later steps act on the result of `top` or `skip` on entities.
    fn settle(&mut self, tr: &mut Translator<'_>, input: Input<'_>) -> Result<(), QueryError> {
        if self.columns.is_some() || self.window.is_none() {
            return Ok(());
        }
        if self.joined {
            return Err(QueryError::not_implemented(
                "a step after top or skip on joined rows",
            ));
        }
        let keys = self.keys_statement(tr, input)?;
        self.restart_with_keys(tr, input, keys)
    }

    /// The current rows, reduced to their entity keys.
    fn keys_statement(
        &mut self,
        tr: &Translator<'_>,
        input: Input<'_>,
    ) -> Result<SelectStatement, QueryError> {
        let keys = key_exprs(tr, &self.frame, input.type_name)?;
        let mut sel = self.sel.take();
        sel.clear_selects();
        for key in keys {
            sel.expr(key);
        }
        Ok(sel)
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
                hidden: false,
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
            let path = vec![item.alias.clone()];
            columns.push(Column {
                path,
                alias,
                ty: item.ty.clone(),
                hidden: false,
            });
        }
        self.sel = outer;
        self.columns = Some(columns);
        self.window = None;
        self.order.clear();
        self.computed.clear();
        tr.computed.clear();
        Ok(())
    }

    /// Expressions that make an order total, so that ties cannot change the rows kept.
    fn tie_breakers(&self, tr: &Translator<'_>, input: Input<'_>) -> Result<Vec<Expr>, QueryError> {
        match &self.columns {
            Some(columns) => Ok(columns
                .iter()
                .map(|c| col(&self.frame.alias, &c.alias))
                .collect()),
            None => key_exprs(tr, &self.frame, input.type_name),
        }
    }

    /// Gives `top` and `skip` a defined order when the pipeline has none.
    fn stabilize(&mut self, tr: &mut Translator<'_>, input: Input<'_>) -> Result<(), QueryError> {
        if self.window.is_some() || !self.order.is_empty() {
            return Ok(());
        }
        self.open(tr);
        for tie in self.tie_breakers(tr, input)? {
            let (direction, nulls) = sort_direction(false);
            self.sel.order_by_expr_with_nulls(tie, direction, nulls);
        }
        Ok(())
    }

    fn rank(
        &mut self,
        tr: &mut Translator<'_>,
        input: Input<'_>,
        kind: RankKind,
        top: bool,
        amount: &UExpr,
        value: &UExpr,
    ) -> Result<(), QueryError> {
        self.settle(tr, input)?;
        self.open(tr);
        let (direction, nulls) = sort_direction(top);
        let measured = tr.expr(&self.frame, value)?;
        let ties = self.tie_breakers(tr, input)?;
        self.order = vec![OrderItem {
            expr: value.clone(),
            descending: top,
        }];
        if kind == RankKind::Count {
            let UExpr::Literal(n) = amount else {
                return Err(QueryError::internal("topcount needs a literal"));
            };
            self.sel.and_where(measured.clone().is_not_null());
            self.sel
                .order_by_expr_with_nulls(measured, direction, nulls);
            for tie in ties {
                let (direction, nulls) = sort_direction(false);
                self.sel.order_by_expr_with_nulls(tie, direction, nulls);
            }
            self.narrow(0, Some(n.as_i64().unwrap_or(0) as u64));
            return Ok(());
        }
        let mut inner = self.sel.take();
        inner.and_where(measured.clone().is_not_null());
        let keys = match self.columns {
            Some(_) => Vec::new(),
            None => key_exprs(tr, &self.frame, input.type_name)?,
        };
        if self.columns.is_none() {
            inner.clear_selects();
            for (i, key) in keys.iter().enumerate() {
                inner.expr_as(key.clone(), Alias::new(format!("k{i}")));
            }
        }
        let mut running = WindowStatement::new();
        running.order_by_expr_with_nulls(measured.clone(), direction, nulls);
        for tie in ties {
            let (direction, nulls) = sort_direction(false);
            running.order_by_expr_with_nulls(tie, direction, nulls);
        }
        inner.expr_as(measured.clone(), Alias::new("rv"));
        inner.expr_window_as(measured.clone().sum(), running, Alias::new("rc"));
        inner.expr_window_as(measured.sum(), WindowStatement::new(), Alias::new("rt"));
        let limit = tr.literal_of(amount)?;
        let alias = tr.alias();
        let mut outer = Query::select();
        outer.from_subquery(inner, Alias::new(alias.as_str()));
        match &self.columns {
            Some(columns) => {
                for c in columns {
                    outer.expr_as(col(&alias, &c.alias), Alias::new(c.alias.as_str()));
                }
            }
            None => {
                for i in 0..keys.len() {
                    outer.expr(col(&alias, &format!("k{i}")));
                }
            }
        }
        let before = col(&alias, "rc").sub(col(&alias, "rv"));
        outer.and_where(match kind {
            RankKind::Sum => before.lt(limit),
            _ => before.mul(100).lt(limit.mul(col(&alias, "rt"))),
        });
        if self.columns.is_some() {
            self.sel = outer;
            return Ok(());
        }
        self.restart_with_keys(tr, input, outer)
    }

    fn join(
        &mut self,
        tr: &mut Translator<'_>,
        path: &Path,
        name: &str,
        outer: bool,
    ) -> Result<(), QueryError> {
        let mut current = tr.root_frame(&self.frame, &path.root)?;
        for step in &path.steps {
            let Step::Navigation(nav) = step else {
                return Err(QueryError::internal("join follows navigation only"));
            };
            current = tr.join_navigation(&mut self.sel, &current, nav, outer)?;
        }
        tr.bind(name, current);
        self.joined = true;
        Ok(())
    }

    fn concat(
        &mut self,
        tr: &mut Translator<'_>,
        input: Input<'_>,
        branches: &[Vec<Transformation>],
        columns: &[ApplyColumn],
    ) -> Result<(), QueryError> {
        self.settle(tr, input)?;
        let targets: Vec<Column> = columns
            .iter()
            .enumerate()
            .map(|(i, c)| Column {
                path: c.path.clone(),
                alias: format!("x{i}"),
                ty: c.ty.clone(),
                hidden: false,
            })
            .collect();
        let computed = tr.computed.clone();
        let mut union: Option<SelectStatement> = None;
        for (index, steps) in branches.iter().enumerate() {
            tr.computed = computed.clone();
            let mut arm = self.clone();
            arm.order.clear();
            run(tr, &mut arm, steps, input)?;
            let selected = arm.arm_statement(tr, input, index, &targets)?;
            match &mut union {
                Some(first) => {
                    first.union(UnionType::All, selected);
                }
                None => union = Some(selected),
            }
        }
        tr.computed.clear();
        self.sel = union.ok_or_else(|| QueryError::internal("concat without branches"))?;
        let mut columns = targets;
        for name in ["sb", "sr"] {
            columns.push(Column {
                path: vec![format!("\u{1}{name}")],
                alias: name.to_owned(),
                ty: ExprType::Primitive(PrimitiveKind::Int64),
                hidden: true,
            });
        }
        self.columns = Some(columns);
        self.window = None;
        self.order.clear();
        self.computed.clear();
        self.joined = false;
        Ok(())
    }

    /// One arm of `concat`, with every target column and a position in the result.
    fn arm_statement(
        mut self,
        tr: &mut Translator<'_>,
        input: Input<'_>,
        index: usize,
        targets: &[Column],
    ) -> Result<SelectStatement, QueryError> {
        if self.columns.is_none() {
            self.settle(tr, input)?;
            self.project_entities(tr, targets)?;
        }
        let mut order = self.order.clone();
        let columns = self.columns.take().unwrap_or_default();
        let (inner, frame) = wrap(tr, self.sel.take(), &columns);
        let alias = tr.alias();
        let mut q = Query::select();
        q.from_subquery(inner, Alias::new(alias.as_str()));
        for target in targets {
            let e = match columns.iter().find(|c| c.path == target.path && !c.hidden) {
                Some(c) => col(&alias, &c.alias),
                None => typed_null(tr, &target.ty),
            };
            q.expr_as(e, Alias::new(target.alias.as_str()));
        }
        q.expr_as(Expr::cust(index.to_string()), Alias::new("sb"));
        let mut position = WindowStatement::new();
        let outer_frame = Frame {
            alias: alias.clone(),
            ..frame
        };
        for item in order.drain(..) {
            let (direction, nulls) = sort_direction(item.descending);
            position.order_by_expr_with_nulls(tr.expr(&outer_frame, &item.expr)?, direction, nulls);
        }
        for c in columns.iter().filter(|c| !c.hidden) {
            let (direction, nulls) = sort_direction(false);
            position.order_by_expr_with_nulls(col(&alias, &c.alias), direction, nulls);
        }
        q.expr_window_as(Expr::cust("ROW_NUMBER()"), position, Alias::new("sr"));
        Ok(q)
    }

    /// Turns entities into the named columns `concat` needs.
    fn project_entities(
        &mut self,
        tr: &mut Translator<'_>,
        targets: &[Column],
    ) -> Result<(), QueryError> {
        self.sel.clear_selects();
        let mut columns = Vec::new();
        for target in targets {
            let frame = self.frame.clone();
            let path = UExpr::Path(Path {
                root: PathRoot::It,
                steps: target.path.iter().cloned().map(Step::Property).collect(),
                ty: target.ty.clone(),
            });
            let computed =
                target.path.len() == 1 && self.computed.iter().any(|i| i.alias == target.path[0]);
            let looked_up = if computed {
                tr.expr(
                    &frame,
                    &UExpr::Computed(target.path[0].clone(), target.ty.clone()),
                )
            } else {
                tr.expr(&frame, &path)
            };
            // Columns only another arm has, and properties hidden from the caller, stay out.
            let Ok(value) = looked_up else {
                continue;
            };
            self.sel.expr_as(value, Alias::new(target.alias.as_str()));
            columns.push(target.clone());
        }
        self.columns = Some(columns);
        Ok(())
    }
}

/// A null that unions with a column of type `ty`.
fn typed_null(tr: &Translator<'_>, ty: &ExprType) -> Expr {
    let dialect = tr.ctx.dialect;
    let name = match ty {
        ExprType::Primitive(k) => dialect.cast_type(*k),
        ExprType::Enum(n) => {
            let text = tr
                .ctx
                .model
                .enum_type(n)
                .is_some_and(|e| e.stored_as_string);
            dialect.cast_type(if text {
                PrimitiveKind::String
            } else {
                PrimitiveKind::Int64
            })
        }
        _ => None,
    };
    match (dialect, name) {
        (Dialect::Postgres, Some(name)) => Expr::cust(format!("CAST(NULL AS {name})")),
        _ => Expr::cust("NULL"),
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

fn run(
    tr: &mut Translator<'_>,
    stage: &mut Stage,
    steps: &[Transformation],
    input: Input<'_>,
) -> Result<(), QueryError> {
    for t in steps {
        match t {
            Transformation::Identity => {}
            Transformation::Filter(e) => {
                stage.settle(tr, input)?;
                stage.open(tr);
                let condition = tr.condition(&stage.frame, e)?;
                stage.sel.and_where(condition);
            }
            Transformation::Search(s) => {
                stage.settle(tr, input)?;
                let condition = search_condition(tr, &stage.frame, input.type_name, s)?;
                stage.sel.and_where(condition);
            }
            Transformation::Compute(items) => {
                stage.settle(tr, input)?;
                if stage.columns.is_none() {
                    for item in items {
                        let e = tr.expr(&stage.frame, &item.expr)?;
                        tr.computed.push((item.alias.clone(), e));
                        stage.computed.push(item.clone());
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
                        hidden: false,
                    });
                }
            }
            Transformation::Aggregate(items) => stage.aggregate(tr, &[], items)?,
            Transformation::GroupBy { keys, aggregates } => {
                stage.aggregate(tr, keys, aggregates)?
            }
            Transformation::OrderBy(items) => {
                stage.settle(tr, input)?;
                stage.open(tr);
                let frame = stage.frame.clone();
                order(tr, &mut stage.sel, &frame, items)?;
                let ties = stage.tie_breakers(tr, input)?;
                for tie in ties {
                    let (direction, nulls) = sort_direction(false);
                    stage.sel.order_by_expr_with_nulls(tie, direction, nulls);
                }
                stage.order = items.clone();
            }
            Transformation::Top(n) => {
                stage.stabilize(tr, input)?;
                stage.narrow(0, Some(*n));
            }
            Transformation::Skip(n) => {
                stage.stabilize(tr, input)?;
                stage.narrow(*n, None);
            }
            Transformation::Rank {
                kind,
                top,
                amount,
                value,
            } => {
                stage.rank(tr, input, *kind, *top, amount, value)?;
            }
            Transformation::Concat { branches, columns } => {
                stage.concat(tr, input, branches, columns)?;
            }
            Transformation::Join { path, alias, outer } => stage.join(tr, path, alias, *outer)?,
        }
    }
    Ok(())
}

/// The keys of the entities a pipeline without aggregation leaves.
pub fn pipeline_keys(
    tr: &mut Translator<'_>,
    segments: &[Segment],
    type_name: &str,
    options: &QueryOptions,
) -> Result<SelectStatement, QueryError> {
    let input = Input {
        segments,
        type_name,
    };
    let mut stage = Stage::start(tr, input)?;
    run(tr, &mut stage, &options.apply, input)?;
    let keys = stage.keys_statement(tr, input)?;
    tr.computed.clear();
    Ok(keys)
}

/// Whether a pipeline that keeps entities needs [`pipeline_keys`] to find them.
pub fn narrows_entities(options: &QueryOptions) -> bool {
    !options.is_aggregated()
        && options.apply.iter().any(|t| {
            matches!(
                t,
                Transformation::OrderBy(_)
                    | Transformation::Top(_)
                    | Transformation::Skip(_)
                    | Transformation::Rank { .. }
            )
        })
}

/// The order the last sorting step of a pipeline leaves.
pub fn pipeline_order(options: &QueryOptions) -> Vec<OrderItem> {
    let mut items = Vec::new();
    for t in &options.apply {
        match t {
            Transformation::OrderBy(list) => items = list.clone(),
            Transformation::Rank { top, value, .. } => {
                items = vec![OrderItem {
                    expr: value.clone(),
                    descending: *top,
                }];
            }
            _ => {}
        }
    }
    items
}

/// `$search` over rows that only have the text of `fields`, ignoring case.
pub(crate) fn text_search(fields: &[UExpr], s: &SearchExpr) -> UExpr {
    let binary = |op, l: UExpr, r: UExpr| UExpr::Binary(op, Box::new(l), Box::new(r));
    match s {
        SearchExpr::Not(inner) => UExpr::Not(Box::new(text_search(fields, inner))),
        SearchExpr::And(l, r) => binary(
            BinaryOp::And,
            text_search(fields, l),
            text_search(fields, r),
        ),
        SearchExpr::Or(l, r) => {
            binary(BinaryOp::Or, text_search(fields, l), text_search(fields, r))
        }
        SearchExpr::Term(term) => {
            let term = term.trim_start_matches('"').to_lowercase();
            fields
                .iter()
                .map(|field| {
                    let lowered = UExpr::Call(Func::ToLower, vec![field.clone()]);
                    let literal = UExpr::Literal(Value::String(term.clone()));
                    UExpr::Call(Func::Contains, vec![lowered, literal])
                })
                .reduce(|a, b| binary(BinaryOp::Or, a, b))
                .unwrap_or(UExpr::Literal(Value::Boolean(false)))
        }
    }
}

/// A string property or column, as a path from the row.
pub(crate) fn text_path(steps: Vec<Step>) -> UExpr {
    UExpr::Path(Path {
        root: PathRoot::It,
        steps,
        ty: ExprType::Primitive(PrimitiveKind::String),
    })
}

/// `$search` over aggregated rows, which only have the text of their string columns.
fn search_columns(columns: &[Column], s: &SearchExpr) -> UExpr {
    let text = ExprType::Primitive(PrimitiveKind::String);
    let fields: Vec<UExpr> = columns
        .iter()
        .filter(|c| !c.hidden && c.ty == text)
        .map(|c| text_path(c.path.iter().cloned().map(Step::Property).collect()))
        .collect();
    text_search(&fields, s)
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
    let input = Input {
        segments,
        type_name,
    };
    let mut stage = Stage::start(tr, input)?;
    run(tr, &mut stage, &options.apply, input)?;
    let columns = stage
        .columns
        .ok_or_else(|| QueryError::internal("the pipeline does not aggregate"))?;
    let (mut outer, frame) = wrap(tr, stage.sel, &columns);
    if let Some(f) = &options.filter {
        let condition = tr.condition(&frame, f)?;
        outer.and_where(condition);
    }
    if let Some(s) = &options.search {
        let condition = tr.condition(&frame, &search_columns(&columns, s))?;
        outer.and_where(condition);
    }
    Ok(ResultQuery {
        sel: outer,
        frame,
        columns,
        order: stage.order,
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
    // The position from `concat` comes first, then every column, which keeps paging stable.
    for c in columns.iter().filter(|c| c.hidden) {
        sel.order_by_expr(col(&frame.alias, &c.alias), Order::Asc);
    }
    for c in columns.iter().filter(|c| !c.hidden) {
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
    let selected = |c: &Column| {
        !c.hidden
            && match &options.select {
                None => true,
                Some(items) => items.iter().any(|i| match i {
                    SelectItem::Star => true,
                    SelectItem::Path(p) => c.path.starts_with(p),
                }),
            }
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
