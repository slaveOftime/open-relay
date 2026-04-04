# 🔒 Open Relay (`oly`) — 安全审计报告

**项目:** [github.com/slaveOftime/open-relay](https://github.com/slaveOftime/open-relay)  
**版本:** 0.2.3  
**审计日期:** 2026-04-02  
**审计方法:** 自动化多角度源码审计（加密/认证、网络攻击面、命令注入/PTY、木马/后门扫描、Web前端安全）  

---

## 📋 执行摘要

Open Relay（`oly`）是一个 Rust 编写的终端会话管理器/复用器，提供持久 PTY 会话、Web UI、HTTP API 和多节点联邦功能。

### 木马/后门结论

> **✅ 未检测到任何木马、后门或恶意代码。**

经过对所有 Rust 源码、Web 前端（TypeScript/React）、npm 包脚本、CI/CD 流水线、SQL 迁移文件和嵌入式资源的全面扫描：

- **无混淆代码** — 无可疑的 base64 载荷、XOR 编码、十六进制字符串
- **无隐藏网络连接** — 所有 URL 均为 localhost 或 GitHub releases
- **无数据外泄** — 不访问 `~/.ssh`、`/etc/passwd`，不采集环境变量
- **无隐藏端点/命令** — 所有路由均有文档且受认证保护
- **无恶意 npm 脚本** — 零 `preinstall`/`postinstall` 钩子
- **无可疑 Web 依赖** — 均为主流包（react, radix, xterm, vite）
- **无 eval/Function 构造函数** — JS 源码和 dist 中零匹配
- **5 个 `unsafe` 块** — 全部为最小化 OS FFI（mimalloc 配置、PID 检查、`setsid`），均正确
- **SQL 迁移** — 仅纯 DDL，无触发器或存储过程

**这是一个合法的、代码质量良好的开源项目。** 以下发现均为安全加固建议，而非恶意行为指标。

### 发现汇总

| 严重程度 | 数量 |
|---------|------|
| 🔴 高 (High) | 3 |
| 🟠 中 (Medium) | 14 |
| 🟡 低 (Low) | 11 |
| ℹ️ 信息 (Info) | 6 |

---

## 🔴 高严重度发现

### H-1: Notification Hook 通过占位符替换导致命令注入

| 属性 | 值 |
|------|-----|
| **文件** | `src/notification/channel.rs:86–117` |
| **类别** | 命令注入 |

**描述：** `run_hook` 函数对 `{title}`、`{body}`、`{session_ids}` 等占位符进行简单的字符串 `.replace()` 替换。虽然 hook 字符串在替换前被分割为 shell-word token（所以 token 数量固定），且使用 `Command::new(program).args(&args)` 直接执行（无 shell），但如果管理员配置了如 `sh -c 'echo {title}'` 这样的 hook，攻击者可以通过 HTTP API 创建标题为 `"; rm -rf / #"` 的会话来实现任意命令执行。

**影响：** 当管理员配置了基于 shell 的通知 hook 时，任何可创建会话的用户都能以 daemon 用户身份执行远程代码。

**建议：** 
1. 禁止在引号内的 shell token 中使用占位符，仅通过 `OLY_EVENT_*` 环境变量暴露数据（已有且安全）
2. 或对占位符值进行 shell 转义
3. 或在配置加载时验证 hook 命令

---

### H-2: `CorsLayer::permissive()` 允许任意源跨域请求

| 属性 | 值 |
|------|-----|
| **文件** | `src/http/mod.rs:120` |
| **类别** | 跨站请求伪造 (CSRF) |

**描述：** 使用 `CorsLayer::permissive()` 设置 `Access-Control-Allow-Origin: *`，允许所有方法和头部。虽然服务器绑定 `127.0.0.1`，但如果通过端口转发（SSH 隧道、VS Code Remote、ngrok）暴露，用户访问的任何恶意网站都可以向 API 发送经过认证的跨域请求。

结合 cookie 认证（`SameSite=Lax`）和 IP 欺骗（H-9），攻击者可以：列出会话、发送输入、创建命令、终止会话 — 完全控制。

**影响：** 恶意网页可全面控制所有会话。

**建议：** 将 `CorsLayer::permissive()` 替换为明确的允许源列表（如 `http://127.0.0.1:{port}`、`http://localhost:{port}`）。由于前端与 API 同源服务，通常根本不需要 CORS。

---

### H-3: `/api/nodes/join` 端点绕过认证中间件

| 属性 | 值 |
|------|-----|
| **文件** | `src/http/mod.rs:116`, `src/http/nodes.rs:43–84` |
| **类别** | 认证绕过 / DoS |

**描述：** `/api/nodes/join` 路由被放置在 `protected_router` **之外**，绕过 `require_auth` 中间件。任何能访问 HTTP 端口的人都可以发起 WebSocket 连接。API 密钥验证在 WS 升级**之后**才在处理器内部进行（line 81），但此时已消耗服务器资源。

此外，联邦加入端点**没有速率限制**（登录端点有3次锁定，但加入端点没有）。每次尝试都触发昂贵的 Argon2 验证，且该验证**同步运行在 async 运行时线程上**（未使用 `spawn_blocking`），可导致运行时饥饿。

**影响：** 
- 预认证资源消耗（WebSocket 升级开销大）
- 可暴力破解 API 密钥，无锁定机制
- CPU 密集型 DoS（Argon2 阻塞 async 运行时）

**建议：**
1. 在 WS 升级前通过 HTTP 头部验证 API 密钥
2. 添加速率限制，复用登录端点的 `AuthState` 锁定机制
3. 将 Argon2 验证包装在 `tokio::task::spawn_blocking` 中

---

## 🟠 中严重度发现

### M-1: Session Token 永不过期

| 属性 | 值 |
|------|-----|
| **文件** | `src/http/auth.rs:28–33, 237–238` |
| **类别** | 会话管理 |

**描述：** Session token 存储在内存 `HashSet<String>` 中，无 TTL 或过期机制。一旦签发，token 在 daemon 重启前永久有效。token 集合大小无上限。

**影响：** 被盗 token 永久有效。

**建议：** 为每个 token 添加创建时间戳并强制执行可配置的最大存活时间（如24小时）。

---

### M-2: 认证 Token 在 URL 查询字符串中泄露

| 属性 | 值 |
|------|-----|
| **文件** | `src/http/auth.rs:388–393`, `web/src/api/client.ts:294, 425` |
| **类别** | 凭证泄露 |

**描述：** `extract_request_token_parts()` 接受 `?token=<value>` 查询参数。SSE（EventSource）和 WebSocket 因 API 限制必须使用此方式。Token 会泄露到浏览器历史记录、HTTP 访问日志、`Referer` 头部和代理日志中。

**影响：** Token 通过多种途径泄露。

**建议：** 仅对 WebSocket/SSE 升级请求允许查询字符串 token。考虑使用短期一次性 token。

---

### M-3: Auth Cookie 缺少 `Secure` 标志

| 属性 | 值 |
|------|-----|
| **文件** | `src/http/auth.rs:415` |
| **类别** | Cookie 安全 |

**描述：** `build_auth_cookie()` 设置 `HttpOnly; SameSite=Lax` 但省略了 `Secure` 标志。Cookie 将通过明文 HTTP 传输。

**影响：** 网络嗅探可获取认证 cookie。

**建议：** 当检测到 TLS 代理时添加 `Secure` 标志。文档说明非 localhost 部署需要 TLS。

---

### M-4: HTTP 服务器无 TLS 支持

| 属性 | 值 |
|------|-----|
| **文件** | `src/http/mod.rs:59, 124–138` |
| **类别** | 传输安全 |

**描述：** HTTP 服务器绑定纯 TCP，无任何 TLS 配置。所有数据（包括 auth token、会话输出中可能包含的密钥、文件上传）均以明文传输。

**建议：** 添加原生 TLS 支持或明确文档说明需要 TLS 反向代理。

---

### M-5: Argon2 PHC Hash 通过 CLI 参数传递（`ps` 中可见）

| 属性 | 值 |
|------|-----|
| **文件** | `src/daemon/lifecycle.rs:288–289`, `src/cli.rs:123–125` |
| **类别** | 凭证暴露 |

**描述：** 当 daemon 脱离前台时，父进程通过 `--auth-hash-internal <hash>` 将 Argon2 密码哈希传递给子进程。命令行参数对所有用户可见（`ps aux`, `/proc/<pid>/cmdline`）。

**影响：** 本地用户可读取 PHC 哈希，用于离线暴力破解。

**建议：** 通过环境变量（读取后清除）、临时文件（0600权限）或管道传递哈希。

---

### M-6: `X-Forwarded-For` / `X-Real-IP` 无条件信任

| 属性 | 值 |
|------|-----|
| **文件** | `src/http/auth.rs:156–169` |
| **类别** | 速率限制绕过 |

**描述：** `effective_ip()` 无条件信任 `X-Real-IP` 和 `X-Forwarded-For` 头部。攻击者可通过在每次请求中设置不同的 `X-Forwarded-For` 值来绕过 IP 锁定机制，获得无限次登录尝试。也可通过设置受害者 IP 来锁定他人。

**建议：** 仅在显式配置的可信代理列表中的对等 IP 才信任代理头部。

---

### M-7: IPC 无任何认证

| 属性 | 值 |
|------|-----|
| **文件** | `src/ipc.rs:35–52`, `src/daemon/rpc.rs:33–88` |
| **类别** | 本地提权 |

**描述：** Unix domain socket IPC 零认证。任何能连接到 socket 的本地进程都可以发出任何 RPC，包括 `DaemonStop`、`Start`（任意命令执行）、`ApiKeyAdd`、`Kill` 等。Session ID 仅有 ~28 位熵（UUID v4 截断为7位十六进制）。

**建议：** 在 Unix 上设置 socket 文件权限为 `0o600`。文档说明信任模型。

---

### M-8: IPC `read_line` 无大小限制 — 内存耗尽 DoS

| 属性 | 值 |
|------|-----|
| **文件** | `src/ipc.rs:79–80, 109, 147, 182` |
| **类别** | 拒绝服务 |

**描述：** 所有 IPC 读取使用 `reader.read_line(&mut line)` 且 `String` 无大小限制。恶意本地客户端可发送无限长行（无换行符），导致 daemon 分配无限内存直到 OOM。

**建议：** 使用带大小限制的读取器，如 `BufReader::new(reader.take(MAX_RPC_SIZE))`。

---

### M-9: Node WebSocket 消息 gzip 炸弹

| 属性 | 值 |
|------|-----|
| **文件** | `src/protocol.rs:102–125` |
| **类别** | 拒绝服务 |

**描述：** `decode_node_ws_payload` 使用 `GzDecoder::read_to_end(&mut json)` 解压 `ONW1` 格式消息，**无解压大小限制**。恶意次级节点可发送小型压缩载荷，解压为 GB 级数据。

**建议：** 使用带限制的读取器：`decoder.take(MAX_DECOMPRESSED_SIZE).read_to_end(&mut json)`。

---

### M-10: 反向代理请求体无限缓冲

| 属性 | 值 |
|------|-----|
| **文件** | `src/http/reverse_proxy.rs:66` |
| **类别** | 拒绝服务 |

**描述：** 代理多目标时使用 `to_bytes(body, usize::MAX)` 缓冲请求体。64 MiB 上传限制仅适用于 `/upload` 路由，不适用于代理路由。

**建议：** 设置合理限制：`to_bytes(body, 10 * 1024 * 1024)`。

---

### M-11: 反向代理 SSRF

| 属性 | 值 |
|------|-----|
| **文件** | `src/http/apps.rs:282–291`, `src/http/reverse_proxy.rs:30–50` |
| **类别** | SSRF |

**描述：** App manifest（`oly.app.json`）可定义指向任意 `http://` 或 `https://` URL 的代理 `entry`。如果攻击者能将 `oly.app.json` 写入 wwwroot 目录，可创建到任何内部服务的 SSRF 代理。

**建议：** 验证代理目标，阻止 RFC 1918 地址、链路本地、环回地址（除非明确允许）。

---

### M-12: 无 Content Security Policy (CSP)

| 属性 | 值 |
|------|-----|
| **文件** | `src/http/mod.rs`, `web/index.html` |
| **类别** | 纵深防御 |

**描述：** 所有 HTTP 响应均未设置 `Content-Security-Policy` 头部。如果发现 XSS 漏洞，没有纵深防御措施阻止脚本执行或数据外泄。

**建议：** 添加 CSP：`default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self' ws: wss:; img-src 'self' data:; font-src 'self'`

---

### M-13: `copy_dir_recursive` 跟随符号链接

| 属性 | 值 |
|------|-----|
| **文件** | `src/clipboard.rs:213–229` |
| **类别** | 信息泄露 |

**描述：** 从剪贴板粘贴目录时，`fs::copy` 会跟随符号链接。含有指向敏感文件（如 `/etc/shadow`）符号链接的恶意目录将被复制到会话数据目录。

**建议：** 检查 `symlink_metadata()` 并跳过或拒绝符号链接。

---

### M-14: 联邦 API 密钥验证未使用 `spawn_blocking`

| 属性 | 值 |
|------|-----|
| **文件** | `src/http/nodes.rs:73–83, 336–344` |
| **类别** | DoS / 运行时饥饿 |

**描述：** `verify_api_key()` 在 async Tokio 运行时线程上同步执行 Argon2id 验证。登录处理器正确使用了 `spawn_blocking`，但联邦加入端点没有。

**建议：** 将 Argon2 验证包装在 `tokio::task::spawn_blocking` 中。

---

## 🟡 低严重度发现

### L-1: `--no-auth` 禁用所有 HTTP 认证

| **文件** | `src/http/auth.rs:317–335` |
|------|------|

设计如此，有交互式确认警告。绑定 `127.0.0.1` 可缓解。文档已说明风险。

### L-2: VAPID 私钥在 `config.json` 中无限制权限

| **文件** | `src/config.rs:197–219` |
|------|------|

`config.json` 使用默认权限（通常 0644）写入，本地用户可读取 VAPID 签名密钥。应设置 0600。

### L-3: API 密钥明文存储在 `joins.json`

| **文件** | `src/client/join.rs:14–48` |
|------|------|

Unix 上 0600 权限正确，但 Windows 上未设置等效 ACL。密钥以明文形式持久化在磁盘上。

### L-4: 状态目录和文件缺少限制性权限

| **文件** | `src/storage.rs:92–104` |
|------|------|

锁文件、PID 文件和 SQLite 数据库创建时未设置明确权限限制。应设置目录 0700、敏感文件 0600。

### L-5: `unique_path_for_name` 中的 TOCTOU 竞争条件

| **文件** | `src/session/file.rs:85–109` |
|------|------|

`exists()` 检查和后续 `fs::write()` 之间存在竞争窗口。应使用 `OpenOptions::new().create_new(true)` 原子创建。

### L-6: innerHTML 用于 ANSI-to-HTML 转换

| **文件** | `web/src/utils/ansi.ts:264, 267, 286` |
|------|------|

`escHtml()` 正确转义 `&`、`<`、`>`，但未转义 `"` 和 `'`。当前用法安全，但建议添加引号转义作为纵深防御。

### L-7: 缺少 X-Frame-Options 和 X-Content-Type-Options 头部

| **文件** | `src/http/mod.rs` |
|------|------|

应添加 `X-Frame-Options: DENY`、`X-Content-Type-Options: nosniff`、`Referrer-Policy: strict-origin-when-cross-origin`。

### L-8: 内存中的锁定表不持久化

| **文件** | `src/http/auth.rs:28–34` |
|------|------|

速率限制锁定表存储在内存中。daemon 重启会清除所有锁定。

### L-9: SSE 广播所有会话（无每会话 ACL）

| **文件** | `src/http/sse.rs:208–260` |
|------|------|

所有经过认证的用户可看到所有会话。单用户工具可接受，但多用户场景需要会话级 ACL。

### L-10: `hex:` 键规格允许任意字节注入

| **文件** | `src/client/send.rs:305–321` |
|------|------|

设计如此。应文档警告集成方在传递给 `oly send` 前清理输入。

### L-11: CI Actions 使用标签引用而非 SHA 固定

| **文件** | `.github/workflows/` |
|------|------|

`actions/checkout@v4` 等使用标签引用。标签劫持风险（标准但非最佳实践）。

---

## ℹ️ 信息/正面发现

### I-1: ✅ 路径遍历防护正确实现

`src/session/file.rs:12–27` 和 `src/http/mod.rs:261–271` 正确拒绝 `..`、`/`、Windows 驱动器前缀。`rust-embed` 仅提供编译时嵌入的文件，提供额外安全边界。

### I-2: ✅ 无 Shell 注入风险

`src/session/runtime.rs:357–386` 使用 `which::which()` 解析二进制文件，然后 `CommandBuilder::new(cmd).args(&meta.args)` 直接调用 — 无 shell 参与。用户命令作为离散参数传递。

### I-3: ✅ SQL 注入已正确缓解

`src/db.rs` 所有查询使用 SQLx 参数化查询（`push_bind()`）。`ORDER BY` 使用硬编码枚举的 `&'static str`。

### I-4: ✅ API 密钥生成安全

`src/daemon/rpc.rs:519–541` — 32 字节 CSPRNG 随机数（256位），十六进制编码后用 Argon2id 哈希存储。明文仅显示一次。

### I-5: ✅ 密码比较使用常量时间

`src/http/auth.rs:223–233` 使用 `argon2::PasswordVerifier::verify_password()`，内部执行常量时间比较。无时序攻击。

### I-6: ✅ 生产构建禁用 Source Maps

`web/vite.config.ts:80` — `sourcemap: false` 防止泄露源码结构。

---

## 🏗 架构安全观察

### 信任模型

```
┌─────────────────────────────────────────────────────────┐
│                    信任边界图                              │
├─────────────────────────────────────────────────────────┤
│                                                         │
│  本地进程 ──IPC(无认证)──> daemon ──PTY──> 子进程         │
│                              │                          │
│  浏览器 ──HTTP(cookie/token)──┘                         │
│                              │                          │
│  次级节点 ──WS(API key)──────┘                          │
│                                                         │
│  关键假设:                                               │
│  1. IPC socket 仅同用户可访问（但未强制 0600）             │
│  2. HTTP 服务仅绑定 localhost（但可被端口转发）            │
│  3. 经过认证 = 完全控制（无角色/权限模型）                 │
│  4. Session ID 非安全边界（28位熵）                       │
│                                                         │
└─────────────────────────────────────────────────────────┘
```

### 依赖安全

| 类别 | 状态 |
|------|------|
| Rust crate 依赖 | ✅ 均为知名维护良好的 crate，无已知恶意或名称抢注 |
| npm 依赖 (web/) | ✅ 均为主流包（React, Radix, xterm.js, Vite） |
| npm 包 (npm/) | ✅ 零安装钩子脚本 |
| 构建脚本 | ⚠️ 在 release 构建时执行 `npm install` + `npm run build` — 标准做法但属供应链攻击面 |

### 组织名称说明

`slaveOftime` 是一个非常规的组织名称，但在所有产物（npm, winget, homebrew, Cargo.toml, GitHub）中保持一致。不存在冒充或误导行为。

---

## 📊 优先修复建议

### 立即修复（高优先级）

| # | 发现 | 修复工作量 | 影响 |
|---|------|-----------|------|
| H-2 | CORS 过于宽松 | 小 | 替换一行代码 |
| H-1 | 通知 hook 命令注入 | 中 | 添加 shell 转义或仅用环境变量 |
| H-3 | 联邦加入端点问题 | 中 | 添加速率限制 + spawn_blocking |

### 短期改进（中优先级）

| # | 发现 | 修复工作量 |
|---|------|-----------|
| M-6 | X-Forwarded-For 信任 | 小 — 添加配置选项 |
| M-5 | CLI 参数中的哈希 | 小 — 改用环境变量 |
| M-7 | IPC socket 权限 | 小 — 添加 chmod 调用 |
| M-8 | IPC read_line 限制 | 小 — 添加 take() 包装 |
| M-9 | gzip 炸弹 | 小 — 添加 take() 限制 |
| M-1 | Token 过期 | 中 — 添加 TTL 机制 |
| M-12 | CSP 头部 | 小 — 添加中间件 |

### 长期加固（低优先级）

- 添加原生 TLS 支持
- 考虑多用户场景的会话级 ACL
- Windows 上为敏感文件设置 ACL
- CI 中固定 Actions SHA
- 考虑命令白名单选项

---

## 🔍 审计方法论

本次审计通过以下 5 个并行分析角度进行：

1. **加密与认证审计** — 密码处理、API 密钥管理、加密原语使用、会话 token、联邦认证
2. **网络攻击面审计** — HTTP 路由、WebSocket、SSE、反向代理、IPC、CORS、TLS
3. **命令注入与 PTY 审计** — 命令执行、PTY 管理、输入注入、通知 hook、文件系统操作、SQL 注入
4. **木马/后门扫描** — 混淆代码、隐藏网络连接、数据外泄、隐藏功能、供应链、unsafe 代码
5. **Web 前端安全审计** — XSS、认证流程、WebSocket 安全、依赖、CSP

所有 Rust 源文件（`src/` 下全部 .rs）、Web 前端（`web/src/` 全部 .ts/.tsx）、配置文件、CI 流水线、npm 包脚本、SQL 迁移文件和测试文件均已被逐文件审查。

---

*报告结束*
