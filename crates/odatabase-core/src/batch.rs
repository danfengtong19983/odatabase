//! `$batch` in the multipart and JSON formats.

use crate::change::ChangeContext;
use crate::{ODataError, ODataRequest, ODataResponse, Service};
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use odatabase_sql::Session;
use serde_json::{Map, Value as Json, json};
use std::collections::HashMap;

/// Requests per `$batch`. Each runs at least one statement.
const MAX_REQUESTS: usize = 1000;

#[derive(Debug, Clone)]
struct SubRequest {
    id: Option<String>,
    method: Method,
    url: String,
    headers: HeaderMap,
    body: Bytes,
}

#[derive(Debug)]
enum Item {
    Single(SubRequest),
    /// Requests that succeed or fail together.
    ChangeSet(Vec<SubRequest>),
}

/// One answer, tagged with the request it belongs to.
struct Reply {
    id: Option<String>,
    response: ODataResponse,
}

/// The answers to one item of a batch.
struct Group {
    replies: Vec<Reply>,
    atomic: bool,
}

impl Service {
    pub(crate) async fn batch(&self, req: &ODataRequest) -> Result<ODataResponse, ODataError> {
        let content_type = req
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| ODataError::bad_request("$batch needs a Content-Type"))?
            .to_owned();
        let mut json_format = false;
        let items = if content_type.starts_with("application/json") {
            json_format = true;
            parse_json(&req.body)?
        } else if content_type.starts_with("multipart/mixed") {
            let boundary = boundary_of(&content_type)
                .ok_or_else(|| ODataError::bad_request("multipart/mixed needs a boundary"))?;
            parse_multipart(&req.body, &boundary)?
        } else {
            return Err(ODataError::new(415, "$batch takes multipart/mixed or JSON"));
        };
        let total: usize = items
            .iter()
            .map(|i| match i {
                Item::Single(_) => 1,
                Item::ChangeSet(list) => list.len(),
            })
            .sum();
        if total > MAX_REQUESTS {
            return Err(ODataError::bad_request(format!(
                "$batch is limited to {MAX_REQUESTS} requests"
            )));
        }
        let continue_on_error = req
            .headers
            .get_all("prefer")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .any(|p| matches!(p.trim(), "continue-on-error" | "odata.continue-on-error"));
        let mut ids = HashMap::new();
        let mut groups: Vec<Group> = Vec::with_capacity(items.len());
        for item in items {
            let (replies, atomic) = match item {
                Item::Single(sub) => {
                    let response = self.single_part(req, &sub, &mut ids).await;
                    (
                        vec![Reply {
                            id: sub.id,
                            response,
                        }],
                        false,
                    )
                }
                Item::ChangeSet(list) => (self.change_set(req, list, &mut ids).await, true),
            };
            let failed = replies.iter().any(|r| r.response.status.as_u16() >= 400);
            groups.push(Group { replies, atomic });
            if failed && !continue_on_error {
                break;
            }
        }
        let mut response = if json_format {
            json_reply(&groups)
        } else {
            multipart_reply(&groups)
        };
        if continue_on_error {
            response.headers.insert(
                "preference-applied",
                HeaderValue::from_static("continue-on-error"),
            );
        }
        Ok(response)
    }

    fn sub_request(
        &self,
        req: &ODataRequest,
        sub: &SubRequest,
        ids: &HashMap<String, String>,
    ) -> Result<ODataRequest, ODataError> {
        let url = resolve_references(&sub.url, ids);
        let (path, query) = split_url(&url, &req.root)?;
        let body = if ids.is_empty() {
            sub.body.clone()
        } else {
            substitute_body(&sub.body, ids)
        };
        Ok(ODataRequest {
            method: sub.method.clone(),
            path,
            query,
            headers: sub.headers.clone(),
            body,
            root: req.root.clone(),
            claims: req.claims.clone(),
        })
    }

    async fn single_part(
        &self,
        req: &ODataRequest,
        sub: &SubRequest,
        ids: &mut HashMap<String, String>,
    ) -> ODataResponse {
        let response = match self.sub_request(req, sub, ids) {
            Ok(inner) => self.run_inner(&inner).await,
            Err(e) => Err(e),
        };
        let response = response.unwrap_or_else(|e| e.to_response());
        remember_location(sub, &response, ids);
        response
    }

    /// Runs an individual request, in its own transaction if it modifies data.
    async fn run_inner(&self, req: &ODataRequest) -> Result<ODataResponse, ODataError> {
        if let Some(response) = self.whole_service(req).await {
            return response;
        }
        let uri = self.parse_uri(req)?;
        match req.method {
            Method::GET => self.get(req, &uri).await,
            Method::POST | Method::PATCH | Method::PUT | Method::DELETE => {
                self.modify(req, &uri).await
            }
            _ => Err(ODataError::new(
                405,
                format!("{} is not allowed here", req.method),
            )),
        }
    }

    async fn change_set(
        &self,
        req: &ODataRequest,
        list: Vec<SubRequest>,
        ids: &mut HashMap<String, String>,
    ) -> Vec<Reply> {
        let session = match Session::begin(&self.inner.database).await {
            Ok(s) => s,
            Err(e) => {
                return vec![Reply {
                    id: None,
                    response: ODataError::from(e).to_response(),
                }];
            }
        };
        let mut replies = Vec::with_capacity(list.len());
        for sub in &list {
            let outcome = async {
                if sub.method == Method::GET {
                    return Err(ODataError::bad_request("a change set cannot contain GET"));
                }
                let inner = self.sub_request(req, sub, ids)?;
                let uri = self.parse_uri(&inner)?;
                self.modify_in(&inner, &uri, &session).await
            }
            .await;
            match outcome {
                Ok(response) => {
                    remember_location(sub, &response, ids);
                    replies.push(Reply {
                        id: sub.id.clone(),
                        response,
                    });
                }
                Err(e) => return abort(&session, sub, e).await,
            }
        }
        let ctx = ChangeContext {
            model: &self.inner.model,
            claims: &req.claims,
            session: &session,
            method: &Method::POST,
        };
        if let Err(e) = self.inner.changes.validate_changes(&ctx).await {
            let last = list.last().expect("a change set is never empty");
            return abort(&session, last, e).await;
        }
        if let Err(e) = session.commit().await {
            return vec![Reply {
                id: None,
                response: ODataError::from(e).to_response(),
            }];
        }
        replies
    }
}

/// Rolls back a change set. Its whole result is the one failure.
async fn abort(session: &Session, sub: &SubRequest, error: ODataError) -> Vec<Reply> {
    let error = match session.rollback().await {
        Ok(()) => error,
        Err(e) => ODataError::from(e),
    };
    vec![Reply {
        id: sub.id.clone(),
        response: error.to_response(),
    }]
}

fn remember_location(
    sub: &SubRequest,
    response: &ODataResponse,
    ids: &mut HashMap<String, String>,
) {
    if let (Some(id), Some(loc)) = (&sub.id, response.headers.get(header::LOCATION))
        && let Ok(loc) = loc.to_str()
    {
        ids.insert(id.clone(), loc.to_owned());
    }
}

/// Replaces a leading `$id` with the location of the entity created by that request.
fn resolve_references(url: &str, ids: &HashMap<String, String>) -> String {
    let Some(rest) = url.strip_prefix('$') else {
        return url.to_owned();
    };
    let (name, tail) = rest.split_at(rest.find(['/', '?']).unwrap_or(rest.len()));
    match ids.get(name) {
        Some(location) => format!("{location}{tail}"),
        None => url.to_owned(),
    }
}

/// Replaces `"$id"` and `"$id/...` string values in a JSON body.
fn substitute_body(body: &Bytes, ids: &HashMap<String, String>) -> Bytes {
    let Ok(text) = std::str::from_utf8(body) else {
        return body.clone();
    };
    let mut out = text.to_owned();
    for (id, location) in ids {
        out = out.replace(&format!("\"${id}\""), &format!("\"{location}\""));
        out = out.replace(&format!("\"${id}/"), &format!("\"{location}/"));
    }
    out.into()
}

/// Splits a request URL into a path below the service root and a query string.
fn split_url(url: &str, root: &str) -> Result<(String, String), ODataError> {
    let root_path = root
        .split("://")
        .nth(1)
        .and_then(|r| r.find('/').map(|i| &r[i..]))
        .unwrap_or("");
    let relative = if let Some(rest) = url.strip_prefix(root) {
        rest
    } else if url.contains("://") {
        return Err(ODataError::bad_request(format!(
            "{url} is outside this service"
        )));
    } else if !root_path.is_empty() && url.starts_with(root_path) {
        &url[root_path.len()..]
    } else {
        url
    };
    let relative = relative.trim_start_matches('/');
    Ok(match relative.split_once('?') {
        Some((p, q)) => (p.to_owned(), q.to_owned()),
        None => (relative.to_owned(), String::new()),
    })
}

fn boundary_of(content_type: &str) -> Option<String> {
    content_type.split(';').find_map(|p| {
        let (name, value) = p.split_once('=')?;
        (name.trim().eq_ignore_ascii_case("boundary"))
            .then(|| value.trim().trim_matches('"').to_owned())
    })
}

/// The parts between boundary lines.
fn split_parts<'b>(body: &'b [u8], boundary: &str) -> Result<Vec<&'b [u8]>, ODataError> {
    let marker = format!("--{boundary}");
    let marker = marker.as_bytes();
    let mut parts = Vec::new();
    let mut current: Option<usize> = None;
    let mut closed = false;
    let mut i = 0;
    while let Some(at) = find(&body[i..], marker) {
        let start = i + at;
        let line_start = start == 0 || body[start - 1] == b'\n';
        let after = start + marker.len();
        if !line_start {
            i = after;
            continue;
        }
        if let Some(from) = current {
            parts.push(trim_line_end(&body[from..start]));
        }
        if body[after..].starts_with(b"--") {
            closed = true;
            break;
        }
        let line_end = body[after..]
            .iter()
            .position(|b| *b == b'\n')
            .map(|p| after + p + 1);
        let Some(next) = line_end else {
            break;
        };
        current = Some(next);
        i = next;
    }
    if !closed {
        return Err(ODataError::bad_request(
            "the multipart body is not terminated",
        ));
    }
    Ok(parts)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn trim_line_end(b: &[u8]) -> &[u8] {
    let b = b.strip_suffix(b"\n").unwrap_or(b);
    b.strip_suffix(b"\r").unwrap_or(b)
}

type Headers = Vec<(String, String)>;

/// Splits leading header lines from what follows the blank line.
fn split_headers(part: &[u8]) -> Result<(Headers, &[u8]), ODataError> {
    let mut headers = Vec::new();
    let mut at = 0;
    loop {
        let Some(len) = part[at..].iter().position(|b| *b == b'\n') else {
            return Ok((headers, &part[part.len()..]));
        };
        let line = trim_line_end(&part[at..at + len + 1]);
        at += len + 1;
        if line.is_empty() {
            return Ok((headers, &part[at..]));
        }
        let text = std::str::from_utf8(line)
            .map_err(|_| ODataError::bad_request("headers are not valid text"))?;
        let (name, value) = text
            .split_once(':')
            .ok_or_else(|| ODataError::bad_request(format!("invalid header line {text}")))?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
    }
}

fn parse_multipart(body: &[u8], boundary: &str) -> Result<Vec<Item>, ODataError> {
    let mut items = Vec::new();
    for part in split_parts(body, boundary)? {
        let (headers, rest) = split_headers(part)?;
        let content_type = headers
            .iter()
            .find(|(n, _)| n == "content-type")
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        if content_type.starts_with("multipart/mixed") {
            let inner = boundary_of(content_type)
                .ok_or_else(|| ODataError::bad_request("a change set needs a boundary"))?;
            let mut list = Vec::new();
            for sub in split_parts(rest, &inner)? {
                let (sub_headers, http) = split_headers(sub)?;
                list.push(parse_http(&sub_headers, http)?);
            }
            if !list.is_empty() {
                items.push(Item::ChangeSet(list));
            }
        } else {
            items.push(Item::Single(parse_http(&headers, rest)?));
        }
    }
    Ok(items)
}

/// Parses the `application/http` payload of a part.
fn parse_http(part_headers: &[(String, String)], http: &[u8]) -> Result<SubRequest, ODataError> {
    let line_len = http
        .iter()
        .position(|b| *b == b'\n')
        .ok_or_else(|| ODataError::bad_request("a batch part has no request line"))?;
    let line = std::str::from_utf8(trim_line_end(&http[..=line_len]))
        .map_err(|_| ODataError::bad_request("the request line is not valid text"))?;
    let mut words = line.split_whitespace();
    let (Some(method), Some(url)) = (words.next(), words.next()) else {
        return Err(ODataError::bad_request(format!(
            "invalid request line {line}"
        )));
    };
    let method = Method::from_bytes(method.as_bytes())
        .map_err(|_| ODataError::bad_request(format!("invalid method {method}")))?;
    let (headers, body) = split_headers(&http[line_len + 1..])?;
    let mut map = HeaderMap::new();
    for (name, value) in &headers {
        if let (Ok(n), Ok(v)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::try_from(value.as_str()),
        ) {
            map.append(n, v);
        }
    }
    let id = part_headers
        .iter()
        .chain(&headers)
        .find(|(n, _)| n == "content-id")
        .map(|(_, v)| v.clone());
    Ok(SubRequest {
        id,
        method,
        url: url.to_owned(),
        headers: map,
        body: Bytes::copy_from_slice(body),
    })
}

fn parse_json(body: &[u8]) -> Result<Vec<Item>, ODataError> {
    let doc: Json = serde_json::from_slice(body)
        .map_err(|e| ODataError::bad_request(format!("invalid JSON: {e}")))?;
    let requests = doc
        .get("requests")
        .and_then(Json::as_array)
        .ok_or_else(|| ODataError::bad_request("expected a requests array"))?;
    let mut items: Vec<Item> = Vec::new();
    let mut open_group: Option<String> = None;
    for r in requests {
        let field = |name: &str| r.get(name).and_then(Json::as_str);
        let method = field("method")
            .ok_or_else(|| ODataError::bad_request("a request needs a method"))?
            .to_ascii_uppercase();
        let method = Method::from_bytes(method.as_bytes())
            .map_err(|_| ODataError::bad_request(format!("invalid method {method}")))?;
        let url = field("url").ok_or_else(|| ODataError::bad_request("a request needs a url"))?;
        let mut headers = HeaderMap::new();
        if let Some(h) = r.get("headers").and_then(Json::as_object) {
            for (name, value) in h {
                if let (Ok(n), Some(Ok(v))) = (
                    HeaderName::try_from(name.as_str()),
                    value.as_str().map(HeaderValue::try_from),
                ) {
                    headers.insert(n, v);
                }
            }
        }
        let body = match r.get("body") {
            None | Some(Json::Null) => Bytes::new(),
            Some(Json::String(s)) => Bytes::from(s.clone()),
            Some(other) => Bytes::from(other.to_string()),
        };
        let sub = SubRequest {
            id: field("id").map(str::to_owned),
            method,
            url: url.to_owned(),
            headers,
            body,
        };
        match field("atomicityGroup") {
            Some(group) => {
                if open_group.as_deref() == Some(group)
                    && let Some(Item::ChangeSet(list)) = items.last_mut()
                {
                    list.push(sub);
                } else {
                    open_group = Some(group.to_owned());
                    items.push(Item::ChangeSet(vec![sub]));
                }
            }
            None => {
                open_group = None;
                items.push(Item::Single(sub));
            }
        }
    }
    Ok(items)
}

fn write_http(out: &mut Vec<u8>, reply: &Reply) {
    let r = &reply.response;
    out.extend_from_slice(
        b"Content-Type: application/http\r\nContent-Transfer-Encoding: binary\r\n",
    );
    if let Some(id) = &reply.id {
        out.extend_from_slice(format!("Content-ID: {id}\r\n").as_bytes());
    }
    let reason = r.status.canonical_reason().unwrap_or("");
    out.extend_from_slice(format!("\r\nHTTP/1.1 {} {reason}\r\n", r.status.as_u16()).as_bytes());
    for (name, value) in &r.headers {
        out.extend_from_slice(name.as_str().as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", r.body.len()).as_bytes());
    out.extend_from_slice(&r.body);
    out.extend_from_slice(b"\r\n");
}

fn multipart_reply(groups: &[Group]) -> ODataResponse {
    let boundary = "batch_response";
    let mut out = Vec::new();
    for (i, group) in groups.iter().enumerate() {
        out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        // A failed change set is answered by a single response outside any multipart.
        let failed = group
            .replies
            .iter()
            .any(|r| r.response.status.as_u16() >= 400);
        if group.atomic && !failed {
            let inner = format!("changes_response_{i}");
            out.extend_from_slice(
                format!("Content-Type: multipart/mixed; boundary={inner}\r\n\r\n").as_bytes(),
            );
            for reply in &group.replies {
                out.extend_from_slice(format!("--{inner}\r\n").as_bytes());
                write_http(&mut out, reply);
            }
            out.extend_from_slice(format!("--{inner}--\r\n").as_bytes());
        } else {
            for reply in &group.replies {
                write_http(&mut out, reply);
            }
        }
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    let mut r = ODataResponse::new(StatusCode::OK);
    r.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("multipart/mixed; boundary=batch_response"),
    );
    r.body = out.into();
    r
}

fn json_reply(groups: &[Group]) -> ODataResponse {
    let mut responses = Vec::new();
    for reply in groups.iter().flat_map(|g| &g.replies) {
        let r = &reply.response;
        let mut item = Map::new();
        if let Some(id) = &reply.id {
            item.insert("id".into(), id.clone().into());
        }
        item.insert("status".into(), r.status.as_u16().into());
        let headers: Map<String, Json> = r
            .headers
            .iter()
            .filter_map(|(k, v)| Some((k.to_string(), v.to_str().ok()?.to_owned().into())))
            .collect();
        item.insert("headers".into(), headers.into());
        if !r.body.is_empty() {
            let body = serde_json::from_slice::<Json>(&r.body)
                .unwrap_or_else(|_| String::from_utf8_lossy(&r.body).into_owned().into());
            item.insert("body".into(), body);
        }
        responses.push(Json::Object(item));
    }
    ODataResponse::json(
        StatusCode::OK,
        HeaderValue::from_static("application/json"),
        &json!({"responses": responses}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_resolve_against_the_root() {
        let root = "http://h/odata";
        let split = |u| split_url(u, root).unwrap();
        assert_eq!(split("A('1')?$top=1"), ("A('1')".into(), "$top=1".into()));
        assert_eq!(split("/odata/A"), ("A".into(), String::new()));
        assert_eq!(split("http://h/odata/A"), ("A".into(), String::new()));
        assert_eq!(split_url("http://evil/A", root).unwrap_err().status, 400);
    }

    #[test]
    fn multipart_parsing_handles_nesting_and_line_endings() {
        let body = "--b\r\nContent-Type: application/http\r\n\r\nGET A HTTP/1.1\r\nAccept: application/json\r\n\r\n\r\n--b\r\nContent-Type: multipart/mixed; boundary=c\r\n\r\n--c\r\nContent-Type: application/http\r\nContent-ID: 1\r\n\r\nPOST A HTTP/1.1\r\nContent-Type: application/json\r\n\r\n{\"--b\":1}\r\n--c--\r\n--b--\r\n";
        let items = parse_multipart(body.as_bytes(), "b").unwrap();
        assert_eq!(items.len(), 2);
        let Item::ChangeSet(list) = &items[1] else {
            panic!()
        };
        assert_eq!(list[0].id.as_deref(), Some("1"));
        assert_eq!(list[0].body, Bytes::from("{\"--b\":1}"));
        let lf = body.replace("\r\n", "\n");
        assert_eq!(parse_multipart(lf.as_bytes(), "b").unwrap().len(), 2);
    }

    #[test]
    fn unterminated_multipart_is_rejected() {
        assert_eq!(parse_multipart(b"--b\r\nx", "b").unwrap_err().status, 400);
    }

    #[test]
    fn references_resolve() {
        let ids = HashMap::from([("1".to_owned(), "http://h/A('x')".to_owned())]);
        assert_eq!(
            resolve_references("$1/Roles", &ids),
            "http://h/A('x')/Roles"
        );
        assert_eq!(resolve_references("$2/Roles", &ids), "$2/Roles");
        assert_eq!(resolve_references("A", &ids), "A");
        let body = Bytes::from_static(b"{\"X@odata.bind\":\"$1\"}");
        assert_eq!(
            substitute_body(&body, &ids),
            Bytes::from("{\"X@odata.bind\":\"http://h/A('x')\"}")
        );
    }
}
