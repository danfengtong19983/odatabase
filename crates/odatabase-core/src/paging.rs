//! Server-driven paging.

use crate::ODataError;
use odatabase_sql::Claims;

/// What a request asks of the pager.
#[derive(Clone, Debug)]
pub struct PageRequest<'a> {
    pub entity_type: &'a str,
    pub top: Option<u64>,
    pub skip: Option<u64>,
    pub skiptoken: Option<&'a str>,
    /// The client's `Prefer: odata.maxpagesize`.
    pub max_page_size: Option<u64>,
    pub claims: &'a Claims,
}

/// The window to read, and the token of the following page if there may be one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageResult {
    pub top: Option<u64>,
    pub skip: Option<u64>,
    /// Token for the next page, used if the page turns out to be full.
    pub next: Option<String>,
    /// Whether `Prefer: odata.maxpagesize` was honored.
    pub applied_max_page_size: bool,
}

/// Decides the pages of collection reads, like `JPAODataPagingProvider`.
pub trait PagingProvider: Send + Sync {
    fn page(&self, request: &PageRequest<'_>) -> Result<PageResult, ODataError>;
}

/// Offset paging. The token is the row offset, so it needs no server state.
#[derive(Clone, Copy, Debug, Default)]
pub struct OffsetPaging {
    /// Server limit per page. `None` returns everything unless the client asks for pages.
    pub page_size: Option<u64>,
}

impl PagingProvider for OffsetPaging {
    fn page(&self, r: &PageRequest<'_>) -> Result<PageResult, ODataError> {
        let client_skip = r.skip.unwrap_or(0);
        let start = match r.skiptoken {
            Some(t) => {
                let at: u64 = t
                    .parse()
                    .map_err(|_| ODataError::bad_request("invalid $skiptoken"))?;
                if at < client_skip {
                    return Err(ODataError::bad_request("invalid $skiptoken"));
                }
                at
            }
            None => client_skip,
        };
        let remaining = r.top.map(|top| (client_skip + top).saturating_sub(start));
        let size = match (self.page_size, r.max_page_size) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let applied = r.max_page_size.is_some_and(|m| size == Some(m));
        let (top, next) = match (size, remaining) {
            (Some(size), Some(rem)) if rem <= size => (Some(rem), None),
            (Some(size), _) => (Some(size), Some((start + size).to_string())),
            (None, rem) => (rem, None),
        };
        Ok(PageResult {
            top,
            skip: (start > 0).then_some(start),
            next,
            applied_max_page_size: applied,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(
        size: Option<u64>,
        top: Option<u64>,
        skip: Option<u64>,
        token: Option<&str>,
    ) -> PageResult {
        let claims = Claims::default();
        OffsetPaging { page_size: size }
            .page(&PageRequest {
                entity_type: "T",
                top,
                skip,
                skiptoken: token,
                max_page_size: None,
                claims: &claims,
            })
            .unwrap()
    }

    #[test]
    fn without_a_page_size_the_client_window_is_used() {
        let p = page(None, Some(5), Some(2), None);
        assert_eq!((p.top, p.skip, p.next), (Some(5), Some(2), None));
    }

    #[test]
    fn server_size_limits_the_page_and_issues_a_token() {
        let p = page(Some(10), None, None, None);
        assert_eq!(
            (p.top, p.skip, p.next.as_deref()),
            (Some(10), None, Some("10"))
        );
        let p = page(Some(10), None, None, Some("10"));
        assert_eq!(
            (p.top, p.skip, p.next.as_deref()),
            (Some(10), Some(10), Some("20"))
        );
    }

    #[test]
    fn client_top_ends_the_sequence() {
        let p = page(Some(10), Some(25), None, Some("20"));
        assert_eq!((p.top, p.skip, p.next), (Some(5), Some(20), None));
        let p = page(Some(10), Some(10), None, None);
        assert_eq!((p.top, p.next), (Some(10), None));
    }

    #[test]
    fn bad_tokens_are_rejected() {
        let claims = Claims::default();
        let r = PageRequest {
            entity_type: "T",
            top: None,
            skip: Some(5),
            skiptoken: Some("1"),
            max_page_size: None,
            claims: &claims,
        };
        assert_eq!(OffsetPaging::default().page(&r).unwrap_err().status, 400);
    }
}
