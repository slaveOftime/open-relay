//! HTML introspection: title/description/kind/icon extraction and asset-href
//! resolution. PLAN2 S1.2.

use std::path::Path;

use super::StaticAppKind;

pub fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let title_start = lower.find("<title")?;
    let content_start = lower[title_start..].find('>')? + title_start + 1;
    let content_end = lower[content_start..].find("</title>")? + content_start;
    let title = html[content_start..content_end].trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_string())
    }
}

pub fn extract_app_description(html: &str) -> Option<String> {
    extract_meta_content(html, "oly:description")
        .or_else(|| extract_meta_content(html, "description"))
        .or_else(|| extract_meta_content(html, "og:description"))
}

pub fn extract_app_kind(html: &str) -> StaticAppKind {
    if let Some(raw_kind) = extract_meta_content(html, "oly:app-type")
        .or_else(|| extract_meta_content(html, "oly:type"))
        && let Some(app_kind) = StaticAppKind::from_meta_value(&raw_kind)
    {
        return app_kind;
    }

    infer_app_kind(html)
}

pub fn extract_app_icon_href(html: &str, app_href: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let mut offset = 0;

    while let Some(relative_start) = lower[offset..].find("<link") {
        let tag_start = offset + relative_start;
        let tag_end = match lower[tag_start..].find('>') {
            Some(relative_end) => tag_start + relative_end + 1,
            None => break,
        };
        let tag = &html[tag_start..tag_end];
        let rel = extract_html_attribute(tag, "rel");
        let href = extract_html_attribute(tag, "href");

        if rel.as_deref().is_some_and(link_rel_mentions_icon)
            && let Some(icon_href) = href.and_then(|value| resolve_app_asset_href(app_href, &value))
        {
            return Some(icon_href);
        }

        offset = tag_end;
    }

    None
}

pub(super) fn detect_app_icon_href(app_dir: Option<&Path>, app_href: &str) -> Option<String> {
    let app_dir = app_dir?;
    for candidate in [
        "favicon.svg",
        "favicon.ico",
        "favicon.png",
        "apple-touch-icon.png",
    ] {
        if app_dir.join(candidate).is_file() {
            return resolve_app_asset_href(app_href, candidate);
        }
    }

    None
}

fn link_rel_mentions_icon(rel: &str) -> bool {
    rel.split_ascii_whitespace().any(|part| {
        part.eq_ignore_ascii_case("icon")
            || part.eq_ignore_ascii_case("shortcut")
            || part.eq_ignore_ascii_case("apple-touch-icon")
    })
}

pub fn resolve_app_asset_href(app_href: &str, asset_href: &str) -> Option<String> {
    let asset_href = asset_href.trim();
    if asset_href.is_empty()
        || asset_href.starts_with("http://")
        || asset_href.starts_with("https://")
        || asset_href.starts_with("//")
        || asset_href.starts_with("data:")
        || asset_href.starts_with('#')
    {
        return None;
    }

    if asset_href.starts_with('/') {
        return Some(asset_href.to_string());
    }

    let mut base = app_href.trim_end_matches('/').to_string();
    if !base.ends_with('/') {
        base.push('/');
    }

    let normalized = asset_href
        .strip_prefix("./")
        .unwrap_or(asset_href)
        .trim_start_matches('/');
    if normalized.contains("../") {
        return None;
    }

    Some(format!("{base}{normalized}"))
}

pub(super) fn resolve_manifest_asset_href(app_href: &str, asset_href: &str) -> Option<String> {
    let asset_href = asset_href.trim();
    if asset_href.is_empty() {
        return None;
    }

    if asset_href.starts_with("http://")
        || asset_href.starts_with("https://")
        || asset_href.starts_with("//")
        || asset_href.starts_with("data:")
    {
        return Some(asset_href.to_string());
    }

    resolve_app_asset_href(app_href, asset_href)
}

pub fn extract_meta_content(html: &str, attribute_value: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let mut offset = 0;

    while let Some(relative_start) = lower[offset..].find("<meta") {
        let tag_start = offset + relative_start;
        let tag_end = match lower[tag_start..].find('>') {
            Some(relative_end) => tag_start + relative_end + 1,
            None => break,
        };
        let tag = &html[tag_start..tag_end];
        let name =
            extract_html_attribute(tag, "name").or_else(|| extract_html_attribute(tag, "property"));

        if name
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case(attribute_value))
        {
            return extract_html_attribute(tag, "content").filter(|value| !value.is_empty());
        }

        offset = tag_end;
    }

    None
}

fn extract_html_attribute(tag: &str, attribute_name: &str) -> Option<String> {
    let bytes = tag.as_bytes();
    let mut index = 0;

    while index < bytes.len() {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }

        if index >= bytes.len() || matches!(bytes[index], b'<' | b'>' | b'/') {
            index += 1;
            continue;
        }

        let name_start = index;
        while index < bytes.len()
            && !bytes[index].is_ascii_whitespace()
            && bytes[index] != b'='
            && bytes[index] != b'>'
        {
            index += 1;
        }

        if name_start == index {
            index += 1;
            continue;
        }

        let candidate_name = &tag[name_start..index];
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }

        let mut value = String::new();
        if index < bytes.len() && bytes[index] == b'=' {
            index += 1;
            while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                index += 1;
            }

            if index < bytes.len() && matches!(bytes[index], b'"' | b'\'') {
                let quote = bytes[index];
                index += 1;
                let value_start = index;
                while index < bytes.len() && bytes[index] != quote {
                    index += 1;
                }
                value = tag[value_start..index].trim().to_string();
                if index < bytes.len() {
                    index += 1;
                }
            } else {
                let value_start = index;
                while index < bytes.len()
                    && !bytes[index].is_ascii_whitespace()
                    && bytes[index] != b'>'
                {
                    index += 1;
                }
                value = tag[value_start..index].trim().to_string();
            }
        }

        if candidate_name.eq_ignore_ascii_case(attribute_name) {
            return Some(value);
        }
    }

    None
}

fn infer_app_kind(html: &str) -> StaticAppKind {
    let lower = html.to_ascii_lowercase();
    let has_module_script = lower.contains("type=\"module\"") || lower.contains("type='module'");
    let has_mount_root = lower.contains("id=\"root\"")
        || lower.contains("id='root'")
        || lower.contains("id=\"app\"")
        || lower.contains("id='app'");
    let has_asset_pipeline = lower.contains("src=\"./assets/")
        || lower.contains("src=\"assets/")
        || lower.contains("src=\"/assets/")
        || lower.contains("href=\"./assets/")
        || lower.contains("href=\"assets/")
        || lower.contains("href=\"/assets/")
        || lower.contains("src='./assets/")
        || lower.contains("src='assets/")
        || lower.contains("src='/assets/")
        || lower.contains("href='./assets/")
        || lower.contains("href='assets/")
        || lower.contains("href='/assets/");

    if has_module_script || has_mount_root || has_asset_pipeline {
        StaticAppKind::Spa
    } else {
        StaticAppKind::SingleHtml
    }
}
