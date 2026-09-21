//! Proxy-target URL construction + SSRF guard.
//!
//! PLAN2 S1.2. Lifted from `src/http/apps.rs`.
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

fn filtered_proxy_query(query: Option<&str>) -> Option<String> {
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

/// Returns `true` if the proxy target URL resolves to a private LAN,
/// link-local, or unspecified address.  Loopback (127.0.0.0/8, ::1) is
/// intentionally allowed because the primary use-case for proxy entries is
/// forwarding to local dev servers (e.g. Vite on 127.0.0.1:5173).
pub(super) fn is_private_proxy_target(url: &Url) -> bool {
    use std::net::IpAddr;

    let host = match url.host_str() {
        Some(h) => h,
        None => return true, // No host → reject
    };

    // Try to parse as IP directly first.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return is_ssrf_dangerous_ip(&ip);
    }

    false
}

/// Returns `true` for IPs that are SSRF-dangerous: private LAN ranges,
/// link-local (cloud metadata), and unspecified.  Loopback is allowed.
pub(super) fn is_ssrf_dangerous_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_private()        // 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
                || v4.is_link_local()  // 169.254.0.0/16 (cloud metadata)
                || v4.is_unspecified() // 0.0.0.0
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_unspecified() // ::
                || v6.to_ipv4_mapped().is_some_and(|v4| {
                    v4.is_private() || v4.is_link_local()
                })
        }
    }
}
