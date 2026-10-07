//! The web client, embedded at build time and served with validators (brief section 5.11).
//! `Cache-Control: no-cache` plus a strong ETag makes WebViews revalidate on every load, so they
//! never run old JS against a new binary, while unchanged files cost only a 304. Bodies are
//! gzipped once at startup for clients that accept it.

use std::sync::LazyLock;

use axum::body::Body;
use axum::http::header::{
    ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_TYPE, ETAG, IF_NONE_MATCH, VARY,
    X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use bytes::Bytes;

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
    Asset {
        path: "/icon.svg",
        content_type: "image/svg+xml",
        body: include_bytes!("../web/icon.svg"),
    },
];

/// What is served for one entry of ASSETS, computed once.
struct Variants {
    etag: HeaderValue,
    /// The body gzipped, with its own ETag: a strong validator names one representation.
    gzip: Bytes,
    gzip_etag: HeaderValue,
}

/// One entry per entry of ASSETS, in the same order.
static VARIANTS: LazyLock<Vec<Variants>> = LazyLock::new(|| {
    ASSETS
        .iter()
        .map(|a| {
            let hex = format!("{:016x}{:08x}", fnv1a64(a.body), a.body.len());
            let tag = |suffix| {
                HeaderValue::from_str(&format!("\"{hex}{suffix}\""))
                    .expect("hex ETag is a valid header value")
            };
            Variants {
                etag: tag(""),
                gzip: Bytes::from(gzip(a.body)),
                gzip_etag: tag("-gz"),
            }
        })
        .collect()
});

/// Compresses every asset and computes its ETags now, so the first request does not pay.
pub fn init() {
    LazyLock::force(&VARIANTS);
}

/// 200 with the body, or 304 when `If-None-Match` carries the ETag of what would be sent. The
/// body is gzipped for clients that accept it (about a third of the bytes on a page load).
/// Both carry the ETag, `Cache-Control: no-cache`, `Vary: Accept-Encoding` and
/// `X-Content-Type-Options: nosniff`.
pub fn respond(index: usize, headers: &HeaderMap) -> Response {
    let asset = &ASSETS[index];
    let variants = &VARIANTS[index];
    let gzipped = accepts_gzip(headers);
    let etag = if gzipped {
        &variants.gzip_etag
    } else {
        &variants.etag
    };
    let not_modified = headers
        .get_all(IF_NONE_MATCH)
        .iter()
        .any(|v| if_none_match_hits(v.as_bytes(), etag.as_bytes()));
    let mut res = if not_modified {
        Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .body(Body::empty())
    } else if gzipped {
        Response::builder()
            .header(CONTENT_TYPE, asset.content_type)
            .header(CONTENT_ENCODING, "gzip")
            .body(Body::from(variants.gzip.clone()))
    } else {
        Response::builder()
            .header(CONTENT_TYPE, asset.content_type)
            .body(Body::from(asset.body))
    }
    .expect("static response parts are valid");
    let h = res.headers_mut();
    h.insert(ETAG, etag.clone());
    h.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert(VARY, HeaderValue::from_static("accept-encoding"));
    h.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    res
}

/// Whether `Accept-Encoding` lists gzip (or `*`) without `q=0`.
fn accepts_gzip(headers: &HeaderMap) -> bool {
    headers
        .get_all(ACCEPT_ENCODING)
        .iter()
        .flat_map(|v| v.as_bytes().split(|&b| b == b','))
        .any(|item| {
            let mut parts = item.split(|&b| b == b';').map(<[u8]>::trim_ascii);
            let coding = parts.next().unwrap_or_default();
            let refused = parts.any(|p| {
                p.strip_prefix(b"q=")
                    .is_some_and(|q| q.iter().all(|&b| b == b'0' || b == b'.'))
            });
            (coding.eq_ignore_ascii_case(b"gzip") || coding == b"*") && !refused
        })
}

/// A gzip member (RFC 1952) holding `data`, deflated at the best level.
fn gzip(data: &[u8]) -> Vec<u8> {
    // No name, no mtime, OS unknown.
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 2, 255];
    out.extend(miniz_oxide::deflate::compress_to_vec(data, 9));
    out.extend(crc32(data).to_le_bytes());
    out.extend((data.len() as u32).to_le_bytes());
    out
}

/// CRC-32 (IEEE), bit by bit: it runs once per asset at startup.
fn crc32(data: &[u8]) -> u32 {
    !data.iter().fold(!0u32, |crc, &b| {
        (0..8).fold(crc ^ u32::from(b), |c, _| {
            (c >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(c & 1))
        })
    })
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

    fn accept(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(ACCEPT_ENCODING, HeaderValue::from_str(v).unwrap());
        h
    }

    #[tokio::test]
    async fn serves_every_asset_with_validators() {
        init();
        for (i, (asset, want_type)) in ASSETS
            .iter()
            .zip([
                "text/html; charset=utf-8",
                "text/javascript; charset=utf-8",
                "text/javascript; charset=utf-8",
                "text/javascript; charset=utf-8",
                "image/svg+xml",
            ])
            .enumerate()
        {
            let res = respond(i, &HeaderMap::new());
            assert_eq!(res.status(), StatusCode::OK);
            let h = res.headers();
            assert_eq!(h[CONTENT_TYPE], want_type, "{}", asset.path);
            assert_eq!(h[CACHE_CONTROL], "no-cache");
            assert_eq!(h[VARY], "accept-encoding");
            assert_eq!(h[X_CONTENT_TYPE_OPTIONS], "nosniff");
            assert!(h.get(CONTENT_ENCODING).is_none());
            let tag = h[ETAG].to_str().unwrap().to_owned();
            assert!(tag.starts_with('"') && tag.ends_with('"') && tag.len() == 26);
            assert_eq!(body(res).await, asset.body);
            // Computed once: the same tag every time.
            assert_eq!(respond(i, &HeaderMap::new()).headers()[ETAG], tag.as_str());

            // Gzipped for a browser, smaller, with a tag of its own, and intact.
            let res = respond(i, &accept("gzip, deflate, br, zstd"));
            let h = res.headers();
            assert_eq!(h[CONTENT_ENCODING], "gzip");
            assert_eq!(h[CONTENT_TYPE], want_type);
            assert_eq!(h[VARY], "accept-encoding");
            let gz_tag = h[ETAG].to_str().unwrap().to_owned();
            assert_ne!(gz_tag, tag);
            let gz = body(res).await;
            assert!(
                gz.len() < asset.body.len(),
                "{}: {} bytes",
                asset.path,
                gz.len()
            );
            assert_eq!(gunzip(&gz), asset.body, "{}", asset.path);
        }
    }

    /// Checks the gzip framing and inflates the member.
    fn gunzip(gz: &[u8]) -> Vec<u8> {
        assert_eq!(gz[..4], [0x1f, 0x8b, 8, 0]);
        let n = gz.len();
        let data =
            miniz_oxide::inflate::decompress_to_vec(&gz[10..n - 8]).expect("valid deflate data");
        assert_eq!(gz[n - 8..n - 4], crc32(&data).to_le_bytes());
        assert_eq!(gz[n - 4..], (data.len() as u32).to_le_bytes());
        data
    }

    #[test]
    fn gzip_is_used_only_when_accepted() {
        for yes in [
            "gzip",
            "GZIP",
            "br;q=1.0, gzip;q=0.8",
            "*",
            "deflate,gzip",
            "gzip;q=1",
        ] {
            assert!(accepts_gzip(&accept(yes)), "{yes}");
        }
        for no in [
            "",
            "identity",
            "br",
            "gzip;q=0",
            "gzip; q=0.0",
            "x-gzip-not",
        ] {
            assert!(!accepts_gzip(&accept(no)), "{no}");
        }
        assert!(!accepts_gzip(&HeaderMap::new()));
        // CRC-32 check value.
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(gunzip(&gzip(b"")), b"");
    }

    #[tokio::test]
    async fn if_none_match_gives_304() {
        for headers in [HeaderMap::new(), accept("gzip")] {
            let tag = respond(1, &headers).headers()[ETAG]
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
                let mut h = with_inm(&inm);
                h.extend(headers.clone());
                let res = respond(1, &h);
                assert_eq!(res.status(), StatusCode::NOT_MODIFIED, "{inm}");
                assert_eq!(res.headers()[ETAG], tag.as_str());
                assert_eq!(res.headers()[CACHE_CONTROL], "no-cache");
                assert_eq!(res.headers()[X_CONTENT_TYPE_OPTIONS], "nosniff");
                assert!(res.headers().get(CONTENT_TYPE).is_none());
                assert!(body(res).await.is_empty());
            }
            for inm in ["\"other\"", "", "W/\"x\"", &tag[1..]] {
                let mut h = with_inm(inm);
                h.extend(headers.clone());
                assert_eq!(respond(1, &h).status(), StatusCode::OK, "{inm}");
            }
        }
        // The plain body's tag does not validate the gzipped one, nor the other way round.
        let plain = respond(1, &HeaderMap::new()).headers()[ETAG].clone();
        let mut h = with_inm(plain.to_str().unwrap());
        h.extend(accept("gzip"));
        assert_eq!(respond(1, &h).status(), StatusCode::OK);
    }

    #[test]
    fn etags_track_content() {
        assert_ne!(fnv1a64(b"a"), fnv1a64(b"b"));
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
