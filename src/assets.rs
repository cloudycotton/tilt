//! The web client, embedded at build time and served with validators (brief section 5.11).
//! `Cache-Control: no-cache` plus a strong ETag makes WebViews revalidate on every load, so they
//! never run old JS against a new binary, while unchanged files cost only a 304.

use std::sync::LazyLock;

use axum::body::Body;
use axum::http::header::{
    CACHE_CONTROL, CONTENT_TYPE, ETAG, IF_NONE_MATCH, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;

pub struct Asset {
    pub path: &'static str,
    pub content_type: &'static str,
    pub body: &'static [u8],
}

const JS: &str = "text/javascript; charset=utf-8";

pub static ASSETS: &[Asset] = &[
    Asset {
        path: "/",
        content_type: "text/html; charset=utf-8",
        body: include_bytes!("../web/index.html"),
    },
    Asset {
        path: "/app.js",
        content_type: JS,
        body: include_bytes!("../web/app.js"),
    },
    Asset {
        path: "/input.js",
        content_type: JS,
        body: include_bytes!("../web/input.js"),
    },
    Asset {
        path: "/keysyms.js",
        content_type: JS,
        body: include_bytes!("../web/keysyms.js"),
    },
];

/// One strong ETag per entry of ASSETS, in the same order.
static ETAGS: LazyLock<Vec<HeaderValue>> = LazyLock::new(|| {
    ASSETS
        .iter()
        .map(|a| {
            let tag = format!("\"{:016x}{:08x}\"", fnv1a64(a.body), a.body.len());
            HeaderValue::from_str(&tag).expect("hex ETag is a valid header value")
        })
        .collect()
});

/// Computes every ETag now, so the first request does not pay for it.
pub fn init() {
    LazyLock::force(&ETAGS);
}

pub fn find(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|a| a.path == path)
}

/// 200 with the body, or 304 when `If-None-Match` carries the asset's ETag. Both carry the
/// ETag, `Cache-Control: no-cache` and `X-Content-Type-Options: nosniff`.
pub fn respond(asset: &'static Asset, headers: &HeaderMap) -> Response {
    let etag = ASSETS
        .iter()
        .position(|a| std::ptr::eq(a, asset))
        .map(|i| ETAGS[i].clone());
    let not_modified = etag.as_ref().is_some_and(|tag| {
        headers
            .get_all(IF_NONE_MATCH)
            .iter()
            .any(|v| if_none_match_hits(v.as_bytes(), tag.as_bytes()))
    });
    let mut res = if not_modified {
        Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .body(Body::empty())
    } else {
        Response::builder()
            .header(CONTENT_TYPE, asset.content_type)
            .body(Body::from(asset.body))
    }
    .expect("static response parts are valid");
    let h = res.headers_mut();
    if let Some(tag) = etag {
        h.insert(ETAG, tag);
    }
    h.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    res
}

/// If-None-Match uses weak comparison (RFC 9110 13.1.2): `*`, or any listed tag whose opaque
/// part equals ours, with or without a `W/` prefix.
fn if_none_match_hits(header: &[u8], etag: &[u8]) -> bool {
    header.split(|&b| b == b',').any(|item| {
        let item = item.trim_ascii();
        item == b"*" || item.strip_prefix(b"W/").unwrap_or(item) == etag
    })
}

fn fnv1a64(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn body(res: Response) -> Vec<u8> {
        axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    fn with_inm(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(IF_NONE_MATCH, HeaderValue::from_str(v).unwrap());
        h
    }

    #[tokio::test]
    async fn serves_every_asset_with_validators() {
        init();
        for (asset, want_type) in ASSETS.iter().zip([
            "text/html; charset=utf-8",
            "text/javascript; charset=utf-8",
            "text/javascript; charset=utf-8",
            "text/javascript; charset=utf-8",
        ]) {
            let res = respond(asset, &HeaderMap::new());
            assert_eq!(res.status(), StatusCode::OK);
            let h = res.headers();
            assert_eq!(h[CONTENT_TYPE], want_type, "{}", asset.path);
            assert_eq!(h[CACHE_CONTROL], "no-cache");
            assert_eq!(h[X_CONTENT_TYPE_OPTIONS], "nosniff");
            let tag = h[ETAG].to_str().unwrap().to_owned();
            assert!(tag.starts_with('"') && tag.ends_with('"') && tag.len() == 26);
            assert_eq!(body(res).await, asset.body);
            // Computed once: the same tag every time.
            assert_eq!(
                respond(asset, &HeaderMap::new()).headers()[ETAG],
                tag.as_str()
            );
        }
    }

    #[tokio::test]
    async fn if_none_match_gives_304() {
        let asset = &ASSETS[1];
        let tag = respond(asset, &HeaderMap::new()).headers()[ETAG]
            .to_str()
            .unwrap()
            .to_owned();
        for inm in [
            tag.clone(),
            format!("W/{tag}"),
            format!("\"other\", {tag}"),
            format!("\"a\",{tag} , \"b\""),
            "*".to_owned(),
        ] {
            let res = respond(asset, &with_inm(&inm));
            assert_eq!(res.status(), StatusCode::NOT_MODIFIED, "{inm}");
            assert_eq!(res.headers()[ETAG], tag.as_str());
            assert_eq!(res.headers()[CACHE_CONTROL], "no-cache");
            assert_eq!(res.headers()[X_CONTENT_TYPE_OPTIONS], "nosniff");
            assert!(res.headers().get(CONTENT_TYPE).is_none());
            assert!(body(res).await.is_empty());
        }
        for inm in ["\"other\"", "", "W/\"x\"", &tag[1..]] {
            assert_eq!(
                respond(asset, &with_inm(inm)).status(),
                StatusCode::OK,
                "{inm}"
            );
        }
    }

    #[test]
    fn etags_track_content() {
        assert_ne!(fnv1a64(b"a"), fnv1a64(b"b"));
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
