//! `POST` of an entity: payload checks, the insert of the row and its tables, nested inserts.

use super::*;

impl Service {
    pub(super) async fn create(
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
    pub(super) async fn created_response(
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
    pub(super) fn insert_entity<'a>(
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
            if let Some(column) = model.open_column(type_name) {
                writes.push(&l.root, dynamic_write(column, payload, true));
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
                if let Some(Inheritance::Joined {
                    join,
                    tenant_column,
                    ..
                }) = &owner.inheritance
                {
                    let tenant =
                        tenant_cell(model, &req.claims, type_name, tenant_column.as_ref())?;
                    let cols = writes.columns(&owner.table);
                    cols.extend(tenant);
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
}
