//! Paths that only matter under restrictions or concurrency.

mod common;

use async_trait::async_trait;
use axum::http::{Method, StatusCode};
use common::{App, BusinessPartner, Role, Ticket};
use odatabase::edm::{Annotation, AnnotationValue as A, Value};
use odatabase::{
    ChangeContext, ChangeHandler, Database, EntityKey, EntityPayload, ODataError, OperationContext,
    OperationHandler, OperationResult, PropertyValue, Service, ServiceBuilder,
};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const JSON: (&str, &str) = ("content-type", "application/json");

async fn send(app: &App, method: Method, uri: &str, body: serde_json::Value) -> common::Response {
    app.request(method, uri, &[JSON], &body.to_string()).await
}

fn restriction(term: &str, field: &str) -> Annotation {
    let name = format!("Capabilities.{term}");
    Annotation::new(name, A::Record(vec![(field.to_owned(), A::Bool(false))]))
}

fn roles_fixed_set(b: ServiceBuilder) -> ServiceBuilder {
    b.annotate::<Role>(restriction("InsertRestrictions", "Insertable"))
        .annotate::<Role>(restriction("DeleteRestrictions", "Deletable"))
}

fn read_only_children(b: ServiceBuilder) -> ServiceBuilder {
    b.annotate::<Role>(restriction("UpdateRestrictions", "Updatable"))
        .annotate::<Ticket>(restriction("UpdateRestrictions", "Updatable"))
}

async fn nested_inserts_and_deletes_are_refused(app: App) {
    let keep = json!([{"Code": 1}, {"Code": 2}, {"Code": 3}]);
    let r = send(
        &app,
        Method::PATCH,
        "BusinessPartners('1')",
        json!({"Name": "Kept", "Roles": keep}),
    )
    .await;
    assert_eq!(
        r.status,
        StatusCode::NO_CONTENT,
        "the same set changes nothing: {}",
        r.text()
    );

    let dropped = json!({"Name": "Lost", "Roles": [{"Code": 1}, {"Code": 2}]});
    let r = send(&app, Method::PATCH, "BusinessPartners('1')", dropped).await;
    assert_eq!(
        r.status,
        StatusCode::METHOD_NOT_ALLOWED,
        "removing means deleting: {}",
        r.text()
    );
    assert_eq!(
        app.ok("BusinessPartners('1')").await["Name"],
        "Kept",
        "rolled back"
    );

    let added =
        json!({"Name": "Lost", "Roles": [{"Code": 1}, {"Code": 2}, {"Code": 3}, {"Code": 9}]});
    let r = send(&app, Method::PATCH, "BusinessPartners('1')", added).await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED, "{}", r.text());
    assert_eq!(
        app.column("BusinessPartners('1')/Roles", "Code")
            .await
            .len(),
        3
    );

    let r = send(
        &app,
        Method::PATCH,
        "Roles(PartnerId='4',Code=9)",
        json!({}),
    )
    .await;
    assert_eq!(
        r.status,
        StatusCode::METHOD_NOT_ALLOWED,
        "an upsert inserts: {}",
        r.text()
    );
    let r = send(
        &app,
        Method::POST,
        "Roles",
        json!({"PartnerId": "4", "Code": 9}),
    )
    .await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
    let r = send(
        &app,
        Method::POST,
        "BusinessPartners('4')/Roles",
        json!({"Code": 9}),
    )
    .await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);

    let deep =
        json!({"@odata.type": "#Test.Person", "Id": "90", "Active": true, "Roles": [{"Code": 1}]});
    let r = send(&app, Method::POST, "BusinessPartners", deep).await;
    assert_eq!(
        r.status,
        StatusCode::METHOD_NOT_ALLOWED,
        "deep insert: {}",
        r.text()
    );
    assert_eq!(
        app.status("BusinessPartners('90')").await,
        404,
        "rolled back"
    );
    let upserted = json!({"@odata.type": "#Test.Person", "Active": true, "Roles": [{"Code": 1}]});
    let r = send(&app, Method::PATCH, "BusinessPartners('91')", upserted).await;
    assert_eq!(
        r.status,
        StatusCode::METHOD_NOT_ALLOWED,
        "upsert with children: {}",
        r.text()
    );
    assert_eq!(app.status("BusinessPartners('91')").await, 404);
}

async fn nested_updates_and_unlinks_are_refused(app: App) {
    let body =
        json!({"Name": "Lost", "Roles": [{"Code": 1, "Label": "x"}, {"Code": 2}, {"Code": 3}]});
    let r = send(&app, Method::PATCH, "BusinessPartners('1')", body).await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED, "{}", r.text());
    assert_eq!(app.ok("BusinessPartners('1')").await["Name"], "First Org");
    assert_eq!(
        app.ok("Roles(PartnerId='1',Code=1)").await["Label"],
        "Buyer"
    );

    let r = send(
        &app,
        Method::POST,
        "Projects(10)/Tickets",
        json!({"Title": "t"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    let r = send(&app, Method::PATCH, "Projects(10)", json!({"Tickets": []})).await;
    assert_eq!(
        r.status,
        StatusCode::METHOD_NOT_ALLOWED,
        "unlinking updates the child: {}",
        r.text()
    );
    assert_eq!(app.column("Projects(10)/Tickets", "Id").await.len(), 1);

    let ticket = app.column("Projects(10)/Tickets", "Id").await[0].clone();
    let url = format!("Tickets({ticket})");
    let r = send(
        &app,
        Method::PATCH,
        "Projects(20)",
        json!({"Tickets@odata.bind": [url.clone()]}),
    )
    .await;
    assert_eq!(
        r.status,
        StatusCode::METHOD_NOT_ALLOWED,
        "a bind updates the child: {}",
        r.text()
    );
    let r = send(
        &app,
        Method::POST,
        "Projects",
        json!({"Id": 77, "Title": "n", "Tickets@odata.bind": [url]}),
    )
    .await;
    assert_eq!(
        r.status,
        StatusCode::METHOD_NOT_ALLOWED,
        "on create too: {}",
        r.text()
    );
    assert_eq!(app.status("Projects(77)").await, 404);
    assert_eq!(
        app.column("Projects(10)/Tickets", "Id").await.len(),
        1,
        "it stayed where it was"
    );
}

/// Records what the hooks see, stamps new roles, and can refuse a delete.
#[derive(Clone, Default)]
struct Recorder {
    calls: Arc<Mutex<Vec<String>>>,
    refuse_delete: bool,
}

#[async_trait]
impl ChangeHandler for Recorder {
    async fn before_create(
        &self,
        _: &ChangeContext<'_>,
        entity: &mut EntityPayload,
    ) -> Result<(), ODataError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("create {}", entity.type_name));
        if entity.type_name == "Role" {
            entity.values.retain(|(n, _)| n != "Label");
            let stamp = PropertyValue::Value(Value::String("stamped".into()));
            entity.values.push(("Label".into(), stamp));
        }
        Ok(())
    }

    async fn before_update(
        &self,
        _: &ChangeContext<'_>,
        key: &EntityKey,
        entity: &mut EntityPayload,
    ) -> Result<(), ODataError> {
        let id = key
            .iter()
            .map(|(_, v)| v.to_text())
            .collect::<Vec<_>>()
            .join(",");
        self.calls
            .lock()
            .unwrap()
            .push(format!("update {} {id}", entity.type_name));
        Ok(())
    }

    async fn before_delete(
        &self,
        _: &ChangeContext<'_>,
        type_name: &str,
        key: &EntityKey,
    ) -> Result<(), ODataError> {
        let id = key
            .iter()
            .map(|(_, v)| v.to_text())
            .collect::<Vec<_>>()
            .join(",");
        self.calls
            .lock()
            .unwrap()
            .push(format!("delete {type_name} {id}"));
        if self.refuse_delete {
            return Err(ODataError::new(403, "this role stays"));
        }
        Ok(())
    }

    async fn validate_changes(&self, _: &ChangeContext<'_>) -> Result<(), ODataError> {
        self.calls.lock().unwrap().push("validate".into());
        Ok(())
    }
}

#[tokio::test]
async fn hooks_see_nested_changes() {
    let recorder = Recorder::default();
    let handler = recorder.clone();
    let app =
        common::sqlite(|db| common::builder().change_handler(handler).build(db).unwrap()).await;
    let body = json!({"Roles": [{"Code": 1, "Label": "changed"}, {"Code": 9, "Label": "ignored"}]});
    let r = send(&app, Method::PATCH, "BusinessPartners('1')", body).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(
        *recorder.calls.lock().unwrap(),
        [
            "update BusinessPartner 1",
            "update Role 1,1",
            "create Role",
            "delete Role 1,2",
            "delete Role 1,3",
            "validate"
        ],
        "every nested row passes through a hook, and validation runs once"
    );
    assert_eq!(
        app.ok("Roles(PartnerId='1',Code=1)").await["Label"],
        "changed"
    );
    assert_eq!(
        app.ok("Roles(PartnerId='1',Code=9)").await["Label"],
        "stamped",
        "a hook can edit a child"
    );
}

#[tokio::test]
async fn hooks_see_a_deep_insert() {
    let recorder = Recorder::default();
    let handler = recorder.clone();
    let app =
        common::sqlite(|db| common::builder().change_handler(handler).build(db).unwrap()).await;
    let body = json!({
        "@odata.type": "#Test.Person", "Id": "95", "Active": true,
        "Roles": [{"Code": 1, "Label": "ignored"}],
        "Owner": {"@odata.type": "#Test.Person", "Id": "96", "Active": true}
    });
    let r = send(&app, Method::POST, "BusinessPartners", body).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    assert_eq!(
        *recorder.calls.lock().unwrap(),
        ["create Person", "create Person", "create Role", "validate"],
        "the parent, then each nested row"
    );
    assert_eq!(
        app.ok("Roles(PartnerId='95',Code=1)").await["Label"],
        "stamped"
    );
}

#[tokio::test]
async fn hooks_can_stop_a_nested_change() {
    let handler = Recorder {
        refuse_delete: true,
        ..Recorder::default()
    };
    let app =
        common::sqlite(|db| common::builder().change_handler(handler).build(db).unwrap()).await;
    let r = send(
        &app,
        Method::PATCH,
        "BusinessPartners('1')",
        json!({"Name": "Lost", "Roles": []}),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.text());
    assert_eq!(
        app.ok("BusinessPartners('1')").await["Name"],
        "First Org",
        "all of it rolled back"
    );
    assert_eq!(
        app.column("BusinessPartners('1')/Roles", "Code")
            .await
            .len(),
        3
    );
}

#[derive(Clone)]
struct SlowCreate;

/// Waits between the existence check and the insert, so that two upserts overlap.
#[async_trait]
impl ChangeHandler for SlowCreate {
    async fn before_create(
        &self,
        _: &ChangeContext<'_>,
        _: &mut EntityPayload,
    ) -> Result<(), ODataError> {
        tokio::time::sleep(Duration::from_millis(150)).await;
        Ok(())
    }
}

async fn concurrent_upserts_of_one_key(app: App) {
    let uri = "Roles(PartnerId='4',Code=30)";
    let body = json!({"Label": "race"}).to_string();
    let (a, b) = tokio::join!(
        app.request(Method::PATCH, uri, &[JSON], &body),
        app.request(Method::PATCH, uri, &[JSON], &body),
    );
    let mut statuses = [a.status.as_u16(), b.status.as_u16()];
    statuses.sort_unstable();
    assert!(
        statuses == [201, 204] || statuses == [201, 409],
        "one creates, the other updates or conflicts: {statuses:?}"
    );
    assert_eq!(
        app.column("BusinessPartners('4')/Roles?$filter=Code eq 30", "Code")
            .await
            .len(),
        1
    );

    let uri = "Roles(PartnerId='4',Code=31)";
    let only_new = [JSON, ("if-none-match", "*")];
    let (a, b) = tokio::join!(
        app.request(Method::PATCH, uri, &only_new, &body),
        app.request(Method::PATCH, uri, &only_new, &body),
    );
    let mut statuses = [a.status.as_u16(), b.status.as_u16()];
    statuses.sort_unstable();
    assert_eq!(
        statuses,
        [201, 412],
        "insert-only never reports a plain conflict"
    );
}

struct SumAll;

#[async_trait]
impl OperationHandler for SumAll {
    async fn call(&self, ctx: &OperationContext<'_>) -> Result<OperationResult, ODataError> {
        let Some((_, PropertyValue::Collection(items))) =
            ctx.parameters.iter().find(|(n, _)| n == "values")
        else {
            return Err(ODataError::bad_request("values is missing"));
        };
        let total: i64 = items
            .iter()
            .filter_map(|i| match i {
                PropertyValue::Value(v) => v.as_i64(),
                _ => None,
            })
            .sum();
        Ok(OperationResult::Value(Value::Int32(total as i32)))
    }
}

struct City;

#[async_trait]
impl OperationHandler for City {
    async fn call(&self, ctx: &OperationContext<'_>) -> Result<OperationResult, ODataError> {
        let Some((_, PropertyValue::Collection(items))) =
            ctx.parameters.iter().find(|(n, _)| n == "places")
        else {
            return Err(ODataError::bad_request("places is missing"));
        };
        Ok(OperationResult::Value(Value::Int32(items.len() as i32)))
    }
}

fn operations(db: Database) -> Service {
    common::builder()
        .action("SumAll", |a| {
            a.param::<Vec<i32>>("values")
                .returns::<i32>()
                .handler(SumAll)
        })
        .action("CountPlaces", |a| {
            a.param::<Vec<common::Address>>("places")
                .returns::<i32>()
                .handler(City)
        })
        .function("SumList", |f| {
            f.param::<Vec<i32>>("values")
                .returns::<i32>()
                .handler(SumAll)
        })
        .build(db)
        .unwrap()
}

async fn collection_parameters(app: App) {
    let r = send(&app, Method::POST, "SumAll", json!({"values": [1, 2, 3]})).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    assert_eq!(r.json()["value"], 6);
    assert_eq!(
        send(&app, Method::POST, "SumAll", json!({"values": []}))
            .await
            .json()["value"],
        0
    );
    for bad in [
        json!({"values": 1}),
        json!({"values": ["a"]}),
        json!({"values": [null]}),
        json!({}),
    ] {
        let r = send(&app, Method::POST, "SumAll", bad.clone()).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{bad}: {}", r.text());
    }
    let places = json!({"places": [{"City": "Rome"}, {"City": "Oslo"}]});
    assert_eq!(
        send(&app, Method::POST, "CountPlaces", places).await.json()["value"],
        2
    );
    let wrong = json!({"places": [{"Nope": 1}]});
    assert_eq!(
        send(&app, Method::POST, "CountPlaces", wrong).await.status,
        StatusCode::BAD_REQUEST
    );
    let xml = app.get("$metadata").await.text();
    assert!(xml.contains("Type=\"Collection(Edm.Int32)\""), "{xml}");

    let status = app.status("SumList(values=@v)?@v=[1,2]").await;
    assert!(
        status == 200 || status == 400 || status == 501,
        "a clean answer, not {status}"
    );
}

mod fixed_set {
    use super::common;
    use odatabase::{Database, Service};

    fn build() -> impl FnOnce(Database) -> Service {
        |db| super::roles_fixed_set(common::builder()).build(db).unwrap()
    }

    crate::scenarios!(build(); nested_inserts_and_deletes_are_refused);
}

mod read_only {
    use super::common;
    use odatabase::{Database, Service};

    fn build() -> impl FnOnce(Database) -> Service {
        |db| {
            super::read_only_children(common::builder())
                .build(db)
                .unwrap()
        }
    }

    crate::scenarios!(build(); nested_updates_and_unlinks_are_refused);
}

mod races {
    use super::common;
    use odatabase::{Database, Service};

    fn build() -> impl FnOnce(Database) -> Service {
        |db| {
            common::builder()
                .change_handler(super::SlowCreate)
                .build(db)
                .unwrap()
        }
    }

    crate::scenarios!(build(); concurrent_upserts_of_one_key);
}

mod parameters {
    use super::common;
    use odatabase::{Database, Service};

    fn build() -> impl FnOnce(Database) -> Service {
        super::operations
    }

    crate::scenarios!(build(); collection_parameters);
}

struct Bump;

#[async_trait]
impl OperationHandler for Bump {
    async fn call(&self, ctx: &OperationContext<'_>) -> Result<OperationResult, ODataError> {
        let Some(odatabase::Binding::Entity { key, .. }) = &ctx.binding else {
            return Err(ODataError::bad_request("bound only"));
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        odatabase::sql::update(
            ctx.model,
            ctx.session,
            "business_partner",
            &[],
            Some("version"),
            &[("id".to_owned(), key[0].1.clone())],
        )
        .await?;
        Ok(OperationResult::Void)
    }
}

/// A pool of several connections to one SQLite file, as a real deployment would have.
async fn sqlite_file_pool(name: &str) -> (sqlx::SqlitePool, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!("odatabase-{name}-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(5))
        .foreign_keys(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(6)
        .connect_with(options)
        .await
        .unwrap();
    let schema = common::SCHEMA
        .replace("BYTEA", "BLOB")
        .replace("SERIAL", "INTEGER");
    sqlx::raw_sql(sqlx::AssertSqlSafe(schema))
        .execute(&pool)
        .await
        .unwrap();
    (pool, path)
}

#[tokio::test]
async fn sqlite_pool_with_several_connections() {
    let (pool, path) = sqlite_file_pool("pool").await;
    let service = common::builder()
        .function("Noop", |f| f.returns::<i32>().handler(Noop))
        .action("Bump", |a| a.bound_to::<BusinessPartner>().handler(Bump))
        .build(pool.clone())
        .unwrap();
    let app = App::new(service);

    let reads = (0..24).map(|_| app.ok("BusinessPartners?$expand=Roles"));
    for doc in futures::future::join_all(reads).await {
        assert_eq!(doc["value"].as_array().unwrap().len(), 4);
    }

    let bump = "BusinessPartners('4')/Test.Bump";
    let tag = [("if-match", "W/\"1\"")];
    let calls = (0..6).map(|_| app.request(Method::POST, bump, &tag, ""));
    let results = futures::future::join_all(calls).await;
    let mut statuses: Vec<u16> = results.iter().map(|r| r.status.as_u16()).collect();
    statuses.sort_unstable();
    assert_eq!(
        statuses.iter().filter(|s| **s == 204).count(),
        1,
        "{statuses:?}"
    );
    assert!(
        statuses.iter().all(|s| [204, 412, 503].contains(s)),
        "no server errors: {statuses:?}"
    );
    assert_eq!(app.ok("BusinessPartners('4')").await["Version"], 2);

    let patch = json!({"Name": "x"}).to_string();
    let tag = [JSON, ("if-match", "W/\"2\"")];
    let calls = (0..6).map(|_| app.request(Method::PATCH, "BusinessPartners('4')", &tag, &patch));
    let results = futures::future::join_all(calls).await;
    let mut statuses: Vec<u16> = results.iter().map(|r| r.status.as_u16()).collect();
    statuses.sort_unstable();
    assert_eq!(
        statuses.iter().filter(|s| **s == 204).count(),
        1,
        "{statuses:?}"
    );
    assert!(
        statuses.iter().all(|s| [204, 412, 503].contains(s)),
        "no server errors: {statuses:?}"
    );

    pool.close().await;
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

struct Noop;

#[async_trait]
impl OperationHandler for Noop {
    async fn call(&self, _: &OperationContext<'_>) -> Result<OperationResult, ODataError> {
        Ok(OperationResult::Void)
    }
}
