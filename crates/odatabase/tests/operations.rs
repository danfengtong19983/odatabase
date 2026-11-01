mod common;

use async_trait::async_trait;
use axum::http::{Method, StatusCode};
use common::{App, BusinessPartner};
use odatabase::edm::Value;
use odatabase::{
    Binding, Database, ODataError, OperationContext, OperationHandler, OperationResult, Service,
};
use serde_json::json;

struct Add;

#[async_trait]
impl OperationHandler for Add {
    async fn call(&self, ctx: &OperationContext<'_>) -> Result<OperationResult, ODataError> {
        let a = ctx.value("a").and_then(Value::as_i64).unwrap_or(0);
        let b = ctx.value("b").and_then(Value::as_i64).unwrap_or(0);
        Ok(OperationResult::Value(Value::Int32((a + b) as i32)))
    }
}

struct Top;

#[async_trait]
impl OperationHandler for Top {
    async fn call(&self, _ctx: &OperationContext<'_>) -> Result<OperationResult, ODataError> {
        Ok(OperationResult::Keys(vec![
            vec![("Id".to_owned(), Value::String("1".into()))],
            vec![("Id".to_owned(), Value::String("2".into()))],
        ]))
    }
}

struct Describe;

#[async_trait]
impl OperationHandler for Describe {
    async fn call(&self, ctx: &OperationContext<'_>) -> Result<OperationResult, ODataError> {
        let text = match &ctx.binding {
            Some(Binding::Entity { key, .. }) => format!("entity {}", key[0].1.to_text()),
            Some(Binding::Collection { type_name }) => format!("set of {type_name}"),
            None => "unbound".to_owned(),
        };
        Ok(OperationResult::Value(Value::String(text)))
    }
}

struct Rename;

#[async_trait]
impl OperationHandler for Rename {
    async fn call(&self, ctx: &OperationContext<'_>) -> Result<OperationResult, ODataError> {
        let Some(Binding::Entity { key, .. }) = &ctx.binding else {
            return Err(ODataError::bad_request("bound only"));
        };
        let name = ctx.value("name").cloned().unwrap_or(Value::Null);
        odatabase::sql::update(
            ctx.model,
            ctx.session,
            "business_partner",
            &[odatabase::sql::ColumnWrite::new("name", name, None)],
            None,
            &[("id".to_owned(), key[0].1.clone())],
        )
        .await?;
        if ctx.value("fail") == Some(&Value::Boolean(true)) {
            return Err(ODataError::new(422, "failed after writing"));
        }
        Ok(OperationResult::Void)
    }
}

struct Numbers;

#[async_trait]
impl OperationHandler for Numbers {
    async fn call(&self, _ctx: &OperationContext<'_>) -> Result<OperationResult, ODataError> {
        Ok(OperationResult::Values(vec![
            Value::Int32(1),
            Value::Int32(2),
        ]))
    }
}

struct Point;

#[async_trait]
impl OperationHandler for Point {
    async fn call(&self, _ctx: &OperationContext<'_>) -> Result<OperationResult, ODataError> {
        Ok(OperationResult::Json(json!({"City": "Rome"})))
    }
}

/// Advances the ETag, slowly enough for concurrent calls to overlap.
struct Bump;

#[async_trait]
impl OperationHandler for Bump {
    async fn call(&self, ctx: &OperationContext<'_>) -> Result<OperationResult, ODataError> {
        let Some(Binding::Entity { key, .. }) = &ctx.binding else {
            return Err(ODataError::bad_request("bound only"));
        };
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
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

fn service(db: Database) -> Service {
    common::builder()
        .function("Add", |f| {
            f.param::<i32>("a")
                .param::<i32>("b")
                .returns::<i32>()
                .handler(Add)
        })
        .function("Add", |f| f.param::<i32>("a").returns::<i32>().handler(Add))
        .function("TopPartners", |f| {
            f.returns_entities::<BusinessPartner>().handler(Top)
        })
        .function("Numbers", |f| f.returns::<Vec<i32>>().handler(Numbers))
        .function("Point", |f| f.returns::<common::Address>().handler(Point))
        .function("Describe", |f| {
            f.bound_to::<BusinessPartner>()
                .returns::<String>()
                .handler(Describe)
        })
        .function("DescribeAll", |f| {
            f.bound_to_collection::<BusinessPartner>()
                .returns::<String>()
                .handler(Describe)
        })
        .function("Abs", |f| {
            f.param::<i32>("x")
                .returns::<i32>()
                .composable()
                .database_function("abs")
        })
        .function("Hidden", |f| {
            f.returns::<i32>().handler(Numbers).visible_for("admin")
        })
        .function("Missing", |f| f.returns::<i32>())
        .action("Rename", |a| {
            a.bound_to::<BusinessPartner>()
                .param::<String>("name")
                .param::<Option<bool>>("fail")
                .handler(Rename)
        })
        .action("Bump", |a| a.bound_to::<BusinessPartner>().handler(Bump))
        .action("Ping", |a| {
            a.param::<Option<i32>>("x").returns::<i32>().handler(Add)
        })
        .build(db)
        .unwrap()
}

async fn functions(app: App) {
    let doc = app.ok("Add(a=2,b=3)").await;
    assert_eq!(doc["value"], 5);
    assert_eq!(
        doc["@odata.context"],
        "http://test.local/odata/$metadata#Edm.Int32"
    );
    assert_eq!(
        app.ok("Add(a=2)").await["value"],
        2,
        "overload by parameters"
    );
    assert_eq!(app.ok("Add(a=@x,b=1)?@x=4").await["value"], 5);
    assert_eq!(app.status("Add(c=1)").await, 404);
    assert_eq!(app.status("Add(a='x')").await, 400);
    assert_eq!(app.ok("Numbers()").await["value"], json!([1, 2]));
    assert_eq!(app.ok("Point()").await["City"], "Rome");
    assert_eq!(app.status("Missing()").await, 501);
}

async fn entity_results(app: App) {
    let doc = app
        .ok("TopPartners()?$select=Name&$expand=Roles($top=1)")
        .await;
    assert_eq!(doc["value"][0]["Name"], "First Org");
    assert!(doc["value"][0].get("Rating").is_none());
    assert_eq!(doc["value"][0]["Roles"].as_array().unwrap().len(), 1);
    assert_eq!(app.status("TopPartners()?$filter=Id eq '1'").await, 501);
}

async fn bound_functions(app: App) {
    assert_eq!(
        app.ok("BusinessPartners('1')/Test.Describe()").await["value"],
        "entity 1"
    );
    assert_eq!(
        app.ok("BusinessPartners/Test.DescribeAll()").await["value"],
        "set of BusinessPartner"
    );
    assert_eq!(
        app.status("BusinessPartners('nope')/Test.Describe()").await,
        404
    );
    assert_eq!(
        app.status("Roles(PartnerId='1',Code=1)/Test.Describe()")
            .await,
        404
    );
}

async fn database_functions(app: App) {
    assert_eq!(app.ok("Abs(x=-4)").await["value"], 4);
    let ids = app
        .ids("BusinessPartners?$filter=Test.Abs(x=Rating) eq 5")
        .await;
    assert_eq!(ids, ["1"]);
}

async fn actions(app: App) {
    let post = |uri: &'static str, body: serde_json::Value| {
        let app = app.clone();
        async move {
            app.request(
                Method::POST,
                uri,
                &[("content-type", "application/json")],
                &body.to_string(),
            )
            .await
        }
    };
    let r = post(
        "BusinessPartners('4')/Test.Rename",
        json!({"name": "Acted"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(app.ok("BusinessPartners('4')").await["Name"], "Acted");

    let r = post(
        "BusinessPartners('4')/Test.Rename",
        json!({"name": "Lost", "fail": true}),
    )
    .await;
    assert_eq!(r.status.as_u16(), 422);
    assert_eq!(
        app.ok("BusinessPartners('4')").await["Name"],
        "Acted",
        "rolled back"
    );

    assert_eq!(
        post("BusinessPartners('4')/Test.Rename", json!({}))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        post("BusinessPartners('4')/Test.Rename", json!({"name": 5}))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        post(
            "BusinessPartners('4')/Test.Rename",
            json!({"name": "a", "x": 1})
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        post("BusinessPartners('nope')/Test.Rename", json!({"name": "a"}))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(app.status("Ping").await, 405, "actions need POST");
    assert_eq!(post("Ping", json!({})).await.status, StatusCode::OK);
    let annotated = json!({"x": 1, "x@odata.type": "Int32"});
    assert_eq!(
        post("Ping", annotated).await.status,
        StatusCode::OK,
        "as Olingo sends it"
    );
}

async fn if_match_on_bound_operations(app: App) {
    let json = ("content-type", "application/json");
    let rename = "BusinessPartners('4')/Test.Rename";
    let body = json!({"name": "Guarded"}).to_string();
    let stale = [json, ("if-match", "W/\"9\"")];
    let r = app.request(Method::POST, rename, &stale, &body).await;
    assert_eq!(r.status, StatusCode::PRECONDITION_FAILED, "{}", r.text());
    assert_eq!(
        app.ok("BusinessPartners('4')").await["Name"],
        "Fourth",
        "the handler did not run"
    );
    let current = [json, ("if-match", "W/\"1\"")];
    let r = app.request(Method::POST, rename, &current, &body).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(app.ok("BusinessPartners('4')").await["Name"], "Guarded");
    let any = [json, ("if-match", "*")];
    let r = app.request(Method::POST, rename, &any, &body).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    let r = app
        .request(
            Method::POST,
            "BusinessPartners('nope')/Test.Rename",
            &any,
            &body,
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    let describe = "BusinessPartners('1')/Test.Describe()";
    let stale = [("if-match", "W/\"9\"")];
    let r = app.request(Method::GET, describe, &stale, "").await;
    assert_eq!(
        r.status,
        StatusCode::PRECONDITION_FAILED,
        "bound functions too"
    );
    let current = [("if-match", "W/\"3\"")];
    let r = app.request(Method::GET, describe, &current, "").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let r = app
        .request(
            Method::GET,
            "BusinessPartners/Test.DescribeAll()",
            &stale,
            "",
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "a collection has no ETag");

    let batch = json!({"requests": [{
        "id": "1", "atomicityGroup": "g", "method": "post",
        "url": "BusinessPartners('4')/Test.Rename",
        "headers": {"content-type": "application/json", "if-match": "W/\"9\""},
        "body": {"name": "Batched"}
    }]});
    let r = app
        .request(Method::POST, "$batch", &[json], &batch.to_string())
        .await;
    assert_eq!(
        r.json()["responses"][0]["status"],
        412,
        "inside a change set"
    );
}

async fn concurrent_actions_with_one_etag(app: App) {
    let bump = "BusinessPartners('4')/Test.Bump";
    let headers = [("if-match", "W/\"1\"")];
    let (a, b) = tokio::join!(
        app.request(Method::POST, bump, &headers, ""),
        app.request(Method::POST, bump, &headers, ""),
    );
    let mut statuses = [a.status.as_u16(), b.status.as_u16()];
    statuses.sort_unstable();
    assert_eq!(
        statuses,
        [204, 412],
        "only one caller can hold the current ETag"
    );
    assert_eq!(app.ok("BusinessPartners('4')").await["Version"], 2);
}

async fn metadata_lists_operations(app: App) {
    let xml = app.get("$metadata").await.text();
    assert!(xml.contains("<Function Name=\"Add\">"), "{xml}");
    assert!(
        xml.contains("<Function Name=\"Describe\" IsBound=\"true\">"),
        "{xml}"
    );
    assert!(
        xml.contains("<Parameter Name=\"bindingParameter\" Type=\"Test.BusinessPartner\""),
        "{xml}"
    );
    assert!(
        xml.contains("<FunctionImport Name=\"TopPartners\""),
        "{xml}"
    );
    assert!(xml.contains("<ActionImport Name=\"Ping\""), "{xml}");
    assert!(
        xml.contains("<Action Name=\"Rename\" IsBound=\"true\">"),
        "{xml}"
    );
    let doc = app.ok("").await;
    let kinds: Vec<&str> = doc["value"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["name"] == "TopPartners")
        .map(|v| v["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["FunctionImport"]);
}

mod run {
    use super::common;
    use odatabase::{Database, Service};

    fn build() -> impl FnOnce(Database) -> Service {
        super::service
    }

    crate::scenarios!(build();
        functions,
        entity_results,
        bound_functions,
        database_functions,
        actions,
        if_match_on_bound_operations,
        concurrent_actions_with_one_etag,
        metadata_lists_operations,
    );
}

#[tokio::test]
async fn metadata_document() {
    let pool = common::sqlite_pool(common::SCHEMA).await;
    insta::assert_snapshot!(odatabase::edm::to_csdl_xml(service(pool.into()).model()));
}

#[tokio::test]
async fn table_functions_compose_on_postgres() {
    if !common::postgres_can_run() {
        return;
    }
    let server = common::PostgresServer::start().await;
    let schema = format!(
        "{}\nCREATE FUNCTION partners_in(c text) RETURNS SETOF business_partner AS $$ SELECT * FROM business_partner WHERE addr_city = c $$ LANGUAGE sql;",
        common::SCHEMA
    );
    let pool = server.pool(&schema).await;
    let service = common::builder()
        .function("PartnersIn", |f| {
            f.param::<String>("c")
                .returns_entities::<BusinessPartner>()
                .composable()
                .database_function("partners_in")
        })
        .build(pool)
        .unwrap();
    let app = App::new(service);
    assert_eq!(app.ids("PartnersIn(c='Berlin')").await, ["1", "4"]);
    assert_eq!(
        app.ids("PartnersIn(c='Berlin')?$filter=Rating gt 2").await,
        ["1"]
    );
    assert_eq!(
        app.get("PartnersIn(c='Berlin')?$count=true&$top=1")
            .await
            .json()["@odata.count"],
        2
    );
    let doc = app
        .ok("PartnersIn(c='Berlin')?$expand=Roles($top=1)&$top=1")
        .await;
    assert_eq!(doc["value"][0]["Roles"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn table_functions_are_not_available_on_sqlite() {
    let app = common::sqlite(|db| {
        common::builder()
            .function("PartnersIn", |f| {
                f.param::<String>("c")
                    .returns_entities::<BusinessPartner>()
                    .composable()
                    .database_function("partners_in")
            })
            .build(db)
            .unwrap()
    })
    .await;
    assert_eq!(app.status("PartnersIn(c='Berlin')").await, 501);
}
