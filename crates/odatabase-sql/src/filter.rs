//! Translation of bound expressions to SQL, and the table frames they refer to.

use crate::context::TableRef;
use crate::values::{enum_bits, to_sea};
use crate::{ClaimValue, Dialect, QueryContext, QueryError};
use odatabase_edm::{
    ColumnPair, EntityType, Inheritance, NavigationProperty, PrimitiveKind, Property, TypeKind,
    Value,
};
use odatabase_uri::{BinaryOp, Expr as UExpr, ExprType, Func, LambdaKind, Path, PathRoot, Step};
use sea_query::{Alias, Asterisk, Expr, ExprTrait, Query, SelectStatement, SubQueryStatement};

/// A table instance in a query, and the type its columns are read as.
#[derive(Clone, Debug)]
pub struct Frame {
    pub alias: String,
    pub kind: FrameKind,
    /// Aliases of the joined tables of `Joined` inheritance, by type name.
    pub joined: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
pub enum FrameKind {
    Entity(String),
    Complex {
        type_name: String,
        prefix: String,
    },
    /// Rows of a primitive collection table.
    Scalar {
        column: String,
    },
    /// A derived table of `$apply`: property paths and the columns holding them.
    Aggregated {
        columns: Vec<(String, String)>,
    },
}

pub fn col(alias: &str, column: &str) -> Expr {
    Expr::col((Alias::new(alias), Alias::new(column)))
}

fn sub(select: SelectStatement) -> Expr {
    Expr::SubQuery(None, Box::new(SubQueryStatement::SelectStatement(select)))
}

fn exists(select: SelectStatement) -> Expr {
    Expr::exists(select)
}

/// A SQL template with `$1`-style arguments. Each backend gets the placeholders it expects:
/// SQLite parameters are positional, so a repeated argument is bound once per use.
pub fn tpl(
    dialect: Dialect,
    template: impl Into<String>,
    args: impl IntoIterator<Item = Expr>,
) -> Expr {
    let template = template.into();
    let args: Vec<Expr> = args.into_iter().collect();
    if dialect == Dialect::Postgres {
        return Expr::cust_with_exprs(template, args);
    }
    let (mut sql, mut ordered) = (String::with_capacity(template.len()), Vec::new());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            sql.push(c);
            continue;
        }
        let mut n = 0;
        while let Some(d) = chars.peek().and_then(|d| d.to_digit(10)) {
            n = n * 10 + d as usize;
            chars.next();
        }
        ordered.push(args[n - 1].clone());
        sql.push('?');
    }
    Expr::cust_with_exprs(sql, ordered)
}

pub fn and_all(items: impl IntoIterator<Item = Expr>) -> Option<Expr> {
    items.into_iter().reduce(|a, b| a.and(b))
}

/// Translates expressions and issues the table aliases of one statement tree.
pub struct Translator<'a> {
    pub ctx: QueryContext<'a>,
    /// Aliases from `compute`, in terms of the current statement.
    pub computed: Vec<(String, Expr)>,
    /// The next expression sits where null and false mean the same, as in a `WHERE`.
    positive: bool,
    /// Left joins already on the statement, by source alias and navigation name.
    joins: Vec<(String, Frame)>,
    next_alias: u32,
    vars: Vec<(String, Frame)>,
}

impl<'a> Translator<'a> {
    pub fn new(ctx: QueryContext<'a>) -> Self {
        Self {
            ctx,
            computed: Vec::new(),
            positive: false,
            joins: Vec::new(),
            next_alias: 0,
            vars: Vec::new(),
        }
    }

    pub fn alias(&mut self) -> String {
        self.next_alias += 1;
        format!("t{}", self.next_alias)
    }

    fn dialect(&self) -> Dialect {
        self.ctx.dialect
    }

    /// The table the root of a type's hierarchy lives in.
    pub fn root_table(&self, type_name: &str) -> Result<&'a str, QueryError> {
        self.ctx
            .model
            .type_chain(type_name)
            .iter()
            .rev()
            .find(|t| !t.table.is_empty())
            .map(|t| t.table.as_str())
            .ok_or_else(|| QueryError::internal(format!("{type_name} has no table")))
    }

    /// Adds the table of `type_name` to `sel`, with every restriction that type carries.
    /// All access to entity tables passes through here, so protections cannot be bypassed.
    pub fn enter(
        &mut self,
        sel: &mut SelectStatement,
        type_name: &str,
    ) -> Result<Frame, QueryError> {
        let model = self.ctx.model;
        let t = model
            .entity_type(type_name)
            .ok_or_else(|| QueryError::internal(format!("unknown type {type_name}")))?;
        if !self.ctx.claims.sees(&t.visible_for) {
            return Err(QueryError::forbidden(format!(
                "{type_name} is not accessible"
            )));
        }
        let alias = self.alias();
        sel.from_as(
            Alias::new(self.root_table(type_name)?),
            Alias::new(alias.as_str()),
        );
        let mut frame = Frame {
            alias,
            kind: FrameKind::Entity(t.name.clone()),
            joined: Vec::new(),
        };
        let mut chain = model.type_chain(type_name);
        chain.reverse();
        for ty in &chain {
            if let Some(Inheritance::Joined { join, .. }) = &ty.inheritance {
                let j = self.alias();
                let on = and_all(join.iter().map(|p| {
                    col(&frame.alias, &p.source)
                        .equals((Alias::new(j.as_str()), Alias::new(p.target.as_str())))
                }));
                let on = on.ok_or_else(|| QueryError::internal("joined type without columns"))?;
                sel.join_as(
                    sea_query::JoinType::InnerJoin,
                    Alias::new(ty.table.as_str()),
                    Alias::new(j.as_str()),
                    on,
                );
                frame.joined.push((ty.name.clone(), j));
            }
        }
        if let Some(cond) = self.discriminator(&frame, t)? {
            sel.and_where(cond);
        }
        for cond in self.row_restrictions(&frame, type_name)? {
            sel.and_where(cond);
        }
        Ok(frame)
    }

    /// Like [`Translator::enter`], reading from a table function instead of a table.
    pub fn enter_function(
        &mut self,
        sel: &mut SelectStatement,
        type_name: &str,
        function: &str,
        args: &[Value],
    ) -> Result<Frame, QueryError> {
        let model = self.ctx.model;
        let t = model
            .entity_type(type_name)
            .ok_or_else(|| QueryError::internal(format!("unknown type {type_name}")))?;
        if !self.ctx.claims.sees(&t.visible_for) {
            return Err(QueryError::forbidden(format!(
                "{type_name} is not accessible"
            )));
        }
        let mut call = sea_query::Func::cust(Alias::new(function));
        for a in args {
            call = call.arg(self.literal(a)?);
        }
        let alias = self.alias();
        sel.from_function(call, Alias::new(alias.as_str()));
        let frame = Frame {
            alias,
            kind: FrameKind::Entity(t.name.clone()),
            joined: Vec::new(),
        };
        for cond in self.row_restrictions(&frame, type_name)? {
            sel.and_where(cond);
        }
        Ok(frame)
    }

    /// `kind IN (...)` for the type and all types derived from it.
    fn discriminator(&self, frame: &Frame, t: &EntityType) -> Result<Option<Expr>, QueryError> {
        let model = self.ctx.model;
        let disc = |ty: &EntityType| match &ty.inheritance {
            Some(Inheritance::SingleTable {
                discriminator,
                value,
            }) => Some((discriminator.clone(), value.clone())),
            Some(Inheritance::Joined {
                discriminator: Some((c, v)),
                ..
            }) => Some((c.clone(), v.clone())),
            _ => None,
        };
        let Some((column, value)) = disc(t) else {
            return Ok(None);
        };
        let mut values = vec![value];
        values.extend(
            model
                .derived_types(&t.name)
                .into_iter()
                .filter_map(disc)
                .map(|d| d.1),
        );
        Ok(Some(col(&frame.alias, &column).is_in(values)))
    }

    /// Conditions from protected properties and query extensions.
    fn row_restrictions(&self, frame: &Frame, type_name: &str) -> Result<Vec<Expr>, QueryError> {
        let model = self.ctx.model;
        let mut out = Vec::new();
        for ty in model.type_chain(type_name) {
            if ty.query_extension {
                let table = TableRef {
                    alias: frame.alias.clone(),
                };
                let ext = self.ctx.extensions.get(&ty.name).ok_or_else(|| {
                    QueryError::internal(format!("no query extension registered for {}", ty.name))
                })?;
                out.extend(ext.condition(&table, self.ctx.claims)?);
            }
            for p in &ty.properties {
                self.protect(frame, p, &ty.name, "", &mut out)?;
            }
        }
        Ok(out)
    }

    fn protect(
        &self,
        frame: &Frame,
        p: &Property,
        owner: &str,
        prefix: &str,
        out: &mut Vec<Expr>,
    ) -> Result<(), QueryError> {
        if let TypeKind::Complex(c) = &p.kind
            && p.collection.is_none()
        {
            let inner = format!("{prefix}{}", p.column);
            if let Some(ct) = self.ctx.model.complex_type(c) {
                for ip in &ct.properties {
                    self.protect(frame, ip, owner, &inner, out)?;
                }
            }
            return Ok(());
        }
        for protection in &p.protections {
            let values = self.ctx.claims.values.get(&protection.claim);
            let values = values.filter(|v| !v.is_empty()).ok_or_else(|| {
                QueryError::forbidden(format!("claim {} is required", protection.claim))
            })?;
            let column = col(
                self.owner_alias(frame, owner),
                &format!("{prefix}{}", p.column),
            );
            let mut any: Option<Expr> = None;
            for value in values {
                let cond = self.claim_condition(&column, value, protection.allow_wildcards)?;
                any = Some(match any {
                    Some(prev) => prev.or(cond),
                    None => cond,
                });
            }
            out.extend(any);
        }
        Ok(())
    }

    fn claim_condition(
        &self,
        column: &Expr,
        claim: &ClaimValue,
        wildcards: bool,
    ) -> Result<Expr, QueryError> {
        let bind = |v: &Value| self.literal(v);
        Ok(match claim {
            ClaimValue::Equals(Value::String(s))
                if wildcards && s.contains(['*', '+', '%', '_']) =>
            {
                let pattern = s.replace('*', "%").replace('+', "_");
                tpl(
                    self.dialect(),
                    "($1 LIKE $2)",
                    [column.clone(), Expr::val(pattern)],
                )
            }
            ClaimValue::Equals(v) => column.clone().eq(bind(v)?),
            ClaimValue::Range(low, high) => column
                .clone()
                .gte(bind(low)?)
                .and(column.clone().lte(bind(high)?)),
        })
    }

    fn owner_alias<'f>(&self, frame: &'f Frame, owner: &str) -> &'f str {
        frame
            .joined
            .iter()
            .find(|(t, _)| t == owner)
            .map_or(frame.alias.as_str(), |(_, a)| a.as_str())
    }

    /// The type in the chain of `type_name` that declares property `name`.
    fn owner_of(&self, type_name: &str, name: &str) -> Option<&'a EntityType> {
        self.ctx
            .model
            .type_chain(type_name)
            .into_iter()
            .find(|t| t.properties.iter().any(|p| p.name == name))
    }

    /// The column expression of a stored scalar on `frame`.
    pub fn scalar(&self, frame: &Frame, p: &Property) -> Result<Expr, QueryError> {
        match &frame.kind {
            FrameKind::Entity(t) => {
                let owner = self
                    .owner_of(t, &p.name)
                    .map_or(t.as_str(), |o| o.name.as_str());
                Ok(col(self.owner_alias(frame, owner), &p.column))
            }
            FrameKind::Complex { prefix, .. } => {
                Ok(col(&frame.alias, &format!("{prefix}{}", p.column)))
            }
            FrameKind::Scalar { column } => Ok(col(&frame.alias, column)),
            FrameKind::Aggregated { .. } => {
                Err(QueryError::internal("an aggregated row has no properties"))
            }
        }
    }

    pub fn literal(&self, v: &Value) -> Result<Expr, QueryError> {
        Ok(Expr::value(to_sea(self.ctx.model, self.dialect(), v)?))
    }

    /// Adds the condition linking `source` and `target`, joining a join table to `sel`,
    /// which is the statement that reads `target` when `sel_reads_target` is set.
    pub fn correlate(
        &mut self,
        sel: &mut SelectStatement,
        source: &Frame,
        nav: &NavigationProperty,
        target: &Frame,
        sel_reads_target: bool,
    ) -> Result<(), QueryError> {
        let eq = |a: &Frame, ac: &str, b: &Frame, bc: &str| {
            col(&a.alias, ac).equals((Alias::new(b.alias.as_str()), Alias::new(bc)))
        };
        if let Some(jt) = &nav.join_table {
            let j = self.alias();
            let link = Frame {
                alias: j.clone(),
                kind: source.kind.clone(),
                joined: Vec::new(),
            };
            let (near, far): (Vec<_>, Vec<_>) = if sel_reads_target {
                (
                    jt.target
                        .iter()
                        .map(|p| eq(&link, &p.source, target, &p.target))
                        .collect(),
                    jt.source
                        .iter()
                        .map(|p| eq(source, &p.source, &link, &p.target))
                        .collect(),
                )
            } else {
                (
                    jt.source
                        .iter()
                        .map(|p| eq(source, &p.source, &link, &p.target))
                        .collect(),
                    jt.target
                        .iter()
                        .map(|p| eq(&link, &p.source, target, &p.target))
                        .collect(),
                )
            };
            let on = and_all(near).ok_or_else(|| QueryError::internal("empty join table"))?;
            sel.join_as(
                sea_query::JoinType::InnerJoin,
                Alias::new(jt.table.as_str()),
                Alias::new(j.as_str()),
                on,
            );
            for cond in far {
                sel.and_where(cond);
            }
            return Ok(());
        }
        for p in &nav.join {
            sel.and_where(eq(source, &p.source, target, &p.target));
        }
        Ok(())
    }

    /// Translates a condition of a `WHERE`, which allows faster forms of to-one paths.
    pub fn condition(&mut self, it: &Frame, e: &UExpr) -> Result<Expr, QueryError> {
        self.positive = true;
        self.expr(it, e)
    }

    /// Reaches a to-one path through a `LEFT JOIN`, for ordering and grouping over many rows.
    /// Returns `None` when a join could change the row count, or the path needs a subquery.
    pub fn left_join_path(
        &mut self,
        sel: &mut SelectStatement,
        frame: &Frame,
        e: &UExpr,
    ) -> Result<Option<Expr>, QueryError> {
        let UExpr::Path(path) = e else {
            return Ok(None);
        };
        if path.root != PathRoot::It || !matches!(path.steps.first(), Some(Step::Navigation(_))) {
            return Ok(None);
        }
        if path
            .steps
            .iter()
            .any(|s| matches!(s, Step::Count | Step::Cast(_)))
        {
            return Ok(None);
        }
        let mut current = frame.clone();
        let mut steps = path.steps.as_slice();
        while let Some((Step::Navigation(name), rest)) = steps.split_first() {
            let nav = self.navigation(&current, name)?;
            if !self.joins_one_row(nav) {
                return Ok(None);
            }
            current = self.left_join(sel, &current, nav)?;
            steps = rest;
        }
        self.walk(&current, steps).map(Some)
    }

    /// Whether a navigation matches at most one row: its target columns are exactly the key.
    fn joins_one_row(&self, nav: &NavigationProperty) -> bool {
        let model = self.ctx.model;
        if nav.collection || nav.join_table.is_some() {
            return false;
        }
        let joined_type = model
            .type_chain(&nav.target)
            .iter()
            .any(|t| matches!(t.inheritance, Some(Inheritance::Joined { .. })));
        let key: Vec<&str> = model
            .key_of(&nav.target)
            .iter()
            .filter_map(|k| model.property(&nav.target, k))
            .map(|p| p.column.as_str())
            .collect();
        !joined_type
            && key.len() == nav.join.len()
            && nav.join.iter().all(|p| key.contains(&p.target.as_str()))
    }

    fn left_join(
        &mut self,
        sel: &mut SelectStatement,
        source: &Frame,
        nav: &NavigationProperty,
    ) -> Result<Frame, QueryError> {
        let id = format!("{}/{}", source.alias, nav.name);
        if let Some((_, frame)) = self.joins.iter().find(|(k, _)| *k == id) {
            return Ok(frame.clone());
        }
        let model = self.ctx.model;
        let t = model
            .entity_type(&nav.target)
            .ok_or_else(|| QueryError::internal(format!("unknown type {}", nav.target)))?;
        if !self.ctx.claims.sees(&t.visible_for) {
            return Err(QueryError::forbidden(format!(
                "{} is not accessible",
                t.name
            )));
        }
        let frame = Frame {
            alias: self.alias(),
            kind: FrameKind::Entity(t.name.clone()),
            joined: Vec::new(),
        };
        // Restrictions belong in `ON`: in a `WHERE` they would drop the source row as well.
        let mut on: Vec<Expr> = nav
            .join
            .iter()
            .map(|p| {
                col(&source.alias, &p.source).equals((
                    Alias::new(frame.alias.as_str()),
                    Alias::new(p.target.as_str()),
                ))
            })
            .collect();
        on.extend(self.discriminator(&frame, t)?);
        on.extend(self.row_restrictions(&frame, &t.name)?);
        let on = and_all(on).ok_or_else(|| QueryError::internal("navigation without columns"))?;
        sel.join_as(
            sea_query::JoinType::LeftJoin,
            Alias::new(self.root_table(&t.name)?),
            Alias::new(frame.alias.as_str()),
            on,
        );
        self.joins.push((id, frame.clone()));
        Ok(frame)
    }

    /// `EXISTS` over a to-one path, which lets the database start from the related table.
    /// A missing relation gives false, where a subquery compared with a literal gives null.
    fn exists_path(
        &mut self,
        frame: &Frame,
        steps: &[Step],
        build: &mut dyn FnMut(&mut Self, Expr) -> Result<Expr, QueryError>,
    ) -> Result<Expr, QueryError> {
        let Some((Step::Navigation(name), rest)) = steps.split_first() else {
            return Err(QueryError::internal(
                "a to-one path starts with a navigation",
            ));
        };
        let nav = self.navigation(frame, name)?;
        if nav.collection {
            return Err(QueryError::bad_request("a collection cannot be used here"));
        }
        let mut q = Query::select();
        q.expr(Expr::val(1));
        let target = self.enter(&mut q, &nav.target)?;
        self.correlate(&mut q, frame, nav, &target, true)?;
        let inner = if matches!(rest.first(), Some(Step::Navigation(_))) {
            self.exists_path(&target, rest, build)?
        } else {
            let value = self.walk(&target, rest)?;
            build(self, value)?
        };
        q.and_where(inner);
        Ok(exists(q))
    }

    /// The frame and steps of a path that can use [`Translator::exists_path`].
    fn to_one_path<'p>(&self, it: &Frame, e: &'p UExpr) -> Option<(Frame, &'p [Step])> {
        let UExpr::Path(p) = e else {
            return None;
        };
        let plain = p
            .steps
            .iter()
            .all(|s| matches!(s, Step::Navigation(_) | Step::Property(_)));
        if !plain || !matches!(p.steps.first(), Some(Step::Navigation(_))) {
            return None;
        }
        Some((self.root_frame(it, &p.root).ok()?, &p.steps))
    }

    pub fn expr(&mut self, it: &Frame, e: &UExpr) -> Result<Expr, QueryError> {
        let positive = std::mem::take(&mut self.positive);
        match e {
            UExpr::Binary(op @ (BinaryOp::And | BinaryOp::Or), l, r) => {
                self.positive = positive;
                let a = self.expr(it, l)?;
                self.positive = positive;
                let b = self.expr(it, r)?;
                Ok(if *op == BinaryOp::And {
                    a.and(b)
                } else {
                    a.or(b)
                })
            }
            UExpr::Binary(
                op @ (BinaryOp::Gt | BinaryOp::Ge | BinaryOp::Lt | BinaryOp::Le),
                l,
                r,
            ) if positive && is_value_literal(r) => {
                let Some((frame, steps)) = self.to_one_path(it, l) else {
                    return self.plain_expr(it, e);
                };
                let (op, lty, rty) = (*op, l.ty(), r.ty());
                let b = self.expr(it, r)?;
                self.exists_path(&frame, steps, &mut |tr, a| {
                    let (a, b) = tr.temporal(&lty, &rty, a, b.clone());
                    Ok(match op {
                        BinaryOp::Gt => a.gt(b),
                        BinaryOp::Ge => a.gte(b),
                        BinaryOp::Lt => a.lt(b),
                        _ => a.lte(b),
                    })
                })
            }
            UExpr::In(left, items) if positive && !items.is_empty() => {
                if !items.iter().all(is_value_literal) {
                    return self.plain_expr(it, e);
                }
                let Some((frame, steps)) = self.to_one_path(it, left) else {
                    return self.plain_expr(it, e);
                };
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(self.expr(it, item)?);
                }
                self.exists_path(&frame, steps, &mut |_, a| {
                    Ok(a.binary(sea_query::BinOper::In, Expr::tuple(values.clone())))
                })
            }
            _ => self.plain_expr(it, e),
        }
    }

    fn plain_expr(&mut self, it: &Frame, e: &UExpr) -> Result<Expr, QueryError> {
        match e {
            UExpr::Literal(Value::Null) => Ok(Expr::cust("NULL")),
            UExpr::Literal(v) => self.literal(v),
            UExpr::Path(p) => self.path(it, p),
            UExpr::Not(inner) => Ok(self.expr(it, inner)?.not()),
            UExpr::Negate(inner) => {
                let v = self.expr(it, inner)?;
                Ok(tpl(self.dialect(), "(- $1)", [v]))
            }
            UExpr::Binary(op, l, r) => self.binary(it, *op, l, r),
            UExpr::In(left, items) => {
                let l = self.expr(it, left)?;
                if items.is_empty() {
                    return Ok(Expr::val(false));
                }
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(self.expr(it, item)?);
                }
                Ok(l.binary(sea_query::BinOper::In, Expr::tuple(values)))
            }
            UExpr::Call(f, args) => {
                let mut out = Vec::with_capacity(args.len());
                for a in args {
                    out.push(self.expr(it, a)?);
                }
                self.call(*f, out)
            }
            UExpr::Lambda {
                source,
                kind,
                var,
                body,
            } => {
                let root = self.root_frame(it, &source.root)?;
                self.lambda(
                    it,
                    &root,
                    &source.steps,
                    *kind,
                    var.as_deref(),
                    body.as_deref(),
                )
            }
            UExpr::Cast(value, ty) => self.cast(it, value, ty),
            UExpr::IsOf(value, ty) => self.isof(it, value.as_deref(), ty),
            UExpr::Function { name, args, .. } => self.function(it, name, args),
            UExpr::Computed(name, _) => self
                .computed
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, e)| e.clone())
                .ok_or_else(|| QueryError::internal(format!("{name} was not computed"))),
        }
    }

    fn root_frame(&self, it: &Frame, root: &PathRoot) -> Result<Frame, QueryError> {
        match root {
            PathRoot::It => Ok(it.clone()),
            PathRoot::Var(name) => self
                .vars
                .iter()
                .rev()
                .find(|(n, _)| n == name)
                .map(|(_, f)| f.clone())
                .ok_or_else(|| QueryError::internal(format!("unbound variable {name}"))),
        }
    }

    fn path(&mut self, it: &Frame, p: &Path) -> Result<Expr, QueryError> {
        let root = self.root_frame(it, &p.root)?;
        self.walk(&root, &p.steps)
    }

    fn lookup(&self, frame: &Frame, name: &str) -> Result<&'a Property, QueryError> {
        let model = self.ctx.model;
        let p = match &frame.kind {
            FrameKind::Entity(t) => model.property(t, name),
            FrameKind::Complex { type_name, .. } => model
                .complex_type(type_name)
                .and_then(|c| c.properties.iter().find(|p| p.name == name)),
            FrameKind::Scalar { .. } | FrameKind::Aggregated { .. } => None,
        };
        let p = p.ok_or_else(|| QueryError::internal(format!("unknown property {name}")))?;
        if !self.ctx.claims.sees(&p.visible_for) {
            return Err(QueryError::forbidden(format!("{name} is not accessible")));
        }
        Ok(p)
    }

    fn navigation(&self, frame: &Frame, name: &str) -> Result<&'a NavigationProperty, QueryError> {
        let FrameKind::Entity(t) = &frame.kind else {
            return Err(QueryError::internal("navigation on a non-entity"));
        };
        let n = self
            .ctx
            .model
            .navigation(t, name)
            .ok_or_else(|| QueryError::internal(format!("unknown navigation {name}")))?;
        if !self.ctx.claims.sees(&n.visible_for) {
            return Err(QueryError::forbidden(format!("{name} is not accessible")));
        }
        Ok(n)
    }

    /// Translates path steps into a value, nesting a subquery at each navigation.
    fn walk(&mut self, frame: &Frame, steps: &[Step]) -> Result<Expr, QueryError> {
        if let FrameKind::Aggregated { columns } = &frame.kind {
            let names: Vec<&str> = steps
                .iter()
                .filter_map(|s| match s {
                    Step::Property(n) => Some(n.as_str()),
                    _ => None,
                })
                .collect();
            let key = names.join("/");
            let found = columns.iter().find(|(path, _)| *path == key);
            let (_, column) = found
                .ok_or_else(|| QueryError::internal(format!("{key} is not a result column")))?;
            return Ok(col(&frame.alias, column));
        }
        let Some((step, rest)) = steps.split_first() else {
            return match &frame.kind {
                FrameKind::Scalar { column } => Ok(col(&frame.alias, column)),
                _ => Err(QueryError::bad_request(
                    "a structured value cannot be used here",
                )),
            };
        };
        match step {
            Step::Property(name) => {
                let p = self.lookup(frame, name)?;
                if p.transient.is_some() {
                    return Err(QueryError::bad_request(format!(
                        "transient property {name} cannot be used in expressions"
                    )));
                }
                if let Some(c) = &p.collection {
                    if rest != [Step::Count] {
                        return Err(QueryError::bad_request("a collection cannot be used here"));
                    }
                    let mut q = Query::select();
                    let alias = self.alias();
                    q.expr(Expr::col(Asterisk).count());
                    q.from_as(Alias::new(c.table.as_str()), Alias::new(alias.as_str()));
                    self.owner_condition(&mut q, frame, &c.owner, &alias);
                    return Ok(sub(q));
                }
                if p.description.is_some() {
                    return self.description(frame, p);
                }
                match &p.kind {
                    TypeKind::Complex(t) => {
                        let prefix = match &frame.kind {
                            FrameKind::Complex { prefix, .. } => format!("{prefix}{}", p.column),
                            _ => p.column.clone(),
                        };
                        let inner = Frame {
                            kind: FrameKind::Complex {
                                type_name: t.clone(),
                                prefix,
                            },
                            ..frame.clone()
                        };
                        self.walk(&inner, rest)
                    }
                    _ if rest.is_empty() => self.scalar(frame, p),
                    _ => Err(QueryError::bad_request(format!("{name} has no members"))),
                }
            }
            Step::Navigation(name) => {
                let nav = self.navigation(frame, name)?;
                let mut q = Query::select();
                let target = self.enter(&mut q, &nav.target)?;
                self.correlate(&mut q, frame, nav, &target, true)?;
                if rest == [Step::Count] {
                    q.expr(Expr::col(Asterisk).count());
                    return Ok(sub(q));
                }
                if nav.collection {
                    return Err(QueryError::bad_request("a collection cannot be used here"));
                }
                let value = self.walk(&target, rest)?;
                q.expr(value);
                Ok(sub(q))
            }
            Step::Cast(type_name) => {
                let t = self.ctx.model.entity_type(type_name);
                let t = t.ok_or_else(|| QueryError::internal("unknown cast target"))?;
                if let Some(Inheritance::Joined { join, .. }) = &t.inheritance
                    && !frame.joined.iter().any(|(n, _)| n == type_name)
                {
                    let alias = self.alias();
                    let mut q = Query::select();
                    q.from_as(Alias::new(t.table.as_str()), Alias::new(alias.as_str()));
                    self.owner_condition(&mut q, frame, join, &alias);
                    let mut inner = frame.clone();
                    inner.joined.push((type_name.clone(), alias));
                    inner.kind = FrameKind::Entity(type_name.clone());
                    let value = self.walk(&inner, rest)?;
                    q.expr(value);
                    return Ok(sub(q));
                }
                let inner = Frame {
                    kind: FrameKind::Entity(type_name.clone()),
                    ..frame.clone()
                };
                self.walk(&inner, rest)
            }
            Step::Count => Err(QueryError::bad_request("$count needs a collection")),
        }
    }

    fn owner_condition(
        &self,
        q: &mut SelectStatement,
        frame: &Frame,
        pairs: &[ColumnPair],
        alias: &str,
    ) {
        for p in pairs {
            q.and_where(
                col(&frame.alias, &p.source)
                    .equals((Alias::new(alias), Alias::new(p.target.as_str()))),
            );
        }
    }

    /// The language-dependent text of a description property, in the request language.
    pub fn description(&mut self, frame: &Frame, p: &Property) -> Result<Expr, QueryError> {
        let d = p.description.as_ref().expect("checked by the caller");
        let alias = self.alias();
        let mut q = Query::select();
        q.expr(col(&alias, &d.text_column));
        q.from_as(Alias::new(d.table.as_str()), Alias::new(alias.as_str()));
        self.owner_condition(&mut q, frame, &d.join, &alias);
        q.and_where(col(&alias, &d.language_column).eq(self.ctx.language));
        q.limit(1);
        Ok(sub(q))
    }

    /// `it` stays the enclosing resource: only the lambda variable reaches the inner rows.
    fn lambda(
        &mut self,
        it: &Frame,
        frame: &Frame,
        steps: &[Step],
        kind: LambdaKind,
        var: Option<&str>,
        body: Option<&UExpr>,
    ) -> Result<Expr, QueryError> {
        let Some((step, rest)) = steps.split_first() else {
            return Err(QueryError::internal("lambda without a collection"));
        };
        match step {
            Step::Property(name) => {
                let p = self.lookup(frame, name)?;
                if let Some(c) = &p.collection {
                    let alias = self.alias();
                    let mut q = Query::select();
                    q.expr(Expr::val(1));
                    q.from_as(Alias::new(c.table.as_str()), Alias::new(alias.as_str()));
                    self.owner_condition(&mut q, frame, &c.owner, &alias);
                    let kind_of = match &p.kind {
                        TypeKind::Complex(t) => FrameKind::Complex {
                            type_name: t.clone(),
                            prefix: String::new(),
                        },
                        _ => FrameKind::Scalar {
                            column: c.value_column.clone().unwrap_or_else(|| p.column.clone()),
                        },
                    };
                    let inner = Frame {
                        alias,
                        kind: kind_of,
                        joined: Vec::new(),
                    };
                    return self.finish_lambda(it, q, &inner, kind, var, body);
                }
                let TypeKind::Complex(t) = &p.kind else {
                    return Err(QueryError::bad_request(format!(
                        "{name} is not a collection"
                    )));
                };
                let prefix = match &frame.kind {
                    FrameKind::Complex { prefix, .. } => format!("{prefix}{}", p.column),
                    _ => p.column.clone(),
                };
                let inner = Frame {
                    kind: FrameKind::Complex {
                        type_name: t.clone(),
                        prefix,
                    },
                    ..frame.clone()
                };
                self.lambda(it, &inner, rest, kind, var, body)
            }
            Step::Navigation(name) => {
                let nav = self.navigation(frame, name)?;
                let mut q = Query::select();
                q.expr(Expr::val(1));
                let target = self.enter(&mut q, &nav.target)?;
                self.correlate(&mut q, frame, nav, &target, true)?;
                if rest.is_empty() {
                    return self.finish_lambda(it, q, &target, kind, var, body);
                }
                let inner = self.lambda(it, &target, rest, kind, var, body)?;
                q.and_where(inner);
                Ok(exists(q))
            }
            Step::Cast(t) => {
                let inner = Frame {
                    kind: FrameKind::Entity(t.clone()),
                    ..frame.clone()
                };
                self.lambda(it, &inner, rest, kind, var, body)
            }
            Step::Count => Err(QueryError::bad_request("$count cannot be followed")),
        }
    }

    fn finish_lambda(
        &mut self,
        it: &Frame,
        mut q: SelectStatement,
        inner: &Frame,
        kind: LambdaKind,
        var: Option<&str>,
        body: Option<&UExpr>,
    ) -> Result<Expr, QueryError> {
        let translated = match (var, body) {
            (Some(v), Some(b)) => {
                self.vars.push((v.to_owned(), inner.clone()));
                // `any` keeps rows where the body is true, and `all` checks `IS NOT TRUE`.
                let e = self.condition(it, b);
                self.vars.pop();
                Some(e?)
            }
            _ => None,
        };
        Ok(match (kind, translated) {
            (LambdaKind::Any, Some(e)) => {
                q.and_where(e);
                exists(q)
            }
            (LambdaKind::Any, None) => exists(q),
            (LambdaKind::All, Some(e)) => {
                // A null body counts as false, so the row must not satisfy the negation by NULL.
                q.and_where(tpl(self.dialect(), "(($1) IS NOT TRUE)", [e]));
                exists(q).not()
            }
            (LambdaKind::All, None) => Expr::val(true),
        })
    }

    fn binary(
        &mut self,
        it: &Frame,
        op: BinaryOp,
        l: &UExpr,
        r: &UExpr,
    ) -> Result<Expr, QueryError> {
        use BinaryOp as B;
        match op {
            B::And | B::Or => Err(QueryError::internal("translated by expr")),
            B::Eq | B::Ne => self.equality(it, op == B::Eq, l, r),
            B::Gt | B::Ge | B::Lt | B::Le => {
                let (a, b) = self.comparable(it, l, r)?;
                Ok(match op {
                    B::Gt => a.gt(b),
                    B::Ge => a.gte(b),
                    B::Lt => a.lt(b),
                    _ => a.lte(b),
                })
            }
            B::Add | B::Sub if is_temporal(&l.ty()) || is_temporal(&r.ty()) => {
                Err(QueryError::not_implemented("date arithmetic"))
            }
            B::Add => Ok(self.expr(it, l)?.add(self.expr(it, r)?)),
            B::Sub => Ok(self.expr(it, l)?.sub(self.expr(it, r)?)),
            B::Mul => Ok(self.expr(it, l)?.mul(self.expr(it, r)?)),
            B::Div => Ok(self.expr(it, l)?.div(self.expr(it, r)?)),
            B::DivBy => {
                let target = self
                    .dialect()
                    .cast_type(PrimitiveKind::Decimal)
                    .expect("every dialect has a decimal type");
                let (a, b) = (self.expr(it, l)?, self.expr(it, r)?);
                Ok(tpl(
                    self.dialect(),
                    format!("(CAST($1 AS {target}) / $2)"),
                    [a, b],
                ))
            }
            B::Mod => Ok(self.expr(it, l)?.modulo(self.expr(it, r)?)),
            B::Has => {
                let UExpr::Literal(Value::Enum { type_name, member }) = r else {
                    return Err(QueryError::bad_request("has needs an enum literal"));
                };
                let e = self.ctx.model.enum_type(type_name);
                let e = e.ok_or_else(|| QueryError::internal("unknown enum"))?;
                let bits = enum_bits(e, member)?;
                if e.stored_as_string {
                    return Err(QueryError::not_implemented("has on enums stored as text"));
                }
                let a = self.expr(it, l)?;
                Ok(tpl(
                    self.dialect(),
                    "(($1 & $2) = $2)",
                    [a, Expr::val(bits)],
                ))
            }
        }
    }

    /// Both operands, adjusted so that SQLite compares timestamps as instants.
    fn comparable(&mut self, it: &Frame, l: &UExpr, r: &UExpr) -> Result<(Expr, Expr), QueryError> {
        let (a, b) = (self.expr(it, l)?, self.expr(it, r)?);
        Ok(self.temporal(&l.ty(), &r.ty(), a, b))
    }

    fn temporal(&self, l: &ExprType, r: &ExprType, a: Expr, b: Expr) -> (Expr, Expr) {
        let timestamp = ExprType::Primitive(PrimitiveKind::DateTimeOffset);
        if (*l == timestamp || *r == timestamp) && self.dialect() == Dialect::Sqlite {
            return (
                tpl(self.dialect(), "julianday($1)", [a]),
                tpl(self.dialect(), "julianday($1)", [b]),
            );
        }
        (a, b)
    }

    /// `eq` and `ne` are two-valued in OData: null equals null and differs from everything else.
    fn equality(
        &mut self,
        it: &Frame,
        positive: bool,
        l: &UExpr,
        r: &UExpr,
    ) -> Result<Expr, QueryError> {
        let is_null = |e: &UExpr| matches!(e, UExpr::Literal(Value::Null));
        if is_null(l) && is_null(r) {
            return Ok(Expr::val(positive));
        }
        if is_null(l) || is_null(r) {
            let side = if is_null(r) { l } else { r };
            if let UExpr::Path(p) = side
                && matches!(p.steps.last(), Some(Step::Navigation(_)))
            {
                let root = self.root_frame(it, &p.root)?;
                let related = self.lambda(it, &root, &p.steps, LambdaKind::Any, None, None)?;
                return Ok(if positive { related.not() } else { related });
            }
            let operand = self.expr(it, side)?;
            return Ok(if positive {
                operand.is_null()
            } else {
                operand.is_not_null()
            });
        }
        // Two-valued like `eq` itself, so this form is safe under `not` as well.
        if positive
            && is_value_literal(r)
            && let Some((frame, steps)) = self.to_one_path(it, l)
        {
            let (lty, rty) = (l.ty(), r.ty());
            let b = self.expr(it, r)?;
            return self.exists_path(&frame, steps, &mut |tr, a| {
                let (a, b) = tr.temporal(&lty, &rty, a, b.clone());
                Ok(a.eq(b))
            });
        }
        let (ln, rn) = (self.nullable(it, l), self.nullable(it, r));
        let (a, b) = self.comparable(it, l, r)?;
        Ok(match (positive, ln, rn) {
            (true, false, false) => a.eq(b),
            (false, false, false) => a.ne(b),
            (true, true, false) => a.clone().eq(b).and(a.is_not_null()),
            (true, false, true) => a.eq(b.clone()).and(b.is_not_null()),
            (false, true, false) => a.clone().ne(b).or(a.is_null()),
            (false, false, true) => a.ne(b.clone()).or(b.is_null()),
            (true, true, true) => tpl(self.dialect(), self.dialect().null_safe_eq(), [a, b]),
            (false, true, true) => tpl(self.dialect(), self.dialect().null_safe_ne(), [a, b]),
        })
    }

    /// Whether `e` can be null. Conservative: only plain non-null columns and literals are not.
    fn nullable(&self, it: &Frame, e: &UExpr) -> bool {
        match e {
            UExpr::Literal(v) => v.is_null(),
            UExpr::Path(p) => {
                let Ok(mut frame) = self.root_frame(it, &p.root) else {
                    return true;
                };
                for (i, step) in p.steps.iter().enumerate() {
                    let Step::Property(name) = step else {
                        return true;
                    };
                    let Ok(prop) = self.lookup(&frame, name) else {
                        return true;
                    };
                    if prop.nullable || prop.description.is_some() {
                        return true;
                    }
                    if i + 1 < p.steps.len() {
                        let TypeKind::Complex(t) = &prop.kind else {
                            return true;
                        };
                        frame.kind = FrameKind::Complex {
                            type_name: t.clone(),
                            prefix: String::new(),
                        };
                    }
                }
                p.steps.is_empty()
            }
            _ => true,
        }
    }

    fn call(&self, f: Func, a: Vec<Expr>) -> Result<Expr, QueryError> {
        let pg = self.dialect() == Dialect::Postgres;
        let date_part = |sqlite: &str, pg_field: &str, mut a: Vec<Expr>| -> Expr {
            let v = a.remove(0);
            if pg {
                tpl(
                    self.dialect(),
                    format!("CAST(EXTRACT({pg_field} FROM $1) AS integer)"),
                    [v],
                )
            } else {
                tpl(
                    self.dialect(),
                    format!("CAST(strftime('{sqlite}', $1) AS INTEGER)"),
                    [v],
                )
            }
        };
        let one = |t: &'static str, mut a: Vec<Expr>| tpl(self.dialect(), t, [a.remove(0)]);
        Ok(match f {
            Func::Contains if pg => tpl(self.dialect(), "(strpos($1, $2) > 0)", a),
            Func::Contains => tpl(self.dialect(), "(instr($1, $2) > 0)", a),
            Func::StartsWith if pg => tpl(self.dialect(), "starts_with($1, $2)", a),
            Func::StartsWith => tpl(self.dialect(), "(substr($1, 1, length($2)) = $2)", a),
            Func::EndsWith if pg => tpl(self.dialect(), "(right($1, length($2)) = $2)", a),
            Func::EndsWith => tpl(
                self.dialect(),
                "(length($2) = 0 OR substr($1, -length($2)) = $2)",
                a,
            ),
            Func::Length => one("length($1)", a),
            Func::IndexOf if pg => tpl(self.dialect(), "(strpos($1, $2) - 1)", a),
            Func::IndexOf => tpl(self.dialect(), "(instr($1, $2) - 1)", a),
            Func::Substring if a.len() == 2 => {
                tpl(self.dialect(), "substr($1, CAST($2 AS integer) + 1)", a)
            }
            Func::Substring => tpl(
                self.dialect(),
                "substr($1, CAST($2 AS integer) + 1, CAST($3 AS integer))",
                a,
            ),
            Func::ToLower => one("lower($1)", a),
            Func::ToUpper => one("upper($1)", a),
            Func::Trim => one("trim($1)", a),
            Func::Concat => tpl(self.dialect(), "($1 || $2)", a),
            Func::MatchesPattern if pg => tpl(self.dialect(), "($1 ~ $2)", a),
            Func::MatchesPattern => {
                return Err(QueryError::not_implemented("matchesPattern on SQLite"));
            }
            Func::Year => date_part("%Y", "YEAR", a),
            Func::Month => date_part("%m", "MONTH", a),
            Func::Day => date_part("%d", "DAY", a),
            Func::Hour => date_part("%H", "HOUR", a),
            Func::Minute => date_part("%M", "MINUTE", a),
            Func::Second if pg => one("CAST(floor(EXTRACT(SECOND FROM $1)) AS integer)", a),
            Func::Second => date_part("%S", "SECOND", a),
            Func::FractionalSeconds if pg => one(
                "(EXTRACT(SECOND FROM $1) - floor(EXTRACT(SECOND FROM $1)))",
                a,
            ),
            Func::FractionalSeconds => one(
                "(CAST(strftime('%f', $1) AS REAL) - CAST(strftime('%S', $1) AS INTEGER))",
                a,
            ),
            Func::TotalSeconds => one("($1 / 1000000.0)", a),
            // Timestamps are normalized to UTC, so the original offset is gone.
            Func::TotalOffsetMinutes => Expr::val(0),
            Func::Date if pg => one("CAST($1 AS date)", a),
            Func::Date => one("date($1)", a),
            Func::Time if pg => one("CAST($1 AS time)", a),
            Func::Time => one("time($1)", a),
            Func::Now if pg => Expr::cust("now()"),
            Func::Now => Expr::cust("strftime('%Y-%m-%dT%H:%M:%fZ', 'now')"),
            Func::MaxDateTime | Func::MinDateTime => {
                return Err(QueryError::not_implemented("maxdatetime and mindatetime"));
            }
            // numeric rounds halves away from zero as OData requires, double to even.
            Func::Round if pg => one("round(CAST($1 AS numeric))", a),
            Func::Round => one("round($1)", a),
            Func::Floor if pg => one("floor($1)", a),
            Func::Floor => one("(CAST($1 AS INTEGER) - ($1 < CAST($1 AS INTEGER)))", a),
            Func::Ceiling if pg => one("ceil($1)", a),
            Func::Ceiling => one("(CAST($1 AS INTEGER) + ($1 > CAST($1 AS INTEGER)))", a),
        })
    }

    fn cast(&mut self, it: &Frame, value: &UExpr, ty: &ExprType) -> Result<Expr, QueryError> {
        match ty {
            ExprType::Primitive(k) => {
                let target = self
                    .dialect()
                    .cast_type(*k)
                    .ok_or_else(|| QueryError::not_implemented("cast to this type"))?;
                let v = self.expr(it, value)?;
                Ok(tpl(self.dialect(), format!("CAST($1 AS {target})"), [v]))
            }
            _ => Err(QueryError::not_implemented("cast to structured types")),
        }
    }

    fn isof(
        &mut self,
        it: &Frame,
        value: Option<&UExpr>,
        ty: &ExprType,
    ) -> Result<Expr, QueryError> {
        match (value, ty) {
            (None, ExprType::Entity(target)) => {
                let t = self.ctx.model.entity_type(target);
                let t = t.ok_or_else(|| QueryError::internal("unknown type"))?;
                let frame = it.clone();
                match self.discriminator(&frame, t)? {
                    Some(cond) => Ok(cond),
                    None => match &t.inheritance {
                        Some(Inheritance::Joined { join, .. }) => {
                            let alias = self.alias();
                            let mut q = Query::select();
                            q.expr(Expr::val(1));
                            q.from_as(Alias::new(t.table.as_str()), Alias::new(alias.as_str()));
                            self.owner_condition(&mut q, &frame, join, &alias);
                            Ok(exists(q))
                        }
                        _ => Ok(Expr::val(true)),
                    },
                }
            }
            (Some(v), other) => Ok(Expr::val(v.ty() == *other)),
            (None, _) => Ok(Expr::val(false)),
        }
    }

    fn function(
        &mut self,
        it: &Frame,
        name: &str,
        args: &[(String, UExpr)],
    ) -> Result<Expr, QueryError> {
        let overloads = self.ctx.model.functions.get(name);
        let op = overloads
            .and_then(|o| {
                o.iter().find(|o| {
                    !o.is_bound
                        && o.parameters.len() == args.len()
                        && o.parameters
                            .iter()
                            .all(|p| args.iter().any(|(n, _)| *n == p.name))
                })
            })
            .ok_or_else(|| QueryError::internal(format!("unknown function {name}")))?;
        let Some(db_name) = &op.database_function else {
            return Err(QueryError::not_implemented(format!(
                "{name} has no database function, so it cannot be used in expressions"
            )));
        };
        let mut values = Vec::with_capacity(args.len());
        let mut holders = Vec::with_capacity(args.len());
        for (i, p) in op.parameters.iter().enumerate() {
            let (_, arg) = args
                .iter()
                .find(|(n, _)| *n == p.name)
                .expect("matched above");
            values.push(self.expr(it, arg)?);
            holders.push(format!("${}", i + 1));
        }
        Ok(tpl(
            self.dialect(),
            format!("{db_name}({})", holders.join(", ")),
            values,
        ))
    }
}

fn is_value_literal(e: &UExpr) -> bool {
    matches!(e, UExpr::Literal(v) if !v.is_null())
}

fn is_temporal(t: &ExprType) -> bool {
    matches!(
        t,
        ExprType::Primitive(
            PrimitiveKind::Date | PrimitiveKind::DateTimeOffset | PrimitiveKind::Duration
        )
    )
}
