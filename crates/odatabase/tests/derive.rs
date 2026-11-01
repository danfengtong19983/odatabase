use odatabase::edm::{
    Inheritance, ModelBuilder, Nav, PrimitiveKind, TypeKind, to_csdl_json, to_csdl_xml,
};
use odatabase::{ODataComplex, ODataEntity, ODataEnum};

#[derive(ODataEnum)]
#[odata(as_string)]
#[allow(dead_code)]
enum Abc {
    A = 1,
    B = 2,
}

#[derive(ODataComplex)]
#[allow(dead_code)]
struct Address {
    street: Option<String>,
    #[odata(max_length = 3)]
    country: Option<String>,
}

#[derive(ODataEntity)]
#[odata(
    table = "business_partner",
    annotation(term = "Core.Description", string = "A <partner>")
)]
#[allow(dead_code)]
struct BusinessPartner {
    #[odata(key, max_length = 32)]
    id: String,
    #[odata(etag)]
    version: i64,
    #[odata(embedded(prefix = "addr_"))]
    address: Address,
    class: Option<Abc>,
    #[odata(collection(table = "comment", owner(id = "partner_id"), value_column = "text"))]
    comments: Vec<String>,
    #[odata(ignore)]
    internal: Vec<u8>,
    #[odata(has_many(target = Role, on(id = "partner_id"), partner = "Partner"))]
    roles: Nav,
}

#[derive(ODataEntity)]
#[allow(dead_code)]
struct Role {
    #[odata(key)]
    partner_id: String,
    #[odata(key)]
    role: String,
    #[odata(belongs_to(target = BusinessPartner, on(partner_id = "id"), required, partner = "Roles"))]
    partner: Nav,
}

#[derive(ODataEntity)]
#[odata(base = BusinessPartner, discriminator(column = "kind", value = "O"))]
#[allow(dead_code)]
struct Organization {
    name: String,
}

#[derive(ODataEntity)]
#[odata(singleton = "Settings", table = "settings")]
#[allow(dead_code)]
struct Setting {
    #[odata(key)]
    id: i32,
}

fn model() -> odatabase::edm::Model {
    ModelBuilder::new("Test")
        .entity::<BusinessPartner>()
        .entity::<Organization>()
        .entity::<Setting>()
        .build()
        .unwrap()
}

#[test]
fn derive_builds_the_model() {
    let m = model();
    let t = m.entity_type("BusinessPartner").unwrap();
    assert_eq!(t.table, "business_partner");
    assert_eq!(t.key, ["Id"]);
    assert_eq!(t.etag.as_deref(), Some("Version"));
    assert!(t.properties.iter().all(|p| p.name != "Internal"));
    let id = m.property("BusinessPartner", "Id").unwrap();
    assert_eq!(id.facets.max_length, Some(32));
    assert!(!id.nullable);
    assert_eq!(id.kind, TypeKind::Primitive(PrimitiveKind::String));
    let comments = m.property("BusinessPartner", "Comments").unwrap();
    assert_eq!(
        comments
            .collection
            .as_ref()
            .unwrap()
            .value_column
            .as_deref(),
        Some("text")
    );
    assert_eq!(
        m.property("BusinessPartner", "Address").unwrap().column,
        "addr_"
    );
    assert!(m.entity_sets.contains_key("BusinessPartners"));
    assert!(
        m.entity_sets.contains_key("Roles"),
        "targets are registered transitively"
    );
    assert!(
        !m.entity_sets.contains_key("Organizations"),
        "derived types have no set"
    );
    assert!(m.singletons.contains_key("Settings"));
    assert!(m.enum_type("Abc").unwrap().stored_as_string);
    assert_eq!(m.key_of("Organization"), ["Id"]);
    assert_eq!(
        m.entity_type("Organization").unwrap().inheritance,
        Some(Inheritance::SingleTable {
            discriminator: "kind".into(),
            value: "O".into()
        })
    );
}

#[test]
fn csdl_xml_describes_the_service() {
    let xml = to_csdl_xml(&model());
    for fragment in [
        "<Schema Namespace=\"Test\"",
        "<EntitySet Name=\"BusinessPartners\" EntityType=\"Test.BusinessPartner\">",
        "<NavigationPropertyBinding Path=\"Roles\" Target=\"Roles\"/>",
        "<PropertyRef Name=\"Id\"/>",
        "Type=\"Collection(Edm.String)\"",
        "<NavigationProperty Name=\"Roles\" Type=\"Collection(Test.Role)\" Partner=\"Partner\"/>",
        "<NavigationProperty Name=\"Partner\" Type=\"Test.BusinessPartner\" Nullable=\"false\" Partner=\"Roles\"/>",
        "BaseType=\"Test.BusinessPartner\"",
        "<Singleton Name=\"Settings\" Type=\"Test.Setting\">",
        "Alias=\"Core\"",
        "String=\"A &lt;partner&gt;\"",
    ] {
        assert!(xml.contains(fragment), "missing {fragment} in:\n{xml}");
    }
}

#[test]
fn csdl_json_describes_the_service() {
    let doc = to_csdl_json(&model());
    assert_eq!(doc["$EntityContainer"], "Test.Container");
    assert_eq!(doc["Test"]["BusinessPartner"]["$Key"][0], "Id");
    assert_eq!(
        doc["Test"]["BusinessPartner"]["Comments"]["$Collection"],
        true
    );
    assert_eq!(
        doc["Test"]["Container"]["BusinessPartners"]["$Type"],
        "Test.BusinessPartner"
    );
}
