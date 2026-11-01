//! Builds and runs the reads of a request: entity sets, navigation paths and counts.

use crate::db::ColumnDecode;
use crate::expand::{ExpandLimits, expand_records};
use crate::filter::{Frame, FrameKind, Translator, and_all, col};
use crate::record::{Node, Record};
use crate::search::search_condition;
use crate::{QueryContext, QueryError, Session};
use odatabase_edm::{Inheritance, Model, Property, TypeKind, Value};
use odatabase_uri::{OrderItem, QueryOptions, Segment, SelectItem};
use sea_query::{Alias, Asterisk, Expr, ExprTrait, NullOrdering, Order, Query, SelectStatement};
use std::collections::{BTreeMap, BTreeSet};

/// `$top` and `$skip` after server-driven paging was applied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Page {
    pub top: Option<u64>,
    pub skip: Option<u64>,
}

/// A read of the entities addressed by `segments`.
#[derive(Clone, Copy)]
pub struct ReadRequest<'a> {
    pub segments: &'a [Segment],
    pub type_name: &'a str,
    pub options: &'a QueryOptions,
    pub page: Page,
    pub limits: ExpandLimits,
}

/// Properties to read. `All` is every visible property.
#[derive(Clone, Debug, PartialEq)]
pub enum Selection {
    All,
    Some(BTreeMap<String, Selection>),
}

impl Selection {
    fn from_items(model: &Model, items: Option<&[SelectItem]>) -> Self {
        let Some(items) = items else {
            return Self::All;
        };
        let mut tree = BTreeMap::new();
        for item in items {
            let path = match item {
                SelectItem::Star => return Self::All,
                SelectItem::Path(path) => path,
            };
            let path: Vec<&String> = path
                .iter()
                .filter(|s| model.entity_type(s).is_none())
                .collect();
            insert_path(&mut tree, &path);
        }
        Self::Some(tree)
    }

    fn child(&self, name: &str) -> Option<&Selection> {
        match self {
            Self::All => Some(&Self::All),
            Self::Some(tree) => tree.get(name),
        }
    }
}

fn insert_path(tree: &mut BTreeMap<String, Selection>, path: &[&String]) {
    let Some((first, rest)) = path.split_first() else {
        return;
    };
    if rest.is_empty() {
        tree.insert((*first).clone(), Selection::All);
        return;
    }
    let slot = tree
        .entry((*first).clone())
        .or_insert_with(|| Selection::Some(BTreeMap::new()));
    if let Selection::Some(inner) = slot {
        insert_path(inner, rest);
    }
}

/// One selected result column and where its value goes.
#[derive(Clone, Debug)]
pub struct Column {
    pub path: Vec<String>,
    pub decode: ColumnDecode,
    /// Type declaring the property, when it exists only on derived types.
    pub only_for: Option<String>,
    /// Name in `Record::hidden` for columns the client did not request.
    pub hidden: Option<String>,
}

/// Selected columns of a statement, in `SELECT` order.
#[derive(Clone, Debug, Default)]
pub struct Layout {
    pub columns: Vec<Column>,
    /// Discriminator tests for polymorphic reads, most derived first.
    pub concrete: Vec<(String, String, String)>,
    /// Hidden flags showing that the row of a `Joined` derived table exists.
    pub present: Vec<(String, String)>,
}

/// Adds the `SELECT` list for an entity and records how to read it back.
pub struct EntitySelect<'t, 'a> {
    pub tr: &'t mut Translator<'a>,
    pub sel: &'t mut SelectStatement,
    pub layout: Layout,
}

impl EntitySelect<'_, '_> {
    fn add(&mut self, expr: Expr, column: Column) {
        let alias = format!("c{}", self.layout.columns.len());
        self.sel.expr_as(expr, Alias::new(alias));
        self.layout.columns.push(column);
    }

    fn decode_of(p: &Property) -> Result<ColumnDecode, QueryError> {
        match &p.kind {
            TypeKind::Primitive(k) => Ok(ColumnDecode::Primitive(*k)),
            TypeKind::Enum(n) => Ok(ColumnDecode::Enum(n.clone())),
            TypeKind::Complex(_) => Err(QueryError::internal("complex has no column")),
        }
    }

    /// Selects the stored properties of `type_name` allowed by `selection`.
    pub fn properties(
        &mut self,
        frame: &Frame,
        props: &[&Property],
        selection: &Selection,
        prefix: &[String],
        only_for: Option<&str>,
    ) -> Result<(), QueryError> {
        for p in props {
            let Some(inner) = selection.child(&p.name) else {
                continue;
            };
            if !self.tr.ctx.claims.sees(&p.visible_for) || p.collection.is_some() {
                continue;
            }
            let mut path = prefix.to_vec();
            path.push(p.name.clone());
            if let Some(t) = &p.transient {
                self.transient_inputs(frame, &t.requires, prefix, only_for)?;
                continue;
            }
            if p.description.is_some() {
                let e = self.tr.description(frame, p)?;
                let decode = Self::decode_of(p)?;
                self.add(
                    e,
                    Column {
                        path,
                        decode,
                        only_for: only_for.map(Into::into),
                        hidden: None,
                    },
                );
                continue;
            }
            if let TypeKind::Complex(t) = &p.kind {
                let model = self.tr.ctx.model;
                let ct = model
                    .complex_type(t)
                    .ok_or_else(|| QueryError::internal(format!("unknown complex type {t}")))?;
                let base = match &frame.kind {
                    FrameKind::Complex { prefix, .. } => format!("{prefix}{}", p.column),
                    _ => p.column.clone(),
                };
                let inner_frame = Frame {
                    kind: FrameKind::Complex {
                        type_name: t.clone(),
                        prefix: base,
                    },
                    ..frame.clone()
                };
                let ps: Vec<&Property> = ct.properties.iter().collect();
                self.properties(&inner_frame, &ps, inner, &path, only_for)?;
                continue;
            }
            let e = self.tr.scalar(frame, p)?;
            let decode = Self::decode_of(p)?;
            self.add(
                e,
                Column {
                    path,
                    decode,
                    only_for: only_for.map(Into::into),
                    hidden: None,
                },
            );
        }
        Ok(())
    }

    /// Loads the inputs of a transient property, whether or not they were selected.
    fn transient_inputs(
        &mut self,
        frame: &Frame,
        requires: &[String],
        prefix: &[String],
        only_for: Option<&str>,
    ) -> Result<(), QueryError> {
        let model = self.tr.ctx.model;
        let FrameKind::Entity(t) = &frame.kind else {
            return Ok(());
        };
        for name in requires {
            let mut path = prefix.to_vec();
            path.push(name.clone());
            if self.layout.columns.iter().any(|c| c.path == path) {
                continue;
            }
            let p = model
                .property(t, name)
                .ok_or_else(|| QueryError::internal(format!("unknown property {name}")))?;
            let e = self.tr.scalar(frame, p)?;
            let decode = Self::decode_of(p)?;
            self.add(
                e,
                Column {
                    path,
                    decode,
                    only_for: only_for.map(Into::into),
                    hidden: Some(format!("transient:{name}")),
                },
            );
        }
        Ok(())
    }

    /// Selects a raw column into `Record::hidden`.
    pub fn hidden(&mut self, frame: &Frame, column: &str) {
        if self
            .layout
            .columns
            .iter()
            .any(|c| c.hidden.as_deref() == Some(column))
        {
            return;
        }
        // Natural type: the value is only compared with the same column of other rows.
        self.add(
            col(&frame.alias, column),
            Column {
                path: Vec::new(),
                decode: ColumnDecode::Primitive(odatabase_edm::PrimitiveKind::String),
                only_for: None,
                hidden: Some(column.to_owned()),
            },
        );
    }
}

/// Everything selected for one entity: properties, keys, ETag, join columns, derived types.
pub fn select_entity(
    tr: &mut Translator<'_>,
    sel: &mut SelectStatement,
    frame: &mut Frame,
    type_name: &str,
    options: &QueryOptions,
) -> Result<Layout, QueryError> {
    let model = tr.ctx.model;
    let mut selection = Selection::from_items(model, options.select.as_deref());
    if let Selection::Some(tree) = &mut selection {
        for k in model.key_of(type_name) {
            tree.entry(k.clone()).or_insert(Selection::All);
        }
        if let Some(e) = model.etag_property(type_name) {
            tree.entry(e.name.clone()).or_insert(Selection::All);
        }
    }
    let derived = model.derived_types(type_name);
    let mut derived_sel = Vec::new();
    for d in &derived {
        if let Some(Inheritance::Joined { join, .. }) = &d.inheritance {
            let j = tr.alias();
            let on = and_all(join.iter().map(|p| {
                col(&frame.alias, &p.source)
                    .equals((Alias::new(j.as_str()), Alias::new(p.target.as_str())))
            }))
            .ok_or_else(|| QueryError::internal("joined type without columns"))?;
            sel.join_as(
                sea_query::JoinType::LeftJoin,
                Alias::new(d.table.as_str()),
                Alias::new(j.as_str()),
                on,
            );
            frame.joined.push((d.name.clone(), j));
        }
        derived_sel.push(*d);
    }
    let props = model.all_properties(type_name);
    let mut es = EntitySelect {
        tr,
        sel,
        layout: Layout::default(),
    };
    es.properties(frame, &props, &selection, &[], None)?;
    let base_props: BTreeSet<&str> = props.iter().map(|p| p.name.as_str()).collect();
    let mut concrete = Vec::new();
    let mut present = Vec::new();
    for d in derived_sel {
        let own: Vec<&Property> = d
            .properties
            .iter()
            .filter(|p| !base_props.contains(p.name.as_str()))
            .collect();
        let frame_for = Frame {
            kind: FrameKind::Entity(d.name.clone()),
            ..frame.clone()
        };
        es.properties(&frame_for, &own, &selection, &[], Some(&d.name))?;
        match &d.inheritance {
            Some(Inheritance::SingleTable {
                discriminator,
                value,
            })
            | Some(Inheritance::Joined {
                discriminator: Some((discriminator, value)),
                ..
            }) => {
                es.hidden(frame, discriminator);
                concrete.push((d.name.clone(), discriminator.clone(), value.clone()));
            }
            Some(Inheritance::Joined { join, .. }) => {
                let alias = frame
                    .joined
                    .iter()
                    .find(|(t, _)| *t == d.name)
                    .map(|(_, a)| a.clone());
                if let (Some(alias), Some(pair)) = (alias, join.first()) {
                    let flag = format!("present:{}", d.name);
                    es.add(
                        col(&alias, &pair.target),
                        Column {
                            path: Vec::new(),
                            decode: ColumnDecode::Primitive(odatabase_edm::PrimitiveKind::String),
                            only_for: None,
                            hidden: Some(flag.clone()),
                        },
                    );
                    present.push((d.name.clone(), flag));
                }
            }
            None => {}
        }
    }
    // Most derived first, so a subtype of a subtype wins.
    concrete.sort_by_key(|(t, ..)| std::cmp::Reverse(model.type_chain(t).len()));
    present.sort_by_key(|(t, _)| std::cmp::Reverse(model.type_chain(t).len()));
    let mut layout = es.layout;
    layout.concrete = concrete;
    layout.present = present;
    Ok(layout)
}

/// Hidden columns are read in their natural type, so they skip `Primitive` conversion errors.
fn hidden_decoders(layout: &Layout) -> Vec<Option<ColumnDecode>> {
    layout
        .columns
        .iter()
        .map(|c| {
            if c.hidden.is_some() && c.path.is_empty() {
                None
            } else {
                Some(c.decode.clone())
            }
        })
        .collect()
}

/// Assembles records from rows laid out by `layout`.
pub fn to_records(
    model: &Model,
    type_name: &str,
    layout: &Layout,
    rows: Vec<Vec<Value>>,
) -> Vec<Record> {
    rows.into_iter()
        .map(|row| {
            let mut hidden: BTreeMap<String, Value> = BTreeMap::new();
            for (c, v) in layout.columns.iter().zip(&row) {
                if let Some(h) = &c.hidden {
                    hidden.insert(h.clone(), v.clone());
                }
            }
            let concrete = concrete_type(type_name, layout, &hidden);
            let mut rec = Record {
                type_name: concrete.clone(),
                ..Record::default()
            };
            for (c, v) in layout.columns.iter().zip(row) {
                if c.hidden.is_some() {
                    continue;
                }
                if let Some(owner) = &c.only_for
                    && !model.type_chain(&concrete).iter().any(|t| t.name == *owner)
                {
                    continue;
                }
                rec.set_path(&c.path, v);
            }
            rec.hidden = hidden;
            rec.collapse_empty_complex();
            rec
        })
        .collect()
}

fn concrete_type(type_name: &str, layout: &Layout, hidden: &BTreeMap<String, Value>) -> String {
    for (t, flag) in &layout.present {
        if hidden.get(flag).is_some_and(|v| !v.is_null()) {
            return t.clone();
        }
    }
    for (t, column, value) in &layout.concrete {
        if hidden.get(column).is_some_and(|v| v.to_text() == *value) {
            return t.clone();
        }
    }
    type_name.to_owned()
}

/// Restricts `sel` to the rows addressed by `segments`, entering `type_name`'s table.
pub fn source(
    tr: &mut Translator<'_>,
    sel: &mut SelectStatement,
    segments: &[Segment],
    type_name: &str,
) -> Result<Frame, QueryError> {
    let model = tr.ctx.model;
    let nav_at = segments
        .iter()
        .rposition(|s| matches!(s, Segment::Navigation { .. }));
    let tail = match nav_at {
        Some(i) => &segments[i + 1..],
        None => segments,
    };
    let frame = match tail.first() {
        Some(Segment::Function {
            name,
            overload,
            args,
            ..
        }) => {
            let op = &model.functions[name][*overload];
            let db_name = op.database_function.as_deref().ok_or_else(|| {
                QueryError::not_implemented(format!("{name} cannot be composed with"))
            })?;
            if tr.ctx.dialect == crate::Dialect::Sqlite {
                return Err(QueryError::not_implemented("table functions on SQLite"));
            }
            let values = literal_args(name, op, args)?;
            tr.enter_function(sel, type_name, db_name, &values)?
        }
        _ => tr.enter(sel, type_name)?,
    };
    for seg in tail {
        if let Segment::Key { values, target } = seg {
            let odatabase_uri::Target::Entity(t) = target else {
                continue;
            };
            for (name, value) in values {
                let p = model
                    .property(t, name)
                    .ok_or_else(|| QueryError::internal(format!("unknown key {name}")))?;
                let c = tr.scalar(&frame, p)?;
                sel.and_where(c.eq(tr.literal(value)?));
            }
        }
    }
    if let Some(i) = nav_at {
        let Segment::Navigation { name, .. } = &segments[i] else {
            unreachable!("found by position");
        };
        let owner = entity_type_at(&segments[..i])
            .ok_or_else(|| QueryError::internal("navigation without a source"))?;
        let nav = model
            .navigation(&owner, name)
            .ok_or_else(|| QueryError::internal(format!("unknown navigation {name}")))?;
        if !tr.ctx.claims.sees(&nav.visible_for) {
            return Err(QueryError::forbidden(format!("{name} is not accessible")));
        }
        let mut parent = Query::select();
        parent.expr(Expr::val(1));
        let parent_frame = source(tr, &mut parent, &segments[..i], &owner)?;
        tr.correlate(&mut parent, &parent_frame, nav, &frame, false)?;
        sel.and_where(Expr::exists(parent));
    }
    Ok(frame)
}

/// Function arguments as literals, in declaration order.
pub fn literal_args(
    name: &str,
    op: &odatabase_edm::Operation,
    args: &[(String, odatabase_uri::Expr)],
) -> Result<Vec<Value>, QueryError> {
    let params = &op.parameters[usize::from(op.is_bound)..];
    params
        .iter()
        .map(|p| match args.iter().find(|(n, _)| *n == p.name) {
            Some((_, odatabase_uri::Expr::Literal(v))) => Ok(v.clone()),
            _ => Err(QueryError::bad_request(format!(
                "{name}: {} must be a literal",
                p.name
            ))),
        })
        .collect()
}

/// `SELECT function(args)`: a scalar database function.
pub async fn call_scalar(
    ctx: &QueryContext<'_>,
    session: &Session,
    function: &str,
    args: &[Value],
    result: ColumnDecode,
) -> Result<Value, QueryError> {
    let tr = Translator::new(ctx.clone());
    let mut call = sea_query::Func::cust(Alias::new(function));
    for a in args {
        call = call.arg(tr.literal(a)?);
    }
    let mut sel = Query::select();
    sel.expr(call);
    let rows = session.fetch(ctx.model, &sel, &[result]).await?;
    Ok(rows
        .into_iter()
        .next()
        .and_then(|mut r| r.pop())
        .unwrap_or(Value::Null))
}

/// The entity type addressed by a path prefix.
fn entity_type_at(segments: &[Segment]) -> Option<String> {
    use odatabase_uri::Target as T;
    segments.iter().rev().find_map(|s| match s.target() {
        T::Entity(t) | T::EntityCollection(t) => Some(t.clone()),
        _ => None,
    })
}

/// Null sorts before every other value in ascending order, on every dialect.
pub fn sort_direction(descending: bool) -> (Order, NullOrdering) {
    if descending {
        (Order::Desc, NullOrdering::Last)
    } else {
        (Order::Asc, NullOrdering::First)
    }
}

pub fn apply_order(
    tr: &mut Translator<'_>,
    sel: &mut SelectStatement,
    frame: &Frame,
    type_name: &str,
    orderby: &[OrderItem],
) -> Result<(), QueryError> {
    for item in orderby {
        let e = tr.expr(frame, &item.expr)?;
        let (order, nulls) = sort_direction(item.descending);
        sel.order_by_expr_with_nulls(e, order, nulls);
    }
    // The key makes paging stable when the requested order has ties.
    for k in tr.ctx.model.key_of(type_name) {
        if let Some(p) = tr.ctx.model.property(type_name, k) {
            let e = tr.scalar(frame, p)?;
            sel.order_by_expr(e, Order::Asc);
        }
    }
    Ok(())
}

/// Adds `$filter` and `$search`.
pub fn apply_conditions(
    tr: &mut Translator<'_>,
    sel: &mut SelectStatement,
    frame: &Frame,
    type_name: &str,
    options: &QueryOptions,
) -> Result<(), QueryError> {
    if let Some(f) = &options.filter {
        let e = tr.expr(frame, f)?;
        sel.and_where(e);
    }
    if let Some(s) = &options.search {
        let e = search_condition(tr, frame, type_name, s)?;
        sel.and_where(e);
    }
    Ok(())
}

/// Reads the entities addressed by a request, including `$expand`.
pub async fn read_entities(
    ctx: &QueryContext<'_>,
    session: &Session,
    req: ReadRequest<'_>,
) -> Result<Vec<Record>, QueryError> {
    let mut tr = Translator::new(ctx.clone());
    let mut sel = Query::select();
    let mut frame = source(&mut tr, &mut sel, req.segments, req.type_name)?;
    let mut layout = select_entity(&mut tr, &mut sel, &mut frame, req.type_name, req.options)?;
    {
        let mut es = EntitySelect {
            tr: &mut tr,
            sel: &mut sel,
            layout: std::mem::take(&mut layout),
        };
        crate::expand::select_join_columns(&mut es, &frame, req.options);
        layout = es.layout;
    }
    apply_conditions(&mut tr, &mut sel, &frame, req.type_name, req.options)?;
    apply_order(
        &mut tr,
        &mut sel,
        &frame,
        req.type_name,
        &req.options.orderby,
    )?;
    if let Some(top) = req.page.top {
        sel.limit(top);
    }
    if let Some(skip) = req.page.skip {
        match req.page.top {
            Some(_) => sel.offset(skip),
            None => sel.limit(i64::MAX as u64).offset(skip),
        };
    }
    let rows = fetch_layout(ctx.model, session, &sel, &layout).await?;
    let mut records = to_records(ctx.model, req.type_name, &layout, rows);
    read_collections(ctx, session, &mut records, req.type_name, req.options).await?;
    expand_records(
        ctx,
        session,
        &mut records,
        &req.options.expand,
        req.limits,
        0,
    )
    .await?;
    Ok(records)
}

/// Fetches rows, leaving hidden columns in their natural type.
pub async fn fetch_layout(
    model: &Model,
    session: &Session,
    sel: &SelectStatement,
    layout: &Layout,
) -> Result<Vec<Vec<Value>>, QueryError> {
    let decoders = hidden_decoders(layout);
    let natural: Vec<ColumnDecode> = decoders
        .iter()
        .map(|d| d.clone().unwrap_or(ColumnDecode::Natural))
        .collect();
    session.fetch(model, sel, &natural).await
}

/// The number of entities matching the request, ignoring `$top` and `$skip`.
pub async fn count_collection(
    ctx: &QueryContext<'_>,
    session: &Session,
    segments: &[Segment],
    type_name: &str,
    options: &QueryOptions,
) -> Result<i64, QueryError> {
    let mut tr = Translator::new(ctx.clone());
    let mut sel = Query::select();
    let frame = source(&mut tr, &mut sel, segments, type_name)?;
    sel.expr(Expr::col(Asterisk).count());
    apply_conditions(&mut tr, &mut sel, &frame, type_name, options)?;
    let rows = session
        .fetch(
            ctx.model,
            &sel,
            &[ColumnDecode::Primitive(odatabase_edm::PrimitiveKind::Int64)],
        )
        .await?;
    Ok(rows
        .first()
        .and_then(|r| r.first())
        .and_then(Value::as_i64)
        .unwrap_or(0))
}

/// Reads a collection property of one entity: `Partners('1')/Tags`.
pub async fn read_collection(
    ctx: &QueryContext<'_>,
    session: &Session,
    owner: &Record,
    type_name: &str,
    property: &str,
) -> Result<Vec<Node>, QueryError> {
    let mut rec = [owner.clone()];
    let options = QueryOptions {
        select: Some(vec![SelectItem::Path(vec![property.to_owned()])]),
        ..QueryOptions::default()
    };
    read_collections(ctx, session, &mut rec, type_name, &options).await?;
    let [rec] = rec;
    match rec.get(property) {
        Some(Node::Collection(items)) => Ok(items.clone()),
        _ => Ok(Vec::new()),
    }
}

/// Loads selected collection properties with one query per property.
pub async fn read_collections(
    ctx: &QueryContext<'_>,
    session: &Session,
    records: &mut [Record],
    type_name: &str,
    options: &QueryOptions,
) -> Result<(), QueryError> {
    if records.is_empty() {
        return Ok(());
    }
    let model = ctx.model;
    let selection = Selection::from_items(model, options.select.as_deref());
    for p in model.all_properties(type_name) {
        let Some(c) = &p.collection else {
            continue;
        };
        let Some(inner) = selection.child(&p.name) else {
            continue;
        };
        if !ctx.claims.sees(&p.visible_for) {
            continue;
        }
        let mut tr = Translator::new(ctx.clone());
        let mut sel = Query::select();
        let alias = tr.alias();
        sel.from_as(Alias::new(c.table.as_str()), Alias::new(alias.as_str()));
        let frame = Frame {
            alias: alias.clone(),
            kind: match &p.kind {
                TypeKind::Complex(t) => FrameKind::Complex {
                    type_name: t.clone(),
                    prefix: String::new(),
                },
                _ => FrameKind::Scalar {
                    column: c.value_column.clone().unwrap_or_else(|| p.column.clone()),
                },
            },
            joined: Vec::new(),
        };
        let mut es = EntitySelect {
            tr: &mut tr,
            sel: &mut sel,
            layout: Layout::default(),
        };
        for pair in &c.owner {
            es.hidden(&frame, &pair.target);
        }
        match &p.kind {
            TypeKind::Complex(t) => {
                let ct = model
                    .complex_type(t)
                    .ok_or_else(|| QueryError::internal(format!("unknown complex type {t}")))?;
                let ps: Vec<&Property> = ct.properties.iter().collect();
                es.properties(&frame, &ps, inner, &[], None)?;
            }
            kind => {
                let decode = match kind {
                    TypeKind::Primitive(k) => ColumnDecode::Primitive(*k),
                    TypeKind::Enum(n) => ColumnDecode::Enum(n.clone()),
                    TypeKind::Complex(_) => unreachable!("handled above"),
                };
                let e = es.tr.scalar(&frame, p)?;
                es.add(
                    e,
                    Column {
                        path: vec!["value".into()],
                        decode,
                        only_for: None,
                        hidden: None,
                    },
                );
            }
        }
        let layout = es.layout;
        if let Some(order) = &c.order_column {
            sel.order_by_expr(col(&alias, order), Order::Asc);
        }
        let keys = distinct_keys(records, c.owner.iter().map(|p| p.source.as_str()));
        let mut rows = Vec::new();
        for chunk in keys.chunks(crate::expand::KEY_CHUNK) {
            let mut q = sel.clone();
            q.and_where(crate::expand::key_condition(
                &tr,
                &alias,
                &c.owner
                    .iter()
                    .map(|p| p.target.as_str())
                    .collect::<Vec<_>>(),
                chunk,
            )?);
            rows.extend(fetch_layout(model, session, &q, &layout).await?);
        }
        let items = to_records(model, "", &layout, rows);
        let targets: Vec<&str> = c.owner.iter().map(|p| p.target.as_str()).collect();
        let is_complex = matches!(p.kind, TypeKind::Complex(_));
        for rec in records.iter_mut() {
            let want = record_key(rec, c.owner.iter().map(|p| p.source.as_str()));
            let values: Vec<Node> = items
                .iter()
                .filter(|i| crate::expand::hidden_key(i, &targets) == want)
                .map(|i| {
                    if is_complex {
                        let mut r = i.clone();
                        r.hidden.clear();
                        Node::Complex(Some(r))
                    } else {
                        Node::Value(i.value("value").cloned().unwrap_or(Value::Null))
                    }
                })
                .collect();
            rec.set(p.name.clone(), Node::Collection(values));
        }
    }
    Ok(())
}

/// Join key of a record, as stable text.
pub fn record_key<'c>(rec: &Record, columns: impl Iterator<Item = &'c str>) -> String {
    columns
        .map(|c| rec.hidden.get(c).map(Value::to_text).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\u{1}")
}

pub(crate) fn distinct_keys<'c>(
    records: &[Record],
    columns: impl Iterator<Item = &'c str> + Clone,
) -> Vec<Vec<Value>> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for rec in records {
        let values: Vec<Value> = columns
            .clone()
            .map(|c| rec.hidden.get(c).cloned().unwrap_or(Value::Null))
            .collect();
        if seen.insert(record_key(rec, columns.clone())) {
            out.push(values);
        }
    }
    out
}
