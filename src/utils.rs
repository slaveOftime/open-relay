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
