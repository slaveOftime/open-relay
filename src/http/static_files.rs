//! Static representation, cache policy, and conditional-request handling.
//! Filename shape alone never makes operator files public or immutable.

use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use rust_embed::EmbeddedFile;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    io::{self, Read},
    path::Path,
    sync::OnceLock,
};

const IMMUTABLE_CACHE: &str = "public, max-age=31536000, immutable";
const REVALIDATE_CACHE: &str = "no-cache";
const PRIVATE_CACHE: &str = "private, no-cache";

pub(super) struct StaticAsset {
    bytes: Cow<'static, [u8]>,
    etag: HeaderValue,
    cache_control: &'static str,
}

impl StaticAsset {
    pub(super) fn embedded(path: &str, asset: EmbeddedFile) -> Self {
        // rust-embed reads the live dist tree in debug builds, so those URLs
        // are not immutable. Release builds use a compile-time representation.
        let cache_control = if !cfg!(debug_assertions) && generated_assets().contains(path) {
            IMMUTABLE_CACHE
        } else {
            REVALIDATE_CACHE
        };
        Self {
            etag: etag_from_sha256(asset.metadata.sha256_hash()),
            bytes: asset.data,
            cache_control,
        }
    }

    fn local(bytes: Vec<u8>) -> Self {
        Self {
            etag: etag_from_sha256(Sha256::digest(&bytes).into()),
            bytes: Cow::Owned(bytes),
            // These files are mutable operator code behind auth, including
            // wwwroot overrides of otherwise public build URLs.
            cache_control: PRIVATE_CACHE,
        }
    }

    pub(super) fn into_response(
        self,
        method: &Method,
        path: impl AsRef<Path>,
        request_headers: &HeaderMap,
    ) -> Response {
        // Proxies have their own method handling; a static resource cannot
        // process unsafe methods. Evaluate this before cache preconditions.
        if method != Method::GET && method != Method::HEAD {
            return (
                StatusCode::METHOD_NOT_ALLOWED,
                [
                    (header::ALLOW, "GET, HEAD"),
                    (header::CACHE_CONTROL, "no-store"),
                ],
            )
                .into_response();
        }
        let not_modified = etag_matches(request_headers, &self.etag);
        let len = self.bytes.len();
        let body = if not_modified || method == Method::HEAD {
            Body::empty()
        } else {
            // Avoid copying embedded bytes, especially on a 304 or HEAD.
            match self.bytes {
                Cow::Borrowed(bytes) => Body::from(bytes),
                Cow::Owned(bytes) => Body::from(bytes),
            }
        };
        let mut response = Response::new(body);
        if not_modified {
            *response.status_mut() = StatusCode::NOT_MODIFIED;
        } else {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_str(mime.essence_str()).expect("valid MIME header"),
            );
            response
                .headers_mut()
                .insert(header::CONTENT_LENGTH, len.into());
        }
        let headers = response.headers_mut();
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(self.cache_control),
        );
        headers.insert(header::ETAG, self.etag);
        // CompressionLayer adds this to compressible 200s, but not bodyless
        // 304s. Keep the negotiated representation's cache key on every reply.
        headers.insert(header::VARY, HeaderValue::from_static("accept-encoding"));
        response
    }
}

pub(super) async fn read_local_asset(
    wwwroot: &Path,
    relative: &str,
) -> io::Result<Option<StaticAsset>> {
    let path = wwwroot.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
    read_static_file(&path).await
}

pub(super) async fn read_static_file(path: &Path) -> io::Result<Option<StaticAsset>> {
    let path = path.to_owned();
    // Read and hash off the async executor. Recheck type on the opened handle,
    // since the pathname can be replaced between metadata and read. The validator
    // describes the actual response bytes, even if the file changes in place.
    tokio::task::spawn_blocking(move || {
        // Reject directories (not openable as Files on Windows) and special
        // files such as FIFOs before attempting a potentially blocking open.
        match std::fs::metadata(&path) {
            Ok(metadata) if !metadata.is_file() => return Ok(None),
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        }
        let mut file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        if !file.metadata()?.is_file() {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(Some(StaticAsset::local(bytes)))
    })
    .await
    .map_err(io::Error::other)?
}

/// Weak content validators remain valid across identity/gzip/br encodings.
/// Compression changes representation bytes, so a shared strong tag would
/// violate RFC 9110 §8.8.3.3. Weak does not mean approximate content tracking.
fn etag_from_sha256(hash: [u8; 32]) -> HeaderValue {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    HeaderValue::try_from(format!("W/\"{}\"", URL_SAFE_NO_PAD.encode(hash)))
        .expect("base64url is a visible-ASCII header value")
}

#[derive(Deserialize)]
struct ManifestEntry {
    file: String,
    #[serde(default)]
    css: Vec<String>,
    #[serde(default)]
    assets: Vec<String>,
}

fn generated_assets() -> &'static HashSet<String> {
    static ASSETS: OnceLock<HashSet<String>> = OnceLock::new();
    ASSETS.get_or_init(|| {
        super::WebAssets::get(".vite/manifest.json")
            .map(|asset| parse_generated_assets(&asset.data))
            .unwrap_or_default()
    })
}

fn parse_generated_assets(manifest: &[u8]) -> HashSet<String> {
    let Ok(entries) = serde_json::from_slice::<HashMap<String, ManifestEntry>>(manifest) else {
        // No trustworthy build inventory means conservative revalidation.
        return HashSet::new();
    };
    entries
        .into_values()
        .flat_map(|entry| {
            std::iter::once(entry.file)
                .chain(entry.css)
                .chain(entry.assets)
        })
        .filter(|path| is_content_hashed_asset(Path::new(path)))
        .collect()
}

/// Only top-level generated assets with Vite's pinned eight-character hash
/// shape are eligible. The build manifest supplies provenance: copied public
/// files and similarly named operator files do not earn immutable caching.
fn is_content_hashed_asset(path: &Path) -> bool {
    const HASH_LEN: usize = 8;
    if path.parent() != Some(Path::new("assets")) || path.extension().is_none() {
        return false;
    }
    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    let Some(hash_start) = stem.len().checked_sub(HASH_LEN) else {
        return false;
    };
    let (Some(prefix), Some(hash)) = (stem.get(..hash_start), stem.get(hash_start..)) else {
        return false;
    };
    prefix.len() > 1
        && prefix.ends_with('-')
        && hash
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

/// If-None-Match is a list-valued field: combine every occurrence, preserve
/// commas in quoted opaque tags, and weakly compare. Malformed input falls
/// back to a full response, never a false 304. `*` is only valid by itself.
fn etag_matches(headers: &HeaderMap, etag: &HeaderValue) -> bool {
    if !headers.contains_key(header::IF_NONE_MATCH) {
        return false;
    }
    // Opaque tags permit obs-text bytes. Do not use HeaderValue::to_str(),
    // which would discard a valid match if another tag is non-ASCII.
    let mut candidates = Vec::new();
    for (index, value) in headers.get_all(header::IF_NONE_MATCH).iter().enumerate() {
        if index != 0 {
            candidates.push(b',');
        }
        candidates.extend_from_slice(value.as_bytes());
    }
    if candidates.trim_ascii() == b"*" {
        return true;
    }
    let tags = split_etag_list(&candidates);
    if !tags.iter().all(|tag| valid_etag(tag)) {
        return false;
    }
    let target = etag.as_bytes();
    let target = target.strip_prefix(b"W/").unwrap_or(target);
    tags.into_iter()
        .any(|tag| tag.strip_prefix(b"W/").unwrap_or(tag) == target)
}

fn valid_etag(tag: &[u8]) -> bool {
    let tag = tag.strip_prefix(b"W/").unwrap_or(tag);
    let Some(opaque) = tag
        .strip_prefix(b"\"")
        .and_then(|tag| tag.strip_suffix(b"\""))
    else {
        return false;
    };
    opaque
        .iter()
        .all(|&b| b == 0x21 || (0x23..=0x7e).contains(&b) || b >= 0x80)
}

fn split_etag_list(value: &[u8]) -> Vec<&[u8]> {
    let mut tags = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    for (index, &ch) in value.iter().enumerate() {
        match ch {
            b'"' => in_quotes = !in_quotes,
            b',' if !in_quotes => {
                tags.push(value[start..index].trim_ascii());
                start = index + 1;
            }
            _ => {}
        }
    }
    tags.push(value[start..].trim_ascii());
    tags.retain(|tag| !tag.is_empty());
    tags
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(cache_control: &'static str) -> StaticAsset {
        StaticAsset {
            bytes: Cow::Borrowed(b"<html>shell</html>"),
            etag: etag_from_sha256([7; 32]),
            cache_control,
        }
    }

    #[test]
    fn immutable_assets_require_generated_provenance_and_recognized_names() {
        let manifest = br#"{
            "index.html":{"file":"assets/index-BV0Ld5m9.js","css":["assets/index-CoUxclTT.css"],"assets":["assets/icon-sLQPUA_C.svg"]},
            "radix":{"file":"assets/radix-vendor-CdGYLb-D.js"},
            "detail":{"file":"assets/SessionDetailPage-CXLxYF5q.js"},
            "unhashed":{"file":"assets/runtime.js"},
            "font":{"file":"fonts/JetBrainsMono-Regular.ttf"}
        }"#;
        let assets = parse_generated_assets(manifest);
        for path in [
            "assets/index-BV0Ld5m9.js",
            "assets/index-CoUxclTT.css",
            "assets/icon-sLQPUA_C.svg",
            "assets/radix-vendor-CdGYLb-D.js",
            "assets/SessionDetailPage-CXLxYF5q.js",
        ] {
            assert!(assets.contains(path), "{path}");
        }
        // Copied public files can have the same shape without a build hash.
        for path in [
            "assets/public-Regular.js",
            "assets/runtime.js",
            "fonts/JetBrainsMono-Regular.ttf",
            "index.html",
            "sw.js",
            "manifest.webmanifest",
        ] {
            assert!(!assets.contains(path), "{path}");
        }
        assert!(parse_generated_assets(b"invalid").is_empty());
    }

    #[test]
    fn unrecognized_or_nested_names_never_earn_immutable() {
        for path in [
            "assets",
            "assets/-abc12345.js",
            "assets/index-abc1234.js",
            "assets/index-abc123456.js",
            "assets/index-abc1234!.js",
            "nested/assets/index-abc12345.js",
            "assets/inner/index-abc12345.js",
            "fonts/JetBrainsMono-Regular.ttf",
            "workbox-fed2bdfe.js",
        ] {
            assert!(!is_content_hashed_asset(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn operator_files_are_private_even_with_build_shaped_names() {
        let response = StaticAsset::local(b"operator override".to_vec()).into_response(
            &Method::GET,
            "assets/main-abc12345.js",
            &HeaderMap::new(),
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], PRIVATE_CACHE);
        assert!(
            response.headers()[header::ETAG]
                .to_str()
                .unwrap()
                .starts_with("W/\"")
        );
    }

    #[tokio::test]
    async fn responses_keep_representation_headers_and_conditional_heads_are_bodyless() {
        for policy in [IMMUTABLE_CACHE, REVALIDATE_CACHE, PRIVATE_CACHE] {
            for method in [Method::GET, Method::HEAD] {
                let response =
                    asset(policy).into_response(&method, "index.html", &HeaderMap::new());
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(response.headers()[header::CACHE_CONTROL], policy);
                assert_eq!(response.headers()[header::CONTENT_TYPE], "text/html");
                assert_eq!(response.headers()[header::CONTENT_LENGTH], "18");
                let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                assert_eq!(body.is_empty(), method == Method::HEAD);
                let mut headers = HeaderMap::new();
                headers.insert(header::IF_NONE_MATCH, asset(policy).etag);
                let response = asset(policy).into_response(&method, "index.html", &headers);
                assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
                assert_eq!(response.headers()[header::CACHE_CONTROL], policy);
                assert_eq!(response.headers()[header::VARY], "accept-encoding");
                assert_eq!(response.headers()[header::ETAG], asset(policy).etag);
                assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
                assert!(
                    axum::body::to_bytes(response.into_body(), usize::MAX)
                        .await
                        .unwrap()
                        .is_empty()
                );
            }
        }
    }

    #[test]
    fn static_resources_reject_unsafe_methods_before_preconditions() {
        for method in [Method::POST, Method::PUT, Method::DELETE, Method::OPTIONS] {
            let mut headers = HeaderMap::new();
            headers.insert(header::IF_NONE_MATCH, asset(REVALIDATE_CACHE).etag);
            let response = asset(REVALIDATE_CACHE).into_response(&method, "index.html", &headers);
            assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
            assert_eq!(response.headers()[header::ALLOW], "GET, HEAD");
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        }
    }

    fn matches(value: &str, target: &str) -> bool {
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, HeaderValue::from_str(value).unwrap());
        etag_matches(&headers, &HeaderValue::from_str(target).unwrap())
    }

    #[test]
    fn if_none_match_matches_weakly_and_parses_lists() {
        assert!(matches("W/\"abc123\"", "\"abc123\""));
        assert!(matches("\"xyz\"", "W/\"xyz\""));
        assert!(matches("\"a,b\", W/\"abc123\"", "\"abc123\""));
        assert!(matches("\"a,b\"", "W/\"a,b\""));
        assert!(matches(" , \"xyz\", , ", "W/\"xyz\""));
        assert!(matches("\"\"", "W/\"\""));
        assert!(matches(" * ", "\"abc123\""));
        assert!(!matches(" \"nope\" ", "\"abc123\""));
        assert!(!matches("", "\"abc123\""));
    }

    #[test]
    fn malformed_conditions_never_produce_a_false_304() {
        for value in [
            "*, \"abc\"",
            "*, *",
            "w/\"abc\"",
            "\"abc\", bogus",
            "\"abc\", W/\"unterminated",
            "\"abc\", \"white space\"",
            "\"abc\"junk",
            "abc",
        ] {
            assert!(!matches(value, "W/\"abc\""), "{value}");
        }
    }

    #[test]
    fn repeated_if_none_match_fields_are_combined() {
        let mut headers = HeaderMap::new();
        headers.append(header::IF_NONE_MATCH, HeaderValue::from_static("\"other\""));
        headers.append(header::IF_NONE_MATCH, asset(REVALIDATE_CACHE).etag);
        let response = asset(REVALIDATE_CACHE).into_response(&Method::GET, "index.html", &headers);
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    }

    #[test]
    fn obs_text_in_other_opaque_tags_does_not_hide_a_valid_match() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_bytes(b"\"\xff\", W/\"abc\"").unwrap(),
        );
        assert!(etag_matches(
            &headers,
            &HeaderValue::from_static("W/\"abc\"")
        ));
    }

    #[tokio::test]
    async fn disk_etags_detect_equal_length_rewrites_with_preserved_mtime() {
        let dir = std::env::temp_dir().join(format!("oly-http-rewrite-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("asset.txt");
        std::fs::write(&path, b"old").unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let first = read_static_file(&path).await.unwrap().unwrap();
        let unchanged = read_static_file(&path).await.unwrap().unwrap();
        assert_eq!(first.etag, unchanged.etag);
        std::fs::write(&path, b"new").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        let second = read_static_file(&path).await.unwrap().unwrap();
        assert_ne!(first.bytes, second.bytes);
        assert_ne!(first.etag, second.etag);
        assert!(read_static_file(&dir).await.unwrap().is_none());
        assert!(
            read_static_file(&dir.join("missing"))
                .await
                .unwrap()
                .is_none()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn compression_uses_weak_etags_and_304_keeps_vary() {
        let app = axum::Router::new()
            .route(
                "/",
                axum::routing::get(|headers: HeaderMap| async move {
                    StaticAsset::local(vec![b'x'; 4096]).into_response(
                        &Method::GET,
                        "index.html",
                        &headers,
                    )
                }),
            )
            .layer(tower_http::compression::CompressionLayer::new());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();
        let mut previous_etag = None;
        for encoding in ["identity", "gzip", "br"] {
            let response = client
                .get(&url)
                .header(header::ACCEPT_ENCODING, encoding)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            if encoding != "identity" {
                assert_eq!(response.headers()[header::CONTENT_ENCODING], encoding);
            }
            let etag = response.headers()[header::ETAG].clone();
            assert!(etag.to_str().unwrap().starts_with("W/"));
            if let Some(previous) = &previous_etag {
                assert_eq!(&etag, previous)
            }
            previous_etag = Some(etag.clone());
            let body = response.bytes().await.unwrap();
            if encoding == "identity" {
                assert_eq!(body.as_ref(), vec![b'x'; 4096]);
            } else {
                assert!(
                    body.len() < 4096,
                    "the representation bytes actually changed"
                );
            }
            let response = client
                .get(&url)
                .header(header::ACCEPT_ENCODING, encoding)
                .header(header::IF_NONE_MATCH, etag)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
            assert!(
                response.headers()[header::VARY]
                    .to_str()
                    .unwrap()
                    .contains("accept-encoding")
            );
            assert!(!response.headers().contains_key(header::CONTENT_ENCODING));
            assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
            assert!(response.bytes().await.unwrap().is_empty());
        }
        server.abort();
        let _ = server.await;
    }

    #[test]
    fn embedded_etags_are_weak_content_hashes() {
        let first = etag_from_sha256([0; 32]);
        assert!(first.to_str().unwrap().starts_with("W/\""));
        assert_ne!(first, etag_from_sha256([1; 32]));
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn release_bundle_manifest_drives_actual_immutable_responses() {
        let assets = generated_assets();
        assert!(
            !assets.is_empty(),
            "release builds must embed the Vite manifest"
        );
        for path in assets {
            let asset = super::super::WebAssets::get(path).expect("manifest file exists");
            let expected_etag = etag_from_sha256(Sha256::digest(asset.data.as_ref()).into());
            let response = StaticAsset::embedded(path, asset).into_response(
                &Method::GET,
                path,
                &HeaderMap::new(),
            );
            assert_eq!(response.headers()[header::CACHE_CONTROL], IMMUTABLE_CACHE);
            assert_eq!(response.headers()[header::ETAG], expected_etag);
        }
        for path in [
            "index.html",
            "sw.js",
            "manifest.webmanifest",
            "fonts/JetBrainsMono-Regular.ttf",
        ] {
            let asset = super::super::WebAssets::get(path).expect("public file exists");
            let response = StaticAsset::embedded(path, asset).into_response(
                &Method::GET,
                path,
                &HeaderMap::new(),
            );
            assert_eq!(response.headers()[header::CACHE_CONTROL], REVALIDATE_CACHE);
        }
    }
}
