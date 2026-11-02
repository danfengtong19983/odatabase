mod common;

use async_trait::async_trait;
use axum::Extension;
use axum::http::{Method, StatusCode};
use common::{App, BusinessPartner};
use odatabase::edm::Value;
use odatabase::sql::sea_query::{Expr, ExprTrait};
use odatabase::{
    ClaimValue, Claims, Database, Nav, ODataEntity, ODataError, OperationContext, OperationHandler,
    OperationResult, QueryError, QueryExtension, Service, TableRef,
};
use serde_json::json;

#[derive(ODataEntity)]
#[odata(table = "secret")]
pub struct Secret {
    #[odata(key)]
    pub id: i32,
    pub partner_id: Option<String>,
    #[odata(protected(claim = "tenant"))]
    pub tenant: String,
    #[odata(protected(claim = "level"))]
    pub level: i32,
    #[odata(protected(claim = "region", wildcards))]
    pub region: Option<String>,
    pub body: Option<String>,
    #[odata(visible_for("hr"))]
    pub salary: Option<i32>,
    #[odata(belongs_to(target = BusinessPartner, on(partner_id = "id")))]
    pub partner: Nav,
}

#[derive(ODataEntity)]
#[odata(table = "secret", set = "Archive", visible_for("admin"))]
pub struct Archived {
    #[odata(key)]
    pub id: i32,
    pub body: Option<String>,
}

/// Partners with a link to protected rows, as a second way to reach them.
#[derive(ODataEntity)]
#[odata(table = "business_partner", set = "Holders")]
pub struct Holder {
    #[odata(key)]
    pub id: String,
    #[odata(has_many(target = Secret, on(id = "partner_id")))]
    pub secrets: Nav,
    #[odata(has_many(target = Archived, on(id = "partner_id"), ))]
    pub archived: Nav,
    #[odata(visible_for("admin"), has_many(target = Secret, on(id = "partner_id")))]
    pub audit: Nav,
}

/// A to-one path into a protected type.
#[derive(ODataEntity)]
#[odata(table = "secret_note")]
pub struct Note {
    #[odata(key)]
    pub id: i32,
    #[odata(belongs_to(target = Secret, on(secret_id = "id")))]
    pub secret: Nav,
}

struct OnlyActive;

impl QueryExtension for OnlyActive {
    fn condition(&self, table: &TableRef, claims: &Claims) -> Result<Option<Expr>, QueryError> {
        if claims.groups.iter().any(|g| g == "admin") {
            return Ok(None);
        }
        Ok(Some(table.col("active").eq(true)))
    }
}

#[derive(ODataEntity)]
#[odata(table = "business_partner", set = "ActivePartners", query_extension)]
pub struct ActivePartner {
    #[odata(key)]
    pub id: String,
}

struct Noop;

#[async_trait]
impl OperationHandler for Noop {
    async fn call(&self, _: &OperationContext<'_>) -> Result<OperationResult, ODataError> {
        Ok(OperationResult::Value(Value::Int32(1)))
    }
}

fn service(db: Database) -> Service {
    Service::builder("Sec")
        .entity::<Secret>()
        .entity::<Archived>()
        .entity::<Holder>()
        .entity::<Note>()
        .entity::<ActivePartner>()
        .query_extension::<ActivePartner>(OnlyActive)
        .function("AdminOnly", |f| {
            f.returns::<i32>().handler(Noop).visible_for("admin")
        })
        .build(db)
        .unwrap()
}

fn acme() -> Claims {
    Claims::default()
        .with("tenant", ClaimValue::Equals(Value::String("acme".into())))
        .with("level", ClaimValue::Range(Value::Int32(0), Value::Int32(3)))
        .with("region", ClaimValue::Equals(Value::String("eu-*".into())))
}

fn as_user(app: &App, claims: Claims) -> App {
    App {
        router: app.router.clone().layer(Extension(claims)),
    }
}

fn ids(values: Vec<serde_json::Value>) -> Vec<i64> {
    values.iter().map(|v| v.as_i64().unwrap()).collect()
}

async fn row_protection_on_reads(app: App) {
    assert_eq!(
        app.status("Secrets").await,
        403,
        "a missing claim denies access"
    );
    let user = as_user(&app, acme());
    assert_eq!(
        ids(user.column("Secrets", "Id").await),
        [1],
        "tenant, level and wildcard apply"
    );
    assert_eq!(user.status("Secrets(1)").await, 200);
    assert_eq!(
        user.status("Secrets(2)").await,
        404,
        "outside the level range"
    );
    assert_eq!(user.status("Secrets(3)").await, 404, "another tenant");
    assert_eq!(user.get("Secrets/$count").await.text(), "1");
    let doc = user.ok("Secrets?$count=true").await;
    assert_eq!(doc["@odata.count"], 1);
    let wide = Claims::default()
        .with("tenant", ClaimValue::Equals(Value::String("acme".into())))
        .with("tenant", ClaimValue::Equals(Value::String("globex".into())))
        .with("level", ClaimValue::Range(Value::Int32(0), Value::Int32(9)))
        .with("region", ClaimValue::Equals(Value::String("*".into())));
    assert_eq!(
        ids(as_user(&app, wide)
            .column("Secrets?$orderby=Id", "Id")
            .await),
        [1, 2, 3]
    );
    let narrow = acme().with(
        "region",
        ClaimValue::Equals(Value::String("us-eas+".into())),
    );
    assert_eq!(
        ids(as_user(&app, narrow)
            .column("Secrets?$orderby=Id", "Id")
            .await),
        [1],
        "level 5 stays out"
    );
}

async fn protection_follows_every_path(app: App) {
    let user = as_user(&app, acme());
    assert_eq!(
        ids(user.column("Holders('1')/Secrets", "Id").await),
        [1],
        "navigation path"
    );
    let doc = user.ok("Holders('1')?$expand=Secrets").await;
    assert_eq!(doc["Secrets"].as_array().unwrap().len(), 1, "$expand");
    let doc = user
        .ok("Holders('1')?$expand=Secrets($count=true;$top=5)")
        .await;
    assert_eq!(doc["Secrets@odata.count"], 1, "$expand with $count");
    let doc = user.ok("Holders('1')?$expand=Secrets/$count").await;
    assert_eq!(doc["Secrets@odata.count"], 1, "$expand of $count");
    assert_eq!(
        user.get("Holders('1')/Secrets/$count").await.text(),
        "1",
        "path $count"
    );
    let found = user.ids("Holders?$filter=Secrets/$count eq 2").await;
    assert!(found.is_empty(), "$count in $filter");
    let found = user
        .ids("Holders?$filter=Secrets/any(s: s/Level eq 5)")
        .await;
    assert!(found.is_empty(), "any in $filter");
    let found = user
        .ids("Holders?$filter=Secrets/all(s: s/Level lt 5)&$orderby=Id")
        .await;
    assert_eq!(found[0], "1", "all sees only permitted rows");
    let found = user
        .ids("Holders?$orderby=Secrets/$count desc&$top=1")
        .await;
    assert_eq!(found.len(), 1);
    assert_eq!(
        app.status("Holders('1')/Secrets").await,
        403,
        "no claims at all"
    );
    assert_eq!(app.status("Holders?$expand=Secrets").await, 403);
    assert_eq!(app.status("Holders?$filter=Secrets/any()").await, 403);
    assert_eq!(app.status("Holders?$filter=Secrets/$count gt 0").await, 403);
}

async fn protection_on_writes(app: App) {
    let user = as_user(&app, acme());
    let json = ("content-type", "application/json");
    let r = user
        .request(
            Method::PATCH,
            "Secrets(2)",
            &[json],
            &json!({"Body": "x"}).to_string(),
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::NOT_FOUND,
        "a hidden row cannot be changed"
    );
    let r = user.request(Method::DELETE, "Secrets(3)", &[], "").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "or deleted");
    let r = user
        .request(
            Method::PATCH,
            "Secrets(1)",
            &[json],
            &json!({"Body": "ok"}).to_string(),
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    let r = user
        .request(
            Method::PATCH,
            "Secrets(1)",
            &[json],
            &json!({"Tenant": "globex"}).to_string(),
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::FORBIDDEN,
        "a row cannot leave the caller's scope"
    );
    assert_eq!(user.ok("Secrets(1)").await["Tenant"], "acme", "rolled back");
    let row = json!({"Id": 9, "Tenant": "globex", "Level": 1, "Region": "eu-x"}).to_string();
    let r = user.request(Method::POST, "Secrets", &[json], &row).await;
    assert_eq!(
        r.status,
        StatusCode::FORBIDDEN,
        "a row cannot be created outside it"
    );
    let admin = Claims::default()
        .with("tenant", ClaimValue::Equals(Value::String("globex".into())))
        .with("level", ClaimValue::Range(Value::Int32(0), Value::Int32(9)))
        .with("region", ClaimValue::Equals(Value::String("*".into())));
    assert_eq!(
        as_user(&app, admin).status("Secrets(9)").await,
        404,
        "and nothing was stored"
    );
    let r = user
        .request(
            Method::POST,
            "Secrets",
            &[json],
            &json!({"Id": 10, "Tenant": "acme", "Level": 1, "Region": "eu-x"}).to_string(),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
}

async fn group_visibility(app: App) {
    let user = as_user(&app, acme());
    let doc = user.ok("Secrets(1)").await;
    assert!(doc.get("Salary").is_none(), "hidden property");
    assert_eq!(user.status("Secrets(1)?$select=Salary").await, 403);
    assert_eq!(user.status("Secrets(1)/Salary").await, 403);
    assert_eq!(user.status("Secrets(1)/Salary/$value").await, 403);
    let hidden_write = user
        .request(Method::DELETE, "Secrets(1)/Salary", &[], "")
        .await;
    assert_eq!(hidden_write.status, StatusCode::FORBIDDEN);
    assert_eq!(user.status("Secrets?$filter=Salary gt 1").await, 403);
    assert_eq!(user.status("Secrets?$orderby=Salary").await, 403);
    let json = ("content-type", "application/json");
    let r = user
        .request(
            Method::PATCH,
            "Secrets(1)",
            &[json],
            &json!({"Salary": 1}).to_string(),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let hr = as_user(&app, acme().with_group("hr"));
    assert_eq!(hr.ok("Secrets(1)").await["Salary"], 100);

    assert_eq!(user.status("Archive").await, 403, "hidden type");
    assert_eq!(user.status("Holders?$expand=Archived").await, 403);
    assert_eq!(user.status("AdminOnly()").await, 403, "hidden function");
    assert_eq!(
        user.status("Holders('1')/Audit").await,
        403,
        "hidden navigation"
    );
    assert_eq!(user.status("Holders?$expand=Audit").await, 403);
    let doc = user.ok("Holders('1')?$expand=*").await;
    assert!(doc.get("Audit").is_none(), "$expand=* skips what is hidden");
    assert!(doc.get("Archived").is_none());
    assert!(doc["Secrets"].is_array());

    let xml = user.get("$metadata").await.text();
    assert!(!xml.contains("Salary"), "{xml}");
    assert!(!xml.contains("Archived"), "{xml}");
    assert!(!xml.contains("AdminOnly"), "{xml}");
    assert!(!xml.contains("Audit"), "{xml}");
    let doc = user.ok("").await;
    assert!(!doc.to_string().contains("Archive"));
    let admin = as_user(&app, acme().with_group("admin").with_group("hr"));
    let xml = admin.get("$metadata").await.text();
    assert!(xml.contains("Salary") && xml.contains("Archived") && xml.contains("AdminOnly"));
    assert_eq!(admin.column("Archive?$orderby=Id", "Id").await.len(), 3);
}

async fn aggregates_respect_protection(app: App) {
    let count = "Secrets?$apply=aggregate($count as N)";
    assert_eq!(
        app.status(count).await,
        403,
        "a missing claim denies aggregates too"
    );
    let user = as_user(&app, acme());
    assert_eq!(
        user.ok(count).await["value"][0]["N"],
        1,
        "only permitted rows are counted"
    );
    let total = "Secrets?$apply=aggregate(Level with sum as T)";
    assert_eq!(user.ok(total).await["value"][0]["T"], 1);
    let groups = user
        .ok("Secrets?$apply=groupby((Tenant),aggregate($count as N))")
        .await;
    assert_eq!(
        groups["value"].as_array().unwrap().len(),
        1,
        "no other tenant appears"
    );
    assert_eq!(
        user.ok("Holders('1')/Secrets?$apply=aggregate($count as N)")
            .await["value"][0]["N"],
        1
    );
    let through = "Secrets?$apply=groupby((Partner/Id),aggregate($count as N))";
    assert_eq!(user.status(through).await, 200);

    let hidden = [
        "Secrets?$apply=aggregate(Salary with sum as T)",
        "Secrets?$apply=groupby((Salary))",
        "Secrets?$apply=filter(Salary gt 1)/aggregate($count as N)",
        "Secrets?$apply=compute(Salary mul 1 as S)/aggregate(S with sum as T)",
        "Secrets?$apply=compute(Salary mul 1 as S)&$select=Id",
    ];
    for uri in hidden {
        assert_eq!(user.status(uri).await, 403, "{uri}");
    }
    let hr = as_user(&app, acme().with_group("hr"));
    let doc = hr
        .ok("Secrets?$apply=aggregate(Salary with sum as T)")
        .await;
    assert_eq!(
        doc["value"][0]["T"], 100,
        "visible, and still limited to permitted rows"
    );
    assert_eq!(
        user.status("Archive?$apply=aggregate($count as N)").await,
        403
    );
}

async fn upsert_and_deep_update_respect_protection(app: App) {
    let json = ("content-type", "application/json");
    let user = as_user(&app, acme());
    let row = json!({"Tenant": "acme", "Level": 1, "Region": "eu-x", "Body": "x"}).to_string();
    let r = user
        .request(Method::PATCH, "Secrets(3)", &[json], &row)
        .await;
    assert_eq!(
        r.status,
        StatusCode::NOT_FOUND,
        "a hidden row reads as missing: {}",
        r.text()
    );
    let free = user
        .request(Method::PATCH, "Secrets(50)", &[json], &row)
        .await;
    assert_eq!(
        free.status, r.status,
        "a free key answers the same, so nothing can be probed"
    );
    let r = user
        .request(Method::PUT, "Secrets(50)", &[json], &row)
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    let all = Claims::default()
        .with("tenant", ClaimValue::Equals(Value::String("acme".into())))
        .with("tenant", ClaimValue::Equals(Value::String("globex".into())))
        .with("level", ClaimValue::Range(Value::Int32(0), Value::Int32(9)))
        .with("region", ClaimValue::Equals(Value::String("*".into())));
    let admin = as_user(&app, all);
    assert_eq!(admin.ok("Secrets(3)").await["Body"], "c", "untouched");
    assert_eq!(
        admin.status("Secrets(50)").await,
        404,
        "and nothing was stored"
    );

    let nested = json!({"Secrets": [{"Id": 3, "Body": "stolen"}]}).to_string();
    let r = user
        .request(Method::PATCH, "Holders('1')", &[json], &nested)
        .await;
    assert_eq!(
        r.status,
        StatusCode::NOT_FOUND,
        "a nested hidden row too: {}",
        r.text()
    );
    let free = json!({"Secrets": [{"Id": 52, "Body": "x"}]}).to_string();
    let r = user
        .request(Method::PATCH, "Holders('1')", &[json], &free)
        .await;
    assert_eq!(
        r.status,
        StatusCode::NOT_FOUND,
        "and a free nested key answers the same"
    );
    assert_eq!(admin.ok("Secrets(3)").await["Body"], "c");
    assert_eq!(
        admin.ok("Secrets(1)").await["PartnerId"],
        "1",
        "the failed updates rolled back"
    );

    let r = user
        .request(
            Method::PATCH,
            "Holders('1')",
            &[json],
            &json!({"Secrets": []}).to_string(),
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(
        admin.ok("Secrets(1)").await["PartnerId"],
        json!(null),
        "a visible child is unlinked"
    );
    assert_eq!(
        admin.ok("Secrets(2)").await["PartnerId"],
        "1",
        "a hidden child stays linked"
    );

    let r = user
        .request(
            Method::PATCH,
            "Holders('1')",
            &[json],
            &json!({"Audit": []}).to_string(),
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::FORBIDDEN,
        "a hidden navigation cannot be written"
    );
    let r = user
        .request(
            Method::PATCH,
            "Holders('1')",
            &[json],
            &json!({"Secrets": [{"Id": 1, "Salary": 1}]}).to_string(),
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::FORBIDDEN,
        "nor a hidden property of a child"
    );
}

async fn post_cannot_probe_hidden_keys(app: App) {
    let json = ("content-type", "application/json");
    let user = as_user(&app, acme());
    let post = |id: i32, tenant: &str| {
        let body = json!({"Id": id, "Tenant": tenant, "Level": 1, "Region": "eu-x"}).to_string();
        let user = user.clone();
        async move { user.request(Method::POST, "Secrets", &[json], &body).await }
    };
    let free = post(70, "globex").await;
    let taken = post(3, "globex").await;
    assert_eq!(free.status, StatusCode::FORBIDDEN, "{}", free.text());
    assert_eq!(
        taken.status, free.status,
        "out of scope: a taken key looks like a free one"
    );
    assert_eq!(taken.text(), free.text());
    let visible = post(1, "acme").await;
    assert_eq!(
        visible.status,
        StatusCode::CONFLICT,
        "a row the caller can see is a real conflict"
    );
    let hidden = post(3, "acme").await;
    assert_eq!(
        hidden.status,
        StatusCode::FORBIDDEN,
        "no 409 for a row the caller cannot see"
    );
    let nested = json!({"Id": "x1", "Secrets": [{"Id": 3, "Tenant": "globex", "Level": 1}]});
    let r = user
        .request(Method::POST, "Holders", &[json], &nested.to_string())
        .await;
    assert_ne!(
        r.status,
        StatusCode::CONFLICT,
        "nor in a deep insert: {}",
        r.text()
    );
}

async fn to_one_paths_respect_protection(app: App) {
    assert_eq!(
        app.status("Notes?$orderby=Secret/Body").await,
        403,
        "a missing claim denies access"
    );
    let user = as_user(&app, acme());
    let ordered = user
        .column("Notes?$orderby=Secret/Body desc,Id", "Id")
        .await;
    assert_eq!(
        ids(ordered),
        [1, 2, 3],
        "hidden targets read as null, and no note is lost"
    );
    let filtered = user.column("Notes?$filter=Secret/Body eq 'b'", "Id").await;
    assert!(filtered.is_empty(), "a hidden value cannot be matched");
    let filtered = user.column("Notes?$filter=Secret/Body eq 'a'", "Id").await;
    assert_eq!(ids(filtered), [1]);
    let doc = user
        .ok("Notes?$apply=groupby((Secret/Body),aggregate($count as N))")
        .await;
    assert_eq!(doc["value"][0]["Secret"], json!(null));
    assert_eq!(
        doc["value"][0]["N"], 2,
        "hidden values collapse into the null group"
    );
    assert_eq!(doc["value"][1]["Secret"]["Body"], "a");
    assert_eq!(
        doc["value"].as_array().unwrap().len(),
        2,
        "no hidden value appears"
    );
    let total = user
        .ok("Notes?$apply=aggregate(Secret/Level with sum as T)")
        .await;
    assert_eq!(total["value"][0]["T"], 1, "hidden values are not summed");
    assert_eq!(user.status("Notes?$orderby=Secret/Salary").await, 403);
    assert_eq!(
        user.status("Notes?$apply=groupby((Secret/Salary))").await,
        403
    );
    assert_eq!(user.status("Notes?$filter=Secret/Salary gt 1").await, 403);
}

async fn query_extensions(app: App) {
    let active = "ActivePartners?$apply=aggregate($count as N)";
    assert_eq!(
        app.ok(active).await["value"][0]["N"],
        3,
        "extensions apply to aggregates"
    );

    assert_eq!(app.ids("ActivePartners").await, ["1", "3", "4"]);
    assert_eq!(app.status("ActivePartners('2')").await, 404);
    let admin = as_user(&app, Claims::default().with_group("admin"));
    assert_eq!(admin.ids("ActivePartners").await.len(), 4);
}

async fn batches_keep_the_caller(app: App) {
    let body = json!({"requests": [{"id": "1", "method": "get", "url": "Secrets"}]});
    let user = as_user(&app, acme());
    let ct = ("content-type", "application/json");
    let doc = user
        .request(Method::POST, "$batch", &[ct], &body.to_string())
        .await
        .json();
    assert_eq!(
        doc["responses"][0]["body"]["value"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let doc = app
        .request(Method::POST, "$batch", &[ct], &body.to_string())
        .await
        .json();
    assert_eq!(doc["responses"][0]["status"], 403);
}

mod run {
    use super::common;
    use odatabase::{Database, Service};

    fn build() -> impl FnOnce(Database) -> Service {
        super::service
    }

    crate::scenarios!(build();
        row_protection_on_reads,
        protection_follows_every_path,
        protection_on_writes,
        group_visibility,
        aggregates_respect_protection,
        upsert_and_deep_update_respect_protection,
        post_cannot_probe_hidden_keys,
        to_one_paths_respect_protection,
        query_extensions,
        batches_keep_the_caller,
    );
}

#[tokio::test]
async fn a_declared_extension_must_be_registered() {
    let pool = common::sqlite_pool(common::SCHEMA).await;
    let result = Service::builder("Sec")
        .entity::<ActivePartner>()
        .build(pool);
    assert!(result.err().unwrap().0.contains("no query extension"));
}
