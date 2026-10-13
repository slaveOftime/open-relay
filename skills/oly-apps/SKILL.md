---
name: oly-apps
description: Use when publishing rich HTML output or a web app through the oly daemon, including static files, SPA routing, and proxied local services.
---

# Publish a web app through oly

Run `oly daemon status` to discover `ROOT` (state directory) and the effective
HTTP endpoint. HTTP must be enabled. An app lives under
`<ROOT>/wwwroot/apps/<app-name>/` and is served at
`http://127.0.0.1:<PORT>/apps/<app-name>/` with the daemon's HTTP authentication.
Use the actual bind/port reported by status, not an assumed default.

## Static HTML

Create `<ROOT>/wwwroot/apps/<app-name>/index.html` and its assets. No manifest
is required; discovery uses the HTML title and supported metadata. Use a simple
URL-safe app name and verify both its route and its entry in the browser UI.

## Static app / SPA manifest

Create `oly.app.json` in the app folder:

```json
{
  "title": "My App",
  "app_type": "spa",
  "entry": "index.html",
  "redirect_files": ["../shared-assets"]
}
```

`../shared-assets` must already exist. Remove `redirect_files` if not needed;
a missing redirect path makes the manifest invalid.

Use relative assets with a suitable base (`./` or `/apps/<app-name>/`). For a
SPA, use hash routing or explicitly provide history fallback: `app_type: "spa"`
is a display hint, **not** automatic routing. A redirect **file** such as
`"redirect_files": ["index.html"]` can supply a fallback for unmatched paths;
it also catches missing assets, so test asset errors and deep links carefully.
A fixed `/apps/<app-name>/` asset base works better than `./` for deep links.

## Proxy a local service

```json
{
  "title": "Local dashboard",
  "app_type": "spa",
  "entry": "http://127.0.0.1:5173/"
}
```

The service must already be running on the **daemon's machine**. Proxy entries
support HTTP(S) and WebSocket upgrades. Do not combine them with `redirect_files`
(local entries only). Loopback targets are supported; private LAN/link-local
literal targets are rejected by the manifest guard. Do not treat this guard as
a complete security boundary: manifests are trusted operator configuration,
not something untrusted users should be allowed to publish.

## Manifest schema

| Field | Type | Meaning |
|---|---|---|
| `entry` | required string | Nonempty local relative entry path, or HTTP(S) proxy URL |
| `title` | optional string | Display title (otherwise HTML title/folder name) |
| `description` | optional string | Short description |
| `icon_href` | optional string | Icon URL or relative asset path |
| `app_type` | optional string | `spa` or `single_html` display hint |
| `redirect_files` | string array | Existing files/directories to try after app-local assets, in order |

Local entry paths must not be absolute or contain parent traversal. Redirect
paths may be relative to the app folder or absolute, and are canonicalized;
**they can expose files outside the app folder**. Only reference assets intended
for publication; never add a home directory, credentials, or private state.

## Verify and hand back

1. Validate manifest JSON and confirm every local/redirect path exists.
2. Open the app through oly with normal authentication; confirm content and assets.
3. For SPAs, test reload on a deep link; for proxies, test the intended interaction
   and WebSocket behavior, not just an HTTP success code.
4. Report the app URL, published files, and any untested behavior.

Oly serves/proxies apps; it does not install dependencies, launch a proxy target,
sandbox app code, or guarantee that arbitrary absolute asset URLs work. Coordinate
with the operator before exposing an app or starting/stopping its backing service.
