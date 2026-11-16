use odatabase::ODataEntity;

#[derive(ODataEntity)]
#[odata(table = "thing", tabel = "typo")]
pub struct Thing {
    #[odata(key)]
    pub id: i32,
}

fn main() {}
