use odatabase::ODataEntity;

#[derive(ODataEntity)]
#[odata(table = "thing", rename_all = "kebab-case")]
pub struct Thing {
    #[odata(key)]
    pub id: i32,
}

fn main() {}
