//! The parsers of untrusted input — URLs and `$batch` bodies — answer every input with a
//! result or an error, never a panic.
//!
//! This runs mutated inputs on every `cargo test`. The `fuzz` directory has coverage-guided
//! targets for the same parsers, which CI runs on nightly.
mod common;

use odatabase::parse_batch_for_fuzzing;

/// A small deterministic generator, so that a failure can be replayed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Characters that mean something to the parsers, so that mutations reach deep paths.
const ALPHABET: &[&str] = &[
    "(",
    ")",
    "'",
    "/",
    "$",
    ",",
    "=",
    "&",
    "?",
    " ",
    ":",
    ";",
    "@",
    "-",
    ".",
    "*",
    "\"",
    "%",
    "0",
    "1",
    "9",
    "e",
    "x",
    "Id",
    "Name",
    "eq",
    "and",
    "or",
    "not",
    "in",
    "any",
    "all",
    "desc",
    "Roles",
    "Address",
    "Test.Person",
    "$filter",
    "$select",
    "$expand",
    "$apply",
    "$orderby",
    "$top",
    "$count",
    "$it",
    "$this",
    "groupby",
    "aggregate",
    "with",
    "as",
    "from",
    "\r\n",
    "--b",
    "--c",
    "Content-ID: 1",
    "Content-Type: application/http",
    "GET ",
    "POST ",
    "{",
    "}",
    "[",
    "]",
    "\u{0}",
    "\u{7f}",
    "é",
    "\u{10ffff}",
    "999999999999999999999",
];

fn mutate(rng: &mut Rng, seed: &str) -> String {
    let mut parts: Vec<String> = seed.chars().map(String::from).collect();
    for _ in 0..1 + rng.below(6) {
        let at = rng.below(parts.len() + 1);
        match rng.below(4) {
            0 => parts.insert(at, ALPHABET[rng.below(ALPHABET.len())].to_owned()),
            1 if at < parts.len() => {
                parts.remove(at);
            }
            2 if at < parts.len() => {
                let end = (at + 1 + rng.below(8)).min(parts.len());
                let copy: Vec<String> = parts[at..end].to_vec();
                parts.splice(at..at, copy);
            }
            _ => {
                let end = (at + rng.below(8)).min(parts.len());
                parts.drain(at..end);
            }
        }
    }
    parts.concat()
}

const URLS: &[&str] = &[
    "BusinessPartners?$filter=Name eq 'x' and Rating gt 2&$orderby=Id desc&$top=2",
    "BusinessPartners('1')/Roles?$expand=Partner($select=Id;$expand=Owner($levels=2))",
    "BusinessPartners?$apply=groupby((Abc),aggregate(Rating with sum as T))/filter(T gt 1)",
    "BusinessPartners?$filter=Roles/any(r:r/Code in (1,2,3)) or not contains(Name,'O''N')",
    "BusinessPartners/Test.Person?$select=FirstName,Address/City&$compute=Rating mul 2 as D",
    "BusinessPartners?$filter=Comments/$count gt 1&$search=\"first org\" OR fourth",
    "Divisions('EU')?$expand=Children($levels=max;$filter=Population gt 10)",
    "$crossjoin(BusinessPartners,Tickets)?$filter=BusinessPartners/Id eq '1'",
];

const BATCHES: &[(&str, &str)] = &[
    (
        "multipart/mixed; boundary=b",
        "--b\r\nContent-Type: application/http\r\nContent-Transfer-Encoding: binary\r\n\r\nGET BusinessPartners('1') HTTP/1.1\r\nAccept: application/json\r\n\r\n\r\n--b\r\nContent-Type: multipart/mixed; boundary=c\r\n\r\n--c\r\nContent-Type: application/http\r\nContent-ID: 1\r\n\r\nPOST Roles HTTP/1.1\r\nContent-Type: application/json\r\n\r\n{\"PartnerId\":\"4\",\"Code\":5}\r\n--c--\r\n--b--\r\n",
    ),
    (
        "application/json",
        r#"{"requests":[{"id":"1","method":"get","url":"BusinessPartners"},{"id":"2","atomicityGroup":"g","method":"post","url":"Roles","headers":{"content-type":"application/json"},"body":{"Code":1},"dependsOn":["1"]}]}"#,
    ),
];

/// Inputs per parser. `PARSER_ROUNDS` raises it for a longer search.
fn rounds() -> usize {
    std::env::var("PARSER_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000)
}

#[tokio::test]
async fn url_parser_never_panics() {
    let pool = common::sqlite_pool(common::SCHEMA).await;
    let service = common::builder().build(pool).unwrap();
    let model = service.model();
    let mut rng = Rng(0x0dda_7a5e);
    for round in 0..rounds() {
        let seed = URLS[round % URLS.len()];
        let input = mutate(&mut rng, seed);
        let (path, query) = input.split_once('?').unwrap_or((&input, ""));
        let outcome = std::panic::catch_unwind(|| {
            let _ = odatabase_uri::parse_with(model, path, query, odatabase::UriLimits::default());
        });
        assert!(outcome.is_ok(), "round {round} panicked on {input:?}");
    }
}

#[test]
fn batch_parser_never_panics() {
    let mut rng = Rng(0xba7c_4b0d);
    for round in 0..rounds() {
        let (content_type, seed) = BATCHES[round % BATCHES.len()];
        let input = mutate(&mut rng, seed);
        let outcome = std::panic::catch_unwind(|| {
            let _ = parse_batch_for_fuzzing(content_type, input.as_bytes());
        });
        assert!(outcome.is_ok(), "round {round} panicked on {input:?}");
    }
}
