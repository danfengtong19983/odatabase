//! Open entity types: dynamic properties kept in a JSON column.

mod common;

use async_trait::async_trait;
use axum::http::{Method, StatusCode};
use common::App;
use odatabase::{
    ChangeContext, ChangeHandler, Database, EntityPayload, Nav, ODataEntity, ODataError, Service,
};
use serde_json::{Value as Json, json};

/// `payload` is `jsonb` on Postgres; `notes` holds its dynamic properties in a plain `json`
/// column there, to show that both work. SQLite keeps JSON as text.
const SCHEMA: &str = "
CREATE TABLE site (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
CREATE TABLE device (
  id INTEGER PRIMARY KEY, name TEXT NOT NULL, site_id INTEGER,
  payload JSONB NOT NULL DEFAULT '{}'
);
CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT NOT NULL, extra JSON);
INSERT INTO site (id, name) VALUES (1, 'Plant'), (2, 'Depot');
INSERT INTO device (id, name, site_id, payload) VALUES
  (1, 'Printer', 1, '{\"serial\": \"SN-1\", \"qty\": 3, \"price\": 12.5, \"active\": true,
                     \"address\": {\"city\": \"Paris\"}, \"tags\": [\"a\", \"b\"], \"name\": \"hidden\"}'),
  (2, 'Scanner', 1, '{\"serial\": \"SN-2\", \"qty\": 1, \"active\": false, \"address\": {\"city\": \"Lyon\"}}'),
  (3, 'Router', 2, '{\"serial\": \"SN-3\", \"qty\": \"many\"}'),
  (4, 'Cable', 2, '{}');
INSERT INTO note (id, body, extra) VALUES (1, 'first', '{\"mood\": \"good\"}'), (2, 'second', NULL);
";

#[derive(ODataEntity)]
#[odata(table = "site", rename_all = "snake_case")]
pub struct Site {
    #[odata(key)]
    pub id: i32,
    pub name: String,
    #[odata(has_many(target = Device, on(id = "site_id"), partner = "site"))]
    pub devices: Nav,
}

#[derive(ODataEntity)]
#[odata(
    table = "device",
    rename_all = "snake_case",
    open_type(column = "payload")
)]
pub struct Device {
    #[odata(key)]
    pub id: i32,
    pub name: String,
    pub site_id: Option<i32>,
    #[odata(belongs_to(target = Site, on(site_id = "id"), partner = "devices"))]
    pub site: Nav,
}

#[derive(ODataEntity)]
#[odata(table = "note", rename_all = "snake_case", open_type(column = "extra"))]
pub struct Note {
    #[odata(key)]
    pub id: i32,
    pub body: String,
}

/// Refuses a `forbidden` dynamic property and stamps `checked` on every write, to show that
/// handlers see and can change dynamic properties.
struct Stamp;

#[async_trait]
impl ChangeHandler for Stamp {
    async fn before_create(
        &self,
        _: &ChangeContext<'_>,
        entity: &mut EntityPayload,
    ) -> Result<(), ODataError> {
        Self::check(entity)
    }

    async fn before_update(
        &self,
        _: &ChangeContext<'_>,
        _: &odatabase::EntityKey,
        entity: &mut EntityPayload,
    ) -> Result<(), ODataError> {
        Self::check(entity)
    }
}

impl Stamp {
    fn check(entity: &mut EntityPayload) -> Result<(), ODataError> {
        if entity.dynamic.iter().any(|(k, _)| k == "forbidden") {
            return Err(ODataError::bad_request("forbidden is not allowed"));
        }
        if entity.type_name == "Device" {
            entity.dynamic.push(("checked".into(), json!(true)));
        }
        Ok(())
    }
}

fn service(db: Database) -> Service {
    Service::builder("Open")
        .entity::<Site>()
        .entity::<Device>()
        .entity::<Note>()
        .change_handler(Stamp)
        .build(db)
        .unwrap()
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

fn names(doc: &Json) -> Vec<String> {
    doc["value"]
        .as_array()
        .unwrap_or_else(|| panic!("{doc}"))
        .iter()
        .map(|d| d["name"].as_str().unwrap_or_default().to_owned())
        .collect()
}

async fn filtered(app: &App, filter: &str) -> Vec<String> {
    names(
        &app.ok(&format!(
            "Devices?$filter={filter}&$orderby=id&$select=name"
        ))
        .await,
    )
}

async fn metadata(app: App) {
    let xml = app.get("$metadata").await.text();
    assert!(
        xml.contains("<EntityType Name=\"Device\" OpenType=\"true\">"),
        "{xml}"
    );
    assert!(!xml.contains("<EntityType Name=\"Site\" OpenType"), "{xml}");
    assert!(!xml.contains("Name=\"payload\""), "{xml}");
    let doc = app.ok("$metadata?$format=json").await;
    assert_eq!(doc["Open"]["Device"]["$OpenType"], true);
    assert!(doc["Open"]["Site"].get("$OpenType").is_none());
}

async fn reads(app: App) {
    let d = app.ok("Devices(1)").await;
    assert_eq!(d["serial"], "SN-1");
    assert_eq!(d["qty"], 3);
    assert_eq!(d["price"], json!(12.5));
    assert_eq!(d["active"], true);
    assert_eq!(d["address"], json!({"city": "Paris"}));
    assert_eq!(d["tags"], json!(["a", "b"]));
    assert_eq!(
        d["name"], "Printer",
        "a declared property wins over a member"
    );
    let keys: Vec<&String> = d.as_object().unwrap().keys().collect();
    let pos = |k: &str| keys.iter().position(|x| *x == k).unwrap();
    assert!(pos("site_id") < pos("serial"), "declared first: {keys:?}");
    assert!(app.ok("Devices(4)").await.get("serial").is_none());

    let d = app.ok("Devices(1)?$select=name,serial").await;
    assert_eq!(d["serial"], "SN-1");
    assert!(d.get("qty").is_none() && d.get("site_id").is_none(), "{d}");
    let d = app.ok("Devices(1)?$select=name").await;
    assert!(d.get("serial").is_none(), "{d}");
    let d = app.ok("Devices(1)?$select=*").await;
    assert_eq!(d["qty"], 3);
    let d = app.ok("Devices(4)?$select=serial").await;
    assert!(
        d.get("serial").is_none(),
        "an absent property stays absent: {d}"
    );
    assert_eq!(app.status("Devices?$select=address/city").await, 400);

    let site = app
        .ok("Sites(1)?$expand=devices($select=serial;$filter=qty gt 1)")
        .await;
    assert_eq!(site["devices"], json!([{"id": 1, "serial": "SN-1"}]));
    let full = app
        .ok("Devices(1)?$format=application/json;odata.metadata=full")
        .await;
    assert_eq!(full["address"]["city"], "Paris");
    assert_eq!(app.ok("Devices/$count?$filter=qty ge 1").await, json!(2));
    let top = app
        .ok("Devices?$orderby=serial desc&$top=1&$select=name")
        .await;
    assert_eq!(names(&top), ["Router"]);
    let r = app.get("Devices(1)/serial").await;
    assert_eq!(
        r.status,
        StatusCode::NOT_FOUND,
        "no path to a dynamic property: {}",
        r.text()
    );
    let note = app.ok("Notes(1)").await;
    assert_eq!(note["mood"], "good");
    assert_eq!(app.ok("Notes(2)").await["body"], "second");
}

async fn filters(app: App) {
    assert_eq!(filtered(&app, "serial eq 'SN-2'").await, ["Scanner"]);
    assert_eq!(
        filtered(&app, "serial ne 'SN-2'").await,
        ["Printer", "Router", "Cable"]
    );
    assert_eq!(
        filtered(&app, "qty gt 1").await,
        ["Printer"],
        "\"many\" is no number"
    );
    assert_eq!(filtered(&app, "qty eq 'many'").await, ["Router"]);
    assert_eq!(filtered(&app, "price lt 13.0").await, ["Printer"]);
    assert_eq!(filtered(&app, "qty add 1 ge 4").await, ["Printer"]);
    assert_eq!(filtered(&app, "active").await, ["Printer"]);
    assert_eq!(filtered(&app, "active eq false").await, ["Scanner"]);
    assert_eq!(filtered(&app, "not active").await, ["Scanner"]);
    assert_eq!(filtered(&app, "contains(serial,'-3')").await, ["Router"]);
    assert_eq!(
        filtered(&app, "tolower(serial) eq 'sn-1'").await,
        ["Printer"]
    );
    assert_eq!(
        filtered(&app, "serial in ('SN-1','SN-3')").await,
        ["Printer", "Router"]
    );
    assert_eq!(filtered(&app, "address/city eq 'Lyon'").await, ["Scanner"]);
    assert_eq!(filtered(&app, "serial eq null").await, ["Cable"]);
    assert_eq!(filtered(&app, "missing eq null").await.len(), 4);
    assert_eq!(
        filtered(&app, "serial ne null and qty eq null").await,
        Vec::<String>::new()
    );
    assert_eq!(
        filtered(&app, "cast(qty,Edm.Int32) eq 3").await,
        ["Printer"]
    );
    assert_eq!(
        filtered(&app, "serial eq name").await,
        Vec::<String>::new(),
        "name is the declared column, not the hidden member"
    );
    let sites = app
        .ok("Sites?$filter=devices/any(d: d/serial eq 'SN-3')&$select=name")
        .await;
    assert_eq!(names(&sites), ["Depot"]);
    let devices = app
        .ok("Devices?$orderby=qty desc,id&$select=name&$filter=qty ne null")
        .await;
    let order = names(&devices);
    assert_eq!(order.len(), 3);
    let numbers: Vec<&str> = order
        .iter()
        .map(String::as_str)
        .filter(|n| *n != "Router")
        .collect();
    assert_eq!(numbers, ["Printer", "Scanner"], "numbers sort numerically");

    for (filter, status) in [
        ("year(serial) eq 2026", 400),
        ("serial eq 2026-01-01", 400),
        ("serial/any(t: t eq 'a')", 400),
    ] {
        let r = app.get(&format!("Devices?$filter={filter}")).await;
        assert_eq!(r.status.as_u16(), status, "{filter}: {}", r.text());
    }
    assert_eq!(
        app.status("Sites?$filter=serial eq 'x'").await,
        400,
        "Site is closed"
    );
    assert_eq!(
        app.status("Devices?$apply=groupby((serial))").await,
        501,
        "grouping by a dynamic property"
    );
    assert_eq!(app.status("Devices?$compute=qty as q").await, 400);
    let r = app
        .get("Devices?$apply=aggregate(cast(qty,Edm.Decimal) with sum as total)")
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    assert_eq!(
        r.json()["value"][0]["total"],
        json!(4),
        "3 + 1; \"many\" is no number"
    );
}

async fn writes(app: App) {
    let r = write(
        &app,
        Method::POST,
        "Devices",
        json!({"id": 9, "name": "Drone", "serial": "SN-9", "spec": {"rotors": 4},
               "parts": [1, 2], "gone": null}),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    let d = r.json();
    assert_eq!(d["serial"], "SN-9");
    assert_eq!(d["spec"], json!({"rotors": 4}));
    assert_eq!(d["parts"], json!([1, 2]));
    assert_eq!(d["checked"], true, "added by the change handler");
    assert!(d.get("gone").is_none(), "a null is not stored: {d}");
    assert_eq!(filtered(&app, "spec/rotors eq 4").await, ["Drone"]);

    let r = write(
        &app,
        Method::PATCH,
        "Devices(9)",
        json!({"serial": "SN-9b", "spec": {"wings": 2}, "parts": null, "color": "red"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    let d = app.ok("Devices(9)").await;
    assert_eq!(d["serial"], "SN-9b");
    assert_eq!(d["spec"], json!({"wings": 2}), "replaced whole, not merged");
    assert_eq!(d["color"], "red");
    assert!(d.get("parts").is_none(), "null removes: {d}");
    assert_eq!(d["name"], "Drone");

    let r = write(
        &app,
        Method::PATCH,
        "Devices(9)",
        json!({"name": "Drone 2"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    let d = app.ok("Devices(9)").await;
    assert_eq!(
        (d["name"].as_str(), d["color"].as_str()),
        (Some("Drone 2"), Some("red"))
    );

    let r = write(
        &app,
        Method::PUT,
        "Devices(9)",
        json!({"name": "Drone 3", "site_id": null, "only": 1}),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    let d = app.ok("Devices(9)").await;
    assert_eq!(d["only"], 1);
    assert_eq!(d["checked"], true);
    assert!(d.get("color").is_none() && d.get("serial").is_none(), "{d}");

    let r = write(
        &app,
        Method::POST,
        "Notes",
        json!({"id": 3, "body": "third", "x": [1]}),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    let r = write(&app, Method::PATCH, "Notes(2)", json!({"mood": "fine"})).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(
        app.ok("Notes(2)").await["mood"],
        "fine",
        "a null column starts empty"
    );
    let notes = app.ok("Notes?$filter=mood eq 'fine'&$select=body").await;
    assert_eq!(notes["value"][0]["body"], "second");

    for (body, why) in [
        (
            json!({"id": 10, "name": "x", "bad-name": 1}),
            "not an identifier",
        ),
        (
            json!({"id": 10, "name": "x", "forbidden": 1}),
            "refused by the handler",
        ),
    ] {
        let r = write(&app, Method::POST, "Devices", body).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{why}: {}", r.text());
    }
    let r = write(
        &app,
        Method::POST,
        "Sites",
        json!({"id": 5, "name": "x", "extra": 1}),
    )
    .await;
    assert_eq!(
        r.status,
        StatusCode::BAD_REQUEST,
        "Site is closed: {}",
        r.text()
    );
    let r = write(
        &app,
        Method::POST,
        "Sites",
        json!({"id": 6, "name": "Yard", "devices": [{"id": 11, "name": "Gate", "lane": 7}]}),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "deep insert: {}", r.text());
    assert_eq!(app.ok("Devices(11)").await["lane"], 7);
}

macro_rules! open_scenarios {
    ($($name:ident),* $(,)?) => {
        $(
            #[tokio::test]
            async fn $name() {
                let schema = super::SCHEMA.replace("JSONB", "TEXT").replace("JSON", "TEXT");
                let pool = common::sqlite_pool(&schema).await;
                super::$name(App::new(super::service(pool.into()))).await;
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
                super::$name(App::new(super::service(pool.into()))).await;
            )*
        }
    };
}

mod scenario {
    use super::*;
    open_scenarios!(metadata, reads, filters, writes);
}

#[test]
fn open_type_on_a_column_that_is_a_property_is_refused() {
    #[derive(ODataEntity)]
    #[odata(table = "thing", open_type(column = "data"))]
    #[allow(dead_code)]
    struct Thing {
        #[odata(key)]
        id: i32,
        data: String,
    }
    let err = odatabase::edm::ModelBuilder::new("T")
        .entity::<Thing>()
        .build()
        .unwrap_err();
    assert!(err.to_string().contains("dynamic properties"), "{err}");
}
