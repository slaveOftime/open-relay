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
        assert_eq!(
            client
                .get(format!("{base}/custom.txt"))
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "operator override"
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
