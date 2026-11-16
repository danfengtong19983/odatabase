//! URLs: the path and query of a request, split at the first `?`.
#![no_main]

use libfuzzer_sys::fuzz_target;
use odatabase::edm::{Model, ModelBuilder};
use odatabase::{Nav, ODataComplex, ODataEntity};
use std::sync::OnceLock;

#[derive(ODataComplex)]
pub struct Address {
    pub city: Option<String>,
}

#[derive(ODataEntity)]
#[odata(table = "partner")]
pub struct Partner {
    #[odata(key)]
    pub id: String,
    #[odata(searchable)]
    pub name: Option<String>,
    pub rating: Option<i32>,
    #[odata(embedded(prefix = "addr_"))]
    pub address: Address,
    #[odata(collection(table = "partner_tag", owner(id = "partner_id"), value_column = "tag"))]
    pub tags: Vec<String>,
    #[odata(has_many(target = Role, on(id = "partner_id"), partner = "Partner"))]
    pub roles: Nav,
}

#[derive(ODataEntity)]
#[odata(table = "role")]
pub struct Role {
    #[odata(key)]
    pub partner_id: String,
    #[odata(key)]
    pub code: i32,
    #[odata(belongs_to(target = Partner, on(partner_id = "id"), required, partner = "Roles"))]
    pub partner: Nav,
}

fn model() -> &'static Model {
    static MODEL: OnceLock<Model> = OnceLock::new();
    MODEL.get_or_init(|| ModelBuilder::new("F").entity::<Partner>().build().unwrap())
}

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let (path, query) = text.split_once('?').unwrap_or((text, ""));
    let _ = odatabase_uri::parse_with(model(), path, query, odatabase_uri::Limits::default());
});
