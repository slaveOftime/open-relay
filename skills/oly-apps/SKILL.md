---
name: oly-apps
description: "Use when building a web app/api to present rich output or interactive experiences to users."
---

## Overview

Publish rich output as a web app served by the oly daemon. Each app lives in its own folder under `<OLY-ROOT>/wwwroot/apps/` and is reachable at:

```
http://127.0.0.1:<OLY-PORT>/apps/<app-name>/
```

Run `oly daemon status` to discover `<OLY-PORT>` and `<OLY-ROOT>`.

## Quick start

### Simple HTML app

Drop an `index.html` (plus any assets) into a new folder:

```
<OLY-ROOT>/wwwroot/apps/<app-name>/index.html
```

No manifest needed — oly discovers `index.html` automatically.

### SPA or proxied service

Create `oly.app.json` in the app folder:

```json
{
  "title": "My App",
  "entry": "index.html",
  "redirect_files": ["../shared-assets"]
}
```

Set the base href to `./` (or `/apps/<app-name>/`) so relative asset paths resolve correctly when served from the discovered route.

## Manifest schema (`oly.app.json`)

| Field | Type | Description |
|---|---|---|
| `title` | `string?` | Display name. |
| `description` | `string?` | Short description. |
| `icon_href` | `string?` | Icon URL or relative path. |
| `app_type` | `string?` | App type hint. |
| `redirect_files` | `string[]` | Extra asset directories outside the app folder. Oly checks the app folder first, then each redirect directory in order; 404 if not found. |
| `entry` | `string` | **Required.** URL, path, or HTML file to serve as the entry point. |

## Rules

- Use **relative asset paths** or the `/apps/<app-name>/` base — absolute paths will break.
- One folder per app, one `index.html` or one `oly.app.json` per folder.
- Use `redirect_files` to share static assets across apps without copying.

