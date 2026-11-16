use odatabase::ODataEntity;

#[derive(ODataEntity)]
#[odata(table = "thing")]
pub struct Thing {
    #[odata(kye)]
    pub id: i32,
}

fn main() {}
