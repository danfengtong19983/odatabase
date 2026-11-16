//! `POST`, `PATCH`, `PUT` and `DELETE`.

use crate::change::{ChangeContext, EntityKey};
use crate::format::Format;
use crate::payload::{
    DeltaItem, EntityPayload, PropertyValue, parse_delta, parse_entity, read_property,
};
use crate::read::{etag_matches, language, split_entity_path};
use crate::url::{entity_id, key_text as url_key_text, set_of};
use crate::{ODataError, ODataRequest, ODataResponse, Service};
use chrono::Utc;
use futures::future::BoxFuture;
use http::{HeaderValue, Method, StatusCode, header};
use odatabase_edm::{
    EntityType, Inheritance, Model, NavigationProperty, PrimitiveKind, Property, TypeKind, Value,
};
use odatabase_sql::{
    ChangeKind, Claims, ColumnValue, ColumnWrite, Page, ReadRequest, Session, collection_positions,
    delete as sql_delete, insert as sql_insert, open_position, read_columns, read_entities,
    update as sql_update,
};
use odatabase_uri::{ExpandItem, ODataUri, QueryOptions, Segment, Target};
use serde_json::Value as Json;

/// A `Prefer: return=` choice.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Return {
    Default,
    Minimal,
    Representation,
}

fn prefer_return(req: &ODataRequest) -> Return {
    let text: Vec<&str> = req
        .headers
        .get_all("prefer")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .collect();
    if text.contains(&"return=minimal") {
        Return::Minimal
    } else if text.contains(&"return=representation") {
        Return::Representation
    } else {
        Return::Default
    }
}

fn with_preference(mut r: ODataResponse, pref: Return) -> ODataResponse {
    let v = match pref {
        Return::Default => return r,
        Return::Minimal => "return=minimal",
        Return::Representation => "return=representation",
    };
    r.headers
        .insert("preference-applied", HeaderValue::from_static(v));
    r
}

/// Tables of an entity type, and which holds each of its properties.
struct Layout<'m> {
    root: String,
    own: Vec<&'m EntityType>,
}

fn layout<'m>(model: &'m Model, type_name: &str) -> Result<Layout<'m>, ODataError> {
    let chain = model.type_chain(type_name);
    let root = chain
        .iter()
        .rev()
        .find(|t| !t.table.is_empty())
        .map(|t| t.table.clone())
        .ok_or_else(|| ODataError::new(500, format!("{type_name} has no table")))?;
    Ok(Layout { root, own: chain })
}

/// The table holding `owner`'s columns, for `Joined` types their own table.
fn table_of<'t>(l: &'t Layout<'_>, owner: &'t EntityType) -> &'t str {
    match owner.inheritance {
        Some(Inheritance::Joined { .. }) => &owner.table,
        _ => &l.root,
    }
}

/// Column assignments per table, in hierarchy order.
#[derive(Default)]
struct TableWrites {
    tables: Vec<(String, Vec<ColumnWrite>)>,
}

impl TableWrites {
    fn push(&mut self, table: &str, write: ColumnWrite) {
        match self.tables.iter_mut().find(|(t, _)| t == table) {
            Some((_, list)) => list.push(write),
            None => self.tables.push((table.to_owned(), vec![write])),
        }
    }

    fn columns(&mut self, table: &str) -> &mut Vec<ColumnWrite> {
        if !self.tables.iter().any(|(t, _)| t == table) {
            self.tables.push((table.to_owned(), Vec::new()));
        }
        let i = self
            .tables
            .iter()
            .position(|(t, _)| t == table)
            .expect("just added");
        &mut self.tables[i].1
    }
}

/// Writes a value tree as columns. With `reset`, absent complex members become null.
fn flatten(
    model: &Model,
    p: &Property,
    prefix: &str,
    value: &PropertyValue,
    reset: bool,
    out: &mut Vec<ColumnWrite>,
) -> Result<(), ODataError> {
    match (value, &p.kind) {
        (PropertyValue::Value(v), _) => {
            out.push(ColumnWrite::new(
                format!("{prefix}{}", p.column),
                v.clone(),
                Some(p.kind.clone()),
            ));
        }
        (PropertyValue::Complex(fields), TypeKind::Complex(t)) => {
            let c = model
                .complex_type(t)
                .ok_or_else(|| ODataError::new(500, format!("unknown complex type {t}")))?;
            let inner_prefix = format!("{prefix}{}", p.column);
            for ip in c.properties.iter().filter(|ip| ip.collection.is_none()) {
                let given = fields
                    .as_ref()
                    .and_then(|f| f.iter().find(|(n, _)| *n == ip.name))
                    .map(|(_, v)| v);
                match given {
                    Some(v) => flatten(model, ip, &inner_prefix, v, reset, out)?,
                    None if fields.is_none() || reset => {
                        let nulled = match ip.kind {
                            TypeKind::Complex(_) => PropertyValue::Complex(None),
                            _ => PropertyValue::Value(Value::Null),
                        };
                        flatten(model, ip, &inner_prefix, &nulled, reset, out)?;
                    }
                    None => {}
                }
            }
        }
        _ => {
            return Err(ODataError::bad_request(format!(
                "invalid value for {}",
                p.name
            )));
        }
    }
    Ok(())
}

/// Whether the source table of `nav` holds the foreign key.
fn fk_on_source(model: &Model, owner: &str, nav: &NavigationProperty) -> bool {
    if nav.join_table.is_some() || nav.collection {
        return false;
    }
    let key_columns: Vec<&str> = model
        .key_of(owner)
        .iter()
        .filter_map(|k| model.property(owner, k))
        .map(|p| p.column.as_str())
        .collect();
    !nav.join
        .iter()
        .all(|p| key_columns.contains(&p.source.as_str()))
}

fn key_columns(model: &Model, type_name: &str) -> Result<Vec<String>, ODataError> {
    model
        .key_of(type_name)
        .iter()
        .map(|k| {
            model
                .property(type_name, k)
                .map(|p| p.column.clone())
                .ok_or_else(|| ODataError::new(500, format!("unknown key {k}")))
        })
        .collect()
}

fn key_segment(type_name: &str, key: &EntityKey) -> Segment {
    Segment::Key {
        values: key.clone(),
        target: Target::Entity(type_name.to_owned()),
    }
}

/// The column conditions for a key.
/// The columns naming one row. A key can be unique only within a tenant, so the tenant is one.
fn key_conditions(
    model: &Model,
    claims: &Claims,
    type_name: &str,
    key: &EntityKey,
) -> Result<Vec<ColumnValue>, ODataError> {
    let mut conditions = key
        .iter()
        .map(|(name, value)| {
            model
                .property(type_name, name)
                .map(|p| (p.column.clone(), value.clone()))
                .ok_or_else(|| ODataError::new(500, format!("unknown key {name}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(tenant) = model.tenant_of(type_name) {
        conditions.push((tenant.column.clone(), claims.writing_tenant(tenant)?));
    }
    Ok(conditions)
}

/// The owner path, owner type, navigation and keyed segment of a `$ref` path.
type RefTarget<'u> = (
    &'u [Segment],
    String,
    &'u NavigationProperty,
    Option<&'u Segment>,
);

/// What to fill in on a row being created below a parent.
type ParentColumns = Vec<(String, Value)>;

mod deep;
mod delete;
mod insert;
mod items;
mod links;
mod properties;
mod update;

impl Service {
    fn change_context<'a>(
        &'a self,
        req: &'a ODataRequest,
        session: &'a Session,
    ) -> ChangeContext<'a> {
        ChangeContext {
            model: &self.inner.model,
            claims: &req.claims,
            session,
            method: &req.method,
        }
    }

    /// Runs a modifying request in its own transaction.
    pub(crate) async fn modify(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
    ) -> Result<ODataResponse, ODataError> {
        let session = self.begin(req).await?;
        let outcome = async {
            let response = self.modify_in(req, uri, &session).await?;
            self.inner
                .changes
                .validate_changes(&self.change_context(req, &session))
                .await?;
            Ok::<_, ODataError>(response)
        }
        .await;
        match outcome {
            Ok(r) => {
                session.commit().await?;
                Ok(r)
            }
            Err(e) => {
                session.rollback().await?;
                Err(e)
            }
        }
    }

    /// Applies a modifying request on `session`. The caller commits.
    pub(crate) async fn modify_in(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        if matches!(uri.target(), Target::Ref { .. }) {
            // A link changes the navigation of its owner, like `Nav@odata.bind` in a PATCH,
            // and creates or deletes no target entity. Writes to a target row (a foreign key
            // it holds) are checked where they happen.
            let (_, owner, _, _) = self.ref_target(uri)?;
            crate::capabilities::check_write(&self.inner.model, &owner, &Method::PATCH)?;
        } else if let Some(t) = changed_type(uri) {
            crate::capabilities::check_write(&self.inner.model, t, &req.method)?;
        }
        let item_insert = req.method == Method::POST
            && matches!(
                uri.target(),
                Target::PrimitiveCollection(_) | Target::ComplexCollection(_)
            );
        if uri.options.index.is_some() && !item_insert {
            return Err(ODataError::bad_request(
                "$index needs a POST to a collection property",
            ));
        }
        match (&req.method, uri.target()) {
            (&Method::POST, Target::ActionResult) => self.call_action(req, uri, session).await,
            (&Method::PATCH | &Method::PUT | &Method::DELETE, Target::RawValue)
                if self.is_media_path(uri) =>
            {
                self.write_media(req, uri, session).await
            }
            (&Method::POST, Target::EntityCollection(t)) => self.create(req, uri, t, session).await,
            (&Method::POST, Target::Ref { .. }) => self.add_link(req, uri, session).await,
            (&Method::POST, Target::PrimitiveCollection(_) | Target::ComplexCollection(_)) => {
                self.insert_item(req, uri, session).await
            }
            (&Method::PATCH, Target::EntityCollection(t)) => {
                self.patch_collection(req, uri, t, session).await
            }
            (&Method::PATCH | &Method::PUT, Target::Entity(t)) => {
                self.replace_entity(req, uri, t, session).await
            }
            (&Method::PATCH | &Method::PUT, Target::Ref { .. }) => {
                self.set_link(req, uri, session).await
            }
            (
                &Method::PATCH | &Method::PUT,
                Target::Primitive(_)
                | Target::Complex(_)
                | Target::PrimitiveCollection(_)
                | Target::ComplexCollection(_)
                | Target::RawValue,
            ) => self.update_property(req, uri, session).await,
            (&Method::DELETE, Target::Entity(t)) => self.delete_entity(req, uri, t, session).await,
            (&Method::DELETE, Target::Ref { .. }) => self.remove_link(req, uri, session).await,
            (
                &Method::DELETE,
                Target::Primitive(_)
                | Target::Complex(_)
                | Target::PrimitiveCollection(_)
                | Target::ComplexCollection(_)
                | Target::RawValue,
            ) => self.clear_property(req, uri, session).await,
            _ => Err(ODataError::new(
                405,
                format!("{} is not allowed here", req.method),
            )),
        }
    }

    async fn column_values(
        &self,
        req: &ODataRequest,
        session: &Session,
        segments: &[Segment],
        type_name: &str,
        columns: &[String],
    ) -> Result<Option<Vec<Value>>, ODataError> {
        let lang = language(req, &self.inner.config.default_language);
        let ctx = self.query_context(req, &lang, session);
        Ok(read_columns(&ctx, session, segments, type_name, columns).await?)
    }
}

/// The tenant as a condition on a collection or join table, for an owner that has one.
fn side_table_key(
    model: &Model,
    claims: &Claims,
    owner: &str,
    column: Option<&String>,
) -> Result<Option<ColumnValue>, ODataError> {
    let (Some(column), Some(tenant)) = (column, model.tenant_of(owner)) else {
        return Ok(None);
    };
    Ok(Some((column.clone(), claims.writing_tenant(tenant)?)))
}

/// The tenant of a row in a collection or join table, for an owner that has one.
fn tenant_cell(
    model: &Model,
    claims: &Claims,
    owner: &str,
    column: Option<&String>,
) -> Result<Option<ColumnWrite>, ODataError> {
    let (Some(column), Some(tenant)) = (column, model.tenant_of(owner)) else {
        return Ok(None);
    };
    let kind = TypeKind::Primitive(tenant.kind);
    let value = claims.writing_tenant(tenant)?;
    Ok(Some(ColumnWrite::new(column.clone(), value, Some(kind))))
}

/// Key conditions for `table`: a `Joined` table holds the key under its own column names.
/// With a tenant, `key` holds the tenant condition too, which maps to the joined table's own
/// tenant column.
fn table_conditions(
    model: &Model,
    l: &Layout<'_>,
    table: &str,
    key: &[ColumnValue],
) -> Vec<ColumnValue> {
    let joined = l
        .own
        .iter()
        .filter(|t| t.table == table)
        .find_map(|t| model.joined_columns(t));
    let Some(join) = joined else {
        return key.to_vec();
    };
    join.iter()
        .filter_map(|p| {
            let (_, value) = key.iter().find(|(c, _)| *c == p.source)?;
            Some((p.target.clone(), value.clone()))
        })
        .collect()
}

/// The entity type whose `Capabilities` restrictions apply to a change.
fn changed_type(uri: &ODataUri) -> Option<&String> {
    let (entities, _) = split_entity_path(&uri.path);
    match entities.last().map(Segment::target) {
        Some(Target::Entity(t) | Target::EntityCollection(t)) => Some(t),
        _ => None,
    }
}

/// The key in the URL of an upsert. Only plain `Set(key)` paths can create an entity.
fn upsert_key(uri: &ODataUri) -> Option<&Vec<(String, Value)>> {
    let plain = uri.path.iter().all(|s| {
        matches!(
            s,
            Segment::EntitySet { .. } | Segment::Cast { .. } | Segment::Key { .. }
        )
    });
    match uri.path.last() {
        Some(Segment::Key { values, .. }) if plain => Some(values),
        _ => None,
    }
}

fn same_key_value(model: &Model, type_name: &str, name: &str, given: &Value, key: &Value) -> bool {
    given == key
        || model.property(type_name, name).is_some_and(
            |p| matches!(p.kind, TypeKind::Primitive(k) if given.coerce(k).as_ref() == Some(key)),
        )
}

/// The key of a nested entity, from its own properties and those a parent fills in.
fn child_key(model: &Model, child: &EntityPayload, parent: &ParentColumns) -> Option<EntityKey> {
    let t = &child.type_name;
    model
        .key_of(t)
        .iter()
        .map(|name| {
            let p = model.property(t, name)?;
            let TypeKind::Primitive(kind) = p.kind else {
                return None;
            };
            let from_body = match child.value(name) {
                Some(PropertyValue::Value(v)) => Some(v.clone()),
                _ => None,
            };
            let from_parent = parent
                .iter()
                .find(|(c, _)| *c == p.column)
                .map(|(_, v)| v.clone());
            Some((name.clone(), fit(&from_parent.or(from_body)?, kind)))
        })
        .collect()
}

fn key_text(key: &EntityKey) -> String {
    key.iter()
        .map(|(_, v)| v.to_text())
        .collect::<Vec<_>>()
        .join("\u{1}")
}

/// The declared type of a column, so that a null is bound with the right type.
fn column_kind(model: &Model, type_name: &str, column: &str) -> Option<TypeKind> {
    model
        .all_properties(type_name)
        .iter()
        .find(|p| p.column == column)
        .map(|p| p.kind.clone())
}

fn has_protection(model: &Model, p: &Property) -> bool {
    if !p.protections.is_empty() {
        return true;
    }
    match &p.kind {
        TypeKind::Complex(t) => model
            .complex_type(t)
            .is_some_and(|c| c.properties.iter().any(|q| has_protection(model, q))),
        _ => false,
    }
}

fn not_accessible() -> ODataError {
    ODataError::new(403, "the created entity is not accessible")
}

/// Whether rows of a type can be hidden from the caller. Such types never upsert: a hidden row
/// looks missing, so answering differently for it and for a free key would reveal that it exists.
pub(crate) fn row_restricted(model: &Model, type_name: &str) -> bool {
    model
        .type_chain(type_name)
        .iter()
        .any(|t| t.query_extension || t.properties.iter().any(|p| has_protection(model, p)))
}

/// Rejects writes to properties and types hidden from the caller.
fn check_writable(
    model: &Model,
    claims: &odatabase_sql::Claims,
    payload: &EntityPayload,
) -> Result<(), ODataError> {
    for (name, _) in &payload.values {
        let hidden = model.property(&payload.type_name, name);
        if hidden.is_some_and(|p| !claims.sees(&p.visible_for)) {
            return Err(ODataError::new(403, format!("{name} is not accessible")));
        }
    }
    for t in model.type_chain(&payload.type_name) {
        if !claims.sees(&t.visible_for) {
            return Err(ODataError::new(
                403,
                format!("{} is not accessible", t.name),
            ));
        }
    }
    for (name, children) in &payload.nested {
        let nav = model.navigation(&payload.type_name, name);
        if nav.is_some_and(|n| !claims.sees(&n.visible_for)) {
            return Err(ODataError::new(403, format!("{name} is not accessible")));
        }
        for child in children {
            check_writable(model, claims, child)?;
        }
    }
    for (name, items) in &payload.deltas {
        let nav = model.navigation(&payload.type_name, name);
        if nav.is_some_and(|n| !claims.sees(&n.visible_for)) {
            return Err(ODataError::new(403, format!("{name} is not accessible")));
        }
        for item in items {
            check_delta_writable(model, claims, item)?;
        }
    }
    Ok(())
}

fn check_delta_writable(
    model: &Model,
    claims: &odatabase_sql::Claims,
    item: &DeltaItem,
) -> Result<(), ODataError> {
    match item {
        DeltaItem::Entity { entity, .. } | DeltaItem::Removed { entity, .. } => {
            check_writable(model, claims, entity)
        }
        DeltaItem::Link(_) => Ok(()),
    }
}

/// Columns that a parent, a link or a nested entity will supply.
fn filled_columns(
    model: &Model,
    payload: &EntityPayload,
    parent_columns: &ParentColumns,
) -> Vec<String> {
    let mut filled: Vec<String> = parent_columns.iter().map(|(c, _)| c.clone()).collect();
    let linked = payload
        .binds
        .iter()
        .map(|(n, _)| n)
        .chain(payload.nested.iter().map(|(n, _)| n));
    for name in linked {
        if let Some(nav) = model.navigation(&payload.type_name, name)
            && fk_on_source(model, &payload.type_name, nav)
        {
            filled.extend(nav.join.iter().map(|p| p.source.clone()));
        }
    }
    filled
}

/// `$expand` for the navigations a deep insert filled, so the response shows them.
fn expand_of(model: &Model, payload: &EntityPayload) -> QueryOptions {
    let mut options = QueryOptions::default();
    for (name, children) in &payload.nested {
        let (Some(child), Some(nav)) =
            (children.first(), model.navigation(&payload.type_name, name))
        else {
            continue;
        };
        options.expand.push(ExpandItem {
            prefix: Vec::new(),
            navigation: name.clone(),
            target: nav.target.clone(),
            collection: nav.collection,
            options: expand_of(model, child),
            levels: None,
            count_only: false,
            ref_only: false,
            from_star: false,
        });
    }
    options
}

fn unique(cols: &[ColumnWrite]) -> Vec<ColumnWrite> {
    let mut out: Vec<ColumnWrite> = Vec::with_capacity(cols.len());
    for c in cols {
        match out.iter_mut().find(|o| o.column == c.column) {
            Some(prev) => *prev = c.clone(),
            None => out.push(c.clone()),
        }
    }
    out
}

/// The first version of a new entity.
fn initial_version(p: &Property) -> Option<Value> {
    match &p.kind {
        TypeKind::Primitive(k) if k.is_integer() => Value::Int64(1).coerce(*k),
        TypeKind::Primitive(PrimitiveKind::DateTimeOffset) => {
            Some(Value::DateTimeOffset(Utc::now().fixed_offset()))
        }
        _ => None,
    }
}

/// Fits a natural database value to its declared kind, leaving it unchanged if it cannot.
fn fit(value: &Value, kind: PrimitiveKind) -> Value {
    if let Value::String(s) = value
        && kind != PrimitiveKind::String
        && let Ok(v) = odatabase_sql::from_sea_string(kind, s)
    {
        return v;
    }
    value.coerce(kind).unwrap_or_else(|| value.clone())
}
