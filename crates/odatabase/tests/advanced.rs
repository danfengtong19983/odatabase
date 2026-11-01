mod common;

use axum::http::{Method, StatusCode};
use common::{App, sqlite_pool};
use odatabase::edm::{Annotation, AnnotationValue as A, GeographyPoint, Value};
use odatabase::{Binary, Database, ODataEntity, Service, TransientCalculator, TransientRow};
use serde_json::json;

#[derive(ODataEntity)]
#[odata(table = "vehicle")]
pub struct Vehicle {
    #[odata(key)]
    pub id: i32,
    pub name: String,
}

#[derive(ODataEntity)]
#[odata(table = "car", base = Vehicle, joined(id = "vehicle_id"))]
pub struct Car {
    pub doors: Option<i32>,
}

#[derive(ODataEntity)]
#[odata(table = "truck", base = Vehicle, joined(id = "vehicle_id"))]
pub struct Truck {
    pub payload: Option<f64>,
}

#[derive(ODataEntity)]
#[odata(table = "place")]
pub struct Place {
    #[odata(key)]
    pub id: i32,
    pub country: Option<String>,
    #[odata(description(table = "country_text", on(country = "code")))]
    pub country_name: Option<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    #[odata(transient(requires("FirstName", "LastName")))]
    pub full_name: Option<String>,
    pub location: Option<GeographyPoint>,
    #[odata(searchable)]
    pub blurb: Option<String>,
}

#[derive(ODataEntity)]
#[odata(
    table = "document",
    has_stream(column = "content", content_type_column = "mime")
)]
pub struct Document {
    #[odata(key)]
    pub id: i32,
    pub title: Option<String>,
    #[odata(etag)]
    pub version: i32,
}

#[derive(ODataEntity)]
#[odata(table = "typed")]
pub struct Typed {
    #[odata(key)]
    pub id: i32,
    #[odata(column = "byte_v")]
    pub byte: Option<u8>,
    #[odata(column = "sbyte_v")]
    pub sbyte: Option<i8>,
    #[odata(column = "short_v")]
    pub short: Option<i16>,
    #[odata(column = "long_v")]
    pub long: Option<i64>,
    #[odata(column = "single_v")]
    pub single: Option<f32>,
    #[odata(column = "time_v")]
    pub time: Option<chrono::NaiveTime>,
    #[odata(column = "duration_v")]
    pub duration: Option<chrono::TimeDelta>,
    #[odata(column = "binary_v")]
    pub binary: Option<Binary>,
    #[odata(column = "guid_v")]
    pub guid: Option<uuid::Uuid>,
    #[odata(column = "date_v")]
    pub date: Option<chrono::NaiveDate>,
    #[odata(column = "stamp_v")]
    pub stamp: Option<chrono::DateTime<chrono::Utc>>,
    #[odata(column = "amount_v", precision = 20, scale = 6)]
    pub amount: Option<rust_decimal::Decimal>,
    #[odata(column = "flag_v")]
    pub flag: Option<bool>,
}

#[derive(ODataEntity)]
#[odata(
    table = "locked",
    annotation(term = "Core.Description", string = "No writes")
)]
pub struct Locked {
    #[odata(key)]
    pub id: i32,
    #[odata(annotation(term = "Core.Immutable", bool = true))]
    pub name: Option<String>,
    pub code: Option<String>,
}

struct FullName;

impl TransientCalculator for FullName {
    fn calculate(&self, row: &TransientRow<'_>) -> Value {
        match (row.value("FirstName"), row.value("LastName")) {
            (Some(Value::String(a)), Some(Value::String(b))) => Value::String(format!("{a} {b}")),
            _ => Value::Null,
        }
    }
}

fn record(fields: &[(&str, A)]) -> A {
    A::Record(
        fields
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect(),
    )
}

fn service(db: Database) -> Service {
    Service::builder("Adv")
        .entity::<Vehicle>()
        .entity::<Car>()
        .entity::<Truck>()
        .entity::<Place>()
        .entity::<Document>()
        .entity::<Typed>()
        .entity::<Locked>()
        .transient::<Place>("FullName", FullName)
        .annotate::<Locked>(Annotation::new(
            "Capabilities.InsertRestrictions",
            record(&[("Insertable", A::Bool(false))]),
        ))
        .annotate::<Locked>(Annotation::new(
            "Capabilities.DeleteRestrictions",
            record(&[("Deletable", A::Bool(false))]),
        ))
        .annotate::<Locked>(Annotation::new(
            "Capabilities.UpdateRestrictions",
            record(&[("Updatable", A::Bool(false))]),
        ))
        .annotate::<Locked>(Annotation::new(
            "Capabilities.FilterRestrictions",
            record(&[
                (
                    "RequiredProperties",
                    A::Collection(vec![A::Path("Code".into())]),
                ),
                (
                    "NonFilterableProperties",
                    A::Collection(vec![A::Path("Name".into())]),
                ),
            ]),
        ))
        .annotate::<Locked>(Annotation::new(
            "Capabilities.SortRestrictions",
            record(&[(
                "NonSortableProperties",
                A::Collection(vec![A::Path("Code".into())]),
            )]),
        ))
        .annotate::<Locked>(Annotation::new(
            "Capabilities.CountRestrictions",
            record(&[("Countable", A::Bool(false))]),
        ))
        .annotate::<Locked>(Annotation::new("Capabilities.TopSupported", A::Bool(false)))
        .build(db)
        .unwrap()
}

async fn joined_inheritance_reads(app: App) {
    let doc = app.ok("Vehicles?$orderby=Id").await;
    let items = doc["value"].as_array().unwrap();
    assert_eq!(items[0]["Name"], "Plain");
    assert!(items[0].get("@odata.type").is_none(), "{}", items[0]);
    assert!(items[0].get("Doors").is_none());
    assert_eq!(items[1]["@odata.type"], "#Adv.Car");
    assert_eq!(items[1]["Doors"], 2);
    assert_eq!(items[2]["@odata.type"], "#Adv.Truck");
    assert_eq!(items[2]["Payload"], json!(7.5));
    assert_eq!(app.column("Vehicles/Adv.Car", "Name").await, ["Coupe"]);
    assert_eq!(app.ok("Vehicles(2)/Adv.Car").await["Doors"], 2);
    assert_eq!(app.status("Vehicles(1)/Adv.Car").await, 404);
    assert_eq!(
        app.column("Vehicles?$filter=Adv.Car/Doors eq 2", "Id")
            .await,
        [2]
    );
    assert_eq!(
        app.column("Vehicles?$filter=isof(Adv.Truck)", "Id").await,
        [3]
    );
    assert_eq!(
        app.column("Vehicles/Adv.Car?$filter=Doors eq 2", "Name")
            .await,
        ["Coupe"]
    );
    assert_eq!(app.get("Vehicles/Adv.Truck/$count").await.text(), "1");
}

async fn joined_inheritance_writes(app: App) {
    let json = ("content-type", "application/json");
    let body = json!({"@odata.type": "#Adv.Car", "Id": 9, "Name": "New", "Doors": 4}).to_string();
    let r = app.request(Method::POST, "Vehicles", &[json], &body).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    assert_eq!(r.json()["Doors"], 4);
    assert_eq!(app.ok("Vehicles(9)/Adv.Car").await["Name"], "New");
    let r = app
        .request(
            Method::PATCH,
            "Vehicles(9)/Adv.Car",
            &[json],
            &json!({"Doors": 5, "Name": "Both"}).to_string(),
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    let doc = app.ok("Vehicles(9)/Adv.Car").await;
    assert_eq!(
        (doc["Doors"].as_i64(), doc["Name"].as_str()),
        (Some(5), Some("Both"))
    );
    let r = app
        .request(Method::DELETE, "Vehicles(9)/Adv.Car", &[], "")
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(app.status("Vehicles(9)").await, 404);
}

async fn descriptions(app: App) {
    assert_eq!(app.ok("Places(1)").await["CountryName"], "Germany");
    let de = [("accept-language", "de-DE,en;q=0.8")];
    let r = app.request(Method::GET, "Places(1)", &de, "").await;
    assert_eq!(r.json()["CountryName"], "Deutschland");
    let r = app.request(Method::GET, "Places(2)", &de, "").await;
    assert_eq!(
        r.json()["CountryName"],
        json!(null),
        "no text in that language"
    );
    assert_eq!(app.ok("Places(3)").await["CountryName"], json!(null));
    assert_eq!(
        app.column("Places?$filter=CountryName eq 'France'", "Id")
            .await,
        [2]
    );
    assert_eq!(
        app.column("Places?$orderby=CountryName desc&$top=1", "Id")
            .await,
        [1]
    );
    let r = app
        .request(
            Method::PATCH,
            "Places(1)",
            &[("content-type", "application/json")],
            &json!({"CountryName": "x"}).to_string(),
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::NO_CONTENT,
        "descriptions are read-only and ignored"
    );
    assert_eq!(app.ok("Places(1)").await["CountryName"], "Germany");
}

async fn transient_properties(app: App) {
    assert_eq!(app.ok("Places(1)").await["FullName"], "Ada Byron");
    assert_eq!(app.ok("Places(3)").await["FullName"], json!(null));
    let doc = app.ok("Places(2)?$select=FullName").await;
    assert_eq!(doc["FullName"], "Alan Turing");
    assert!(
        doc.get("FirstName").is_none(),
        "inputs stay out of the response: {doc}"
    );
    let doc = app.ok("Places?$select=FirstName&$top=1").await;
    assert!(doc["value"][0].get("FullName").is_none());
    assert_eq!(app.status("Places?$filter=FullName eq 'x'").await, 400);
}

async fn search(app: App) {
    let ids = |q: &'static str| {
        let app = app.clone();
        async move {
            app.column(&format!("Places?$search={q}&$orderby=Id"), "Id")
                .await
        }
    };
    assert_eq!(ids("bike").await, [1, 2]);
    assert_eq!(ids("bike AND red").await, [1]);
    assert_eq!(ids("tent OR road").await, [2, 3]);
    assert_eq!(ids("bike NOT red").await, [2]);
    assert_eq!(ids("mountain").await, [1, 3], "case-insensitive");
    assert_eq!(ids("\"road bike\"").await, [2], "phrase");
    assert_eq!(app.get("Places/$count?$search=bike").await.text(), "2");
    assert_eq!(
        app.status("Vehicles?$search=x").await,
        400,
        "no searchable property"
    );
}

async fn media_streams(app: App) {
    assert_eq!(
        app.status("Documents(1)/$value").await,
        204,
        "nothing stored yet"
    );
    let r = app
        .request(
            Method::PUT,
            "Documents(1)/$value",
            &[("content-type", "image/png")],
            "PNGDATA",
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    let r = app.get("Documents(1)/$value").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("content-type"), Some("image/png"));
    assert_eq!(r.body, b"PNGDATA");
    assert_eq!(
        app.ok("Documents(1)").await["Version"],
        2,
        "the version advances"
    );
    let r = app
        .request(Method::DELETE, "Documents(1)/$value", &[], "")
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(app.status("Documents(1)/$value").await, 204);
    assert_eq!(app.status("Vehicles(1)/$value").await, 400);
    assert_eq!(app.status("Documents(9)/$value").await, 404);
    let xml = app.get("$metadata").await.text();
    assert!(xml.contains("<EntityType Name=\"Document\" HasStream=\"true\">"));
}

async fn geospatial(app: App) {
    let doc = app.ok("Places(1)").await;
    assert_eq!(
        doc["Location"],
        json!({"type": "Point", "coordinates": [13.4, 52.5]})
    );
    let body = json!({"Location": {"type": "Point", "coordinates": [2.3, 48.8]}}).to_string();
    let r = app
        .request(
            Method::PATCH,
            "Places(2)",
            &[("content-type", "application/json")],
            &body,
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(
        app.ok("Places(2)").await["Location"]["coordinates"][0],
        json!(2.3)
    );
    let xml = app.get("$metadata").await.text();
    assert!(xml.contains("Type=\"Edm.GeographyPoint\""));
    assert_eq!(
        app.status("Places?$filter=geo.distance(Location,Location) lt 1")
            .await,
        501
    );
}

async fn every_primitive_round_trips(app: App) {
    let body = json!({
        "Id": 1, "Byte": 200, "Sbyte": -5, "Short": -300, "Long": 9007199254740993i64,
        "Single": 1.5, "Time": "12:34:56", "Duration": "P1DT2H", "Binary": "AQID",
        "Guid": "01234567-89ab-cdef-0123-456789abcdef", "Date": "2026-10-31",
        "Stamp": "2026-10-31T10:00:00.250Z", "Amount": 1234.5678, "Flag": true
    });
    let r = app
        .request(
            Method::POST,
            "Typeds",
            &[("content-type", "application/json")],
            &body.to_string(),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    let doc = app.ok("Typeds(1)").await;
    for (field, expected) in body.as_object().unwrap() {
        assert_eq!(&doc[field], expected, "{field}");
    }
    let body = json!({"Id": 2}).to_string();
    let r = app
        .request(
            Method::POST,
            "Typeds",
            &[("content-type", "application/json")],
            &body,
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    assert_eq!(app.ok("Typeds(2)").await["Stamp"], json!(null));
    let cases = [
        ("Byte eq 200", 1),
        ("Sbyte lt 0", 1),
        ("Long eq 9007199254740993", 1),
        ("Single gt 1", 1),
        ("Time eq 12:34:56", 1),
        ("Date eq 2026-10-31", 1),
        ("Stamp eq 2026-10-31T10:00:00.25Z", 1),
        ("Amount gt 1234", 1),
        ("Guid eq 01234567-89ab-cdef-0123-456789abcdef", 1),
        ("Flag", 1),
        ("Binary eq binary'AQID'", 1),
        ("Duration eq duration'P1DT2H'", 1),
    ];
    for (filter, id) in cases {
        assert_eq!(
            app.column(&format!("Typeds?$filter={filter}"), "Id").await,
            [id],
            "{filter}"
        );
    }
    let r = app
        .request(
            Method::POST,
            "Typeds",
            &[("content-type", "application/json")],
            &json!({"Id": 3, "Byte": 300}).to_string(),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

async fn capabilities(app: App) {
    let json = ("content-type", "application/json");
    let body = json!({"Id": 5}).to_string();
    assert_eq!(
        app.request(Method::POST, "Lockeds", &[json], &body)
            .await
            .status,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        app.request(Method::PATCH, "Lockeds(1)", &[json], &body)
            .await
            .status,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        app.request(Method::DELETE, "Lockeds(1)", &[], "")
            .await
            .status,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        app.request(Method::DELETE, "Lockeds(1)/Name", &[], "")
            .await
            .status,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(app.status("Lockeds").await, 400, "Code must be filtered");
    assert_eq!(app.status("Lockeds?$filter=Id eq 1").await, 400);
    assert_eq!(app.column("Lockeds?$filter=Code eq 'x'", "Id").await, [1]);
    assert_eq!(
        app.status("Lockeds(1)").await,
        200,
        "a key lookup needs no filter"
    );
    assert_eq!(
        app.status("Lockeds?$filter=Code eq 'x' and Name eq 'a'")
            .await,
        400
    );
    assert_eq!(
        app.status("Lockeds?$filter=Code eq 'x'&$orderby=Code")
            .await,
        400
    );
    assert_eq!(
        app.status("Lockeds?$filter=Code eq 'x'&$count=true").await,
        400
    );
    assert_eq!(app.status("Lockeds/$count?$filter=Code eq 'x'").await, 400);
    assert_eq!(app.status("Lockeds?$filter=Code eq 'x'&$top=1").await, 501);
}

async fn aggregation_features(app: App) {
    let doc = app
        .ok("Places?$apply=search(bike)/aggregate($count as N)")
        .await;
    assert_eq!(doc["value"][0]["N"], 2, "search before aggregating");
    let doc = app.ok("Places?$apply=groupby((CountryName),aggregate($count as N))&$filter=CountryName eq 'France'").await;
    assert_eq!(doc["value"][0]["N"], 1, "group by a description");
    let r = app
        .request(
            Method::GET,
            "Places?$apply=groupby((CountryName))&$orderby=CountryName desc",
            &[("accept-language", "de")],
            "",
        )
        .await;
    assert_eq!(r.json()["value"][0]["CountryName"], "Deutschland");
    assert_eq!(
        app.status("Places?$apply=aggregate(FullName with max as M)")
            .await,
        400,
        "transient"
    );
    let doc = app
        .ok("Vehicles?$apply=aggregate(Adv.Truck/Payload with sum as P)")
        .await;
    assert_eq!(
        doc["value"][0]["P"],
        json!(7.5),
        "a property of a joined derived table"
    );
    assert_eq!(
        app.status("Lockeds?$apply=aggregate($count as N)").await,
        400,
        "RequiredProperties applies to aggregates"
    );
    let allowed = "Lockeds?$apply=filter(Code eq 'x')/aggregate($count as N)";
    assert_eq!(
        app.ok(allowed).await["value"][0]["N"],
        1,
        "a filter in the pipeline counts"
    );
    let banned = "Lockeds?$apply=filter(Code eq 'x' and Name eq 'a')/aggregate($count as N)";
    assert_eq!(
        app.status(banned).await,
        400,
        "NonFilterableProperties applies in the pipeline"
    );
}

async fn annotations_in_metadata(app: App) {
    let xml = app.get("$metadata").await.text();
    assert!(xml.contains("Alias=\"Core\""), "{xml}");
    assert!(xml.contains("Alias=\"Capabilities\""), "{xml}");
    assert!(
        xml.contains("<Annotation Term=\"Core.Description\" String=\"No writes\"/>"),
        "{xml}"
    );
    assert!(
        xml.contains("<Annotation Term=\"Core.Immutable\" Bool=\"true\"/>"),
        "{xml}"
    );
    assert!(
        xml.contains("<PropertyValue Property=\"Insertable\"><Bool>false</Bool></PropertyValue>"),
        "{xml}"
    );
    assert!(xml.contains("<Path>Code</Path>"), "{xml}");
    let r = app
        .request(
            Method::GET,
            "$metadata",
            &[("accept", "application/json")],
            "",
        )
        .await;
    assert_eq!(
        r.json()["Adv"]["Locked"]["@Capabilities.TopSupported"],
        false
    );
}

async fn metadata_etag(app: App) {
    let r = app.get("$metadata").await;
    let tag = r.header("etag").unwrap().to_owned();
    let r = app
        .request(
            Method::GET,
            "$metadata",
            &[("if-none-match", tag.as_str())],
            "",
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_MODIFIED);
    assert!(r.body.is_empty());
    let r = app
        .request(
            Method::GET,
            "$metadata",
            &[("if-none-match", "\"other\"")],
            "",
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
}

mod run {
    use super::common;
    use odatabase::{Database, Service};

    fn build() -> impl FnOnce(Database) -> Service {
        super::service
    }

    crate::scenarios!(build();
        joined_inheritance_reads,
        joined_inheritance_writes,
        descriptions,
        transient_properties,
        search,
        media_streams,
        geospatial,
        every_primitive_round_trips,
        capabilities,
        aggregation_features,
        annotations_in_metadata,
        metadata_etag,
    );
}

#[tokio::test]
async fn metadata_document() {
    let pool = sqlite_pool(common::SCHEMA).await;
    insta::assert_snapshot!(odatabase::edm::to_csdl_xml(service(pool.into()).model()));
}

#[tokio::test]
async fn transient_properties_need_a_calculator() {
    let pool = sqlite_pool(common::SCHEMA).await;
    let result = Service::builder("Adv").entity::<Place>().build(pool);
    assert!(result.err().unwrap().0.contains("no calculator"));
}

#[tokio::test]
async fn api_versions_are_separate_services() {
    let pool = sqlite_pool(common::SCHEMA).await;
    let v1 = Service::builder("V1")
        .entity::<Vehicle>()
        .build(pool.clone())
        .unwrap();
    let v2 = Service::builder("V2")
        .entity::<Vehicle>()
        .entity::<Car>()
        .build(pool)
        .unwrap();
    let router = odatabase::mount("/odata/v1", v1).merge(odatabase::mount("/odata/v2", v2));
    let app = App { router };
    assert!(
        app.get("v1/$metadata")
            .await
            .text()
            .contains("Namespace=\"V1\"")
    );
    assert_eq!(app.status("v2/Vehicles/V2.Car").await, 200);
    assert_eq!(app.status("v1/Vehicles/V2.Car").await, 404);
    let doc = app.ok("v2/Vehicles(1)").await;
    assert_eq!(
        doc["@odata.context"],
        "http://test.local/odata/v2/$metadata#Vehicles/$entity"
    );
}
