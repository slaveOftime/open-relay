//! Proxy-target URL construction + SSRF guard.
//!
//! Lifted from `src/http/apps.rs`.
use reqwest::Url;
use std::io;

/// Build the list of upstream URLs to try, ordered so that the most specific
/// matching variant appears first and "root-relative" / "public-path" fall
/// through on 404.
pub(super) fn build_proxy_target_urls(
    entry_url: &Url,
    request_tail: &str,
    query: Option<&str>,
) -> io::Result<Vec<Url>> {
    let mut targets = Vec::new();
    if request_tail.is_empty() {
        targets.push(with_proxy_query(entry_url.clone(), query));
    } else {
        let entry_relative = entry_url.join(request_tail).map_err(|err| {
            invalid_data(format!(
                "failed to join proxied app URL {entry_url} with {request_tail}: {err}"
            ))
        })?;
        targets.push(with_proxy_query(entry_relative, query));

        let root_relative = origin_root_url(entry_url)
            .join(request_tail)
            .map_err(|err| {
                invalid_data(format!(
                    "failed to build root-relative proxied app URL {entry_url} with {request_tail}: {err}"
                ))
            })?;
        let root_relative = with_proxy_query(root_relative, query);
        if !targets.iter().any(|existing| existing == &root_relative) {
            targets.push(root_relative);
        }

        let public_path_relative = origin_root_url(entry_url)
            .join(request_tail.trim_start_matches('/'))
            .map_err(|err| {
                invalid_data(format!(
                    "failed to build public-path proxied app URL {entry_url} with {request_tail}: {err}"
                ))
            })?;
        let public_path_relative = with_proxy_query(public_path_relative, query);
        if !targets
            .iter()
            .any(|existing| existing == &public_path_relative)
        {
            targets.push(public_path_relative);
        }
    }

    // URL joining must not expand a valid manifest into a different host.
    if targets
        .iter()
        .any(|target| !is_loopback_proxy_target(target) || target.origin() != entry_url.origin())
    {
        return Err(invalid_data(
            "app proxy request must stay on its configured loopback origin",
        ));
    }
    Ok(targets)
}

fn with_proxy_query(mut target: Url, query: Option<&str>) -> Url {
    if let Some(filtered_query) = filtered_proxy_query(query) {
        let merged_query = match target.query() {
            Some(existing) if !existing.is_empty() => format!("{existing}&{filtered_query}"),
            _ => filtered_query,
        };
        target.set_query(Some(&merged_query));
    }

    target
}

fn origin_root_url(entry_url: &Url) -> Url {
    let mut root = entry_url.clone();
    root.set_path("/");
    root.set_query(None);
    root.set_fragment(None);
    root
}

pub(in crate::http) fn filtered_proxy_query(query: Option<&str>) -> Option<String> {
    let query = query?;
    let filtered = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter(|pair| *pair != "token" && !pair.starts_with("token="))
        .collect::<Vec<_>>();
    if filtered.is_empty() {
        None
    } else {
        Some(filtered.join("&"))
    }
}

pub(super) fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Only explicit loopback literals are supported; hostnames never enter DNS.
/// Strip IPv6 URL brackets before parsing; never resolve a domain.
pub(super) fn is_loopback_proxy_target(url: &Url) -> bool {
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return false;
    }
    url.host_str()
        .and_then(|host| {
            host.trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .ok()
        })
        .is_some_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_scope_is_loopback_literals_only() {
        for value in [
            "http://127.0.0.1:5173/",
            "https://127.0.0.2/",
            "http://[::1]:5173/",
        ] {
            assert!(
                is_loopback_proxy_target(&Url::parse(value).unwrap()),
                "{value}"
            );
        }
        for value in [
            "http://localhost:5173/",
            "https://example.com/",
            "http://10.0.0.1/",
            "http://169.254.169.254/",
            "http://0.0.0.0/",
            "http://[::]/",
            "http://[fe80::1]/",
            "http://[fc00::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://user:secret@127.0.0.1/",
            "ftp://127.0.0.1/",
        ] {
            assert!(
                !is_loopback_proxy_target(&Url::parse(value).unwrap()),
                "{value}"
            );
        }
    }

    #[test]
    fn joined_targets_never_escape_manifest_origin() {
        let origin = Url::parse("http://127.0.0.1:5173/dash/").unwrap();
        assert!(build_proxy_target_urls(&origin, "//example.com/secret", None).is_err());
        assert!(build_proxy_target_urls(&origin, "http://127.0.0.1:9999/", None).is_err());
        assert!(build_proxy_target_urls(&origin, "assets/app.js", None).is_ok());
    }
}
