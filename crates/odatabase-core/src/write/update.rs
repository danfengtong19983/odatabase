//! `PATCH` and `PUT` of an entity, upserts, and the update of its rows.

use super::*;

impl Service {
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
    pub(super) async fn apply_update(
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
            let mut conds = table_conditions(model, &l, &table, &conditions);
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

    pub(super) async fn replace_entity(
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
    pub(super) async fn upsert(
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
    pub(super) fn update_row<'a>(
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
    pub(super) async fn update_row_inner(
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
    pub(super) async fn parent_columns(
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
}
