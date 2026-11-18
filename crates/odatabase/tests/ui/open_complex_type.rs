use odatabase::ODataComplex;

#[derive(ODataComplex)]
#[odata(open_type(column = "extra"))]
pub struct Address {
    pub city: Option<String>,
}

fn main() {}
