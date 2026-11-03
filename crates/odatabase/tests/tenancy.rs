mod common;

use axum::Extension;
use axum::http::{Method, StatusCode};
use common::App;
use odatabase::edm::Value;
use odatabase::sql::{ClaimValue, Session, record_change};
use odatabase::{Claims, Database, Nav, ODataEntity, Service, ServiceBuilder};
use serde_json::{Value as Json, json};
use uuid::Uuid;

const ACME: &str = "aaaaaaaa-0000-0000-0000-000000000001";
const GLOBEX: &str = "bbbbbbbb-0000-0000-0000-000000000002";

/// Keys are unique per tenant, so both tenants hold a row with the same key.
const SCHEMA: &str = "
CREATE TABLE person (
  tenant_id UUID NOT NULL, id INTEGER NOT NULL, name TEXT NOT NULL,
  PRIMARY KEY (tenant_id, id)
);
CREATE TABLE customer (
  tenant_id UUID NOT NULL, id INTEGER NOT NULL, name TEXT NOT NULL, rating INTEGER,
  PRIMARY KEY (tenant_id, id)
);
CREATE TABLE deal (
  tenant_id UUID NOT NULL, id INTEGER NOT NULL, title TEXT NOT NULL, customer_id INTEGER,
  PRIMARY KEY (tenant_id, id)
);
CREATE TABLE deal_tag (
  tenant_id UUID NOT NULL, deal_id INTEGER NOT NULL, position INTEGER NOT NULL, tag TEXT NOT NULL,
  PRIMARY KEY (tenant_id, deal_id, position)
);
CREATE TABLE deal_member (
  tenant_id UUID NOT NULL, deal_id INTEGER NOT NULL, person_id INTEGER NOT NULL,
  PRIMARY KEY (tenant_id, deal_id, person_id)
);
INSERT INTO person (tenant_id, id, name) VALUES
  ('aaaaaaaa-0000-0000-0000-000000000001', 1, 'Ann'),
  ('bbbbbbbb-0000-0000-0000-000000000002', 1, 'Bob');
INSERT INTO customer (tenant_id, id, name, rating) VALUES
  ('aaaaaaaa-0000-0000-0000-000000000001', 1, 'Acme One', 5),
  ('aaaaaaaa-0000-0000-0000-000000000001', 2, 'Acme Two', 3),
  ('bbbbbbbb-0000-0000-0000-000000000002', 1, 'Globex One', 9);
INSERT INTO deal (tenant_id, id, title, customer_id) VALUES
  ('aaaaaaaa-0000-0000-0000-000000000001', 1, 'Acme deal', 1),
  ('bbbbbbbb-0000-0000-0000-000000000002', 1, 'Globex deal', 1),
  ('bbbbbbbb-0000-0000-0000-000000000002', 2, 'Globex only', 1);
INSERT INTO deal_tag (tenant_id, deal_id, position, tag) VALUES
  ('aaaaaaaa-0000-0000-0000-000000000001', 1, 1, 'acme-tag'),
  ('bbbbbbbb-0000-0000-0000-000000000002', 1, 1, 'globex-tag');
";

#[derive(ODataEntity)]
#[odata(table = "person", set = "People")]
pub struct Person {
    #[odata(tenant)]
    pub tenant_id: Uuid,
    #[odata(key)]
    pub id: i32,
    pub name: String,
}

#[derive(ODataEntity)]
#[odata(table = "customer")]
pub struct Customer {
    #[odata(tenant)]
    pub tenant_id: Uuid,
    #[odata(key)]
    pub id: i32,
    pub name: String,
    pub rating: Option<i32>,
    #[odata(has_many(target = Deal, on(id = "customer_id")))]
    pub deals: Nav,
}

#[derive(ODataEntity)]
#[odata(table = "deal", track_changes)]
pub struct Deal {
    #[odata(tenant)]
    pub tenant_id: Uuid,
    #[odata(key)]
    pub id: i32,
    pub title: String,
    pub customer_id: Option<i32>,
    #[odata(collection(
        table = "deal_tag",
        owner(id = "deal_id"),
        value_column = "tag",
        order_column = "position",
        tenant_column = "tenant_id"
    ))]
    pub tags: Vec<String>,
    #[odata(belongs_to(target = Customer, on(customer_id = "id")))]
    pub customer: Nav,
    #[odata(has_many(
        target = Person,
        join_table(
            table = "deal_member",
            source(id = "deal_id"),
            target(person_id = "id"),
            tenant_column = "tenant_id"
        )
    ))]
    pub members: Nav,
}

fn builder() -> ServiceBuilder {
    Service::builder("Tenancy")
        .entity::<Person>()
        .entity::<Customer>()
        .entity::<Deal>()
}

fn tenant(id: &str) -> Claims {
    Claims::default().with("tenant", ClaimValue::Equals(Value::String(id.to_owned())))
}

fn as_user(app: &App, claims: Claims) -> App {
    App {
        router: app.router.clone().layer(Extension(claims)),
    }
}

struct Tenants {
    db: Database,
    plain: App,
    acme: App,
    globex: App,
}

impl Tenants {
    async fn new(db: Database) -> Self {
        db.create_change_log().await.unwrap();
        let plain = App::new(builder().build(db.clone()).unwrap());
        let acme = as_user(&plain, tenant(ACME));
        let globex = as_user(&plain, tenant(GLOBEX));
        Self {
            db,
            plain,
            acme,
            globex,
        }
    }
}

async fn write(app: &App, method: Method, uri: &str, body: Json) -> common::Response {
    app.request(
        method,
        uri,
        &[("content-type", "application/json")],
        &body.to_string(),
    )
    .await
}

fn rows(doc: &Json, field: &str) -> Vec<Json> {
    let rows = doc["value"].as_array().unwrap_or_else(|| panic!("{doc}"));
    rows.iter().map(|r| r[field].clone()).collect()
}

async fn reads(t: Tenants) {
    let a = &t.acme;
    assert_eq!(
        rows(&a.ok("Customers?$orderby=Id").await, "Name"),
        [json!("Acme One"), json!("Acme Two")]
    );
    assert_eq!(
        a.ok("Customers(1)").await["Name"],
        "Acme One",
        "same key, own row"
    );
    assert_eq!(t.globex.ok("Customers(1)").await["Name"], "Globex One");
    assert_eq!(
        t.globex.status("Customers(2)").await,
        404,
        "another tenant's row is missing"
    );
    assert_eq!(a.get("Customers/$count").await.text(), "2");
    assert_eq!(a.get("Deals/$count").await.text(), "1");

    let doc = a.ok("Customers(1)?$expand=Deals").await;
    assert_eq!(
        rows(&json!({"value": doc["Deals"]}), "Title"),
        [json!("Acme deal")],
        "$expand"
    );
    let doc = a
        .ok("Customers(1)?$expand=Deals($apply=aggregate($count as N))")
        .await;
    assert_eq!(doc["Deals"][0]["N"], 1, "per-parent pipeline");
    assert_eq!(
        a.ok("Deals(1)?$expand=Customer").await["Customer"]["Name"],
        "Acme One"
    );
    assert_eq!(
        a.ok("Deals(1)/Customer").await["Name"],
        "Acme One",
        "navigation path"
    );
    assert_eq!(
        rows(&a.ok("Customers(1)/Deals").await, "Title"),
        [json!("Acme deal")]
    );
    assert_eq!(
        a.ok("Deals(1)/Tags").await["value"],
        json!(["acme-tag"]),
        "collection property"
    );

    let doc = a
        .ok("Customers?$filter=Deals/any(d: d/Title eq 'Globex deal')")
        .await;
    assert_eq!(rows(&doc, "Id"), [] as [Json; 0], "lambda");
    for filter in ["Tags/any(t: t eq 'globex-tag')", "Tags/$count eq 2"] {
        let doc = a.ok(&format!("Deals?$filter={filter}")).await;
        assert_eq!(rows(&doc, "Id"), [] as [Json; 0], "{filter}");
    }
    let doc = a.ok("Deals?$filter=Customer/Name eq 'Acme One'").await;
    assert_eq!(rows(&doc, "Id"), [json!(1)], "to-one path");
    let doc = a.ok("Customers?$orderby=Deals/$count desc,Id").await;
    assert_eq!(rows(&doc, "Id"), [json!(1), json!(2)], "$count path");

    let doc = a
        .ok("Customers?$apply=aggregate(Rating with sum as T)")
        .await;
    assert_eq!(doc["value"][0]["T"], 8, "$apply");
    let doc = a
        .ok("Customers?$apply=join(Deals as D)/aggregate($count as N)")
        .await;
    assert_eq!(doc["value"][0]["N"], 1, "join");
    let doc = a.ok("Customers?$apply=topcount(1,Rating)").await;
    assert_eq!(rows(&doc, "Name"), [json!("Acme One")]);

    assert_eq!(a.ok("$entity?$id=Customers(1)").await["Name"], "Acme One");
    let doc = a.ok("$crossjoin(Customers,Deals)?$count=true").await;
    assert_eq!(doc["@odata.count"], 2, "two customers, one deal");
    let doc = a.ok("$all").await;
    assert_eq!(doc["value"].as_array().unwrap().len(), 4, "{doc}");
    let body = doc.to_string();
    assert!(!body.contains("Globex"), "{body}");

    let batch = json!({"requests": [{"id": "1", "method": "GET", "url": "Customers(1)"}]});
    let r = write(a, Method::POST, "$batch", batch).await;
    assert_eq!(r.json()["responses"][0]["body"]["Name"], "Acme One");
}

async fn the_column_is_invisible(t: Tenants) {
    let a = &t.acme;
    let doc = a.ok("Customers(1)").await;
    assert!(doc.get("TenantId").is_none(), "{doc}");
    let xml = a.get("$metadata").await.text();
    assert!(
        !xml.contains("TenantId") && !xml.contains("tenant_id"),
        "{xml}"
    );
    for uri in [
        "Customers?$filter=TenantId eq null",
        "Customers?$orderby=TenantId",
        "Customers?$select=TenantId",
        "Customers?$apply=groupby((TenantId))",
        "Customers?$compute=TenantId as T",
    ] {
        assert_eq!(a.status(uri).await, 400, "{uri}");
    }
    let body = json!({"Id": 9, "Name": "Smuggled", "TenantId": GLOBEX});
    let r = write(a, Method::POST, "Customers", body).await;
    assert_eq!(
        r.status,
        StatusCode::BAD_REQUEST,
        "a body can't name a tenant"
    );
    let body = json!({"tenant_id": GLOBEX});
    let r = write(a, Method::PATCH, "Customers(1)", body).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

async fn claims(t: Tenants) {
    assert_eq!(t.plain.status("Customers").await, 403, "no claim, no rows");
    assert_eq!(t.plain.status("$entity?$id=Customers(1)").await, 403);
    let r = write(
        &t.plain,
        Method::POST,
        "Customers",
        json!({"Id": 9, "Name": "x"}),
    )
    .await;
    assert_eq!(r.status.as_u16(), 403);

    let bad = as_user(&t.plain, tenant("not-a-uuid"));
    assert_eq!(
        bad.status("Customers").await,
        403,
        "an invalid claim reaches nothing"
    );
    let range = Claims::default().with(
        "tenant",
        ClaimValue::Range(Value::Int32(0), Value::Int32(9)),
    );
    assert_eq!(as_user(&t.plain, range).status("Customers").await, 403);

    let operator = as_user(&t.plain, Claims::default().with("tenant", ClaimValue::Any));
    assert_eq!(
        operator.get("Customers/$count").await.text(),
        "3",
        "reads every tenant"
    );
    for (method, uri, body) in [
        (Method::POST, "Customers", json!({"Id": 9, "Name": "x"})),
        (Method::PATCH, "Customers(2)", json!({"Name": "x"})),
        (Method::DELETE, "Customers(2)", json!({})),
    ] {
        let r = write(&operator, method, uri, body).await;
        assert_eq!(
            r.status.as_u16(),
            403,
            "{uri}: an operator picks a tenant to write"
        );
    }

    let both = tenant(ACME).with(
        "tenant",
        ClaimValue::Equals(Value::String(GLOBEX.to_owned())),
    );
    let both = as_user(&t.plain, both);
    assert_eq!(both.get("Customers/$count").await.text(), "3");
    let r = write(&both, Method::PATCH, "Customers(2)", json!({"Name": "x"})).await;
    assert_eq!(r.status.as_u16(), 403);
    assert_eq!(t.acme.ok("Customers(2)").await["Name"], "Acme Two");
}

async fn writes(t: Tenants) {
    let (a, g) = (&t.acme, &t.globex);
    let r = write(
        a,
        Method::POST,
        "Customers",
        json!({"Id": 7, "Name": "Acme Seven"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    assert_eq!(
        a.ok("Customers(7)").await["Name"],
        "Acme Seven",
        "stamped with the claim"
    );
    assert_eq!(g.status("Customers(7)").await, 404);
    let r = write(
        g,
        Method::POST,
        "Customers",
        json!({"Id": 7, "Name": "Globex Seven"}),
    )
    .await;
    assert_eq!(
        r.status,
        StatusCode::CREATED,
        "the key is free in this tenant"
    );
    let r = write(
        a,
        Method::POST,
        "Customers",
        json!({"Id": 7, "Name": "Again"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "a real conflict stays one");

    let r = write(
        a,
        Method::PATCH,
        "Customers(1)",
        json!({"Name": "Acme Renamed"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(a.ok("Customers(1)").await["Name"], "Acme Renamed");
    assert_eq!(
        g.ok("Customers(1)").await["Name"],
        "Globex One",
        "the same key elsewhere"
    );

    let r = write(g, Method::PATCH, "Customers(2)", json!({"Name": "Its own"})).await;
    assert_eq!(
        r.status,
        StatusCode::CREATED,
        "a free key here, whatever others hold"
    );
    assert_eq!(
        a.ok("Customers(2)").await["Name"],
        "Acme Two",
        "theirs is untouched"
    );

    let r = write(a, Method::DELETE, "Deals(1)", json!({})).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(a.status("Deals(1)").await, 404);
    assert_eq!(
        g.ok("Deals(1)").await["Title"],
        "Globex deal",
        "the same key elsewhere"
    );
    assert_eq!(g.ok("Deals(1)/Tags").await["value"], json!(["globex-tag"]));
    let r = write(a, Method::DELETE, "Deals(2)", json!({})).await;
    assert_eq!(r.status.as_u16(), 404);
    assert_eq!(g.status("Deals(2)").await, 200);
}

async fn upsert_and_deep(t: Tenants) {
    let (a, g) = (&t.acme, &t.globex);
    let r = write(
        a,
        Method::PATCH,
        "Customers(8)",
        json!({"Name": "Upserted"}),
    )
    .await;
    assert_eq!(
        r.status,
        StatusCode::CREATED,
        "unlike hidden rows, tenants can upsert: {}",
        r.text()
    );
    assert_eq!(g.status("Customers(8)").await, 404);

    let body = json!({"Id": 20, "Title": "Deep", "Tags": ["one", "two"], "Customer": {"Id": 21, "Name": "Nested"}});
    let r = write(a, Method::POST, "Deals", body).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    assert_eq!(
        a.ok("Deals(20)/Tags").await["value"],
        json!(["one", "two"]),
        "tenant on rows of the collection"
    );
    assert_eq!(
        a.ok("Customers(21)").await["Name"],
        "Nested",
        "nested rows too"
    );
    assert_eq!(g.status("Customers(21)").await, 404);

    let r = write(
        a,
        Method::POST,
        "Deals(20)/Tags?$index=0",
        json!({"value": "zero"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(
        a.ok("Deals(20)/Tags").await["value"],
        json!(["zero", "one", "two"])
    );

    let r = write(
        a,
        Method::PATCH,
        "Customers(1)",
        json!({"Deals@delta": [{"Id": 30, "Title": "Delta"}]}),
    )
    .await;
    assert_eq!(
        r.status,
        StatusCode::NO_CONTENT,
        "keyed children work: {}",
        r.text()
    );
    assert_eq!(a.ok("Deals(30)").await["CustomerId"], 1);
    let r = write(
        a,
        Method::PATCH,
        "Deals",
        json!({"value": [{"Id": 31, "Title": "Patched"}]}),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(g.status("Deals(31)").await, 404);
    let r = write(
        a,
        Method::PATCH,
        "Deals",
        json!({"value": [{"@removed": {"reason": "deleted"}, "@id": "Deals(2)"}]}),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(
        g.status("Deals(2)").await,
        200,
        "a removal can't reach across"
    );
}

async fn links(t: Tenants) {
    let (a, g) = (&t.acme, &t.globex);
    let r = write(
        a,
        Method::POST,
        "Deals(1)/Members/$ref",
        json!({"@odata.id": "People(1)"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(
        rows(&a.ok("Deals(1)/Members").await, "Name"),
        [json!("Ann")],
        "the link has the tenant"
    );
    assert_eq!(
        rows(&g.ok("Deals(1)/Members").await, "Name"),
        [] as [Json; 0]
    );

    let doc = g.ok("Customers(1)?$expand=Deals($expand=Members)").await;
    assert!(
        !doc.to_string().contains("Bob"),
        "expand follows links of its own tenant: {doc}"
    );
    let doc = a.ok("Deals?$filter=Members/any()&$count=true").await;
    assert_eq!(doc["@odata.count"], 1);
    let doc = g.ok("Deals?$filter=Members/any()&$count=true").await;
    assert_eq!(doc["@odata.count"], 0, "and lambdas");
    let doc = g
        .ok("Deals?$apply=join(Members as M)/aggregate($count as N)")
        .await;
    assert_eq!(doc["value"][0]["N"], 0, "and joins");
    let doc = g
        .ok("Deals?$apply=outerjoin(Members as M)/aggregate($count as N)")
        .await;
    assert_eq!(
        doc["value"][0]["N"], 2,
        "a link of another tenant doesn't multiply rows"
    );

    let r = write(g, Method::PUT, "Deals(1)/Tags", json!({"value": ["new"]})).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(g.ok("Deals(1)/Tags").await["value"], json!(["new"]));
    assert_eq!(
        a.ok("Deals(1)/Tags").await["value"],
        json!(["acme-tag"]),
        "the same owner key elsewhere"
    );

    let r = write(
        g,
        Method::POST,
        "Deals(1)/Members/$ref",
        json!({"@odata.id": "People(1)"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    let r = g
        .request(
            Method::DELETE,
            "Deals(1)/Members/$ref?$id=People(1)",
            &[],
            "",
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(
        rows(&a.ok("Deals(1)/Members").await, "Name"),
        [json!("Ann")],
        "only its own link went"
    );

    let body = json!({"Id": 40, "Title": "Bound", "Customer@odata.bind": "Customers(2)"});
    let r = write(g, Method::POST, "Deals", body).await;
    assert_ne!(
        r.status,
        StatusCode::CREATED,
        "another tenant's row can't be bound"
    );
    let r = write(
        g,
        Method::POST,
        "Deals(1)/Members/$ref",
        json!({"@odata.id": "People(9)"}),
    )
    .await;
    assert_eq!(r.status.as_u16(), 404);
}

async fn change_tracking(t: Tenants) {
    let (a, g) = (&t.acme, &t.globex);
    let track = |app: &App, uri: &'static str| {
        let app = app.clone();
        async move {
            let r = app
                .request(Method::GET, uri, &[("prefer", "odata.track-changes")], "")
                .await;
            let doc = r.json();
            doc["@odata.deltaLink"]
                .as_str()
                .unwrap_or_else(|| panic!("{doc}"))
                .trim_start_matches("http://test.local/odata/")
                .to_owned()
        }
    };
    let acme_link = track(a, "Deals").await;
    let globex_link = track(g, "Deals").await;

    let r = write(
        a,
        Method::PATCH,
        "Deals(1)",
        json!({"Title": "Acme changed"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    write(g, Method::DELETE, "Deals(2)", json!({})).await;

    let doc = a.ok(&acme_link).await;
    assert_eq!(
        rows(&doc, "Title"),
        [json!("Acme changed")],
        "only its own: {doc}"
    );
    assert!(
        !doc.to_string().contains("Deals(2)"),
        "no key of another tenant: {doc}"
    );
    let doc = g.ok(&globex_link).await;
    assert_eq!(rows(&doc, "id").len(), 1, "{doc}");
    assert_eq!(doc["value"][0]["reason"], "deleted");

    assert_eq!(
        g.ok(&acme_link).await["value"].as_array().unwrap().len(),
        1,
        "a stolen link shows the holder's own changes, not the link's"
    );

    let session = Session::from(t.db.clone());
    record_change(
        &session,
        "Deals",
        Some(GLOBEX),
        "(77)",
        odatabase::sql::ChangeKind::Deleted,
    )
    .await
    .unwrap();
    let doc = a.ok(&acme_link).await;
    assert!(!doc.to_string().contains("(77)"), "{doc}");
}

macro_rules! scenarios {
    ($($name:ident),* $(,)?) => {
        $(
            #[tokio::test]
            async fn $name() {
                let pool = common::sqlite_pool(super::SCHEMA).await;
                super::$name(super::Tenants::new(pool.into()).await).await;
            }
        )*

        #[tokio::test]
        async fn on_postgres() {
            if !common::postgres_can_run() {
                return;
            }
            let server = common::PostgresServer::start().await;
            $(
                let pool = server.pool(super::SCHEMA).await;
                eprintln!("postgres: {}", stringify!($name));
                super::$name(super::Tenants::new(pool.into()).await).await;
            )*
            super::row_level_security(&server).await;
        }
    };
}

/// An entity that doesn't declare its tenant, so that only the database keeps rows apart.
#[derive(ODataEntity)]
#[odata(table = "customer", set = "Open")]
pub struct OpenCustomer {
    #[odata(key)]
    pub id: i32,
    pub name: String,
}

const POLICIES: &str = "
CREATE ROLE app_user LOGIN PASSWORD 'app';
GRANT ALL ON ALL TABLES IN SCHEMA public TO app_user;
ALTER TABLE customer ALTER COLUMN tenant_id SET DEFAULT 'cccccccc-0000-0000-0000-000000000003';
ALTER TABLE customer ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_rows ON customer
  USING (tenant_id::text = current_setting('app.tenant_id', true));
";

async fn row_level_security(server: &common::PostgresServer) {
    let url = server.database(&format!("{SCHEMA}{POLICIES}")).await;
    let (head, database) = url.rsplit_once('/').unwrap();
    let (_, host) = head.rsplit_once('@').unwrap();
    let url = format!("postgres://app_user:app@{host}/{database}");
    let pool = sqlx::PgPool::connect(&url).await.unwrap();

    let service = Service::builder("Rls")
        .entity::<OpenCustomer>()
        .session_settings(|claims| {
            let tenant = claims.values.get("tenant").and_then(|v| v.first());
            match tenant {
                Some(ClaimValue::Equals(v)) => {
                    vec![("app.tenant_id".to_owned(), v.to_text())]
                }
                _ => Vec::new(),
            }
        })
        .build(pool.clone())
        .unwrap();
    let plain = App::new(service);
    let (a, g) = (
        as_user(&plain, tenant(ACME)),
        as_user(&plain, tenant(GLOBEX)),
    );

    assert_eq!(
        a.get("Open/$count").await.text(),
        "2",
        "the database filtered"
    );
    assert_eq!(g.get("Open/$count").await.text(), "1");
    assert_eq!(rows(&g.ok("Open").await, "Name"), [json!("Globex One")]);
    assert_eq!(
        plain.get("Open/$count").await.text(),
        "0",
        "no setting, no rows"
    );
    assert_eq!(a.status("Open(2)").await, 200);
    assert_eq!(g.status("Open(2)").await, 404);
    assert_eq!(a.ok("$all").await["value"].as_array().unwrap().len(), 2);

    let r = write(&g, Method::PATCH, "Open(2)", json!({"Name": "Stolen"})).await;
    assert_eq!(
        r.status.as_u16(),
        403,
        "the row looks free, and the policy refuses the insert: {}",
        r.text()
    );
    assert_eq!(a.ok("Open(2)").await["Name"], "Acme Two");
    let r = write(&a, Method::PATCH, "Open(2)", json!({"Name": "Mine"})).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(a.ok("Open(2)").await["Name"], "Mine");
    let batch = json!({"requests": [
        {"id": "1", "atomicityGroup": "g", "method": "PATCH", "url": "Open(2)",
         "headers": {"content-type": "application/json"}, "body": {"Name": "Batched"}},
    ]});
    let r = write(&a, Method::POST, "$batch", batch).await;
    assert_eq!(
        r.json()["responses"][0]["status"],
        204,
        "change sets carry the settings"
    );
    assert_eq!(a.ok("Open(2)").await["Name"], "Batched");
}

mod run {
    use super::common;

    scenarios!(
        reads,
        the_column_is_invisible,
        claims,
        writes,
        upsert_and_deep,
        links,
        change_tracking,
    );
}
