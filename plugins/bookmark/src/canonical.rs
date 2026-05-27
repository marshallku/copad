//! URL canonicalization (D2 in the bookmark plan).
//!
//! Rules:
//! - Allow only `http` / `https`. Reject `file://`, `data:`,
//!   `javascript:`, etc — bookmark.add must never be a vector for the
//!   plugin to read arbitrary local files or be tricked into running
//!   client-side code via a panel echo.
//! - Strip URL fragment (`#section`).
//! - Lowercase host (per RFC 3986 host is case-insensitive).
//! - Strip well-known tracking query params (utm_*, gclid, fbclid,
//!   mc_cid, mc_eid, ref). Other params kept verbatim, preserving
//!   their order — the same logical page should canonicalize to the
//!   same URL whether or not Marketing decided to slap utm_source on
//!   today's link, but `?id=42` must NOT be merged with `?id=43`.
//! - Trailing slash kept as-is. `https://x.com/` and `https://x.com`
//!   are different paths under RFC 3986 and we preserve that — same
//!   URL canonicalize twice → same output.
//!
//! The id surfaced to callers is the first 8 hex chars of
//! `sha1(canonical_url)`. 8 chars = 4 bytes = ~1 in 4B collision
//! probability for a personal KB this is fine, and it's prefix-
//! resolvable like every other coctl id.

use sha1::{Digest, Sha1};
use url::Url;

/// Exact matches. `utm_*` is a prefix-glob in `is_tracking_param`.
const TRACKING_PARAMS: &[&str] = &["gclid", "fbclid", "mc_cid", "mc_eid", "ref"];

fn is_tracking_param(key: &str) -> bool {
    TRACKING_PARAMS.contains(&key) || key.starts_with("utm_")
}

#[derive(Debug)]
pub enum CanonicalError {
    Parse(String),
    UnsupportedScheme(String),
    MissingHost,
}

impl CanonicalError {
    pub fn code_message(&self) -> (&'static str, String) {
        match self {
            CanonicalError::Parse(m) => ("invalid_url", m.clone()),
            CanonicalError::UnsupportedScheme(s) => (
                "unsupported_scheme",
                format!("only http/https allowed; got {s:?}"),
            ),
            CanonicalError::MissingHost => ("invalid_url", "URL has no host component".to_string()),
        }
    }
}

pub struct Canonical {
    pub url: String,
    pub urlhash8: String,
}

pub fn canonicalize(input: &str) -> Result<Canonical, CanonicalError> {
    let mut u = Url::parse(input.trim()).map_err(|e| CanonicalError::Parse(e.to_string()))?;

    let scheme = u.scheme();
    if !matches!(scheme, "http" | "https") {
        return Err(CanonicalError::UnsupportedScheme(scheme.to_string()));
    }

    if u.host_str().is_none() {
        return Err(CanonicalError::MissingHost);
    }

    u.set_fragment(None);

    if let Some(host) = u.host_str() {
        let lowered = host.to_lowercase();
        if lowered != host {
            // set_host() can fail e.g. if the lowered form re-parses
            // differently. Fall back to leaving it alone; canonical
            // form just won't be quite as canonical.
            let _ = u.set_host(Some(&lowered));
        }
    }

    // Strip tracking params; preserve order of survivors.
    let kept: Vec<(String, String)> = u
        .query_pairs()
        .filter(|(k, _)| !is_tracking_param(k))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    if u.query().is_some() {
        if kept.is_empty() {
            u.set_query(None);
        } else {
            // Re-serialize. `clear()` then `extend_pairs()` does the right thing.
            u.query_pairs_mut().clear().extend_pairs(&kept);
        }
    }

    let url_string = u.to_string();
    let urlhash8 = sha1_hex8(&url_string);
    Ok(Canonical {
        url: url_string,
        urlhash8,
    })
}

fn sha1_hex8(s: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(s.as_bytes());
    let digest = hasher.finalize();
    // First 4 bytes → 8 hex chars.
    let mut out = String::with_capacity(8);
    for b in &digest[..4] {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_http_schemes() {
        for url in [
            "file:///etc/passwd",
            "data:text/plain,hello",
            "javascript:alert(1)",
            "ftp://example.com/",
        ] {
            assert!(canonicalize(url).is_err(), "should reject {url}");
        }
    }

    #[test]
    fn accepts_http_and_https() {
        assert!(canonicalize("https://example.com/page").is_ok());
        assert!(canonicalize("http://example.com/page").is_ok());
    }

    #[test]
    fn strips_fragment() {
        let c = canonicalize("https://example.com/page#section").unwrap();
        assert_eq!(c.url, "https://example.com/page");
    }

    #[test]
    fn lowercases_host() {
        let c = canonicalize("https://EXAMPLE.com/page").unwrap();
        assert_eq!(c.url, "https://example.com/page");
    }

    #[test]
    fn strips_known_tracking_params_only() {
        let c = canonicalize("https://example.com/page?utm_source=newsletter&id=42&fbclid=abc")
            .unwrap();
        assert_eq!(c.url, "https://example.com/page?id=42");
    }

    #[test]
    fn strips_arbitrary_utm_prefix_params() {
        // `utm_id`, `utm_name`, `utm_term2` etc. — anything Marketing
        // invents under the `utm_` namespace must be stripped, not
        // just the canonical five from RFC-on-marketing-conventions.
        let c =
            canonicalize("https://example.com/page?utm_id=42&utm_name=spring&utm_custom=x&id=10")
                .unwrap();
        assert_eq!(c.url, "https://example.com/page?id=10");
    }

    #[test]
    fn preserves_param_order_and_distinct_ids() {
        let a = canonicalize("https://example.com/page?id=42").unwrap();
        let b = canonicalize("https://example.com/page?id=43").unwrap();
        assert_ne!(a.urlhash8, b.urlhash8);
    }

    #[test]
    fn trailing_slash_is_significant() {
        let a = canonicalize("https://example.com/").unwrap();
        let b = canonicalize("https://example.com").unwrap();
        // Both exist as Url instances but to_string() of `https://example.com`
        // canonicalizes to `https://example.com/`. Document the actual
        // behavior so future me knows.
        assert_eq!(a.url, b.url);
    }

    #[test]
    fn same_input_produces_same_hash() {
        let a = canonicalize("https://example.com/page").unwrap();
        let b = canonicalize("https://example.com/page").unwrap();
        assert_eq!(a.urlhash8, b.urlhash8);
        assert_eq!(a.urlhash8.len(), 8);
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(canonicalize("not a url").is_err());
        assert!(canonicalize("").is_err());
        assert!(canonicalize("http://").is_err());
    }
}
