//! `$expand`: one query per level, restricted to the keys of the parent rows.

use crate::aggregate::{narrows_entities, read_aggregated};
use crate::db::ColumnDecode;
use crate::filter::{Frame, Translator, and_all, col};
use crate::query::{
    EntitySelect, Layout, Page, ReadRequest, apply_conditions, count_collection, distinct_keys,
    fetch_layout, read_collections, read_entities, select_entity, sort_direction, to_records,
};
use crate::record::{Node, Record, Refs};
use crate::{QueryContext, QueryError, Session};
use futures::future::{BoxFuture, try_join_all};
use odatabase_edm::{NavigationProperty, PrimitiveKind, Value};
use odatabase_uri::{ExpandItem, Levels, QueryOptions, Segment, SelectItem, Target};
use sea_query::{
    Alias, Asterisk, Expr, ExprTrait, NullOrdering, Order, OverStatement, Query, SelectStatement,
};
use std::collections::HashMap;

type Direction = (Order, NullOrdering);

/// Parent keys per statement. Keeps parameter counts under the driver limits.
pub const KEY_CHUNK: usize = 500;

/// Bounds on the work a single `$expand` may cause.
#[derive(Clone, Copy, Debug)]
pub struct ExpandLimits {
    /// The depth `$levels=max` stops at.
    pub max_levels: u32,
    /// Upper bound of related entities per request level.
    pub max_rows: u64,
}

impl Default for ExpandLimits {
    fn default() -> Self {
        Self {
            max_levels: 8,
            max_rows: 100_000,
        }
    }
}

/// Adds the source columns of every expanded navigation to the parent's `SELECT`.
pub fn select_join_columns(es: &mut EntitySelect<'_, '_>, frame: &Frame, options: &QueryOptions) {
    let model = es.tr.ctx.model;
    let crate::filter::FrameKind::Entity(type_name) = &frame.kind else {
        return;
    };
    for item in &options.expand {
        if let Some(nav) = model.navigation(type_name, &item.navigation) {
            for column in source_columns(nav) {
                es.hidden(frame, column);
            }
            // A pipeline reads through the path from each parent.
            if !item.options.apply.is_empty() {
                let keys = model.key_of(type_name).iter();
                for p in keys.filter_map(|k| model.property(type_name, k)) {
                    es.hidden(frame, &p.column);
                }
            }
        }
    }
    for p in model.all_properties(type_name) {
        if let Some(c) = &p.collection {
            let selected = match &options.select {
                None => true,
                Some(items) => items.iter().any(|i| match i {
                    SelectItem::Star => true,
                    SelectItem::Path(path) => path.contains(&p.name),
                }),
            };
            if selected {
                for pair in &c.owner {
                    es.hidden(frame, &pair.source);
                }
            }
        }
    }
}

fn source_columns(nav: &NavigationProperty) -> Vec<&str> {
    match &nav.join_table {
        Some(jt) => jt.source.iter().map(|p| p.source.as_str()).collect(),
        None => nav.join.iter().map(|p| p.source.as_str()).collect(),
    }
}

/// `(a = ? AND b = ?) OR ...` over `keys`, or `a IN (...)` for single-column keys.
pub fn key_condition(
    tr: &Translator<'_>,
    alias: &str,
    columns: &[&str],
    keys: &[Vec<Value>],
) -> Result<Expr, QueryError> {
    if let [column] = columns {
        let mut values = Vec::with_capacity(keys.len());
        for k in keys {
            values.push(hidden_literal(tr, &k[0])?);
        }
        return Ok(col(alias, column).binary(sea_query::BinOper::In, Expr::tuple(values)));
    }
    let mut any: Option<Expr> = None;
    for key in keys {
        let mut all = Vec::with_capacity(columns.len());
        for (c, v) in columns.iter().zip(key) {
            all.push(col(alias, c).eq(hidden_literal(tr, v)?));
        }
        let cond = and_all(all).ok_or_else(|| QueryError::internal("empty join"))?;
        any = Some(match any {
            Some(prev) => prev.or(cond),
            None => cond,
        });
    }
    any.ok_or_else(|| QueryError::internal("no parent keys"))
}

fn hidden_literal(tr: &Translator<'_>, v: &Value) -> Result<Expr, QueryError> {
    tr.literal(v)
}

/// The text form of the hidden columns `columns` on `rec`.
pub fn hidden_key(rec: &Record, columns: &[&str]) -> String {
    crate::query::record_key(rec, columns.iter().copied())
}

/// What one expand item contributes to its parents.
struct Loaded {
    children: HashMap<String, Vec<Record>>,
    counts: HashMap<String, i64>,
    keys: Vec<String>,
}

/// Loads expanded navigation properties into `records`, recursing into nested expands.
pub fn expand_records<'r>(
    ctx: &'r QueryContext<'r>,
    session: &'r Session,
    records: &'r mut [Record],
    items: &'r [ExpandItem],
    limits: ExpandLimits,
    depth: u32,
) -> BoxFuture<'r, Result<(), QueryError>> {
    Box::pin(async move {
        if records.is_empty() || items.is_empty() {
            return Ok(());
        }
        let parents: &[Record] = records;
        let type_name = parents[0].type_name.clone();
        let items: Vec<&ExpandItem> = items
            .iter()
            .filter(|i| star_item_visible(ctx, &type_name, i))
            .collect();
        let loads = items.iter().map(|item| {
            let owner = owner_type(ctx, parents, &type_name, item);
            load_item(ctx, session, parents, owner, item, limits, depth)
        });
        let results = try_join_all(loads).await?;
        for (item, loaded) in items.into_iter().zip(results) {
            attach(ctx, records, item, loaded);
        }
        Ok(())
    })
}

/// `$expand=*` leaves out navigations the caller may not see, instead of failing.
fn star_item_visible(ctx: &QueryContext<'_>, type_name: &str, item: &ExpandItem) -> bool {
    if !item.from_star {
        return true;
    }
    let nav_visible = ctx
        .model
        .navigation(type_name, &item.navigation)
        .is_none_or(|n| ctx.claims.sees(&n.visible_for));
    let target_visible = ctx
        .model
        .entity_type(&item.target)
        .is_none_or(|t| ctx.claims.sees(&t.visible_for));
    nav_visible && target_visible
}

/// The type declaring the navigation. In a polymorphic read it may be a derived type.
fn owner_type(
    ctx: &QueryContext<'_>,
    parents: &[Record],
    type_name: &str,
    item: &ExpandItem,
) -> String {
    if ctx.model.navigation(type_name, &item.navigation).is_some() {
        return type_name.to_owned();
    }
    parents
        .iter()
        .find(|p| {
            ctx.model
                .navigation(&p.type_name, &item.navigation)
                .is_some()
        })
        .map_or_else(|| type_name.to_owned(), |p| p.type_name.clone())
}

async fn load_item(
    ctx: &QueryContext<'_>,
    session: &Session,
    parents: &[Record],
    owner: String,
    item: &ExpandItem,
    limits: ExpandLimits,
    depth: u32,
) -> Result<Loaded, QueryError> {
    let model = ctx.model;
    let nav = model
        .navigation(&owner, &item.navigation)
        .ok_or_else(|| QueryError::internal(format!("unknown navigation {}", item.navigation)))?;
    if !ctx.claims.sees(&nav.visible_for) {
        return Err(QueryError::forbidden(format!(
            "{} is not accessible",
            nav.name
        )));
    }
    let sources = source_columns(nav);
    let keys = distinct_keys(parents, sources.iter().copied())
        .into_iter()
        .filter(|k| k.iter().all(|v| !v.is_null()))
        .collect::<Vec<_>>();
    let mut loaded = Loaded {
        children: HashMap::new(),
        counts: HashMap::new(),
        keys: sources.iter().map(|s| (*s).to_owned()).collect(),
    };
    if keys.is_empty() {
        return Ok(loaded);
    }
    let options = effective_options(ctx, item, limits);
    if options.is_aggregated() || narrows_entities(&options) {
        return load_pipeline(ctx, session, parents, item, &options, loaded, limits).await;
    }
    let want_rows = !item.count_only;
    let want_count = item.count_only || item.options.count;
    let mut total: u64 = 0;
    for chunk in keys.chunks(KEY_CHUNK) {
        if want_count {
            for (key, n) in count_children(ctx, session, nav, item, &options, chunk).await? {
                loaded.counts.insert(key, n);
            }
        }
        if want_rows {
            let rows = load_children(ctx, session, nav, item, &options, chunk, limits).await?;
            total += rows.len() as u64;
            if total > limits.max_rows {
                return Err(QueryError::bad_request(format!(
                    "$expand of {} exceeds {} rows",
                    nav.name, limits.max_rows
                )));
            }
            for (key, rec) in rows {
                loaded.children.entry(key).or_default().push(rec);
            }
        }
    }
    if want_rows {
        let mut all: Vec<Record> = Vec::new();
        let mut order = Vec::new();
        for (key, rows) in loaded.children.drain() {
            order.push((key, rows.len()));
            all.extend(rows);
        }
        read_collections(ctx, session, &mut all, &item.target, &options).await?;
        expand_records(ctx, session, &mut all, &options.expand, limits, depth + 1).await?;
        let mut rest = all.into_iter();
        for (key, n) in order {
            loaded.children.insert(key, rest.by_ref().take(n).collect());
        }
    }
    Ok(loaded)
}

/// Runs a `$apply` that changes the rows, once per parent: unlike a filter, its result depends
/// on all the rows of that parent.
#[allow(clippy::too_many_arguments)]
async fn load_pipeline(
    ctx: &QueryContext<'_>,
    session: &Session,
    parents: &[Record],
    item: &ExpandItem,
    options: &QueryOptions,
    mut loaded: Loaded,
    limits: ExpandLimits,
) -> Result<Loaded, QueryError> {
    let model = ctx.model;
    let cols: Vec<&str> = loaded.keys.iter().map(String::as_str).collect();
    let mut total = 0u64;
    for parent in parents {
        let group = hidden_key(parent, &cols);
        if loaded.children.contains_key(&group) {
            continue;
        }
        // The parent's own type: a list can mix derived types, and each filters to its rows.
        let owner = parent.type_name.as_str();
        let values = model
            .key_of(owner)
            .iter()
            .map(|k| {
                let p = model.property(owner, k)?;
                let value = parent.value(k).or(parent.hidden.get(&p.column))?;
                Some((k.clone(), value.clone()))
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| QueryError::internal("a parent without its key"))?;
        let segments = [
            Segment::Key {
                values,
                target: Target::Entity(owner.to_owned()),
            },
            Segment::Navigation {
                name: item.navigation.clone(),
                target: Target::EntityCollection(item.target.clone()),
            },
        ];
        let page = Page {
            top: options.top,
            skip: options.skip,
        };
        let aggregated = options.is_aggregated();
        let rows = if aggregated {
            read_aggregated(ctx, session, &segments, &item.target, options, page).await?
        } else {
            let read = ReadRequest {
                segments: &segments,
                type_name: &item.target,
                options,
                page,
                limits,
            };
            read_entities(ctx, session, read).await?
        };
        if options.count {
            let n = count_collection(ctx, session, &segments, &item.target, options).await?;
            loaded.counts.insert(group.clone(), n);
        }
        total += rows.len() as u64;
        if total > limits.max_rows {
            return Err(QueryError::bad_request(format!(
                "$expand of {} exceeds {} rows",
                item.navigation, limits.max_rows
            )));
        }
        loaded.children.insert(group, rows);
    }
    Ok(loaded)
}

/// The nested options, with `$levels` written out as nested expands.
fn effective_options(
    ctx: &QueryContext<'_>,
    item: &ExpandItem,
    limits: ExpandLimits,
) -> QueryOptions {
    let mut options = item.options.clone();
    if item.ref_only {
        options.select = Some(Vec::new());
    }
    let remaining = match item.levels {
        Some(Levels::Number(n)) => Ord::min(n, limits.max_levels),
        Some(Levels::Max) => limits.max_levels,
        None => 0,
    };
    // A navigation that doesn't lead back to its own type has no further levels.
    let recursive = ctx
        .model
        .navigation(&item.target, &item.navigation)
        .is_some();
    if remaining > 1 && recursive {
        let mut next = item.clone();
        next.levels = Some(Levels::Number(remaining - 1));
        options.expand.push(next);
    }
    options
}

/// Selects children with hidden copies of the parent join columns.
struct ChildQuery {
    sel: SelectStatement,
    layout: Layout,
    frame: Frame,
    parent_alias: String,
    parent_columns: Vec<String>,
}

fn child_query(
    tr: &mut Translator<'_>,
    nav: &NavigationProperty,
    item: &ExpandItem,
    options: &QueryOptions,
) -> Result<ChildQuery, QueryError> {
    let mut sel = Query::select();
    let mut frame = tr.enter(&mut sel, &item.target)?;
    let mut layout = select_entity(tr, &mut sel, &mut frame, &item.target, options)?;
    let (parent_alias, parent_columns) = match &nav.join_table {
        Some(jt) => {
            let j = tr.alias();
            let on = and_all(jt.target.iter().map(|p| {
                col(&j, &p.source).equals((
                    Alias::new(frame.alias.as_str()),
                    Alias::new(p.target.as_str()),
                ))
            }))
            .ok_or_else(|| QueryError::internal("empty join table"))?;
            sel.join_as(
                sea_query::JoinType::InnerJoin,
                Alias::new(jt.table.as_str()),
                Alias::new(j.as_str()),
                on,
            );
            (
                j,
                jt.source
                    .iter()
                    .map(|p| p.target.clone())
                    .collect::<Vec<_>>(),
            )
        }
        None => (
            frame.alias.clone(),
            nav.join
                .iter()
                .map(|p| p.target.clone())
                .collect::<Vec<_>>(),
        ),
    };
    {
        let mut es = EntitySelect {
            tr,
            sel: &mut sel,
            layout: std::mem::take(&mut layout),
        };
        let link = Frame {
            alias: parent_alias.clone(),
            ..frame.clone()
        };
        for c in &parent_columns {
            es.hidden(&link, c);
        }
        crate::expand::select_join_columns(&mut es, &frame, options);
        layout = es.layout;
    }
    apply_conditions(tr, &mut sel, &frame, &item.target, options)?;
    let layout = crate::query::select_computed(tr, &mut sel, &frame, layout, options)?;
    Ok(ChildQuery {
        sel,
        layout,
        frame,
        parent_alias,
        parent_columns,
    })
}

async fn load_children(
    ctx: &QueryContext<'_>,
    session: &Session,
    nav: &NavigationProperty,
    item: &ExpandItem,
    options: &QueryOptions,
    keys: &[Vec<Value>],
    limits: ExpandLimits,
) -> Result<Vec<(String, Record)>, QueryError> {
    let mut tr = Translator::new(ctx.clone());
    let q = child_query(&mut tr, nav, item, options)?;
    let ChildQuery {
        mut sel,
        layout,
        frame,
        parent_alias,
        parent_columns,
    } = q;
    let cols: Vec<&str> = parent_columns.iter().map(String::as_str).collect();
    sel.and_where(key_condition(&tr, &parent_alias, &cols, keys)?);

    let (order, exprs) = order_exprs(&mut tr, &frame, &item.target, options)?;
    let paged = options.top.is_some() || options.skip.is_some();
    let stmt = if paged {
        windowed(
            &mut sel,
            &parent_alias,
            &cols,
            &order,
            &exprs,
            options,
            &layout,
        )
    } else {
        for (e, (o, nulls)) in exprs.iter().cloned().zip(order.iter().cloned()) {
            sel.order_by_expr_with_nulls(e, o, nulls);
        }
        sel.limit(limits.max_rows + 1);
        sel
    };
    let rows = fetch_layout(ctx.model, session, &stmt, &layout).await?;
    let records = to_records(ctx.model, &item.target, &layout, rows);
    Ok(records
        .into_iter()
        .map(|r| (hidden_key(&r, &cols), r))
        .collect())
}

fn order_exprs(
    tr: &mut Translator<'_>,
    frame: &Frame,
    type_name: &str,
    options: &QueryOptions,
) -> Result<(Vec<Direction>, Vec<Expr>), QueryError> {
    let (mut order, mut exprs) = (Vec::new(), Vec::new());
    for o in &options.orderby {
        exprs.push(tr.expr(frame, &o.expr)?);
        order.push(sort_direction(o.descending));
    }
    for k in tr.ctx.model.key_of(type_name) {
        if let Some(p) = tr.ctx.model.property(type_name, k) {
            exprs.push(tr.scalar(frame, p)?);
            order.push(sort_direction(false));
        }
    }
    Ok((order, exprs))
}

/// Applies `$top` and `$skip` per parent with `ROW_NUMBER()`.
fn windowed(
    sel: &mut SelectStatement,
    parent_alias: &str,
    cols: &[&str],
    order: &[Direction],
    exprs: &[Expr],
    options: &QueryOptions,
    layout: &Layout,
) -> SelectStatement {
    let mut window = sea_query::WindowStatement::new();
    for c in cols {
        window.add_partition_by(col(parent_alias, c));
    }
    for (e, (o, nulls)) in exprs.iter().cloned().zip(order.iter().cloned()) {
        window.order_by_expr_with_nulls(e, o, nulls);
    }
    sel.expr_window_as(Expr::cust("ROW_NUMBER()"), window, Alias::new("rn"));
    let mut outer = Query::select();
    for i in 0..layout.columns.len() {
        outer.column(Alias::new(format!("c{i}")));
    }
    outer.from_subquery(sel.take(), Alias::new("w"));
    let skip = options.skip.unwrap_or(0);
    outer.and_where(Expr::col(Alias::new("rn")).gt(skip as i64));
    if let Some(top) = options.top {
        let last = i64::try_from(skip.saturating_add(top)).unwrap_or(i64::MAX);
        outer.and_where(Expr::col(Alias::new("rn")).lte(last));
    }
    outer.order_by(Alias::new("rn"), Order::Asc);
    outer
}

async fn count_children(
    ctx: &QueryContext<'_>,
    session: &Session,
    nav: &NavigationProperty,
    item: &ExpandItem,
    options: &QueryOptions,
    keys: &[Vec<Value>],
) -> Result<Vec<(String, i64)>, QueryError> {
    let mut tr = Translator::new(ctx.clone());
    let mut sel = Query::select();
    let frame = tr.enter(&mut sel, &item.target)?;
    let (alias, columns): (String, Vec<&str>) = match &nav.join_table {
        Some(jt) => {
            let j = tr.alias();
            let on = and_all(jt.target.iter().map(|p| {
                col(&j, &p.source).equals((
                    Alias::new(frame.alias.as_str()),
                    Alias::new(p.target.as_str()),
                ))
            }))
            .ok_or_else(|| QueryError::internal("empty join table"))?;
            sel.join_as(
                sea_query::JoinType::InnerJoin,
                Alias::new(jt.table.as_str()),
                Alias::new(j.as_str()),
                on,
            );
            (j, jt.source.iter().map(|p| p.target.as_str()).collect())
        }
        None => (
            frame.alias.clone(),
            nav.join.iter().map(|p| p.target.as_str()).collect(),
        ),
    };
    apply_conditions(&mut tr, &mut sel, &frame, &item.target, options)?;
    sel.and_where(key_condition(&tr, &alias, &columns, keys)?);
    let mut decoders = Vec::new();
    for c in &columns {
        sel.expr(col(&alias, c));
        sel.add_group_by([col(&alias, c)]);
        decoders.push(ColumnDecode::Natural);
    }
    sel.expr(Expr::col(Asterisk).count());
    decoders.push(ColumnDecode::Primitive(PrimitiveKind::Int64));
    let rows = session.fetch(ctx.model, &sel, &decoders).await?;
    Ok(rows
        .into_iter()
        .map(|mut row| {
            let n = row.pop().and_then(|v| v.as_i64()).unwrap_or(0);
            let key = row
                .iter()
                .map(Value::to_text)
                .collect::<Vec<_>>()
                .join("\u{1}");
            (key, n)
        })
        .collect())
}

fn attach(ctx: &QueryContext<'_>, records: &mut [Record], item: &ExpandItem, loaded: Loaded) {
    let cols: Vec<&str> = loaded.keys.iter().map(String::as_str).collect();
    for rec in records.iter_mut() {
        if ctx
            .model
            .navigation(&rec.type_name, &item.navigation)
            .is_none()
        {
            continue;
        }
        let key = hidden_key(rec, &cols);
        let children = loaded.children.get(&key).cloned().unwrap_or_default();
        let count = loaded.counts.get(&key).copied();
        let node = if item.count_only {
            Node::Count(count.unwrap_or(0))
        } else if item.ref_only {
            Node::Refs(Refs {
                collection: item.collection,
                items: children,
            })
        } else if item.collection {
            Node::Entities {
                items: children,
                count: item.options.count.then(|| count.unwrap_or(0)),
                next_link: None,
            }
        } else {
            Node::Entity(children.into_iter().next().map(Box::new))
        };
        rec.set(item.navigation.clone(), node);
    }
}
