use odatabase::ODataEntity;

#[derive(ODataEntity)]
#[odata(table = "thing")]
pub struct Thing(i32);

fn main() {}
