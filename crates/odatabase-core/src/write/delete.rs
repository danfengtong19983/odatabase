//! `DELETE` of an entity and its rows.

use super::*;

impl Service {
    pub(super) async fn delete_entity(
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
    pub(super) async fn delete_row(
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
            if let Some(join) = model.joined_columns(owner) {
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
}
