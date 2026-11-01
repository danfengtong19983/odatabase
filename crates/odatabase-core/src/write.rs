//! `POST`, `PATCH`, `PUT` and `DELETE`.

use crate::change::{ChangeContext, EntityKey};
use crate::format::Format;
use crate::payload::{EntityPayload, PropertyValue, parse_entity, read_property};
use crate::read::{etag_matches, language, split_entity_path};
use crate::url::entity_id;
use crate::{ODataError, ODataRequest, ODataResponse, Service};
use chrono::Utc;
use futures::future::BoxFuture;
use http::{HeaderValue, Method, StatusCode, header};
use odatabase_edm::{
    EntityType, Inheritance, Model, NavigationProperty, PrimitiveKind, Property, TypeKind, Value,
};
use odatabase_sql::{
    ColumnValue, ColumnWrite, Session, delete as sql_delete, insert as sql_insert, read_columns,
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
fn key_conditions(
    model: &Model,
    type_name: &str,
    key: &EntityKey,
) -> Result<Vec<ColumnValue>, ODataError> {
    key.iter()
        .map(|(name, value)| {
            model
                .property(type_name, name)
                .map(|p| (p.column.clone(), value.clone()))
                .ok_or_else(|| ODataError::new(500, format!("unknown key {name}")))
        })
        .collect()
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
        let session = Session::begin(&self.inner.database).await?;
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
        match (&req.method, uri.target()) {
            (&Method::POST, Target::ActionResult) => self.call_action(req, uri, session).await,
            (&Method::POST, Target::EntityCollection(t)) => self.create(req, uri, t, session).await,
            (&Method::POST, Target::Ref { .. }) => self.add_link(req, uri, session).await,
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
        let options = expand_of(model, &payload);
        let segs = [key_segment(&payload.type_name, &created)];
        let rec = self
            .read_one(req, session, &segs, &payload.type_name, &options)
            .await?
            .ok_or_else(|| ODataError::new(403, "the created entity is not accessible"))?;
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
            let l = layout(model, type_name)?;
            let mut writes = TableWrites::default();
            writes.columns(&l.root);
            let filled = filled_columns(model, payload, &parent_columns);
            let filled: Vec<&str> = filled.iter().map(String::as_str).collect();
            for (column, value) in parent_columns {
                writes.push(&l.root, ColumnWrite::new(column, value, None));
            }
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
                    let child = &children[0];
                    let key = self.insert_entity(req, session, child, Vec::new()).await?;
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

            let key_cols = key_columns(model, type_name)?;
            let mut returned = None;
            for (table, cols) in &writes.tables {
                if *table == l.root {
                    let cols = unique(cols);
                    returned = Some(sql_insert(model, session, table, &cols, &key_cols).await?);
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
                        let child_key = self.insert_entity(req, session, child, Vec::new()).await?;
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
                        self.insert_entity(req, session, child, parent).await?;
                    }
                }
            }
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
        let conditions = key_conditions(model, &t, &key)?;
        sql_update(model, session, &l.root, &sets, None, &conditions).await?;
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
        sql_insert(&self.inner.model, session, &jt.table, &row, &[]).await?;
        Ok(())
    }

    /// The key of the entity at `path`, respecting row restrictions.
    pub(crate) async fn entity_key_of(
        &self,
        req: &ODataRequest,
        session: &Session,
        path: &[Segment],
        type_name: &str,
    ) -> Result<EntityKey, ODataError> {
        self.entity_key(req, session, path, type_name).await
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
            let conditions: Vec<ColumnValue> = c
                .owner
                .iter()
                .map(|o| o.target.clone())
                .zip(owner_values.iter().cloned())
                .collect();
            sql_delete(model, session, &c.table, &conditions).await?;
        }
        for (i, item) in items.iter().enumerate() {
            let mut cols: Vec<ColumnWrite> = c
                .owner
                .iter()
                .zip(&owner_values)
                .map(|(o, v)| ColumnWrite::new(o.target.clone(), v.clone(), None))
                .collect();
            if let Some(order) = &c.order_column {
                cols.push(ColumnWrite::new(
                    order.clone(),
                    Value::Int32(i as i32 + 1),
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
        }
        Ok(())
    }

    /// Reads the row to change, checking existence and `If-Match`.
    async fn locate(
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
    async fn apply_update(
        &self,
        session: &Session,
        type_name: &str,
        key: &EntityKey,
        version: Option<Value>,
        writes: TableWrites,
        guard: bool,
    ) -> Result<(), ODataError> {
        let model = &self.inner.model;
        let l = layout(model, type_name)?;
        let conditions = key_conditions(model, type_name, key)?;
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
            let mut conds = conditions.clone();
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
        Ok(())
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
        let (key, version) = self.locate(req, session, &uri.path, type_name).await?;
        let mut payload = parse_entity(model, type_name, &req.body)?;
        self.inner
            .changes
            .before_update(&self.change_context(req, session), &key, &mut payload)
            .await?;
        let l = layout(model, type_name)?;
        let mut writes = TableWrites::default();
        let concrete = &payload.type_name;
        for (name, value) in &key {
            if let Some(PropertyValue::Value(v)) = payload.value(name)
                && v != value
                && !model.property(type_name, name).is_some_and(|p| {
                    matches!(p.kind, TypeKind::Primitive(k) if v.coerce(k).as_ref() == Some(value))
                })
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
        if !payload.nested.is_empty() {
            return Err(ODataError::not_implemented("deep update"));
        }
        let guarded = req.headers.contains_key(header::IF_MATCH);
        self.apply_update(session, type_name, &key, version, writes, guarded)
            .await?;
        for p in model.all_properties(type_name) {
            if let (Some(_), Some(PropertyValue::Collection(items))) =
                (&p.collection, payload.value(&p.name))
            {
                self.write_collection(req, session, type_name, p, &key, items, true)
                    .await?;
            }
        }
        let segs = [key_segment(type_name, &key)];
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
        self.after_update(req, uri, session, type_name, &key, format)
            .await
    }

    /// The response of an update: empty, unless a representation was asked for.
    async fn after_update(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        session: &Session,
        type_name: &str,
        key: &EntityKey,
        format: Format,
    ) -> Result<ODataResponse, ODataError> {
        let pref = prefer_return(req);
        let segs = [key_segment(type_name, key)];
        let rec = self
            .read_one(req, session, &segs, type_name, &uri.options)
            .await?
            .ok_or_else(|| ODataError::new(403, "the changed entity is not accessible"))?;
        let mut r = if pref == Return::Representation {
            self.entity_response(req, &rec, type_name, &uri.options, format, StatusCode::OK)
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
            self.apply_update(session, &t, &key, version, TableWrites::default(), false)
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
        self.apply_update(session, &t, &key, version, writes, guarded)
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
        let (key, version) = self.locate(req, session, entity_path, &t).await?;
        let (prefix, leaf) = self.walk_property(&t, &names)?;
        if leaf.collection.is_some() {
            self.write_collection(req, session, &t, leaf, &key, &[], true)
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
        self.apply_update(session, &t, &key, version, writes, guarded)
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
        let model = &self.inner.model;
        let (key, _) = self.locate(req, session, &uri.path, type_name).await?;
        self.inner
            .changes
            .before_delete(&self.change_context(req, session), type_name, &key)
            .await?;
        let segs = [key_segment(type_name, &key)];
        for p in model.all_properties(type_name) {
            if p.collection.is_some() {
                self.write_collection(req, session, type_name, p, &key, &[], true)
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
                let conditions: Vec<ColumnValue> = jt
                    .source
                    .iter()
                    .map(|p| p.target.clone())
                    .zip(values)
                    .collect();
                sql_delete(model, session, &jt.table, &conditions).await?;
            }
        }
        let l = layout(model, type_name)?;
        let conditions = key_conditions(model, type_name, &key)?;
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
        Ok(ODataResponse::new(StatusCode::NO_CONTENT))
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
        self.apply_update(session, &owner, &key, version, writes, false)
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
            sql_delete(model, session, &jt.table, &conditions).await?;
            return Ok(ODataResponse::new(StatusCode::NO_CONTENT));
        }
        let clear = |pairs: Vec<String>, table: &str| -> TableWrites {
            let mut w = TableWrites::default();
            for c in pairs {
                w.push(table, ColumnWrite::new(c, Value::Null, None));
            }
            w
        };
        if fk_on_source(model, &owner, nav) {
            let (key, version) = self.locate(req, session, source, &owner).await?;
            let l = layout(model, &owner)?;
            let cols = nav.join.iter().map(|p| p.source.clone()).collect();
            let writes = clear(cols, &l.root);
            self.apply_update(session, &owner, &key, version, writes, false)
                .await?;
            return Ok(ODataResponse::new(StatusCode::NO_CONTENT));
        }
        let (segs, t) = target_segments
            .ok_or_else(|| ODataError::bad_request("a target entity is required"))?;
        let (key, version) = self.locate(req, session, &segs, &t).await?;
        let l = layout(model, &t)?;
        let cols = nav.join.iter().map(|p| p.target.clone()).collect();
        let writes = clear(cols, &l.root);
        self.apply_update(session, &t, &key, version, writes, false)
            .await?;
        Ok(ODataResponse::new(StatusCode::NO_CONTENT))
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
