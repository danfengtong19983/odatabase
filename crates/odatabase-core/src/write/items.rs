//! Collection properties: replacing their items, and `POST` of one item.

use super::*;

impl Service {
    /// Replaces the child rows of a collection property.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn write_collection(
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
    pub(super) async fn write_item(
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
    pub(super) async fn insert_item(
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
}
