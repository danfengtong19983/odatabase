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
        if let Some(t) = changed_type(uri) {
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

    async fn create(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        declared: &str,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        let format = Format::negotiate(uri.options.format.as_deref(), &req.headers)?;
        let mut payload = parse_entity(&self.inner.model, declared, &req.body)?;
        check_writable(&self.inner.model, &req.claims, &payload)?;
        self.inner
            .changes
            .before_create(&self.change_context(req, session), &mut payload)
            .await?;
        let model = &self.inner.model;
        let mut parent_columns = ParentColumns::new();
        let mut link = None;
        if let Some(i) = uri
            .path
            .iter()
            .rposition(|s| matches!(s, Segment::Navigation { .. }))
        {
            let Segment::Navigation { name, .. } = &uri.path[i] else {
                unreachable!("found by position");
            };
            let (owner_path, _) = uri.path.split_at(i);
            let owner = match owner_path.last().map(Segment::target) {
                Some(Target::Entity(t)) => t.clone(),
                _ => return Err(ODataError::bad_request("the parent needs a key")),
            };
            let nav = model
                .navigation(&owner, name)
                .ok_or_else(|| ODataError::new(500, format!("unknown navigation {name}")))?;
            if let Some(jt) = &nav.join_table {
                let cols: Vec<String> = jt.source.iter().map(|p| p.source.clone()).collect();
                let values = self
                    .column_values(req, session, owner_path, &owner, &cols)
                    .await?
                    .ok_or_else(|| ODataError::not_found("the parent does not exist"))?;
                let row: Vec<(String, Value)> = jt
                    .source
                    .iter()
                    .map(|p| p.target.clone())
                    .zip(values)
                    .collect();
                link = Some((jt, row));
            } else {
                let cols: Vec<String> = nav.join.iter().map(|p| p.source.clone()).collect();
                let values = self
                    .column_values(req, session, owner_path, &owner, &cols)
                    .await?
                    .ok_or_else(|| ODataError::not_found("the parent does not exist"))?;
                parent_columns = nav
                    .join
                    .iter()
                    .map(|p| p.target.clone())
                    .zip(values)
                    .collect();
            }
        }
        let created = self
            .insert_entity(req, session, &payload, parent_columns)
            .await?;
        if let Some((jt, mut row)) = link {
            let target_cols: Vec<String> = jt.target.iter().map(|p| p.target.clone()).collect();
            let segs = [key_segment(&payload.type_name, &created)];
            let values = self
                .column_values(req, session, &segs, &payload.type_name, &target_cols)
                .await?
                .ok_or_else(|| ODataError::new(500, "the created entity is missing"))?;
            row.extend(jt.target.iter().map(|p| p.source.clone()).zip(values));
            let cols: Vec<ColumnWrite> = row
                .into_iter()
                .map(|(c, v)| ColumnWrite::new(c, v, None))
                .collect();
            sql_insert(model, session, &jt.table, &cols, &[]).await?;
        }
        self.created_response(req, session, &payload, &created, declared, format)
            .await
    }

    /// The response to an insert: the new entity, or only its location.
    async fn created_response(
        &self,
        req: &ODataRequest,
        session: &Session,
        payload: &EntityPayload,
        created: &EntityKey,
        declared: &str,
        format: Format,
    ) -> Result<ODataResponse, ODataError> {
        let model = &self.inner.model;
        let options = expand_of(model, payload);
        let segs = [key_segment(&payload.type_name, created)];
        let rec = self
            .read_one(req, session, &segs, &payload.type_name, &options)
            .await?
            .ok_or_else(not_accessible)?;
        let id = entity_id(model, &payload.type_name, &rec).unwrap_or_default();
        let location = HeaderValue::try_from(format!("{}/{id}", req.root))
            .map_err(|_| ODataError::new(500, "invalid entity URL"))?;
        let pref = prefer_return(req);
        let mut r = if pref == Return::Minimal {
            let mut r = ODataResponse::new(StatusCode::NO_CONTENT);
            r.headers.insert("odata-entityid", location.clone());
            r
        } else {
            self.entity_response(req, &rec, declared, &options, format, StatusCode::CREATED)
        };
        r.headers.insert(header::LOCATION, location);
        Ok(with_preference(r, pref))
    }

    /// Inserts an entity with its collections, links and nested entities. Returns its key.
    fn insert_entity<'a>(
        &'a self,
        req: &'a ODataRequest,
        session: &'a Session,
        payload: &'a EntityPayload,
        parent_columns: ParentColumns,
    ) -> BoxFuture<'a, Result<EntityKey, ODataError>> {
        Box::pin(async move {
            let model = &self.inner.model;
            let type_name = &payload.type_name;
            let t = model
                .entity_type(type_name)
                .ok_or_else(|| ODataError::new(500, "unknown type"))?;
            if t.is_abstract {
                return Err(ODataError::bad_request(format!("{type_name} is abstract")));
            }
            if !payload.deltas.is_empty() {
                return Err(ODataError::bad_request(
                    "a delta payload needs an existing entity",
                ));
            }
            let l = layout(model, type_name)?;
            let mut writes = TableWrites::default();
            writes.columns(&l.root);
            if let Some(tenant) = model.tenant_of(type_name) {
                let value = req.claims.writing_tenant(tenant)?;
                let kind = TypeKind::Primitive(tenant.kind);
                writes.push(
                    &l.root,
                    ColumnWrite::new(tenant.column.clone(), value, Some(kind)),
                );
            }
            let restricted = row_restricted(model, type_name);
            if restricted && let Some(key) = child_key(model, payload, &parent_columns) {
                let segs = [key_segment(type_name, &key)];
                let columns = key_columns(model, type_name)?;
                let visible = self
                    .column_values(req, session, &segs, type_name, &columns)
                    .await?;
                if visible.is_some() {
                    return Err(ODataError::new(409, "the entity already exists"));
                }
            }
            let filled = filled_columns(model, payload, &parent_columns);
            let filled: Vec<&str> = filled.iter().map(String::as_str).collect();
            for owner in l.own.iter().rev() {
                let table = table_of(&l, owner).to_owned();
                match &owner.inheritance {
                    Some(Inheritance::SingleTable {
                        discriminator,
                        value,
                    })
                    | Some(Inheritance::Joined {
                        discriminator: Some((discriminator, value)),
                        ..
                    }) => {
                        writes.push(
                            &l.root,
                            ColumnWrite::new(
                                discriminator.clone(),
                                Value::String(value.clone()),
                                None,
                            ),
                        );
                    }
                    _ => {}
                }
                for p in &owner.properties {
                    if p.computed || p.transient.is_some() || p.description.is_some() {
                        continue;
                    }
                    if p.collection.is_some() {
                        continue;
                    }
                    let is_key = model.key_of(type_name).contains(&p.name);
                    match payload.value(&p.name) {
                        Some(v) => flatten(model, p, "", v, true, writes.columns(&table))?,
                        None if owner.etag.as_deref() == Some(p.name.as_str()) => {
                            if let Some(v) = initial_version(p) {
                                writes.push(
                                    &table,
                                    ColumnWrite::new(p.column.clone(), v, Some(p.kind.clone())),
                                );
                            }
                        }
                        // A required column can still come from a parent or a link.
                        None if !filled.contains(&p.column.as_str())
                            && (is_key || (!p.nullable && !p.is_complex())) =>
                        {
                            return Err(ODataError::bad_request(format!("{} is required", p.name)));
                        }
                        None => {}
                    }
                }
            }
            // Links whose foreign key is on this row must resolve before the insert.
            for (nav_name, urls) in &payload.binds {
                let nav = model
                    .navigation(type_name, nav_name)
                    .expect("checked by the parser");
                if fk_on_source(model, type_name, nav) {
                    let values = self.bind_target(req, session, nav, &urls[0]).await?;
                    for (pair, value) in nav.join.iter().zip(values) {
                        writes.push(&l.root, ColumnWrite::new(pair.source.clone(), value, None));
                    }
                }
            }
            for (nav_name, children) in &payload.nested {
                let nav = model
                    .navigation(type_name, nav_name)
                    .expect("checked by the parser");
                if fk_on_source(model, type_name, nav) {
                    let Some(child) = children.first() else {
                        continue;
                    };
                    let key = self.insert_child(req, session, child, Vec::new()).await?;
                    let segs = [key_segment(&child.type_name, &key)];
                    let cols: Vec<String> = nav.join.iter().map(|p| p.target.clone()).collect();
                    let values = self
                        .column_values(req, session, &segs, &child.type_name, &cols)
                        .await?
                        .ok_or_else(|| ODataError::new(500, "the nested entity is missing"))?;
                    for (pair, value) in nav.join.iter().zip(values) {
                        writes.push(&l.root, ColumnWrite::new(pair.source.clone(), value, None));
                    }
                }
            }

            // Last, so that the body cannot move the row to a different parent.
            for (column, value) in parent_columns {
                writes.push(&l.root, ColumnWrite::new(column, value, None));
            }
            let key_cols = key_columns(model, type_name)?;
            let mut returned = None;
            for (table, cols) in &writes.tables {
                if *table == l.root {
                    let cols = unique(cols);
                    let inserted = sql_insert(model, session, table, &cols, &key_cols)
                        .await
                        .map_err(ODataError::from)
                        .map_err(|e| match e.status {
                            // The caller sees no such row, so a collision is with a hidden one.
                            // It answers like any row outside the caller's scope.
                            409 if restricted => not_accessible(),
                            _ => e,
                        })?;
                    returned = Some(inserted);
                }
            }
            let returned = returned.ok_or_else(|| ODataError::new(500, "no root table"))?;
            let mut key: EntityKey = model
                .key_of(type_name)
                .iter()
                .cloned()
                .zip(returned)
                .collect();
            for (name, value) in &mut key {
                if let Some(TypeKind::Primitive(k)) =
                    model.property(type_name, name).map(|p| &p.kind)
                {
                    *value = fit(value, *k);
                }
            }
            let segs = [key_segment(type_name, &key)];
            for owner in l.own.iter().rev() {
                if let Some(Inheritance::Joined { join, .. }) = &owner.inheritance {
                    let cols = writes.columns(&owner.table);
                    for pair in join {
                        // Joined tables share the key of the root row.
                        let i =
                            key_cols
                                .iter()
                                .position(|k| *k == pair.source)
                                .ok_or_else(|| {
                                    ODataError::new(500, "a joined table must join on the key")
                                })?;
                        cols.push(ColumnWrite::new(
                            pair.target.clone(),
                            key[i].1.clone(),
                            None,
                        ));
                    }
                    let cols = cols.clone();
                    sql_insert(model, session, &owner.table, &cols, &[]).await?;
                }
            }
            for p in model.all_properties(type_name) {
                if let (Some(_), Some(PropertyValue::Collection(items))) =
                    (&p.collection, payload.value(&p.name))
                {
                    self.write_collection(req, session, type_name, p, &key, items, false)
                        .await?;
                }
            }
            for (nav_name, urls) in &payload.binds {
                let nav = model
                    .navigation(type_name, nav_name)
                    .expect("checked by the parser");
                if !fk_on_source(model, type_name, nav) {
                    for url in urls {
                        self.link_existing(req, session, &segs, type_name, nav, url)
                            .await?;
                    }
                }
            }
            for (nav_name, children) in &payload.nested {
                let nav = model
                    .navigation(type_name, nav_name)
                    .expect("checked by the parser");
                if fk_on_source(model, type_name, nav) {
                    continue;
                }
                for child in children {
                    if let Some(jt) = &nav.join_table {
                        let child_key = self.insert_child(req, session, child, Vec::new()).await?;
                        self.insert_link(
                            req,
                            session,
                            &segs,
                            type_name,
                            jt,
                            &child.type_name,
                            &child_key,
                        )
                        .await?;
                    } else {
                        let cols: Vec<String> = nav.join.iter().map(|p| p.source.clone()).collect();
                        let values = self
                            .column_values(req, session, &segs, type_name, &cols)
                            .await?
                            .ok_or_else(|| ODataError::new(500, "the entity is missing"))?;
                        let parent = nav
                            .join
                            .iter()
                            .map(|p| p.target.clone())
                            .zip(values)
                            .collect();
                        self.insert_child(req, session, child, parent).await?;
                    }
                }
            }
            self.record(req, session, type_name, &key, ChangeKind::Created)
                .await?;
            Ok(key)
        })
    }

    /// The target columns of a bound navigation.
    async fn bind_target(
        &self,
        req: &ODataRequest,
        session: &Session,
        nav: &NavigationProperty,
        url: &str,
    ) -> Result<Vec<Value>, ODataError> {
        let (segs, t) = self.parse_entity_url(req, url)?;
        let cols: Vec<String> = nav.join.iter().map(|p| p.target.clone()).collect();
        self.column_values(req, session, &segs, &t, &cols)
            .await?
            .ok_or_else(|| ODataError::bad_request(format!("{url} does not exist")))
    }

    /// Resolves an entity URL, absolute or relative to the service root.
    fn parse_entity_url(
        &self,
        req: &ODataRequest,
        url: &str,
    ) -> Result<(Vec<Segment>, String), ODataError> {
        let relative = url.strip_prefix(req.root.as_str()).unwrap_or(url);
        let uri = odatabase_uri::parse(&self.inner.model, relative, "")?;
        match uri.target().clone() {
            Target::Entity(t) => Ok((uri.path, t)),
            _ => Err(ODataError::bad_request(format!("{url} is not an entity"))),
        }
    }

    async fn link_existing(
        &self,
        req: &ODataRequest,
        session: &Session,
        source: &[Segment],
        source_type: &str,
        nav: &NavigationProperty,
        url: &str,
    ) -> Result<(), ODataError> {
        let model = &self.inner.model;
        let (segs, t) = self.parse_entity_url(req, url)?;
        if let Some(jt) = &nav.join_table {
            let target_key = self.entity_key(req, session, &segs, &t).await?;
            return self
                .insert_link(req, session, source, source_type, jt, &t, &target_key)
                .await;
        }
        // Linking writes the foreign key on the target row.
        crate::capabilities::check_write(model, &t, &Method::PATCH)?;
        let cols: Vec<String> = nav.join.iter().map(|p| p.source.clone()).collect();
        let values = self
            .column_values(req, session, source, source_type, &cols)
            .await?
            .ok_or_else(|| ODataError::not_found("the entity does not exist"))?;
        let sets: Vec<ColumnWrite> = nav
            .join
            .iter()
            .zip(values)
            .map(|(p, v)| ColumnWrite::new(p.target.clone(), v, None))
            .collect();
        let key = self.entity_key(req, session, &segs, &t).await?;
        let l = layout(model, &t)?;
        let conditions = key_conditions(model, &req.claims, &t, &key)?;
        sql_update(model, session, &l.root, &sets, None, &conditions).await?;
        self.record(req, session, &t, &key, ChangeKind::Updated)
            .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_link(
        &self,
        req: &ODataRequest,
        session: &Session,
        source: &[Segment],
        source_type: &str,
        jt: &odatabase_edm::JoinTable,
        target_type: &str,
        target_key: &EntityKey,
    ) -> Result<(), ODataError> {
        let cols: Vec<String> = jt.source.iter().map(|p| p.source.clone()).collect();
        let source_values = self
            .column_values(req, session, source, source_type, &cols)
            .await?
            .ok_or_else(|| ODataError::not_found("the entity does not exist"))?;
        let cols: Vec<String> = jt.target.iter().map(|p| p.target.clone()).collect();
        let segs = [key_segment(target_type, target_key)];
        let target_values = self
            .column_values(req, session, &segs, target_type, &cols)
            .await?
            .ok_or_else(|| ODataError::not_found("the target does not exist"))?;
        let mut row = Vec::new();
        for (p, v) in jt.source.iter().zip(source_values) {
            row.push(ColumnWrite::new(p.target.clone(), v, None));
        }
        for (p, v) in jt.target.iter().zip(target_values) {
            row.push(ColumnWrite::new(p.source.clone(), v, None));
        }
        row.extend(tenant_cell(
            &self.inner.model,
            &req.claims,
            source_type,
            jt.tenant_column.as_ref(),
        )?);
        sql_insert(&self.inner.model, session, &jt.table, &row, &[]).await?;
        Ok(())
    }

    async fn entity_key(
        &self,
        req: &ODataRequest,
        session: &Session,
        path: &[Segment],
        type_name: &str,
    ) -> Result<EntityKey, ODataError> {
        let model = &self.inner.model;
        let columns = key_columns(model, type_name)?;
        let values = self
            .column_values(req, session, path, type_name, &columns)
            .await?
            .ok_or_else(|| ODataError::not_found("the entity does not exist"))?;
        let mut key: EntityKey = model
            .key_of(type_name)
            .iter()
            .cloned()
            .zip(values)
            .collect();
        for (name, value) in &mut key {
            if let Some(TypeKind::Primitive(k)) = model.property(type_name, name).map(|p| &p.kind) {
                *value = fit(value, *k);
            }
        }
        Ok(key)
    }

    /// Replaces the child rows of a collection property.
    #[allow(clippy::too_many_arguments)]
    async fn write_collection(
        &self,
        req: &ODataRequest,
        session: &Session,
        type_name: &str,
        p: &Property,
        key: &EntityKey,
        items: &[PropertyValue],
        clear_first: bool,
    ) -> Result<(), ODataError> {
        let model = &self.inner.model;
        let c = p.collection.as_ref().expect("checked by the caller");
        let segs = [key_segment(type_name, key)];
        let owner_cols: Vec<String> = c.owner.iter().map(|o| o.source.clone()).collect();
        let owner_values = self
            .column_values(req, session, &segs, type_name, &owner_cols)
            .await?
            .ok_or_else(|| ODataError::not_found("the entity does not exist"))?;
        if clear_first {
            let mut conditions: Vec<ColumnValue> = c
                .owner
                .iter()
                .map(|o| o.target.clone())
                .zip(owner_values.iter().cloned())
                .collect();
            conditions.extend(side_table_key(
                model,
                &req.claims,
                type_name,
                c.tenant_column.as_ref(),
            )?);
            sql_delete(model, session, &c.table, &conditions).await?;
        }
        for (i, item) in items.iter().enumerate() {
            self.write_item(
                req,
                session,
                type_name,
                p,
                &owner_values,
                item,
                i as i32 + 1,
            )
            .await?;
        }
        Ok(())
    }

    /// Inserts one row of a collection property.
    #[allow(clippy::too_many_arguments)]
    async fn write_item(
        &self,
        req: &ODataRequest,
        session: &Session,
        type_name: &str,
        p: &Property,
        owner_values: &[Value],
        item: &PropertyValue,
        position: i32,
    ) -> Result<(), ODataError> {
        let model = &self.inner.model;
        let c = p.collection.as_ref().expect("checked by the caller");
        let mut cols: Vec<ColumnWrite> = c
            .owner
            .iter()
            .zip(owner_values)
            .map(|(o, v)| ColumnWrite::new(o.target.clone(), v.clone(), None))
            .collect();
        cols.extend(tenant_cell(
            model,
            &req.claims,
            type_name,
            c.tenant_column.as_ref(),
        )?);
        if let Some(order) = &c.order_column {
            cols.push(ColumnWrite::new(
                order.clone(),
                Value::Int32(position),
                Some(TypeKind::Primitive(PrimitiveKind::Int32)),
            ));
        }
        match item {
            PropertyValue::Value(v) => {
                let column = c.value_column.clone().unwrap_or_else(|| p.column.clone());
                cols.push(ColumnWrite::new(column, v.clone(), Some(p.kind.clone())));
            }
            complex => {
                let as_entity = Property {
                    column: String::new(),
                    ..p.clone()
                };
                flatten(model, &as_entity, "", complex, true, &mut cols)?;
            }
        }
        sql_insert(model, session, &c.table, &cols, &[]).await?;
        Ok(())
    }

    /// `POST` to a collection property: one more item, at `$index` if the collection is ordered.
    /// Negative values count from the end, so that -1 appends.
    async fn insert_item(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        let model = &self.inner.model;
        let (entity_path, t, names) = self.property_target(uri)?;
        self.check_path_visible(req, &t, &names)?;
        let (key, version) = self.locate(req, session, entity_path, &t).await?;
        let (_, leaf) = self.walk_property(&t, &names)?;
        let c = leaf
            .collection
            .as_ref()
            .ok_or_else(|| ODataError::bad_request("the property is not a collection"))?;
        let doc: Json = serde_json::from_slice(&req.body)
            .map_err(|e| ODataError::bad_request(format!("invalid JSON: {e}")))?;
        let item = match (doc.get("value"), matches!(leaf.kind, TypeKind::Complex(_))) {
            (Some(v), _) => v.clone(),
            (None, true) => doc,
            (None, false) => {
                return Err(ODataError::bad_request(
                    "expected a JSON object with a value member",
                ));
            }
        };
        let PropertyValue::Collection(mut items) =
            read_property(model, leaf, &Json::Array(vec![item]), 0)?
        else {
            return Err(ODataError::new(500, "the item did not parse as a list"));
        };
        let item = items
            .pop()
            .ok_or_else(|| ODataError::new(500, "the item did not parse"))?;
        let segs = [key_segment(&t, &key)];
        let owner_cols: Vec<String> = c.owner.iter().map(|o| o.source.clone()).collect();
        let owner_values = self
            .column_values(req, session, &segs, &t, &owner_cols)
            .await?
            .ok_or_else(|| ODataError::not_found("the entity does not exist"))?;
        let mut owner: Vec<ColumnValue> = c
            .owner
            .iter()
            .map(|o| o.target.clone())
            .zip(owner_values.iter().cloned())
            .collect();
        owner.extend(side_table_key(
            model,
            &req.claims,
            &t,
            c.tenant_column.as_ref(),
        )?);
        let position = match (&c.order_column, uri.options.index) {
            (Some(order), index) => {
                let used = collection_positions(model, session, &c.table, &owner, order).await?;
                let n = used.len() as i64;
                let at = match index {
                    Some(i) if i >= 0 => Ord::min(i, n),
                    Some(i) => Ord::max(n + i + 1, 0),
                    None => n,
                };
                match used.get(at as usize) {
                    Some(from) => {
                        open_position(model, session, &c.table, &owner, order, *from).await?;
                        *from
                    }
                    None => used.last().copied().unwrap_or(0) + 1,
                }
            }
            (None, Some(_)) => {
                return Err(ODataError::bad_request(format!(
                    "{} has no order",
                    leaf.name
                )));
            }
            (None, None) => 0,
        };
        self.write_item(
            req,
            session,
            &t,
            leaf,
            &owner_values,
            &item,
            position as i32,
        )
        .await?;
        self.apply_update(
            req,
            session,
            &t,
            &key,
            version,
            TableWrites::default(),
            false,
        )
        .await?;
        Ok(ODataResponse::new(StatusCode::NO_CONTENT))
    }

    /// Reads the row to change, checking existence and `If-Match`.
    pub(crate) async fn locate(
        &self,
        req: &ODataRequest,
        session: &Session,
        path: &[Segment],
        type_name: &str,
    ) -> Result<(EntityKey, Option<Value>), ODataError> {
        let key = self.entity_key(req, session, path, type_name).await?;
        let model = &self.inner.model;
        let Some(etag) = model.etag_property(type_name) else {
            return Ok((key, None));
        };
        let current = self
            .column_values(
                req,
                session,
                path,
                type_name,
                std::slice::from_ref(&etag.column),
            )
            .await?
            .and_then(|mut v| v.pop())
            .unwrap_or(Value::Null);
        if let TypeKind::Primitive(k) = &etag.kind
            && let Some(header_value) = req
                .headers
                .get(header::IF_MATCH)
                .and_then(|v| v.to_str().ok())
        {
            let tag = crate::json::etag_header(&fit(&current, *k));
            if !etag_matches(header_value, &tag) {
                return Err(ODataError::new(412, "the entity tag does not match"));
            }
        }
        Ok((key, Some(current)))
    }

    /// Applies column changes, bumping the version and guarding against concurrent writers.
    #[allow(clippy::too_many_arguments)]
    async fn apply_update(
        &self,
        req: &ODataRequest,
        session: &Session,
        type_name: &str,
        key: &EntityKey,
        version: Option<Value>,
        writes: TableWrites,
        guard: bool,
    ) -> Result<(), ODataError> {
        let model = &self.inner.model;
        let l = layout(model, type_name)?;
        let conditions = key_conditions(model, &req.claims, type_name, key)?;
        let etag = model.etag_property(type_name);
        let (bump, stamp) = match etag {
            Some(p) if matches!(p.kind, TypeKind::Primitive(k) if k.is_integer()) => {
                (Some(p.column.as_str()), None)
            }
            Some(p) if p.kind == TypeKind::Primitive(PrimitiveKind::DateTimeOffset) => (
                None,
                Some(ColumnWrite::new(
                    p.column.clone(),
                    Value::DateTimeOffset(Utc::now().fixed_offset()),
                    Some(p.kind.clone()),
                )),
            ),
            _ => (None, None),
        };
        let etag_column = etag.map(|p| p.column.clone());
        let etag_table = etag
            .and_then(|p| {
                l.own
                    .iter()
                    .find(|t| t.properties.iter().any(|q| q.name == p.name))
            })
            .map(|t| table_of(&l, t).to_owned());
        let mut tables = writes.tables;
        if let Some(t) = &etag_table
            && !tables.iter().any(|(n, _)| n == t)
        {
            tables.push((t.clone(), Vec::new()));
        }
        for (table, mut sets) in tables {
            let mut conds = table_conditions(&l, &table, &conditions);
            let is_etag_table = etag_table.as_deref() == Some(table.as_str());
            if is_etag_table {
                sets.extend(stamp.clone());
                if guard
                    && let (Some(c), Some(v)) = (&etag_column, &version)
                    && !v.is_null()
                {
                    conds.push((c.clone(), v.clone()));
                }
            }
            let bump_here = if is_etag_table { bump } else { None };
            let n = sql_update(model, session, &table, &sets, bump_here, &conds).await?;
            if n == 0 {
                return Err(ODataError::new(412, "the entity was changed concurrently"));
            }
        }
        self.record(req, session, type_name, key, ChangeKind::Updated)
            .await
    }

    async fn replace_entity(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        type_name: &str,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        let model = &self.inner.model;
        let format = Format::negotiate(uri.options.format.as_deref(), &req.headers)?;
        let put = req.method == Method::PUT;
        let columns = key_columns(model, type_name)?;
        let found = self
            .column_values(req, session, &uri.path, type_name, &columns)
            .await?
            .is_some();
        let insert_only = req
            .headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim() == "*");
        if found && insert_only {
            return Err(ODataError::new(412, "the entity already exists"));
        }
        if !found {
            return self
                .upsert(req, uri, type_name, session, insert_only, format)
                .await;
        }
        let (key, version) = self.locate(req, session, &uri.path, type_name).await?;
        let mut payload = parse_entity(model, type_name, &req.body)?;
        check_writable(model, &req.claims, &payload)?;
        self.inner
            .changes
            .before_update(&self.change_context(req, session), &key, &mut payload)
            .await?;
        let guarded = req.headers.contains_key(header::IF_MATCH);
        self.update_row(
            req, session, type_name, &key, version, &payload, put, guarded,
        )
        .await?;
        let mut options = uri.options.clone();
        for item in expand_of(model, &payload).expand {
            if !options
                .expand
                .iter()
                .any(|e| e.navigation == item.navigation)
            {
                options.expand.push(item);
            }
        }
        self.after_update(req, &options, session, type_name, &key, format)
            .await
    }

    /// PATCH or PUT on a key that does not exist creates the entity.
    async fn upsert(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        type_name: &str,
        session: &Session,
        insert_only: bool,
        format: Format,
    ) -> Result<ODataResponse, ODataError> {
        let model = &self.inner.model;
        if req.headers.contains_key(header::IF_MATCH) {
            return Err(ODataError::new(412, "the entity does not exist"));
        }
        let Some(values) = upsert_key(uri) else {
            return Err(ODataError::not_found("the entity does not exist"));
        };
        let generated = model
            .key_of(type_name)
            .iter()
            .filter_map(|k| model.property(type_name, k))
            .any(|p| p.computed);
        if generated || row_restricted(model, type_name) {
            return Err(ODataError::not_found("the entity does not exist"));
        }
        crate::capabilities::check_write(model, type_name, &Method::POST)?;
        let mut payload = parse_entity(model, type_name, &req.body)?;
        check_writable(model, &req.claims, &payload)?;
        for (name, value) in values {
            match payload.value(name) {
                Some(PropertyValue::Value(v))
                    if same_key_value(model, type_name, name, v, value) => {}
                Some(_) => return Err(ODataError::bad_request("the key cannot be changed")),
                None => payload
                    .values
                    .push((name.clone(), PropertyValue::Value(value.clone()))),
            }
        }
        self.inner
            .changes
            .before_create(&self.change_context(req, session), &mut payload)
            .await?;
        let created = self
            .insert_entity(req, session, &payload, Vec::new())
            .await
            .map_err(|e| match e.status {
                // Another request created it first.
                409 if insert_only => ODataError::new(412, "the entity already exists"),
                _ => e,
            })?;
        self.created_response(req, session, &payload, &created, type_name, format)
            .await
    }

    /// Applies a body to an existing row, including related entities given inline.
    #[allow(clippy::too_many_arguments)]
    fn update_row<'a>(
        &'a self,
        req: &'a ODataRequest,
        session: &'a Session,
        type_name: &'a str,
        key: &'a EntityKey,
        version: Option<Value>,
        payload: &'a EntityPayload,
        put: bool,
        guarded: bool,
    ) -> BoxFuture<'a, Result<(), ODataError>> {
        Box::pin(
            self.update_row_inner(req, session, type_name, key, version, payload, put, guarded),
        )
    }

    #[allow(clippy::too_many_arguments)]
    async fn update_row_inner(
        &self,
        req: &ODataRequest,
        session: &Session,
        type_name: &str,
        key: &EntityKey,
        version: Option<Value>,
        payload: &EntityPayload,
        put: bool,
        guarded: bool,
    ) -> Result<(), ODataError> {
        let model = &self.inner.model;
        let l = layout(model, type_name)?;
        let mut writes = TableWrites::default();
        let concrete = &payload.type_name;
        for (name, value) in key {
            if let Some(PropertyValue::Value(v)) = payload.value(name)
                && !same_key_value(model, type_name, name, v, value)
            {
                return Err(ODataError::bad_request("the key cannot be changed"));
            }
        }
        for owner in l.own.iter().rev() {
            let table = table_of(&l, owner).to_owned();
            for p in &owner.properties {
                let is_key = model.key_of(concrete).contains(&p.name);
                let is_etag = owner.etag.as_deref() == Some(p.name.as_str());
                if is_key
                    || is_etag
                    || p.computed
                    || p.transient.is_some()
                    || p.description.is_some()
                    || p.collection.is_some()
                {
                    continue;
                }
                match payload.value(&p.name) {
                    Some(v) => flatten(model, p, "", v, put, writes.columns(&table))?,
                    None if put && p.nullable => {
                        let nulled = match p.kind {
                            TypeKind::Complex(_) => PropertyValue::Complex(None),
                            _ => PropertyValue::Value(Value::Null),
                        };
                        flatten(model, p, "", &nulled, true, writes.columns(&table))?;
                    }
                    None if put => {
                        return Err(ODataError::bad_request(format!("{} is required", p.name)));
                    }
                    None => {}
                }
            }
        }
        for (nav_name, urls) in &payload.binds {
            let nav = model
                .navigation(concrete, nav_name)
                .expect("checked by the parser");
            if fk_on_source(model, concrete, nav) {
                let values = self.bind_target(req, session, nav, &urls[0]).await?;
                for (pair, value) in nav.join.iter().zip(values) {
                    writes.push(&l.root, ColumnWrite::new(pair.source.clone(), value, None));
                }
            }
        }
        // Foreign keys on this row have to be known before it is written.
        let segs = [key_segment(type_name, key)];
        for (nav_name, children) in &payload.nested {
            let nav = model
                .navigation(concrete, nav_name)
                .expect("checked by the parser");
            if fk_on_source(model, concrete, nav) {
                let target_values = self
                    .sync_source_link(req, session, &segs, nav, children, put)
                    .await?;
                for (pair, value) in nav.join.iter().zip(target_values) {
                    let kind = column_kind(model, type_name, &pair.source);
                    writes.push(&l.root, ColumnWrite::new(pair.source.clone(), value, kind));
                }
            }
        }
        self.apply_update(req, session, type_name, key, version, writes, guarded)
            .await?;
        for p in model.all_properties(type_name) {
            if let (Some(_), Some(PropertyValue::Collection(items))) =
                (&p.collection, payload.value(&p.name))
            {
                self.write_collection(req, session, type_name, p, key, items, true)
                    .await?;
            }
        }
        for (nav_name, urls) in &payload.binds {
            let nav = model
                .navigation(concrete, nav_name)
                .expect("checked by the parser");
            if !fk_on_source(model, concrete, nav) {
                for url in urls {
                    self.link_existing(req, session, &segs, type_name, nav, url)
                        .await?;
                }
            }
        }
        for (nav_name, children) in &payload.nested {
            let nav = model
                .navigation(concrete, nav_name)
                .expect("checked by the parser");
            if !fk_on_source(model, concrete, nav) {
                self.sync_target_links(req, session, &segs, type_name, nav, children, put)
                    .await?;
            }
        }
        for (nav_name, items) in &payload.deltas {
            let nav = model
                .navigation(concrete, nav_name)
                .expect("checked by the parser");
            self.apply_delta(req, session, &segs, type_name, nav, items)
                .await?;
        }
        Ok(())
    }

    /// The columns a row takes from its parent through `nav`.
    async fn parent_columns(
        &self,
        req: &ODataRequest,
        session: &Session,
        parent: &[Segment],
        parent_type: &str,
        nav: &NavigationProperty,
    ) -> Result<ParentColumns, ODataError> {
        if nav.join_table.is_some() {
            return Ok(Vec::new());
        }
        let columns: Vec<String> = nav.join.iter().map(|p| p.source.clone()).collect();
        let values = self
            .column_values(req, session, parent, parent_type, &columns)
            .await?
            .ok_or_else(|| ODataError::not_found("the entity does not exist"))?;
        Ok(nav
            .join
            .iter()
            .map(|p| p.target.clone())
            .zip(values)
            .collect())
    }

    /// The key an `@id` names. It must be of `declared` or a type derived from it.
    fn key_of_url(
        &self,
        req: &ODataRequest,
        url: &str,
        declared: &str,
    ) -> Result<EntityKey, ODataError> {
        let model = &self.inner.model;
        let (segs, t) = self.parse_entity_url(req, url)?;
        if !model.type_chain(&t).iter().any(|b| b.name == declared) {
            return Err(ODataError::bad_request(format!(
                "{url} is not a {declared}"
            )));
        }
        match segs.as_slice() {
            [_, Segment::Key { values, .. }] | [Segment::Key { values, .. }] => model
                .key_of(&t)
                .iter()
                .map(|k| {
                    let found = values.iter().find(|(n, _)| n == k);
                    found
                        .cloned()
                        .ok_or_else(|| ODataError::bad_request(format!("{url} does not name {k}")))
                })
                .collect(),
            _ => Err(ODataError::bad_request(format!(
                "{url} is not a canonical URL"
            ))),
        }
    }

    /// The entity of a delta entry, with the key its `@id` gives.
    fn delta_entity(
        &self,
        req: &ODataRequest,
        id: Option<&String>,
        entity: &EntityPayload,
        declared: &str,
    ) -> Result<EntityPayload, ODataError> {
        let model = &self.inner.model;
        let mut entity = entity.clone();
        let Some(url) = id else {
            return Ok(entity);
        };
        for (name, value) in self.key_of_url(req, url, declared)? {
            match entity.value(&name) {
                Some(PropertyValue::Value(v))
                    if same_key_value(model, declared, &name, v, &value) => {}
                Some(_) => return Err(ODataError::bad_request("@id and the key disagree")),
                None => entity.values.push((name, PropertyValue::Value(value))),
            }
        }
        Ok(entity)
    }

    /// Applies `Nav@delta`. Members the payload doesn't name stay as they are, and removing an
    /// entity that isn't a member does nothing: entries may arrive twice.
    async fn apply_delta(
        &self,
        req: &ODataRequest,
        session: &Session,
        parent: &[Segment],
        parent_type: &str,
        nav: &NavigationProperty,
        items: &[DeltaItem],
    ) -> Result<(), ODataError> {
        let model = &self.inner.model;
        let mut current = self.related_keys(req, session, parent, nav).await?;
        let direct = self
            .parent_columns(req, session, parent, parent_type, nav)
            .await?;
        let member = |current: &[EntityKey], key: &EntityKey| {
            current.iter().any(|k| key_text(k) == key_text(key))
        };
        for item in items {
            match item {
                DeltaItem::Entity { id, entity } => {
                    let child = self.delta_entity(req, id.as_ref(), entity, &nav.target)?;
                    let key = self
                        .write_child(req, session, &child, direct.clone(), false)
                        .await?;
                    if !member(&current, &key) {
                        if let Some(jt) = &nav.join_table {
                            self.insert_link(
                                req,
                                session,
                                parent,
                                parent_type,
                                jt,
                                &child.type_name,
                                &key,
                            )
                            .await?;
                        }
                        current.push(key);
                    }
                }
                DeltaItem::Link(url) => {
                    let key = self.key_of_url(req, url, &nav.target)?;
                    if !member(&current, &key) {
                        self.link_existing(req, session, parent, parent_type, nav, url)
                            .await?;
                        current.push(key);
                    }
                }
                DeltaItem::Removed {
                    id,
                    entity,
                    deleted,
                } => {
                    let named = self.delta_entity(req, id.as_ref(), entity, &nav.target)?;
                    let key = child_key(model, &named, &direct).ok_or_else(|| {
                        ODataError::bad_request("a removed entry needs an @id or a key")
                    })?;
                    if !member(&current, &key) {
                        continue;
                    }
                    if *deleted {
                        self.delete_listed(req, session, &nav.target, &key).await?;
                    } else {
                        self.remove_related(req, session, parent, parent_type, nav, &key)
                            .await?;
                    }
                    current.retain(|k| key_text(k) != key_text(&key));
                }
            }
        }
        Ok(())
    }

    /// Deletes an entity a delta payload named.
    async fn delete_listed(
        &self,
        req: &ODataRequest,
        session: &Session,
        type_name: &str,
        key: &EntityKey,
    ) -> Result<(), ODataError> {
        crate::capabilities::check_write(&self.inner.model, type_name, &Method::DELETE)?;
        self.inner
            .changes
            .before_delete(&self.change_context(req, session), type_name, key)
            .await?;
        self.delete_row(req, session, type_name, key).await
    }

    /// `PATCH` on a collection: a delta payload, applied as one change.
    async fn patch_collection(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        type_name: &str,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        let model = &self.inner.model;
        let format = Format::negotiate(uri.options.format.as_deref(), &req.headers)?;
        let items = parse_delta(model, type_name, &req.body)?;
        for item in &items {
            check_delta_writable(model, &req.claims, item)?;
        }
        if let Some((Segment::Navigation { name, .. }, owner_path)) = uri.path.split_last() {
            let owner = match owner_path.last().map(Segment::target) {
                Some(Target::Entity(t)) => t.clone(),
                _ => return Err(ODataError::bad_request("the parent needs a key")),
            };
            let nav = model
                .navigation(&owner, name)
                .ok_or_else(|| ODataError::new(500, format!("unknown navigation {name}")))?;
            if !req.claims.sees(&nav.visible_for) {
                return Err(ODataError::new(403, format!("{name} is not accessible")));
            }
            self.apply_delta(req, session, owner_path, &owner, nav, &items)
                .await?;
            return Ok(ODataResponse::new(StatusCode::NO_CONTENT));
        }
        if !matches!(uri.path.as_slice(), [Segment::EntitySet { .. }]) {
            return Err(ODataError::new(405, "PATCH needs an entity set"));
        }
        let mut value = Vec::with_capacity(items.len());
        let represent = prefer_return(req) == Return::Representation;
        let set = set_of(model, type_name)
            .ok_or_else(|| ODataError::new(500, format!("{type_name} has no entity set")))?;
        for item in &items {
            match item {
                DeltaItem::Entity { id, entity } => {
                    let child = self.delta_entity(req, id.as_ref(), entity, type_name)?;
                    let key = self
                        .write_child(req, session, &child, Vec::new(), false)
                        .await?;
                    if represent {
                        let segs = [key_segment(&child.type_name, &key)];
                        let rec = self
                            .read_one(
                                req,
                                session,
                                &segs,
                                &child.type_name,
                                &QueryOptions::default(),
                            )
                            .await?
                            .ok_or_else(not_accessible)?;
                        let w = crate::json::Writer {
                            model,
                            root: &req.root,
                            format,
                        };
                        value.push(Json::Object(w.entity(&rec, type_name)));
                    }
                }
                DeltaItem::Link(url) => {
                    return Err(ODataError::bad_request(format!(
                        "{url} has nothing to link to"
                    )));
                }
                DeltaItem::Removed { id, entity, .. } => {
                    let named = self.delta_entity(req, id.as_ref(), entity, type_name)?;
                    let key = child_key(model, &named, &Vec::new()).ok_or_else(|| {
                        ODataError::bad_request("a removed entry needs an @id or a key")
                    })?;
                    let segs = [key_segment(type_name, &key)];
                    let columns = key_columns(model, type_name)?;
                    let visible = self
                        .column_values(req, session, &segs, type_name, &columns)
                        .await?;
                    if visible.is_some() {
                        self.delete_listed(req, session, type_name, &key).await?;
                    }
                    if represent {
                        value.push(self.removed_entry(req, set, &url_key_text(&key), "deleted"));
                    }
                }
            }
        }
        if !represent {
            return Ok(with_preference(
                ODataResponse::new(StatusCode::NO_CONTENT),
                Return::Minimal,
            ));
        }
        let body = serde_json::json!({
            "@odata.context": format!("{}/$metadata#{set}/$delta", req.root),
            "value": value,
        });
        Ok(with_preference(
            ODataResponse::json(StatusCode::OK, format.content_type(), &body),
            Return::Representation,
        ))
    }

    /// The keys of the entities currently related through `nav`, as the caller may see them.
    async fn related_keys(
        &self,
        req: &ODataRequest,
        session: &Session,
        parent: &[Segment],
        nav: &NavigationProperty,
    ) -> Result<Vec<EntityKey>, ODataError> {
        let model = &self.inner.model;
        let target = match nav.collection {
            true => Target::EntityCollection(nav.target.clone()),
            false => Target::Entity(nav.target.clone()),
        };
        let mut segments = parent.to_vec();
        segments.push(Segment::Navigation {
            name: nav.name.clone(),
            target,
        });
        let options = QueryOptions {
            select: Some(Vec::new()),
            ..QueryOptions::default()
        };
        let lang = language(req, &self.inner.config.default_language);
        let ctx = self.query_context(req, &lang, session);
        let records = read_entities(
            &ctx,
            session,
            ReadRequest {
                segments: &segments,
                type_name: &nav.target,
                options: &options,
                page: Page::default(),
                limits: self.inner.config.expand_limits,
            },
        )
        .await?;
        records
            .iter()
            .map(|r| {
                model
                    .key_of(&nav.target)
                    .iter()
                    .map(|k| {
                        let v = r
                            .value(k)
                            .ok_or_else(|| ODataError::new(500, "no key value"))?;
                        Ok((k.clone(), v.clone()))
                    })
                    .collect()
            })
            .collect()
    }

    /// Updates a related entity that already exists.
    async fn update_child(
        &self,
        req: &ODataRequest,
        session: &Session,
        child: &EntityPayload,
        key: &EntityKey,
        put: bool,
    ) -> Result<(), ODataError> {
        let model = &self.inner.model;
        let method = if put { Method::PUT } else { Method::PATCH };
        crate::capabilities::check_write(model, &child.type_name, &method)?;
        let segs = [key_segment(&child.type_name, key)];
        let mut version = None;
        if let Some(p) = model.etag_property(&child.type_name) {
            let current = self
                .column_values(
                    req,
                    session,
                    &segs,
                    &child.type_name,
                    std::slice::from_ref(&p.column),
                )
                .await?
                .and_then(|mut v| v.pop())
                .unwrap_or(Value::Null);
            if let (Some(given), TypeKind::Primitive(k)) = (&child.etag, &p.kind)
                && !etag_matches(given, &crate::json::etag_header(&fit(&current, *k)))
            {
                return Err(ODataError::new(412, "the entity tag does not match"));
            }
            version = Some(current);
        }
        let mut payload = child.clone();
        self.inner
            .changes
            .before_update(&self.change_context(req, session), key, &mut payload)
            .await?;
        let guarded = child.etag.is_some();
        self.update_row(
            req,
            session,
            &child.type_name,
            key,
            version,
            &payload,
            put,
            guarded,
        )
        .await
    }

    /// Creates a related entity.
    async fn insert_child(
        &self,
        req: &ODataRequest,
        session: &Session,
        child: &EntityPayload,
        parent_columns: ParentColumns,
    ) -> Result<EntityKey, ODataError> {
        let model = &self.inner.model;
        crate::capabilities::check_write(model, &child.type_name, &Method::POST)?;
        let mut payload = child.clone();
        self.inner
            .changes
            .before_create(&self.change_context(req, session), &mut payload)
            .await?;
        self.insert_entity(req, session, &payload, parent_columns)
            .await
    }

    /// Updates the entity with the key a nested payload names, or creates it.
    async fn write_child(
        &self,
        req: &ODataRequest,
        session: &Session,
        child: &EntityPayload,
        parent_columns: ParentColumns,
        put: bool,
    ) -> Result<EntityKey, ODataError> {
        let model = &self.inner.model;
        let Some(key) = child_key(model, child, &parent_columns) else {
            return self.insert_child(req, session, child, parent_columns).await;
        };
        let columns = key_columns(model, &child.type_name)?;
        let segs = [key_segment(&child.type_name, &key)];
        let exists = self
            .column_values(req, session, &segs, &child.type_name, &columns)
            .await?
            .is_some();
        if !exists {
            if row_restricted(model, &child.type_name) {
                return Err(ODataError::not_found("a related entity does not exist"));
            }
            return self.insert_child(req, session, child, parent_columns).await;
        }
        self.update_child(req, session, child, &key, put).await?;
        // A child named here may belong to another parent so far.
        let moved: Vec<ColumnWrite> = parent_columns
            .iter()
            .filter(|(c, _)| !columns.contains(c))
            .map(|(c, v)| {
                ColumnWrite::new(
                    c.clone(),
                    v.clone(),
                    column_kind(model, &child.type_name, c),
                )
            })
            .collect();
        if !moved.is_empty() {
            let l = layout(model, &child.type_name)?;
            let conditions = key_conditions(model, &req.claims, &child.type_name, &key)?;
            sql_update(model, session, &l.root, &moved, None, &conditions).await?;
            self.record(req, session, &child.type_name, &key, ChangeKind::Updated)
                .await?;
        }
        Ok(key)
    }

    /// Applies a nested to-one entity whose foreign key is on the parent row, returning the
    /// values to store there. The previous target is left alone: it is only unlinked.
    async fn sync_source_link(
        &self,
        req: &ODataRequest,
        session: &Session,
        parent: &[Segment],
        nav: &NavigationProperty,
        children: &[EntityPayload],
        put: bool,
    ) -> Result<Vec<Value>, ODataError> {
        let model = &self.inner.model;
        let Some(child) = children.first() else {
            return Ok(vec![Value::Null; nav.join.len()]);
        };
        let key = match child_key(model, child, &ParentColumns::new()) {
            Some(_) => {
                self.write_child(req, session, child, Vec::new(), put)
                    .await?
            }
            None => match self.related_keys(req, session, parent, nav).await?.pop() {
                Some(current) => {
                    self.update_child(req, session, child, &current, put)
                        .await?;
                    current
                }
                None => self.insert_child(req, session, child, Vec::new()).await?,
            },
        };
        let columns: Vec<String> = nav.join.iter().map(|p| p.target.clone()).collect();
        let segs = [key_segment(&child.type_name, &key)];
        self.column_values(req, session, &segs, &child.type_name, &columns)
            .await?
            .ok_or_else(|| ODataError::new(500, "the related entity is missing"))
    }

    /// Makes the related entities of a navigation match the body. Those left out are unlinked,
    /// or deleted when they cannot exist without the parent.
    #[allow(clippy::too_many_arguments)]
    async fn sync_target_links(
        &self,
        req: &ODataRequest,
        session: &Session,
        parent: &[Segment],
        parent_type: &str,
        nav: &NavigationProperty,
        children: &[EntityPayload],
        put: bool,
    ) -> Result<(), ODataError> {
        let model = &self.inner.model;
        let current = self.related_keys(req, session, parent, nav).await?;
        let direct_columns = self
            .parent_columns(req, session, parent, parent_type, nav)
            .await?;
        let mut kept = Vec::with_capacity(children.len());
        for child in children {
            let keyless = child_key(model, child, &direct_columns).is_none();
            let key = match (keyless && !nav.collection, current.first()) {
                (true, Some(existing)) => {
                    self.update_child(req, session, child, existing, put)
                        .await?;
                    existing.clone()
                }
                _ => {
                    self.write_child(req, session, child, direct_columns.clone(), put)
                        .await?
                }
            };
            if let Some(jt) = &nav.join_table
                && !current.iter().any(|k| key_text(k) == key_text(&key))
            {
                self.insert_link(
                    req,
                    session,
                    parent,
                    parent_type,
                    jt,
                    &child.type_name,
                    &key,
                )
                .await?;
            }
            kept.push(key_text(&key));
        }
        for key in current.iter().filter(|k| !kept.contains(&key_text(k))) {
            self.remove_related(req, session, parent, parent_type, nav, key)
                .await?;
        }
        Ok(())
    }

    /// Ends one relationship of `nav`. Only a row that needs the parent is deleted.
    async fn remove_related(
        &self,
        req: &ODataRequest,
        session: &Session,
        parent: &[Segment],
        parent_type: &str,
        nav: &NavigationProperty,
        key: &EntityKey,
    ) -> Result<(), ODataError> {
        let model = &self.inner.model;
        let target = nav.target.as_str();
        let segs = [key_segment(target, key)];
        if let Some(jt) = &nav.join_table {
            let source: Vec<String> = jt.source.iter().map(|p| p.source.clone()).collect();
            let values = self
                .column_values(req, session, parent, parent_type, &source)
                .await?
                .ok_or_else(|| ODataError::not_found("the entity does not exist"))?;
            let mut conditions: Vec<ColumnValue> = jt
                .source
                .iter()
                .map(|p| p.target.clone())
                .zip(values)
                .collect();
            let columns: Vec<String> = jt.target.iter().map(|p| p.target.clone()).collect();
            let values = self
                .column_values(req, session, &segs, target, &columns)
                .await?
                .ok_or_else(|| ODataError::not_found("the related entity does not exist"))?;
            conditions.extend(jt.target.iter().map(|p| p.source.clone()).zip(values));
            conditions.extend(side_table_key(
                model,
                &req.claims,
                parent_type,
                jt.tenant_column.as_ref(),
            )?);
            sql_delete(model, session, &jt.table, &conditions).await?;
            return Ok(());
        }
        let key_cols = key_columns(model, target)?;
        let needs_parent = nav.join.iter().any(|p| {
            key_cols.contains(&p.target)
                || model
                    .all_properties(target)
                    .iter()
                    .any(|q| q.column == p.target && !q.nullable)
        });
        if needs_parent {
            crate::capabilities::check_write(model, target, &Method::DELETE)?;
            self.inner
                .changes
                .before_delete(&self.change_context(req, session), target, key)
                .await?;
            return self.delete_row(req, session, target, key).await;
        }
        crate::capabilities::check_write(model, target, &Method::PATCH)?;
        let sets: Vec<ColumnWrite> = nav
            .join
            .iter()
            .map(|p| {
                ColumnWrite::new(
                    p.target.clone(),
                    Value::Null,
                    column_kind(model, target, &p.target),
                )
            })
            .collect();
        let l = layout(model, target)?;
        let conditions = key_conditions(model, &req.claims, target, key)?;
        sql_update(model, session, &l.root, &sets, None, &conditions).await?;
        self.record(req, session, target, key, ChangeKind::Updated)
            .await
    }

    /// The response of an update: empty, unless a representation was asked for.
    async fn after_update(
        &self,
        req: &ODataRequest,
        options: &QueryOptions,
        session: &Session,
        type_name: &str,
        key: &EntityKey,
        format: Format,
    ) -> Result<ODataResponse, ODataError> {
        let pref = prefer_return(req);
        let segs = [key_segment(type_name, key)];
        let rec = self
            .read_one(req, session, &segs, type_name, options)
            .await?
            .ok_or_else(|| ODataError::new(403, "the changed entity is not accessible"))?;
        let mut r = if pref == Return::Representation {
            self.entity_response(req, &rec, type_name, options, format, StatusCode::OK)
        } else {
            ODataResponse::new(StatusCode::NO_CONTENT)
        };
        let w = crate::json::Writer {
            model: &self.inner.model,
            root: &req.root,
            format,
        };
        if let Some(tag) = w.etag(&rec).and_then(|t| HeaderValue::try_from(t).ok()) {
            r.headers.insert(header::ETAG, tag);
        }
        Ok(with_preference(r, pref))
    }

    /// True for `$value` of an entity rather than of a property.
    pub(crate) fn is_media_path(&self, uri: &ODataUri) -> bool {
        matches!(
            uri.path.iter().rev().nth(1).map(Segment::target),
            Some(Target::Entity(_))
        )
    }

    /// Stores or clears the stream of a media entity.
    async fn write_media(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        let model = &self.inner.model;
        let path = &uri.path[..uri.path.len() - 1];
        let Some(Target::Entity(t)) = path.last().map(Segment::target) else {
            return Err(ODataError::bad_request("$value needs an entity"));
        };
        let media = model
            .type_chain(t)
            .into_iter()
            .find_map(|e| e.media.as_ref())
            .ok_or_else(|| ODataError::bad_request(format!("{t} has no stream")))?;
        let (key, version) = self.locate(req, session, path, t).await?;
        let l = layout(model, t)?;
        let mut writes = TableWrites::default();
        let binary = TypeKind::Primitive(PrimitiveKind::Binary);
        let content = if req.method == Method::DELETE {
            Value::Null
        } else {
            Value::Binary(req.body.to_vec())
        };
        writes.push(
            &l.root,
            ColumnWrite::new(media.content_column.clone(), content, Some(binary)),
        );
        if let Some(column) = &media.content_type_column {
            let mime = req
                .headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map_or(Value::Null, |v| Value::String(v.to_owned()));
            let text = TypeKind::Primitive(PrimitiveKind::String);
            writes.push(&l.root, ColumnWrite::new(column.clone(), mime, Some(text)));
        }
        let guarded = req.headers.contains_key(header::IF_MATCH);
        self.apply_update(req, session, t, &key, version, writes, guarded)
            .await?;
        Ok(ODataResponse::new(StatusCode::NO_CONTENT))
    }

    /// Splits `.../Set(key)/A/B[/$value]` into the owner and the property names.
    fn property_target<'u>(
        &self,
        uri: &'u ODataUri,
    ) -> Result<(&'u [Segment], String, Vec<&'u str>), ODataError> {
        let path = match uri.path.last() {
            Some(Segment::Value) => &uri.path[..uri.path.len() - 1],
            _ => &uri.path[..],
        };
        let (entity_path, tail) = split_entity_path(path);
        let t = match entity_path.last().map(Segment::target) {
            Some(Target::Entity(t)) => t.clone(),
            _ => {
                return Err(ODataError::bad_request(
                    "the property needs a single entity",
                ));
            }
        };
        let names: Vec<&str> = tail
            .iter()
            .filter_map(|s| match s {
                Segment::Property { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        Ok((entity_path, t, names))
    }

    async fn update_property(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        let model = &self.inner.model;
        let (entity_path, t, names) = self.property_target(uri)?;
        self.check_path_visible(req, &t, &names)?;
        let (key, version) = self.locate(req, session, entity_path, &t).await?;
        let raw = matches!(uri.path.last(), Some(Segment::Value));
        let p = self.walk_property(&t, &names)?;
        let (prefix, leaf) = (p.0, p.1);
        if model.key_of(&t).iter().any(|k| names == [k.as_str()]) {
            return Err(ODataError::bad_request("the key cannot be changed"));
        }
        let value = if raw {
            let TypeKind::Primitive(k) = leaf.kind else {
                return Err(ODataError::bad_request("$value needs a primitive"));
            };
            let text = std::str::from_utf8(&req.body)
                .map_err(|_| ODataError::bad_request("the value is not text"))?;
            PropertyValue::Value(
                Value::parse_text(k, text).map_err(|e| ODataError::bad_request(e.to_string()))?,
            )
        } else {
            let doc: Json = serde_json::from_slice(&req.body)
                .map_err(|e| ODataError::bad_request(format!("invalid JSON: {e}")))?;
            let body = if matches!(leaf.kind, TypeKind::Complex(_)) && leaf.collection.is_none() {
                match doc {
                    Json::Object(mut o) if o.contains_key("value") => {
                        o.remove("value").unwrap_or(Json::Null)
                    }
                    other => other,
                }
            } else {
                doc.get("value").cloned().ok_or_else(|| {
                    ODataError::bad_request("expected a JSON object with a value member")
                })?
            };
            read_property(model, leaf, &body, 0)?
        };
        if let PropertyValue::Collection(items) = &value {
            self.write_collection(req, session, &t, leaf, &key, items, true)
                .await?;
            self.apply_update(
                req,
                session,
                &t,
                &key,
                version,
                TableWrites::default(),
                false,
            )
            .await?;
            return Ok(ODataResponse::new(StatusCode::NO_CONTENT));
        }
        let mut writes = TableWrites::default();
        let l = layout(model, &t)?;
        let owner = l
            .own
            .iter()
            .find(|o| o.properties.iter().any(|q| q.name == names[0]))
            .ok_or_else(|| ODataError::new(500, "unknown owner"))?;
        let table = table_of(&l, owner).to_owned();
        flatten(
            model,
            leaf,
            &prefix,
            &value,
            req.method == Method::PUT,
            writes.columns(&table),
        )?;
        let guarded = req.headers.contains_key(header::IF_MATCH);
        self.apply_update(req, session, &t, &key, version, writes, guarded)
            .await?;
        Ok(ODataResponse::new(StatusCode::NO_CONTENT))
    }

    /// The column prefix and declaration of a property path.
    fn walk_property(
        &self,
        type_name: &str,
        names: &[&str],
    ) -> Result<(String, &Property), ODataError> {
        let model = &self.inner.model;
        let first = names
            .first()
            .ok_or_else(|| ODataError::bad_request("no property"))?;
        let mut p = model
            .property(type_name, first)
            .ok_or_else(|| ODataError::bad_request(format!("unknown property {first}")))?;
        let mut prefix = String::new();
        for name in &names[1..] {
            let TypeKind::Complex(t) = &p.kind else {
                return Err(ODataError::bad_request(format!("{name} has no parent")));
            };
            prefix.push_str(&p.column);
            let c = model
                .complex_type(t)
                .ok_or_else(|| ODataError::new(500, format!("unknown complex type {t}")))?;
            p = c
                .properties
                .iter()
                .find(|i| i.name == *name)
                .ok_or_else(|| ODataError::bad_request(format!("unknown property {name}")))?;
        }
        Ok((prefix, p))
    }

    async fn clear_property(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        let model = &self.inner.model;
        let (entity_path, t, names) = self.property_target(uri)?;
        self.check_path_visible(req, &t, &names)?;
        let (key, version) = self.locate(req, session, entity_path, &t).await?;
        let (prefix, leaf) = self.walk_property(&t, &names)?;
        if leaf.collection.is_some() {
            self.write_collection(req, session, &t, leaf, &key, &[], true)
                .await?;
            self.record(req, session, &t, &key, ChangeKind::Updated)
                .await?;
            return Ok(ODataResponse::new(StatusCode::NO_CONTENT));
        }
        if !leaf.nullable {
            return Err(ODataError::bad_request(format!(
                "{} cannot be null",
                leaf.name
            )));
        }
        let nulled = match leaf.kind {
            TypeKind::Complex(_) => PropertyValue::Complex(None),
            _ => PropertyValue::Value(Value::Null),
        };
        let l = layout(model, &t)?;
        let owner = l
            .own
            .iter()
            .find(|o| o.properties.iter().any(|q| q.name == names[0]))
            .ok_or_else(|| ODataError::new(500, "unknown owner"))?;
        let table = table_of(&l, owner).to_owned();
        let mut writes = TableWrites::default();
        flatten(model, leaf, &prefix, &nulled, true, writes.columns(&table))?;
        let guarded = req.headers.contains_key(header::IF_MATCH);
        self.apply_update(req, session, &t, &key, version, writes, guarded)
            .await?;
        Ok(ODataResponse::new(StatusCode::NO_CONTENT))
    }

    async fn delete_entity(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        type_name: &str,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        let (key, _) = self.locate(req, session, &uri.path, type_name).await?;
        self.inner
            .changes
            .before_delete(&self.change_context(req, session), type_name, &key)
            .await?;
        self.delete_row(req, session, type_name, &key).await?;
        Ok(ODataResponse::new(StatusCode::NO_CONTENT))
    }

    /// Deletes a row with its collection rows and join table links.
    async fn delete_row(
        &self,
        req: &ODataRequest,
        session: &Session,
        type_name: &str,
        key: &EntityKey,
    ) -> Result<(), ODataError> {
        let model = &self.inner.model;
        let segs = [key_segment(type_name, key)];
        for p in model.all_properties(type_name) {
            if p.collection.is_some() {
                self.write_collection(req, session, type_name, p, key, &[], true)
                    .await?;
            }
        }
        for n in model.all_navigation(type_name) {
            if let Some(jt) = &n.join_table {
                let cols: Vec<String> = jt.source.iter().map(|p| p.source.clone()).collect();
                let values = self
                    .column_values(req, session, &segs, type_name, &cols)
                    .await?
                    .ok_or_else(|| ODataError::not_found("the entity does not exist"))?;
                let mut conditions: Vec<ColumnValue> = jt
                    .source
                    .iter()
                    .map(|p| p.target.clone())
                    .zip(values)
                    .collect();
                conditions.extend(side_table_key(
                    model,
                    &req.claims,
                    type_name,
                    jt.tenant_column.as_ref(),
                )?);
                sql_delete(model, session, &jt.table, &conditions).await?;
            }
        }
        let l = layout(model, type_name)?;
        let conditions = key_conditions(model, &req.claims, type_name, key)?;
        for owner in &l.own {
            if let Some(Inheritance::Joined { join, .. }) = &owner.inheritance {
                let cols: Vec<String> = join.iter().map(|p| p.source.clone()).collect();
                let values = self
                    .column_values(req, session, &segs, type_name, &cols)
                    .await?
                    .ok_or_else(|| ODataError::not_found("the entity does not exist"))?;
                let conds: Vec<ColumnValue> =
                    join.iter().map(|p| p.target.clone()).zip(values).collect();
                sql_delete(model, session, &owner.table, &conds).await?;
            }
        }
        let n = sql_delete(model, session, &l.root, &conditions).await?;
        if n == 0 {
            return Err(ODataError::not_found("the entity does not exist"));
        }
        self.record(req, session, type_name, key, ChangeKind::Deleted)
            .await
    }

    /// The owner and navigation of a `.../Nav/$ref` path.
    fn ref_target<'u>(&'u self, uri: &'u ODataUri) -> Result<RefTarget<'u>, ODataError> {
        let path = &uri.path[..uri.path.len() - 1];
        let i = path
            .iter()
            .rposition(|s| matches!(s, Segment::Navigation { .. }))
            .ok_or_else(|| ODataError::bad_request("$ref needs a navigation property"))?;
        let Segment::Navigation { name, .. } = &path[i] else {
            unreachable!("found by position");
        };
        let owner = match i.checked_sub(1).map(|j| path[j].target()) {
            Some(Target::Entity(t)) => t.clone(),
            _ => return Err(ODataError::bad_request("$ref needs a single entity")),
        };
        let nav = self
            .inner
            .model
            .navigation(&owner, name)
            .ok_or_else(|| ODataError::new(500, "unknown navigation"))?;
        Ok((&path[..i], owner, nav, path.get(i + 1)))
    }

    fn ref_body(body: &[u8]) -> Result<String, ODataError> {
        let doc: Json = serde_json::from_slice(body)
            .map_err(|e| ODataError::bad_request(format!("invalid JSON: {e}")))?;
        doc.get("@odata.id")
            .or(doc.get("@id"))
            .and_then(Json::as_str)
            .map(str::to_owned)
            .ok_or_else(|| ODataError::bad_request("expected @odata.id"))
    }

    async fn add_link(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        let (source, owner, nav, _) = self.ref_target(uri)?;
        if !nav.collection {
            return Err(ODataError::new(
                405,
                "use PUT for a single-valued navigation",
            ));
        }
        let url = Self::ref_body(&req.body)?;
        self.link_existing(req, session, source, &owner, nav, &url)
            .await?;
        Ok(ODataResponse::new(StatusCode::NO_CONTENT))
    }

    async fn set_link(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        let model = &self.inner.model;
        let (source, owner, nav, _) = self.ref_target(uri)?;
        if nav.collection {
            return Err(ODataError::new(405, "use POST for a collection navigation"));
        }
        let url = Self::ref_body(&req.body)?;
        if !fk_on_source(model, &owner, nav) {
            self.link_existing(req, session, source, &owner, nav, &url)
                .await?;
            return Ok(ODataResponse::new(StatusCode::NO_CONTENT));
        }
        let values = self.bind_target(req, session, nav, &url).await?;
        let (key, version) = self.locate(req, session, source, &owner).await?;
        let mut writes = TableWrites::default();
        let l = layout(model, &owner)?;
        for (pair, value) in nav.join.iter().zip(values) {
            writes.push(&l.root, ColumnWrite::new(pair.source.clone(), value, None));
        }
        self.apply_update(req, session, &owner, &key, version, writes, false)
            .await?;
        Ok(ODataResponse::new(StatusCode::NO_CONTENT))
    }

    async fn remove_link(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        let model = &self.inner.model;
        let (source, owner, nav, keyed) = self.ref_target(uri)?;
        let target_url = uri
            .options
            .custom
            .iter()
            .find(|(n, _)| n == "$id")
            .map(|(_, v)| v.clone());
        let target_segments: Option<(Vec<Segment>, String)> = match (keyed, &target_url) {
            (
                Some(Segment::Key {
                    values,
                    target: Target::Entity(t),
                }),
                _,
            ) => Some((
                vec![Segment::Key {
                    values: values.clone(),
                    target: Target::Entity(t.clone()),
                }],
                t.clone(),
            )),
            (_, Some(url)) => Some(self.parse_entity_url(req, url)?),
            _ => None,
        };
        if let Some(jt) = &nav.join_table {
            let (segs, t) = target_segments
                .ok_or_else(|| ODataError::bad_request("a target entity is required"))?;
            let cols: Vec<String> = jt.source.iter().map(|p| p.source.clone()).collect();
            let values = self
                .column_values(req, session, source, &owner, &cols)
                .await?
                .ok_or_else(|| ODataError::not_found("the entity does not exist"))?;
            let mut conditions: Vec<ColumnValue> = jt
                .source
                .iter()
                .map(|p| p.target.clone())
                .zip(values)
                .collect();
            let cols: Vec<String> = jt.target.iter().map(|p| p.target.clone()).collect();
            let values = self
                .column_values(req, session, &segs, &t, &cols)
                .await?
                .ok_or_else(|| ODataError::not_found("the target does not exist"))?;
            conditions.extend(jt.target.iter().map(|p| p.source.clone()).zip(values));
            conditions.extend(side_table_key(
                model,
                &req.claims,
                &owner,
                jt.tenant_column.as_ref(),
            )?);
            sql_delete(model, session, &jt.table, &conditions).await?;
            return Ok(ODataResponse::new(StatusCode::NO_CONTENT));
        }
        let clear = |pairs: Vec<String>, table: &str, type_name: &str| -> TableWrites {
            let mut w = TableWrites::default();
            for c in pairs {
                let kind = column_kind(model, type_name, &c);
                w.push(table, ColumnWrite::new(c, Value::Null, kind));
            }
            w
        };
        if fk_on_source(model, &owner, nav) {
            let (key, version) = self.locate(req, session, source, &owner).await?;
            let l = layout(model, &owner)?;
            let cols = nav.join.iter().map(|p| p.source.clone()).collect();
            let writes = clear(cols, &l.root, &owner);
            self.apply_update(req, session, &owner, &key, version, writes, false)
                .await?;
            return Ok(ODataResponse::new(StatusCode::NO_CONTENT));
        }
        let (segs, t) = target_segments
            .ok_or_else(|| ODataError::bad_request("a target entity is required"))?;
        let (key, version) = self.locate(req, session, &segs, &t).await?;
        let l = layout(model, &t)?;
        let cols = nav.join.iter().map(|p| p.target.clone()).collect();
        let writes = clear(cols, &l.root, &t);
        self.apply_update(req, session, &t, &key, version, writes, false)
            .await?;
        Ok(ODataResponse::new(StatusCode::NO_CONTENT))
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
fn table_conditions(l: &Layout<'_>, table: &str, key: &[ColumnValue]) -> Vec<ColumnValue> {
    let joined = l.own.iter().find_map(|t| match &t.inheritance {
        Some(Inheritance::Joined { join, .. }) if t.table == table => Some(join),
        _ => None,
    });
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
