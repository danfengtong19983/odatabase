//! Deep updates and delta payloads: nested entities, related keys and removals.

use super::*;

impl Service {
    /// The key an `@id` names. It must be of `declared` or a type derived from it.
    pub(super) fn key_of_url(
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
    pub(super) fn delta_entity(
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
    pub(super) async fn apply_delta(
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
    pub(super) async fn delete_listed(
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
    pub(super) async fn patch_collection(
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
    pub(super) async fn related_keys(
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
    pub(super) async fn update_child(
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
    pub(super) async fn insert_child(
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
    pub(super) async fn write_child(
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
    pub(super) async fn sync_source_link(
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
    pub(super) async fn sync_target_links(
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
    pub(super) async fn remove_related(
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
    pub(super) async fn after_update(
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
}
