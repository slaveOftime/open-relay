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
