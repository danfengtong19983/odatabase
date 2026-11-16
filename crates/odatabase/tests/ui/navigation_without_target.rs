use odatabase::{Nav, ODataEntity};

#[derive(ODataEntity)]
#[odata(table = "thing")]
pub struct Thing {
    #[odata(key)]
    pub id: i32,
    #[odata(has_many(on(id = "thing_id")))]
    pub parts: Nav,
}

fn main() {}
