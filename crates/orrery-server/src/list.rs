//! The control plane's list contract: a list body takes `page_size` and an
//! opaque `page_token`, orders by `sort_key` and `sort_dir`, and is answered
//! `{items, next_page_token, total_count}`.
//!
//! A token only continues a walk. A position a caller holds from another
//! route is a filter on the list, never a token, so what a token carries is
//! the server's to change. It carries a keyset position, the key of the last
//! item served, so a walk neither repeats nor skips an item while the list
//! grows, in whichever order it is read.

use axum::body::Body;
use axum::http::{Response, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::control::{Fault, Fields, LIST_PAGE_DEFAULT, LIST_PAGE_MAX, keyset_page};
use crate::json;

/// What a list body's `page_size` and `page_token` ask for.
pub(crate) struct Window<K> {
    /// How many items the page holds. Zero is the count-only read.
    pub(crate) page_size: usize,
    /// The key of the previous page's last item.
    pub(crate) resume: Option<K>,
    fingerprint: String,
}

impl<K> Window<K> {
    /// How many rows to read: one past the page, which is how the last page
    /// is known to be last, and none for a count.
    pub(crate) fn fetch(&self) -> usize {
        if self.page_size == 0 {
            0
        } else {
            self.page_size + 1
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Token<K> {
    /// The key of the last item served.
    k: K,
    /// The fingerprint of the query the token was issued for.
    f: String,
}

/// Read `page_size` and `page_token`, after the route has read the filters
/// and order that make up `query`. `query` is canonical, so two bodies asking
/// the same question give the same value, and a token is refused for any
/// query but the one it was issued for: its position would name different
/// items in another.
pub(crate) fn read_window<K: DeserializeOwned>(
    fields: &mut Fields,
    query: &impl Serialize,
) -> Window<K> {
    // A token checked against a query that failed to read would be refused
    // for a fault the caller is already being told about.
    let query_is_sound = !fields.has_faults();
    let page_size = fields.optional("page_size").unwrap_or(LIST_PAGE_DEFAULT);
    if page_size > LIST_PAGE_MAX {
        fields.fault(Fault::out_of_range(
            format!("page_size must be 0..={LIST_PAGE_MAX}; 0 answers only total_count"),
            "page_size",
        ));
    }
    let fingerprint = fingerprint(query);
    let token: Option<String> = fields.optional("page_token");
    let resume = match token.map(|t| decode::<K>(&t)) {
        None => None,
        Some(None) => {
            fields.fault(Fault::invalid(
                "page_token is not a token this list issued".to_string(),
                "page_token",
            ));
            None
        }
        Some(Some(token)) if token.f != fingerprint => {
            if query_is_sound {
                fields.fault(Fault::invalid(
                    "page_token was issued for other filters or another order; \
                     start again from the first page"
                        .to_string(),
                    "page_token",
                ));
            }
            None
        }
        Some(Some(token)) => Some(token.k),
    };
    Window {
        page_size,
        resume,
        fingerprint,
    }
}

/// A list's sort direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SortDir {
    Asc,
    Desc,
}

/// One `sort_key` a list orders by, and the direction it takes when a body
/// names it without a `sort_dir`.
pub(crate) struct SortKey {
    pub(crate) name: &'static str,
    pub(crate) default_dir: SortDir,
}

/// Read `sort_key` and `sort_dir` against the list's own keys. `None` asks
/// for the list's default order, and is also the answer when the pair is
/// refused, which is recorded. A `sort_dir` without a `sort_key` is refused
/// rather than ignored: the default order has no direction to set.
pub(crate) fn read_sort(fields: &mut Fields, keys: &[SortKey]) -> Option<(&'static str, SortDir)> {
    let key: Option<String> = fields.optional("sort_key");
    let known = key
        .as_deref()
        .map(|name| keys.iter().find(|k| k.name == name));
    if let (Some(name), Some(None)) = (&key, known) {
        let names: Vec<_> = keys.iter().map(|k| k.name).collect();
        fields.fault(Fault::invalid(
            format!(
                "unknown sort_key {name:?}; this list sorts by {}",
                names.join(", ")
            ),
            "sort_key",
        ));
    }
    let dir: Option<SortDir> = fields.optional("sort_dir");
    match known {
        Some(Some(k)) => Some((k.name, dir.unwrap_or(k.default_dir))),
        Some(None) => None,
        None => {
            if dir.is_some() {
                fields.fault(Fault::invalid(
                    "sort_dir requires sort_key".to_string(),
                    "sort_dir",
                ));
            }
            None
        }
    }
}

/// The answer to a list read of `window.fetch()` rows: the page, the
/// filtered total, and a token for the next page when there is one.
/// `next_page_token` is absent on the last page.
pub(crate) fn answer<T: Serialize, K: Serialize>(
    rows: Vec<T>,
    window: &Window<K>,
    total: i64,
    key: impl Fn(&T) -> K,
) -> Response<Body> {
    let (items, next) = keyset_page(rows, window.page_size, key);
    let mut body = serde_json::json!({ "items": items, "total_count": total });
    if let Some(k) = next {
        body["next_page_token"] = serde_json::Value::String(encode(k, &window.fingerprint));
    }
    json(StatusCode::OK, body)
}

/// A short hash of a query. It detects a token reused on another query; it
/// is not a secret, and a collision costs a page of the wrong query.
fn fingerprint(query: &impl Serialize) -> String {
    let canonical = serde_json::to_vec(query).unwrap_or_default();
    hex::encode(&Sha256::digest(canonical)[..6])
}

fn encode<K: Serialize>(k: K, fingerprint: &str) -> String {
    let token = Token {
        k,
        f: fingerprint.to_string(),
    };
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(&token).unwrap_or_default())
}

fn decode<K: DeserializeOwned>(token: &str) -> Option<Token<K>> {
    let raw = URL_SAFE_NO_PAD.decode(token).ok()?;
    serde_json::from_slice(&raw).ok()
}
