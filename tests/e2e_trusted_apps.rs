mod e2e;

use e2e::*;
use serde_json::json;
use std::time::{Duration, Instant};

#[test]
fn trusted_apps_require_opt_in_and_auth_and_can_be_disabled_live() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("trusted_apps");
    let apps = tmp.join("oly/wwwroot/apps/report");
    std::fs::create_dir_all(&apps).unwrap();
    std::fs::write(
        apps.join("index.html"),
        "<h1>trusted report</h1><script>document.title='report';</script>",
    )
    .unwrap();
    std::fs::write(tmp.join("oly/wwwroot/custom.txt"), "operator override").unwrap();
    std::fs::create_dir_all(tmp.join("oly/wwwroot/assets")).unwrap();
    std::fs::write(
        tmp.join("oly/wwwroot/assets/main-abc12345.js"),
        "console.log('hashed');",
    )
    .unwrap();
    std::fs::create_dir_all(apps.join("assets")).unwrap();
    std::fs::write(
        apps.join("assets/main-abc12345.js"),
        "console.log('hashed');",
    )
    .unwrap();
    let port = pick_free_port();
    let daemon = start_daemon_http_authenticated(&tmp, port, "test-app-password");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let base = format!("http://127.0.0.1:{port}");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if client
                .get(format!("{base}/api/health"))
                .send()
                .await
                .is_ok()
            {
                break;
            }
            assert!(Instant::now() < deadline, "HTTP listener did not start");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let response = client
            .post(format!("{base}/api/auth/login"))
            .header("content-type", "application/json")
            .body(json!({"password":"test-app-password"}).to_string())
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let token: serde_json::Value = serde_json::from_str(&response).unwrap();
        let token = token["token"].as_str().unwrap();
        assert_eq!(
            client
                .get(format!("{base}/api/static/apps"))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        let response = client
            .get(format!("{base}/api/static/apps"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let inventory: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(inventory, json!([]));
        for path in ["/apps/report/", "/custom.txt"] {
            assert_eq!(
                client
                    .get(format!("{base}{path}"))
                    .bearer_auth(token)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                404
            );
        }
        std::fs::write(
            tmp.join("oly/config.json"),
            r#"{"trusted_apps_enabled":true}"#,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let response = loop {
            let response = client
                .get(format!("{base}/apps/report/"))
                .bearer_auth(token)
                .send()
                .await
                .unwrap();
            if response.status() == 200 {
                break response;
            }
            assert!(
                Instant::now() < deadline,
                "apps enable was not hot-reloaded"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert!(
            response.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("script-src 'self' 'unsafe-inline'")
        );
        assert!(response.text().await.unwrap().contains("trusted report"));
        assert_eq!(
            client
                .get(format!("{base}/apps/report/"))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        let shell = client.get(format!("{base}/login")).send().await.unwrap();
        assert!(
            shell.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("script-src 'self';")
        );
        let embedded_available = !cfg!(debug_assertions)
            || std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("web/dist/index.html")
                .is_file();
        if embedded_available {
            assert_eq!(
                shell.status(),
                200,
                "a present bundle must serve the login shell"
            );
            let shell_etag = shell.headers()["etag"].to_str().unwrap().to_owned();
            assert!(shell_etag.starts_with("W/\""));
            assert_eq!(shell.headers()["cache-control"], "no-cache");
            for path in ["/login", "/sessions", "/sw.js", "/manifest.webmanifest"] {
                let response = client.get(format!("{base}{path}")).send().await.unwrap();
                assert_eq!(response.status(), 200, "{path}");
                assert_eq!(response.headers()["cache-control"], "no-cache");
                let etag = response.headers()["etag"].to_str().unwrap().to_owned();
                let response = client
                    .head(format!("{base}{path}"))
                    .header("if-none-match", etag.clone())
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), 304, "{path}");
                assert_eq!(response.headers()["etag"], etag);
                assert_eq!(response.headers()["cache-control"], "no-cache");
                assert!(
                    response.headers()["vary"]
                        .to_str()
                        .unwrap()
                        .contains("accept-encoding")
                );
                assert!(response.bytes().await.unwrap().is_empty());
            }
        } else {
            assert_eq!(
                shell.status(),
                404,
                "only missing debug web/dist may skip embedded checks"
            );
            eprintln!(
                "embedded HTTP checks skipped: build web/dist to exercise the debug frontend"
            );
        }
        let custom = client
            .get(format!("{base}/custom.txt"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(
            custom.headers()["cache-control"].to_str().unwrap(),
            "private, no-cache"
        );
        let custom_etag = custom.headers()["etag"].to_str().unwrap().to_string();
        assert!(
            custom_etag.starts_with("W/\""),
            "disk-served files use a weak validator, got {custom_etag}"
        );
        assert_eq!(custom.text().await.unwrap(), "operator override");

        // Filename shape never makes mutable operator files public/immutable.
        let hashed = client
            .get(format!("{base}/assets/main-abc12345.js"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(hashed.status(), 200);
        assert_eq!(
            hashed.headers()["cache-control"].to_str().unwrap(),
            "private, no-cache"
        );
        let hashed_etag = hashed.headers()["etag"].to_str().unwrap().to_string();
        assert_eq!(hashed.text().await.unwrap(), "console.log('hashed');");
        let app_asset = client
            .get(format!("{base}/apps/report/assets/main-abc12345.js"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(app_asset.status(), 200);
        assert_eq!(app_asset.headers()["cache-control"], "private, no-cache");
        assert_eq!(app_asset.text().await.unwrap(), "console.log('hashed');");
        for path in [
            "/custom.txt",
            "/assets/main-abc12345.js",
            "/apps/report/assets/main-abc12345.js",
        ] {
            let denied = client
                .get(format!("{base}{path}"))
                .header("if-none-match", "*")
                .send()
                .await
                .unwrap();
            assert_eq!(
                denied.status(),
                401,
                "auth must precede preconditions for {path}"
            );
            assert!(!denied.headers().contains_key("etag"));
        }

        // The validator turns revalidation into a bodyless 304, not a refetch.
        let custom_revalidated = client
            .get(format!("{base}/custom.txt"))
            .bearer_auth(token)
            .header("if-none-match", custom_etag.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(custom_revalidated.status(), 304);
        assert_eq!(
            custom_revalidated.headers()["cache-control"],
            "private, no-cache"
        );
        assert!(
            custom_revalidated.headers()["vary"]
                .to_str()
                .unwrap()
                .contains("accept-encoding")
        );
        assert!(custom_revalidated.bytes().await.unwrap().is_empty());
        let head = client
            .head(format!("{base}/custom.txt"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(head.status(), 200);
        assert_eq!(head.headers()["content-length"], "17");
        assert_eq!(head.headers()["etag"], custom_etag);
        assert!(head.bytes().await.unwrap().is_empty());
        let mut headers = reqwest::header::HeaderMap::new();
        headers.append("if-none-match", "\"not-this-file\"".parse().unwrap());
        headers.append("if-none-match", custom_etag.parse().unwrap());
        let head = client
            .head(format!("{base}/custom.txt"))
            .bearer_auth(token)
            .headers(headers)
            .send()
            .await
            .unwrap();
        assert_eq!(head.status(), 304);

        // Equal length + restored mtime must not let changed bytes reuse a tag.
        let custom_path = tmp.join("oly/wwwroot/custom.txt");
        let modified = std::fs::metadata(&custom_path).unwrap().modified().unwrap();
        std::fs::write(&custom_path, "operator updated!").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&custom_path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        let updated = client
            .get(format!("{base}/custom.txt"))
            .bearer_auth(token)
            .header("if-none-match", &custom_etag)
            .send()
            .await
            .unwrap();
        assert_eq!(updated.status(), 200);
        assert_ne!(updated.headers()["etag"], custom_etag);
        assert_eq!(updated.text().await.unwrap(), "operator updated!");
        let posted = client
            .post(format!("{base}/custom.txt"))
            .bearer_auth(token)
            .header("if-none-match", "*")
            .send()
            .await
            .unwrap();
        assert_eq!(posted.status(), 405);
        assert_eq!(posted.headers()["allow"], "GET, HEAD");

        let hashed_revalidated = client
            .get(format!("{base}/assets/main-abc12345.js"))
            .bearer_auth(token)
            .header("if-none-match", hashed_etag.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(hashed_revalidated.status(), 304);
        assert_eq!(
            hashed_revalidated.headers()["etag"].to_str().unwrap(),
            hashed_etag
        );
        assert!(hashed_revalidated.bytes().await.unwrap().is_empty());
        let app_revalidated = client
            .get(format!("{base}/apps/report/assets/main-abc12345.js"))
            .bearer_auth(token)
            .header("if-none-match", "*")
            .send()
            .await
            .unwrap();
        assert_eq!(app_revalidated.status(), 304);
        assert_eq!(
            app_revalidated.headers()["cache-control"],
            "private, no-cache"
        );

        // A benign local upstream verifies actual proxy delivery and credential isolation.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = listener.local_addr().unwrap().port();
        let proxy_dir = tmp.join("oly/wwwroot/apps/proxy");
        std::fs::create_dir_all(&proxy_dir).unwrap();
        std::fs::write(
            proxy_dir.join("oly.app.json"),
            json!({"entry":format!("http://127.0.0.1:{upstream_port}/")}).to_string(),
        )
        .unwrap();
        // Cross-site browser WS requests must fail before reaching the upstream.
        let denied = client
            .get(format!("{base}/apps/proxy/"))
            .bearer_auth(token)
            .header("origin", "http://untrusted.example.invalid")
            .header("connection", "upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), 403);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "rejected origin reached upstream"
        );
        let upstream = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0; 1024];
            while !request.windows(4).any(|v| v == b"\r\n\r\n") {
                let read = socket.read(&mut buf).await.unwrap();
                assert!(read > 0 && request.len() < 16 * 1024);
                request.extend_from_slice(&buf[..read]);
            }
            let request = String::from_utf8(request).unwrap().to_lowercase();
            assert!(
                !request.contains("authorization:")
                    && !request.contains("cookie:")
                    && !request.contains("token="),
                "credentials leaked: {request}"
            );
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nproxy okay",
                )
                .await
                .unwrap();
        });
        let proxied = client
            .get(format!("{base}/apps/proxy/?token={token}"))
            .bearer_auth(token)
            .header("cookie", format!("oly_auth_token={token}"))
            .send()
            .await
            .unwrap();
        assert_eq!(proxied.status(), 200);
        assert_eq!(proxied.text().await.unwrap(), "proxy okay");
        tokio::time::timeout(Duration::from_secs(5), upstream)
            .await
            .unwrap()
            .unwrap();

        std::fs::write(
            tmp.join("oly/config.json"),
            r#"{"trusted_apps_enabled":false}"#,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let response = client
                .get(format!("{base}/apps/report/"))
                .bearer_auth(token)
                .send()
                .await
                .unwrap();
            if response.status() == 404 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "apps disable was not hot-reloaded"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        for path in [
            "/assets/main-abc12345.js",
            "/apps/report/assets/main-abc12345.js",
        ] {
            let disabled = client
                .get(format!("{base}{path}"))
                .bearer_auth(token)
                .header("if-none-match", "*")
                .send()
                .await
                .unwrap();
            assert_eq!(
                disabled.status(),
                404,
                "disabled apps cannot return a cached 304"
            );
        }
        assert_eq!(
            client
                .get(format!("{base}/api/sessions"))
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    });
    drop(daemon);
    std::fs::remove_dir_all(tmp).unwrap();
}
