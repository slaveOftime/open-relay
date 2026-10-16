pub fn get_base_url(endpoint: &str) -> String {
    if let Ok(url) = reqwest::Url::parse(endpoint) {
        let mut origin = format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default());
        if let Some(port) = url.port() {
            origin.push(':');
            origin.push_str(&port.to_string());
        }
        return origin;
    }
    endpoint.to_string()
}

pub fn format_http_url(bind: &str, port: u16) -> String {
    if bind.contains(':') {
        format!("http://[{bind}]:{port}")
    } else {
        format!("http://{bind}:{port}")
    }
}

/// Human-readable byte size for status output (binary units, one decimal
/// below 10). Display only: protocol DTOs keep exact byte counts.
pub fn format_bytes_human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}
