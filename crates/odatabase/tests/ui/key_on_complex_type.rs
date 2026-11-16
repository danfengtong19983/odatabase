use odatabase::ODataComplex;

#[derive(ODataComplex)]
pub struct Address {
    #[odata(key)]
    pub city: String,
}

fn main() {}
