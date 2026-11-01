//! Content negotiation.

use crate::ODataError;
use http::{HeaderMap, HeaderValue, header};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MetadataLevel {
    None,
    #[default]
    Minimal,
    Full,
}

/// The negotiated JSON format of a response.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Format {
    pub metadata: MetadataLevel,
    /// Write `Edm.Int64` and `Edm.Decimal` as strings.
    pub ieee754: bool,
}

impl Format {
    pub fn content_type(self) -> HeaderValue {
        let metadata = match self.metadata {
            MetadataLevel::None => "none",
            MetadataLevel::Minimal => "minimal",
            MetadataLevel::Full => "full",
        };
        let ieee = if self.ieee754 {
            ";IEEE754Compatible=true"
        } else {
            ""
        };
        HeaderValue::try_from(format!("application/json;odata.metadata={metadata}{ieee}"))
            .expect("built from fixed text")
    }

    /// Reads `$format`, which overrides `Accept`. Formats other than JSON are a 406.
    pub fn negotiate(format_option: Option<&str>, headers: &HeaderMap) -> Result<Self, ODataError> {
        if let Some(text) = format_option {
            let shorthand = if text == "json" {
                "application/json"
            } else {
                text
            };
            return parse_media_type(shorthand)
                .ok_or_else(|| ODataError::new(406, format!("format {text} is not supported")));
        }
        let Some(accept) = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()) else {
            return Ok(Self::default());
        };
        accept
            .split(',')
            .find_map(parse_media_type)
            .ok_or_else(|| ODataError::new(406, "only application/json is supported"))
    }
}

fn parse_media_type(text: &str) -> Option<Format> {
    let mut parts = text.split(';').map(str::trim);
    let media = parts.next()?;
    if !matches!(media, "application/json" | "application/*" | "*/*") {
        return None;
    }
    let mut format = Format::default();
    for param in parts {
        let (name, value) = param.split_once('=').unwrap_or((param, ""));
        match (name.trim().to_ascii_lowercase().as_str(), value.trim()) {
            ("odata.metadata" | "metadata", "none") => format.metadata = MetadataLevel::None,
            ("odata.metadata" | "metadata", "minimal") => format.metadata = MetadataLevel::Minimal,
            ("odata.metadata" | "metadata", "full") => format.metadata = MetadataLevel::Full,
            ("odata.metadata" | "metadata", _) => return None,
            ("ieee754compatible", "true") => format.ieee754 = true,
            _ => {}
        }
    }
    Some(format)
}

/// Whether a request for `$metadata` asks for CSDL JSON rather than XML.
pub fn metadata_as_json(
    format_option: Option<&str>,
    headers: &HeaderMap,
) -> Result<bool, ODataError> {
    if let Some(f) = format_option {
        return match f {
            "json" | "application/json" => Ok(true),
            "xml" | "application/xml" => Ok(false),
            other => Err(ODataError::new(
                406,
                format!("format {other} is not supported"),
            )),
        };
    }
    let Some(accept) = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()) else {
        return Ok(false);
    };
    for item in accept.split(',') {
        match item.split(';').next().unwrap_or("").trim() {
            "application/xml" | "*/*" | "application/*" => return Ok(false),
            "application/json" => return Ok(true),
            _ => {}
        }
    }
    Err(ODataError::new(
        406,
        "$metadata is available as XML and JSON",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accept(v: &'static str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::ACCEPT, HeaderValue::from_static(v));
        h
    }

    #[test]
    fn negotiation() {
        let f = Format::negotiate(None, &accept("application/json;odata.metadata=full")).unwrap();
        assert_eq!(f.metadata, MetadataLevel::Full);
        let f = Format::negotiate(None, &accept("text/html, */*;q=0.8")).unwrap();
        assert_eq!(f, Format::default());
        let f =
            Format::negotiate(None, &accept("application/json;IEEE754Compatible=true")).unwrap();
        assert!(f.ieee754);
        assert_eq!(
            Format::negotiate(None, &accept("application/xml"))
                .unwrap_err()
                .status,
            406
        );
        assert_eq!(
            Format::negotiate(Some("atom"), &HeaderMap::new())
                .unwrap_err()
                .status,
            406
        );
        let f = Format::negotiate(
            Some("application/json;odata.metadata=none"),
            &accept("text/xml"),
        );
        assert_eq!(f.unwrap().metadata, MetadataLevel::None);
    }

    #[test]
    fn metadata_format() {
        assert!(!metadata_as_json(None, &HeaderMap::new()).unwrap());
        assert!(metadata_as_json(None, &accept("application/json")).unwrap());
        assert!(metadata_as_json(Some("json"), &HeaderMap::new()).unwrap());
        assert_eq!(
            metadata_as_json(None, &accept("text/plain"))
                .unwrap_err()
                .status,
            406
        );
    }
}
