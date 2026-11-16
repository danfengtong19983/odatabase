use odatabase::{Nav, ODataEntity};

#[derive(ODataEntity)]
#[odata(table = "thing")]
pub struct Thing {
    #[odata(key)]
    pub id: i32,
    #[odata(has_many(target = Thing, join_table(table = "link", sorce(id = "a"))))]
    pub links: Nav,
}

fn main() {}
