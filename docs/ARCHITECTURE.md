# pcstui 架构说明

本文描述 `pcstui` v0.1.0 的分层、数据流、并发模型、状态文件与扩展点。目标是让后续维护者在不读完 6k 行源码的情况下，知道“某件事应该改哪里”。

## 1. 总览

```text
┌─────────────────────────────────────────────────────────────┐
│                         main.rs                             │
│  tracing 初始化 → clap 解析 → cli::run                       │
└──────────────────────────┬──────────────────────────────────┘
                           │
             ┌─────────────┴─────────────┐
             │                           │
      ┌──────▼──────┐             ┌──────▼──────┐
      │  cli.rs     │             │  tui/mod.rs │
      │ 无头命令     │             │ 终端事件循环 │
      └──────┬──────┘             └──────┬──────┘
             │                           │
             │                    ┌──────▼──────┐
             │                    │  tui/app.rs │
             │                    │ 状态/键位    │
             │                    └──────┬──────┘
             │                           │
             │                    ┌──────▼──────┐
             │                    │  tui/ui.rs  │
             │                    │ ratatui 渲染 │
             │                    └─────────────┘
             │
      ┌──────▼──────────────────────────────────────┐
      │              transfer/                      │
      │  TransferEngine · download · upload · types │
      └──────┬──────────────────────────────────────┘
             │
      ┌──────▼──────────────────────────────────────┐
      │                 api/                        │
      │  BaiduClient · sign · models                │
      └──────┬──────────────────────────────────────┘
             │
      ┌──────▼──────────────────────────────────────┐
      │               config.rs                     │
      │ 路径发现 · 可移植配置 · 导入导出 · 原子保存   │
      └─────────────────────────────────────────────┘
```

依赖方向严格单向：

```text
main → cli → tui → transfer → api → config
```

`api` 不依赖 `transfer` 或 `tui`；`transfer` 不依赖 `tui`。这样 CLI 与 TUI 共享同一套传输实现，协议层可以单独测试。

## 2. 模块职责

| 文件 | 职责 | 不应做什么 |
| --- | --- | --- |
| `src/main.rs` | 日志、`clap` 入口、错误退出码 | 业务逻辑 |
| `src/lib.rs` | 模块导出、版本常量 | 运行时状态 |
| `src/config.rs` | 路径发现、配置模型、导入/导出、权限、保存、凭据掩码 | 网络请求 |
| `src/api/client.rs` | HTTP 客户端、所有百度接口、错误映射 | 本地文件规划、TUI 状态 |
| `src/api/sign.rs` | `locatedownload` 签名、MD5 混淆解码、`sign2` | 网络请求 |
| `src/api/models.rs` | `RemoteItem`、`Quota`、排序、路径工具、格式化 | I/O |
| `src/transfer/types.rs` | 进度/状态/选项/事件类型、`ProgressReporter` | 具体传输逻辑 |
| `src/transfer/download.rs` | 下载规划、Range 续传、MD5、原子落盘 | UI 渲染 |
| `src/transfer/upload.rs` | 哈希、分片、秒传、断点状态、合并 | UI 渲染 |
| `src/transfer/mod.rs` | `TransferEngine`、任务 spawn、取消、事件通道 | 协议细节 |
| `src/tui/mod.rs` | 终端初始化/恢复、首次登录入口、事件循环、Yazi 挂起 | 业务状态机 |
| `src/tui/login.rs` | 首次登录/导入配置、掩码输入、在线验证 | 主界面状态 |
| `src/tui/config_panel.rs` | 配置/诊断快照、本地权限与可写检查 | 网络请求、绘制 |
| `src/tui/app.rs` | 面板状态、键位、对话框、任务派发 | 绘制细节 |
| `src/tui/ui.rs` | 布局、表格、进度条、浮层 | 网络/磁盘 I/O |
| `src/cli.rs` | 子命令、参数校验、无头输出 | TUI 状态 |

## 3. 启动流程

### 3.1 通用启动

```text
main()
  ├─ tracing_subscriber（默认 warn，可用 RUST_LOG 覆盖）
  ├─ Cli::parse()
  └─ cli::run(cli)
       ├─ RuntimeConfig::load_from(cli.config)
       │    ├─ Paths::discover()
       │    ├─ 读取 config/pcstui.json（若存在）
       │    ├─ 创建 upload-state 目录并 chmod 700
       │    └─ 必要时原子保存新配置（0600）
       └─ 分发到 TUI 或具体 CLI 命令
```

配置只从 `config/pcstui.json`（或 `--config` 指定文件）读取；没有账号时，TUI 会进入登录页，CLI 会提示运行 `pcstui login`。

### 3.2 TUI 启动

```text
tui::run(config)
  ├─ install_panic_hook()
  ├─ account 为空 → login::run(config)（掩码输入/导入 + 在线验证）
  └─ loop:
       ├─ BaiduClient::new(config)
       ├─ TransferEngine::new(client, config) → (engine, transfer_rx)
       ├─ App::new(...)
       ├─ App::start()
       │    ├─ load_remote()  → tokio::spawn(list_dir) → AppEvent::RemoteLoaded
       │    ├─ load_quota()   → AppEvent::Quota
       │    ├─ load_user()    → AppEvent::User
       │    └─ resume_queue() → 去重并重新 spawn 未完成任务
       ├─ init_terminal()
       ├─ event_loop(...)
       ├─ engine.cancel_all(); save_if_dirty(); restore_terminal()
       ├─ relogin_requested → login::run(当前配置) → 用新配置重建客户端/引擎
       ├─ restart_requested → 用导入后的配置重建客户端/引擎
       └─ 否则退出
```

TUI 内的导入/重新登录不会原地修改运行中的 `BaiduClient`。事件循环退出后，`run` 重新构造客户端与传输引擎；队列文件保留未完成任务，重建后自动恢复。

退出时：

```text
engine.cancel_all()
app.save_if_dirty()      # 保存 last_remote_dir 等
restore_terminal()
```

即使渲染或事件循环返回错误，也会先恢复终端；panic hook 也会尝试恢复 raw mode/alternate screen。

## 4. 配置层

### 4.1 路径发现

`Paths::discover()` 按以下顺序确定根目录：

1. `PCSTUI_HOME`；
2. 可执行文件位于 `<root>/target/{debug,release}/pcstui`；
3. 可执行文件位于 `<root>/bin/pcstui`；
4. 当前目录含 `Cargo.toml`；
5. 编译期 `CARGO_MANIFEST_DIR`；
6. 当前目录。

默认布局：

```text
<root>/config/pcstui.json
<root>/config/queue.json               # 0600，持久化待办队列
<root>/config/upload-state/
<root>/downloads/
<root>/tmp/
```

环境变量覆盖：

| 变量 | 作用 |
| --- | --- |
| `PCSTUI_HOME` | 项目根 |
| `PCSTUI_CONFIG` | 配置文件完整路径 |
| `PCSTUI_CONFIG_DIR` | 配置目录 |
| `PCSTUI_DOWNLOAD_DIR` | 默认下载目录 |
| `PCSTUI_TMP_DIR` | 临时目录 |

### 4.2 配置模型

```rust
ConfigFile {
    version: u32,
    account: Option<Account>,   // uid, name, bduss, stoken, baiduid, access_token
    app_id: u32,                // 默认 266719
    pan_ua: String,
    user_agent: String,
    download_dir: Option<PathBuf>,
    last_remote_dir: String,
    transfer: TransferConfig,
}
```

`TransferConfig` 字段：

- `download_connections`：单个大文件的多连接 Range 连接数，`1..=16`，默认 4；
- `upload_connections`：同时执行的上传任务数，`1..=8`，默认 2；
- `max_active_downloads`：同时执行的下载任务数，`1..=8`，默认 3；
- `retries`：下载请求/分片和上传分片的重试次数，`0..=10`，默认 3；
- `verify_md5`：下载完成是否校验；
- `upload_chunk_mib`：`0` 表示按文件大小自动选择 4/16/64 MiB。

加载时统一 `sanitized()`，避免配置中的极端值导致资源问题。

### 4.3 登录与配置导入

首次启动 TUI 且 `account` 为空时，`tui::login::run` 先于主界面运行：

- BDUSS/STOKEN 掩码输入，支持 `Event::Paste` 粘贴完整 Cookie；
- 第三个字段可填已有明文 `pcstui.json` 路径，直接读取账号；
- `Enter` 后构造临时 `RuntimeConfig` 调用 `whoami`，验证成功才替换配置；
- 替换前把旧配置复制为 `pcstui.json.bak-<时间戳>`（`0600`，`create_new` 防覆盖）；
- 失败停留在登录页并显示错误，`Esc` 退出。

CLI 登录：`login [--cookie ... | --bduss ... --stoken ...] [--uid] [--name]` 构造 `Account`，原子写入后调用 `whoami` 回填 uid/name。`uid` 可省略（下载签名接受 `0`，在线信息会覆盖它）。交互式 `pcstui login` 在 TTY 下隐藏输入。CLI 是次级入口，不扩展 TUI 的配置面板功能。

TUI 主界面的“配置与诊断”面板（`C`）是配置操作的主入口：

- `e` 导出含凭据的明文副本，`E` 导出脱敏模板；导出目标已存在时拒绝写入；
- `export_to` 拒绝把当前活动配置文件当作导出目标；
- `i` 读取明文配置 → 在线验证 → 二次确认 → 备份旧配置 → 原子替换 → 重启客户端与引擎；
- `r` 重新登录：取消当前传输后回到登录页，重建 `BaiduClient` / `TransferEngine`，未完成任务由 `config/queue.json` 恢复；
- `d` 刷新本地权限检查与在线诊断。

缓存安全边界：

- 加密/压缩配置（age、gpg、gzip、zip、7z 或非 UTF-8 二进制）直接拒绝，并提示先用外部工具解密；pcstui 不实现解密；
- `config import` / 登录导入使用同一套校验和备份逻辑。

`config export [--redacted] <FILE>` 生成可移植副本；`--redacted` 移除账号，供安全分享。导出使用 `create_new` 保证绝不覆盖已有文件，并保持 `0600` 权限。

未配置账号时，`BaiduClient::new` 返回提示启动 TUI 登录页或运行 `pcstui login`。

### 4.4 原子保存与权限

```text
写 .pcstui.json.tmp
  → fsync
  → chmod 0600
  → rename 到 pcstui.json
```

目录在创建后 `chmod 0700`；上传状态文件写入后 `chmod 0600`。非 Unix 平台跳过权限设置。
备份文件 `pcstui.json.bak-*` 在保存前生成，采用保留策略最多保留最近 3 份历史备份。

### 4.5 单实例排他锁（Instance Lock）

为防止多进程并发启动 TUI 或运行变更型 CLI（下载、上传、登录、配置导入/初始化）造成 `config/pcstui.json`、`config/queue.json` 与上传断点状态写冲突，pcstui 在 `config/pcstui.lock` 维护单实例排他锁：

- **内核级建议锁**：基于 `fd-lock`，在 Unix 平台使用内核 `flock`，在 Windows 平台使用 `LockFileEx`。进程异常崩溃或被终止时，操作系统内核会自动释放文件描述符锁，彻底杜绝孤儿死锁；
- **友好冲突提示**：持有锁时向 `pcstui.lock` 写入当前进程 PID、运行模式及启动时间；若锁已被占用，冲突进程会读取持有者信息并输出阻断提示；
- **读写解耦**：只读 CLI 指令（`whoami`、`quota`、`ls`、`search`、`doctor` 等）不尝试获取排他锁，可与正在运行的 TUI 实例安全并发执行。

## 5. API 层

### 5.1 两个 HTTP 客户端

`BaiduClient` 持有：

- `http`：带 `Cookie`、`Referer`、PCS `pan_ua`，用于所有登录态接口；大文件 Range 下载也优先使用它，因为百度对 PCS UA + Cookie 的闭合 Range 最稳定；
- `download_http`：不带 Cookie，使用配置中的 `user_agent`（浏览器 UA）；当 `http` 返回 401/403 时作为兜底，用于部分签名直链。

超时：

- 连接 15s；
- 控制与常规请求总超时 120s；
- 当前实现中，主客户端设置了 120s 总超时（覆盖完整响应体）；若在慢速网络下单分片或单流传输超过 120s 将会触发超时重试（慢网拆分细粒度读写空闲超时的优化已在待规划清单中）。

### 5.2 接口封装

| 方法 | 端点 | 说明 |
| --- | --- | --- |
| `whoami` | `/rest/2.0/xpan/nas?method=uinfo` | 账号信息 |
| `quota` | `/api/quota` | 容量 |
| `list_dir` | `/api/list` | 1000/页，按 `fs_id` 去重，最多 2000 页保护 |
| `stat` | 父目录 `list_dir` | 百度无稳定跨端 `stat`，用父目录查找 |
| `search` | `/rest/2.0/xpan/file?method=search` | 当前目录递归 |
| `mkdir` / `ensure_dir` | PCS `mkdir` | 已存在视为成功 |
| `remove` | PCS `delete` | 批量 `param` |
| `rename` | PCS `move` | 单条 from/to |
| `move_paths` / `copy_paths` | PCS `move` / `copy` | 目标目录 + 原文件名 |
| `download_link` | PCS `locatedownload` | 签名直链，加密链接回退 |
| `download_get` | 直链 GET | 无 Range 或开放 Range，作为小文件/兜底 |
| `download_get_range` | 直链 GET + `Range` | 闭合字节区间；优先 PCS UA + Cookie |
| `content_md5` | 直链 1 字节 Range | 读取下载主机权威 `Content-MD5`（ETag 兜底） |
| `precreate` | `/api/precreate` | 上传预创建/秒传 |
| `upload_part` | PCS `superfile2` | 流式 multipart 分片 |
| `create_file` | `/api/create` | 合并分片 |

### 5.3 错误模型

```rust
ApiError::Network(reqwest::Error)
ApiError::Url(url::ParseError)
ApiError::Remote { errno, message, request_id, context }
ApiError::Parse { context, message }
ApiError::Other(String)
```

`Remote` 保留：

- `errno`：百度业务错误码；
- `request_id`：排障用；
- `context`：调用语义（“获取目录列表”“上传分片”等）。

`is_not_found()` / `is_already_exists()` 将常见 errno 与中文/英文消息归一化，供 mkdir、stat 等使用。

### 5.4 签名

`sign.rs` 实现：

- `dev_uid(bduss)`：从 BDUSS 派生设备 ID；
- `locate_download_sign_with(uid, bduss, time)`：生成 `rand` 与 `devuid`；
- `decrypt_md5(raw)`：百度 MD5 混淆解码；
- `sign2(key, input)`：上传/其他接口的签名辅助；
- `base64_encode`。

签名算法来自 BaiduPCS-Go，`NOTICE` 中保留归属。固定向量测试确保重构不会静默破坏协议。

## 6. 传输层

### 6.1 任务模型

```text
TUI/CLI
  → TransferEngine::spawn_download / spawn_upload
      ├─ 生成 id
      ├─ 创建 CancellationToken
      ├─ 发送 TransferEvent::Added（状态 Queued）
      └─ tokio::spawn(async {
             acquire download_sem / upload_sem（可被取消）
             → 规划 → 执行 → finish_job
         })
            └─ ProgressFn → TransferEvent::Updated
```

`TransferEngine` 持有：

- `client: BaiduClient`（可克隆，内部 `reqwest::Client` 共享连接池）；
- `config: RuntimeConfig`（上传状态目录等）；
- `tx: UnboundedSender<TransferEvent>`；
- `cancels: Arc<Mutex<HashMap<u64, CancellationToken>>>`；
- `download_sem` / `upload_sem: Arc<Semaphore>`：分别限制同时执行的下载/上传任务；
- `next_id: AtomicU64`。

事件：

```rust
TransferEvent::Added(TransferProgress)
TransferEvent::Updated(TransferProgress)
TransferEvent::Finished(TransferProgress)
```

TUI 只消费事件，不直接等待网络任务。CLI 使用 `ProgressReporter` 直接打印。

### 6.2 并发模型

并发分两层：

1. **任务级**：`spawn_download` 与 `spawn_upload` 会立即创建任务并显示为“排队”，但在真正开始规划/传输前必须先取得对应 `Semaphore` 的 permit。下载并发由 `max_active_downloads`（默认 3）限制，上传并发由 `upload_connections`（默认 2）限制。等待 permit 时监听 `CancellationToken`，因此排队中的任务也能立即取消。
2. **单文件连接级**：大文件（≥64 MiB）下载在 `download_connections`（默认 4）范围内并行拉取 4 MiB Range 分片；普通文件仍为单流闭合 Range。

因此“传输面板”同时是任务列表和实际调度视图：`Queued` 表示等待 permit 或等待上传规划，`Running` 表示已持有 permit。下载与上传使用独立信号量，互不阻塞。队列没有单独的 FIFO 等待队列；Tokio 信号量的公平性保证先到先得。

CLI 的 `download` 直接调用 `download_plan`，不经过 `TransferEngine`，因此 CLI 单次调用天然只有一个任务；多文件下载在任务内部串行，单文件内部可按 `--connections` 并行。

### 6.3 进度

`ProgressReporter`：

- 最多每 200ms 发一次更新（约 5 Hz），避免 TUI 被高频事件淹没；
- 速度用指数滑动平均（新值权重 0.3）；
- 多文件任务用 `base_done` + 当前文件 `raw_done` 计算总进度；
- `finish()` 强制发送最终状态；
- 完成时把 `done` 对齐到 `total`，避免 99% 卡住。

### 6.4 下载流程

```text
plan_download(item, cwd, base)
  ├─ 文件：base/<name>
  └─ 目录：BFS 列目录
       ├─ 保留空目录
       ├─ sanitize 每个路径分量
       └─ 累加 total_bytes

download_plan(plan, options, cancel, reporter)
  └─ 对每个文件 download_one(...)

download_one
  ├─ resolve_target：存在时按 overwrite/rename/同大小跳过
  ├─ 目标目录创建
  ├─ .<name>.<fs_id>.pcstui.part（fs_id 缺失时用远程路径指纹）
  ├─ 若存在 .state.json：
  │    ├─ 有效且 .part 长度/指纹匹配 → 恢复多连接分片位图
  │    ├─ .part 缺失或截断 → 删除 .part 与状态，从头开始
  │    ├─ 指纹变化 → 重新校验已完成分片 MD5，坏分片标记为未完成
  │    └─ 全部完成且 .part 长度正确 → 直接 finalize
  ├─ 大文件（≥64 MiB）且 connections>1 且无既有断点：
  │    ├─ download_link(path)
  │    ├─ 1 字节 Range 预检
  │    ├─ 写 RangeState（4 MiB 分片位图 + 分片 MD5）
  │    └─ download_multi：最多 connections 个 worker
  │         ├─ 每个分片闭合 Range [start, end]
  │         ├─ 分片失败重新取链 + 指数退避重试
  │         ├─ 分片 flush + sync_data 后记录 MD5、位图和 .part 指纹
  │         └─ 返回 200/401/403 → Unsupported，回退单流
  ├─ 单流：闭合 Range 4 MiB 一段，从 .part 长度续传
  │    ├─ 206 → 追加
  │    ├─ 200 → 从头开始（服务端忽略 Range）
  │    └─ 416 且 offset==size → 直接 finalize
  ├─ 流式写入 + 取消检查
  ├─ 完整性检查（done == total）
  └─ finalize_download
       ├─ 本地 MD5 vs 目录 md5
       ├─ 不一致时读取服务端 Content-MD5 作为权威值
       ├─ 仍不一致时尝试“分片列表 JSON 的 MD5”兼容匹配
       ├─ 覆盖/重命名策略
       ├─ sync_all(.part)
       ├─ rename(.part → target)
       └─ 设置 mtime
```

关键设计：

- `.part` 与目标同目录，避免跨文件系统 rename；
- `.part` 名称包含远程身份（`fs_id`，缺失时用远程路径指纹），同名不同远程文件不会共用断点；
- 多连接状态文件为 `.<name>.<fs_id>.pcstui.part.state.json`，记录总大小、分片大小、完成位图、每个已完成分片的 MD5 和 `.part` 指纹；`.part` 缺失/截断时整段丢弃，指纹变化时重新哈希已完成分片，绝不拼接稀疏文件；
- 百度对单个闭合 Range 有约 5 MiB 上限，因此分片固定 4 MiB；大文件不能用开放 Range 或无 Range 请求（会返回 403 `31326 user is not authorized`）；
- 下载请求优先使用 PCS UA + Cookie；401/403 时回退浏览器 UA 的 `download_http`；
- 目录列表对分片上传合并出的文件返回的是 block-list JSON 的 MD5，不是内容 MD5；下载完成后用下载主机的 `Content-MD5` 做权威校验，避免误删正确文件；
- MD5 失败删除损坏断点，避免“永远续传一个坏文件”；
- 目标已存在且大小相同、MD5 一致时跳过，适合重复执行脚本；
- TUI 默认 `rename_existing=true`，避免覆盖；CLI 默认报错，显式 `--overwrite`/`--rename`。

### 6.5 上传流程

```text
upload_one
  ├─ 读取 size/mtime
  ├─ hash_file：单遍计算
  │    ├─ 整文件 MD5
  │    ├─ 前 256 KiB slice-MD5
  │    ├─ CRC32
  │    └─ 每个 block 的 MD5（空文件固定为 `[EMPTY_MD5]`）
  ├─ state_path：MD5(local_path, remote_path, size, mtime)
  ├─ load_state + matches 校验（包含本地整文件 MD5）
  ├─ 无可用 uploadid：
  │    ├─ ensure_remote_dir
  │    ├─ precreate
  │    ├─ return_type=2 → 秒传完成
  │    └─ 保存新 state（写入后 sync_all）
  ├─ 逐分片：
  │    ├─ 已完成 → 跳过
  │    ├─ superfile2 上传（multipart 字段 `uploadedfile`）
  │    ├─ 成功 → 比对本块本地 MD5；一致才记录 + 保存 state
  │    ├─ MD5 不一致 → 视为失败重试
  │    └─ 失败 → 指数退避重试（retries）
  ├─ create_file 合并
  ├─ verify_uploaded_content：读取下载主机 Content-MD5
  │    ├─ 与本地整文件 MD5 一致 → 成功
  │    ├─ 明确不一致 → 重试 3 次后失败
  │    └─ 头部缺失/接口不可用 → 容忍，不误报失败
  └─ 校验通过后删除 state（失败时保留，便于重新合并）
```

分块大小：

| 文件大小 | 默认块 |
| --- | --- |
| < 8 GiB | 4 MiB |
| 8–32 GiB | 16 MiB |
| ≥ 32 GiB | 64 MiB |

`--chunk-mib` 可覆盖为 1–64 MiB。状态文件记录 `block_size`/`block_count`，配置变化后旧状态不会误用。

### 6.6 取消

- 每个任务一个 `CancellationToken`；
- 等待下载/上传 permit 时用 `tokio::select!` 同时监听取消，排队任务可立即撤销；
- 多连接下载的每个 worker 在请求前、流式写入循环中检查取消；
- 单流下载在 `tokio::select!` 中监听 `cancel.cancelled()`；
- 上传在每个分片前后检查；
- `cancel(id)` 触发 token，任务返回错误并由 `finish_job` 标记 `Cancelled`；
- 退出 TUI 时 `cancel_all()`。

取消不是“立即杀死线程”，而是协作式停止；已写入的 `.part` 或已确认分片会保留，便于下次续传。

## 7. TUI 层

### 7.1 状态

`App` 主要字段：

- `local: LocalPane`：当前目录、条目、过滤、选择、隐藏文件；
- `remote: RemotePane`：当前目录、条目、模式（Browse/Search）、过滤、排序、加载/错误；
- `focus: Local | Remote | Transfers`；
- `transfers: Vec<TransferProgress>` + 选中行；
- `input: Option<InputState>`：过滤/搜索/mkdir/goto/rename/move/copy，含字符索引光标；
- `confirm: Option<ConfirmState>`：删除/退出；
- `marked_local` / `marked_remote`：多选；
- `queue: QueueStore` + `queue_ids`：持久化待办任务及“传输 id → 队列 id”映射；
- `quick_index` / `quick_at`：数字快速跳转缓冲与延迟判定；
- `completion_token`：丢弃过期的 Tab 补全响应；
- `generation`：防止过期异步结果覆盖新目录；
- `dirty_config`：退出时保存 `last_remote_dir`。

底部元数据行不缓存额外状态，而是由 `ui.rs` 根据当前焦点和选中项即时渲染（路径、大小、mtime/ctime、`fs_id`、MD5）。

### 7.2 事件循环

```rust
loop {
    terminal.draw(...)
    tokio::select! {
        event = events.next() => app.handle_key(...)
        Some(event) = rx.recv() => app.handle_event(event)
        Some(event) = transfer_rx.recv() => app.handle_transfer(event)
        _ = ticker.tick() => app.on_tick()
    }
    if yazi_requested { 恢复终端 → 启动 yazi → 重新初始化 }
    if should_quit { break }
}
```

- 输入事件：`crossterm::EventStream`；
- 应用事件：`AppEvent::{RemoteLoaded, SearchLoaded, Quota, User, TaskDone, Completions}`；
- 传输事件：`TransferEvent`；
- tick：120ms，用于刷新状态/进度重绘，并检查单位数快速跳转的 450ms 超时；
- Tab 补全通过独立 `tokio::spawn` 列父目录，结果带 `completion_token` 返回；输入已变化或已关闭时丢弃。

### 7.3 异步任务与代际

目录加载/搜索：

```text
generation += 1
spawn(async { client.list_dir/search; tx.send(AppEvent::... { generation }) })
```

处理事件时若 `generation != self.generation` 直接丢弃。这样快速连续 `cd` 时，旧目录的慢响应不会覆盖新目录。

文件操作（mkdir/rename/move/copy/delete）通过 `tokio::spawn` 执行，完成后发送 `TaskDone { message, error, refresh }`，由 TUI 决定刷新本地/远程。

### 7.4 布局与窄屏

`ui.rs` 根据终端宽度选择：

- 宽屏：左右双栏 + 底部传输面板；
- 窄屏（<100 列）：上下分栏；
- 传输面板可隐藏（`p`）；
- 帮助/输入/确认使用浮层，避免破坏主布局。

表格列宽按可用空间裁剪，名称列优先；时间列在窄屏隐藏。

### 7.5 Yazi 与外部程序

- `y` 设置 `request_yazi`；
- 事件循环检测后 `restore_terminal()`；
- 以本地当前目录为 cwd 启动 `yazi`；
- 返回后 `init_terminal()` 并刷新本地列表；
- `o` 用 `xdg-open` 打开本地文件。

Yazi 不存在或退出非零时，状态栏显示错误，不退出 TUI。

## 8. CLI 层

`clap` 子命令：

```text
tui
whoami [--json]
quota [--json]
ls [path] [--json] [--long]
search <keyword> [--dir] [--no-recursive] [--json]
download <remote> [--dest] [--overwrite] [--rename] [--no-verify] [--connections N] [--retries N]
upload <local...> [--dest] [--chunk-mib] [--quiet]
mkdir <path>
rm <path...> --yes
mv <source> <dest>
cp <source> <dest>
rename <from> <to>
doctor
login [--from-config FILE | --cookie COOKIE | --bduss --stoken --uid --name]
config <show|path|init|export|import|set-download-dir|set-remote-dir>
```

CLI 与 TUI 共用：

- `RuntimeConfig`；
- `BaiduClient`；
- `plan_download` / `download_plan`；
- `plan_upload` / `upload_one`；
- `ProgressReporter`。

差异主要在进度输出与退出码。CLI 的 `download --dest` / `upload --dest` 都按“目录”语义处理；`--connections`/`--retries` 覆盖本次下载的配置。CLI 不经过 `TransferEngine`，也不写 `config/queue.json`；它的断点仍由 `.pcstui.part` 和 `upload-state` 保证，重新执行同一命令即可续传。

CLI 定位为次级/脚本入口，不再扩展与 TUI 对等的功能；配置面板、诊断浮层和重新登录只在 TUI 提供。`login --from-config` 与 `config import` 现在都先 `adopt_file` 到内存、在线验证成功后才 `save_with_backup()` 替换，验证失败不会破坏当前配置。

## 9. 错误处理与可观测性

- 入口统一 `anyhow::Result`，`main` 打印 `{:#}` 链式错误并以 1 退出；
- API 层保留 `errno`/`request_id`，不把业务错误压扁成字符串；
- TUI 状态栏显示错误，危险操作需确认；
- `doctor` 检查：
  - 配置路径/权限；
  - 账号是否存在；
  - 网络/认证/配额/根目录；
  - 下载目录可写性。
- TUI 配置面板（`C`）提供同类本地检查（配置文件/目录权限、下载目录、上传断点目录、队列可读性）和在线账号/容量/根目录状态，无需离开终端。
- `RUST_LOG=debug` 可打开 `tracing` 日志（默认 `warn`，写 stderr）。

## 10. 安全模型

| 资产 | 措施 |
| --- | --- |
| BDUSS/STOKEN | 仅存本地配置，`0600`；日志/`doctor` 掩码 |
| 配置替换 | 导入先在线验证，再二次确认；替换前备份旧文件（`create_new`、`0600`）；导出拒绝覆盖活动配置 |
| 加密配置 | 不实现解密；age/gpg/压缩/二进制文件直接拒绝并提示用户先解密 |
| 上传状态 | 目录 `0700`，文件 `0600`；不含 Cookie |
| 下载临时文件 | 与目标同目录，完成后原子替换 |
| 远程路径 | `sanitize_component` 去除 `/`、`..` 等；本地拼接用 `safe_local_path` |
| 终端注入 | 文件名按普通文本渲染；控制字符显示为 `�`，避免 ANSI 转义破坏界面 |
| 外部程序 | 仅 `yazi`/`xdg-open`，通过 `Command` 参数传递路径 |

限制：

- Cookie 以明文 JSON 保存，未接入系统 keyring；
- 不防御本机同用户恶意进程（同用户可读 0600）；
- 不实现证书 pinning；
- 不支持硬件密钥/二次验证。

## 11. 测试策略

### 11.1 单元测试

- `api::sign`：签名向量、MD5 解码、`sign2`；
- `api::models`：路径规范化、父子目录、排序、`fs_id` 转换；
- `config`：导入/导出往返、脱敏、Cookie 解析、掩码；
- `transfer::upload`：块大小、空文件、整文件/分片哈希、上传计划、状态匹配；
- `transfer::queue`：`QueuedTask`/`QueueStore` 保存-读取-删除往返；
- `cli`：参数解析、Unicode 截断；
- `tui::app`：过滤、排序、输入路径、光标/Unicode 编辑、单词删除、Tab 补全目标解析；
- `tui::ui`：控制字符清理。

### 11.2 集成/冒烟

真实 API 冒烟需要有效 Cookie；建议在专用测试目录进行，避免污染正式目录。完整命令见 `docs/ACCEPTANCE.md` 第 3 节。

### 11.3 未来测试

- `wiremock`/`httpmock` 模拟百度接口，覆盖 errno、分页、Range、秒传、分片失败；
- 上传中断注入（在第 N 个分片后取消）；
- TUI 快照测试（`ratatui::TestBackend`）；
- 可移植配置在全新 `PCSTUI_HOME` 下的导入回归测试。

## 12. 扩展点

### 12.1 增加 API

在 `api/client.rs` 增加方法，返回强类型或 `Value`；在 `api/models.rs` 增加模型；错误统一走 `parse_response`。不要在 TUI/CLI 里直接拼 URL。

### 12.2 增加传输类型

实现 `spawn_*`，复用：

- `TransferProgress` / `TransferEvent`；
- `CancellationToken`；
- `ProgressReporter`；
- `finish_job`。

### 12.3 增加 TUI 操作

1. 在 `InputKind`/`ConfirmAction` 增加枚举；
2. 在 `start_*` 打开输入/确认；
3. 在 `submit_input`/`execute_confirm` 派发任务；
4. 用 `TaskDone` 刷新；
5. 在 `ui.rs` 增加提示/帮助文案。

### 12.4 调整并发策略

当前实现位于 `TransferEngine::new` 与两个 `spawn_*`：

- `download_sem` 的 permit 数 = `max_active_downloads`；
- `upload_sem` 的 permit 数 = `upload_connections`；
- 单文件 Range 并行度 = `DownloadOptions::connections`。

若要进一步做全局带宽限制或优先级队列，可在 permit 之外增加 `tokio::sync::Semaphore` 的加权许可，或在 `spawn_*` 前维护 `VecDeque<Job>`；事件模型与 TUI 不需要变化。注意保持“等待调度时可取消”这一性质。

### 12.5 持久化任务队列

`QueueStore` 已实现 `config/queue.json` 的持久化：

- `QueuedTaskKind::{Download, Upload}` 记录重建任务所需的源、目标与选项；
- `QueuedTask::identity()` 定义任务身份（下载：`fs_id + 远程路径 + 本地根目录`；上传：`本地路径 + 远程目录`），`QueueStore::add` 拒绝重复任务；
- `App::start()` 调用 `resume_queue()` 重新 `spawn_*`，启动时先清理历史重复项；
- `TransferEvent::Finished` 时按 `queue_ids` 移除对应任务并保存；
- 字节级断点仍由 `.pcstui.part` / `config/upload-state` 管理，队列不重复记录分片进度。

若要扩展队列（例如暂停/重排/失败重试），在 `QueuedTask` 增加状态字段并在 `resume_queue` 中解释即可；不要把大块进度写进队列文件，避免频繁原子写入。

## 13. 已知限制

- 多连接下载只对 ≥64 MiB 且 `connections>1` 的文件启用；小文件仍为单流闭合 Range；
- 单个上传文件内的分片仍串行；`upload_connections` 并发的是多个上传任务，目录上传作为一个任务内部按文件串行；
- 持久化队列没有暂停/恢复/重排/优先级 UI；重启后所有未完成任务立即重新排队；
- 上传断点续传会重新计算整文件哈希；超大文件可把整文件 MD5/slice-MD5/CRC32 持久化到状态文件以跳过；
- 多连接下载有独立位图状态；单流下载仍只依赖 `.part` 长度与远端 size；
- 百度目录列表对分片上传合并出的文件返回 block-list JSON 的 MD5，而不是内容 MD5；本项目已用下载主机 `Content-MD5` 绕过该问题，但无法修复网盘目录元数据本身；
- 无分享/离线/回收站/多账号；
- 加密直链不破解签名，回退 PCS；
- 仅保证 Linux TUI；
- `stat` 依赖父目录列表，超大目录下单次 stat 可能较慢；
- 删除/移动/复制/重命名前会按 `fs_id` 重新 stat 校验；项目已变化时整批中止；
- 导入/导出只处理明文 JSON；不实现凭据加密/解密，需要时由用户使用 age/gpg；
- 慢网下 API 请求仍共用 120s 总超时，连接/读取超时拆分尚未实现。

## 14. 相关文档

- [`ACCEPTANCE.md`](ACCEPTANCE.md)：协议验证、验收清单、可移植性、风险与恢复；
- [`FEATURE-MATRIX.md`](FEATURE-MATRIX.md)：TUI / CLI 功能矩阵与路线。
