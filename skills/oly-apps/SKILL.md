---
name: oly-apps
description: Use when publishing rich HTML output or a web app through the oly daemon, including static files, SPA routing, and proxied local services.
---

# Publish a web app through oly

## 1) Locate state and enable trusted apps

1. Run `oly daemon status` to get `ROOT` (`<STATE_DIR>`) and the live `HTTP` endpoint (HTTP must be enabled).
2. Apps live at `<ROOT>/wwwroot/apps/<app-name>/` and are served at `http://127.0.0.1:<PORT>/apps/<app-name>/` behind the daemon's HTTP authentication.
3. Custom apps, proxies, and `wwwroot` overrides are **disabled by default**. With operator authorization, set flat `"trusted_apps_enabled": true` in `<ROOT>/config.json` and confirm hot reload.
4. **Security boundary:** App routes share the control-plane origin and allow inline scripts. Only publish reviewed, trusted code — never untrusted output.

## 2) Choose an app mode

### Static HTML (no manifest needed)
Create `<ROOT>/wwwroot/apps/<app-name>/index.html` and relative assets (`./` or `/apps/<app-name>/`). Discovery uses the HTML `<title>`.

### Static app / SPA (`oly.app.json`)
Create `<ROOT>/wwwroot/apps/<app-name>/oly.app.json`:

```json
{
  "title": "My App",
  "app_type": "spa",
  "entry": "index.html",
  "redirect_files": ["index.html"]
}
```

- `app_type: "spa"` is a display hint only; use hash routing or `"redirect_files": ["index.html"]` for history-route fallback (prefer `/apps/<app-name>/` asset paths so deep links resolve assets cleanly).
- Every path in `redirect_files` must already exist on disk or the manifest is rejected.

### Loopback service proxy (`oly.app.json`)

```json
{
  "title": "Local dashboard",
  "app_type": "spa",
  "entry": "http://127.0.0.1:5173/"
}
```

- Proxies HTTP(S) and WebSocket upgrades to a service already running on the daemon host.
- **Loopback IP literals only** (`127.0.0.1` or `[::1]`), with no URL credentials and no `redirect_files`. Hostnames (including `localhost`), LAN/public IPs, and redirects are rejected.

## 3) Manifest schema (`oly.app.json`)

| Field | Type | Meaning |
|---|---|---|
| `entry` | required string | Local relative path (no `..` or leading `/`) or loopback HTTP(S) URL |
| `title` | optional string | Display title (falls back to HTML title or folder name) |
| `description` | optional string | Short description |
| `icon_href` | optional string | Icon URL or relative asset path |
| `app_type` | optional string | `spa` or `single_html` display hint |
| `redirect_files` | optional string[] | Existing files/dirs tried in order after app-local assets (local `entry` only) |

`redirect_files` may resolve outside the app folder after canonicalization — reference only intended static assets, never home directories, credentials, or private state.

## 4) Verify and report

1. Validate `oly.app.json` with `jq .` and confirm all `entry` / `redirect_files` paths exist.
2. Load `/apps/<app-name>/` with normal HTTP auth and verify assets, deep links (for SPAs), or WebSocket traffic (for proxies).
3. Report the app URL, files created, and verification results.
