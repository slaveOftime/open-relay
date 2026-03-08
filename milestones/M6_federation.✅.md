# Milestone 6: Primary/Secondary Node Federation

> **Status:** ✅ Completed  
> **SPEC ref:** §16

## Goal

Allow users with multiple machines to expose only **one** `oly` daemon publicly (the *primary*) while running daemons on all other devices (*secondaries*). The primary proxies CLI and web-UI commands to the correct secondary via `--node <name>`, keeping the UX identical to local usage.

---

## Background

Motivated by: multi-device developers who run long-running agent sessions on several machines but don't want to open ports on each one. The primary acts as a single entry point; secondaries initiate outbound connections so they never need inbound firewall rules.

Design decisions:
- **Terminology:** primary / secondary (not master/slave).
- **Attach strategy:** PTY I/O is proxied through the primary (single hop, no direct client→secondary path).
- **Default scope:** `oly ls` with no `--node` shows local sessions only.
- **Key UX:** `oly api-key add <name>` on primary prints a key; secondary uses it in `oly join start`. A key is independent of node names — the same key can be reused for different nodes.
- **Duplicate name check:** primary rejects a join if the requested node name is already connected.
- **TLS:** deferred to M9 — assumes VPN/LAN or operator-managed TLS terminator in front.

---

## Tasks

### Database

- [x] Migration `0003_create_nodes.sql` — initial `nodes` table (superseded).
- [x] Migration `0004_migrate_to_api_keys.sql` — drop `nodes`, create `api_keys` table: `name`, `api_key_hash`, `created_at`.
- [x] `db.rs`: `add_api_key`, `list_api_key_hashes`, `delete_api_key`, `list_api_keys`.

### Protocol

- [x] `protocol.rs`: `NodeProxy { node, inner }` variant in `RpcRequest`.
- [x] `protocol.rs`: `ApiKeyAdd`, `ApiKeyList`, `ApiKeyRemove` variants in `RpcRequest`.
- [x] `protocol.rs`: `JoinStart`, `JoinStop` variants in `RpcRequest`.
- [x] `protocol.rs`: `NodeSummary`, `ApiKeySummary`, `NodeWsMessage` types.
- [x] `protocol.rs`: `ApiKeyAdd`, `ApiKeyList`, `ApiKeyRemove`, `JoinList` response variants.

### Node registry (primary side)

- [x] `src/node/registry.rs` — `NodeRegistry` + `NodeHandle` (mpsc channel + pending oneshot map).
- [x] Wire `NodeRegistry` into `http::AppState` and daemon `handle_client`.
- [x] Dispatch `NodeProxy` requests through `node_registry.proxy_rpc`.
- [x] `handle_client`: dispatch `ApiKeyAdd`, `ApiKeyList`, `ApiKeyRemove`.
- [x] `handle_client`: dispatch `JoinStart`, `JoinStop`.

### HTTP join endpoint (primary side)

- [x] `src/http/nodes.rs` — WebSocket handler at `GET /api/nodes/join` (WebSocket upgrade).
  - Handshake: read `join` message, validate key against **any** registered `api_keys` hash (key is independent of node name).
  - Reject with error if requested node name is already connected (duplicate check).
  - On success: send `joined`, register `NodeHandle` in `NodeRegistry`.
  - Relay loop: forward outgoing RPCs over WS and route incoming responses back to callers.
  - On disconnect: call `node_registry.disconnect(name)`.
- [x] `GET /api/nodes` — list currently connected secondary nodes (from `NodeRegistry`).
- [x] Register routes in `http/mod.rs`.

### Secondary connector

- [x] `src/client/join.rs` — `JoinConfig`, `run_join_connector` (outgoing WS + relay + backoff retry).
- [x] Joins.json persistence: `load_join_configs`, `save_join_config`, `remove_join_config`.
- [x] `run_join, run_join_stop` — CLI handlers (save/remove config + IPC JoinStart/JoinStop).
- [x] Daemon startup: load `joins.json` and spawn connector tasks.

### CLI

- [x] `cli.rs`: `oly api-key add <name>`, `oly api-key ls`, `oly api-key remove <name>`.
- [x] `cli.rs`: `oly join start --name <name> --key <key> <url>`.
- [x] `cli.rs`: `oly join stop --name <name>`.
- [x] `cli.rs`: `--node <name>` flag on `start`, `list`, `stop`, `attach`, `logs`, `input`.
- [x] `main.rs`: dispatch all new commands; `node_wrap()` helper wraps session RPCs in `NodeProxy`.

### Error handling

- [x] `error.rs`: `NodeNotConnected(String)`, `NodeNotFound(String)` variants.

---

## Verification

- [x] `oly api-key add mykey` on primary → prints a 64-hex-char API key.
- [x] `oly api-key ls` on primary → shows `mykey` with creation timestamp.
- [x] `oly join start --name worker1 --key <key> http://primary:15443` on secondary → connects.
- [x] `GET /api/nodes` on primary → returns `[{"name":"worker1","connected":true}]`.
- [x] A second secondary attempts `oly join start --name worker1 ...` → rejected (duplicate name).
- [x] `oly join start --name worker2 --key <same-key> http://primary:15443` → connects (key reuse).
- [x] `oly start --node worker1 -- sleep 60` → creates session on secondary.
- [x] `oly ls --node worker1` → shows the session; `oly ls` (no flag) does not show it.
- [x] `oly attach --node worker1 <id>` → interactive PTY proxied correctly.
- [x] `oly logs --node worker1 <id>` → returns logs from secondary.
- [x] `oly stop --node worker1 <id>` → stopped session on secondary.
- [x] Kill secondary daemon → `GET /api/nodes` shows empty list.
- [x] Restart secondary daemon → auto-reconnects (validates `joins.json` persistence).
- [x] `oly join stop --name worker1` on secondary → disconnects + removes config.
- [x] `oly api-key remove mykey` on primary → key gone; subsequent joins with that key are rejected.

Verified in automated integration tests:
- `tests/e2e_daemon.rs::e2e_federation_api_keys_and_join_handshake`
- `tests/e2e_daemon.rs::e2e_federation_primary_secondary_full_lifecycle`
