# pcstui 原生实现可行性报告

评估对象：`/home/mat/pansync/pcstui`（`pcstui` v0.1.0）
参照实现：BaiduPCS-Go v4.x（Apache-2.0）
验证时间：2026-10-27
结论：**核心文件管理与传输能力完全可行；全量功能对等不是 MVP 的合理目标。**

## 1. 结论先行

### 1.1 可行性判断

| 能力域 | 可行性 | 说明 |
| --- | --- | --- |
| Cookie 登录态复用 | 高 | `BDUSS` + `STOKEN` 足以调用网盘 Web/PCS 接口；无需模拟密码登录 |
| 目录浏览 / 搜索 / 配额 / 账号 | 高 | 已验证 `/api/list`、`xpan/file?method=search`、`/api/quota`、`xpan/nas?method=uinfo` |
| mkdir / rm / mv / cp / rename | 高 | 已验证 PCS `method=mkdir/delete/move/copy` |
| 下载（含 Range 续传、MD5、mtime） | 高 | `locatedownload` 签名 + PCS `download` 回退均已验证 |
| 上传（含秒传、分片、断点） | 高 | `precreate → superfile2 → create` 全链路已验证 |
| 双栏 TUI / 队列 / 取消 | 高 | `ratatui` + `tokio` 事件循环已实现 |
| 密码 / 验证码 / 短信登录 | 低（不建议） | 涉及验证码、风控、二次验证，维护成本高且易失效 |
| 分享 / 离线下载 / 回收站 | 中 | 协议存在，但接口更易变，适合作为后续独立模块 |
| 多账号切换 | 中 | 配置模型需从单账号扩展为账号列表 + 活跃账号 |
| 全量 BaiduPCS-Go 命令对等 | 中低 | 不是技术不可行，而是工作量与收益不匹配 |

### 1.2 实际落地结果

本项目不是“可行性纸面分析”，而是已经跑通的 MVP：

- 约 11k 行 Rust（不含依赖与文档），17 个源文件；
- `cargo check` 无警告，`cargo test` 34 项通过；
- 真实账号完成 `doctor`、`whoami`、`quota`、`ls`、`search`、上传、下载、断点续传、mkdir、cp、rename、rm 冒烟；
- 真实账号完成 70 MiB 多连接下载 + 服务端 `Content-MD5` 校验、10 MiB 上传、697 MiB/1.21 GiB 大文件 Range 探测；
- TUI 在 140×40 与 90×30 下完成渲染、序号跳转、元数据、输入光标、传输面板与队列恢复冒烟；
- 在全新 `PCSTUI_HOME` 中完成 TUI 登录、CLI 登录与可移植配置导入；
- 新配置 `config/pcstui.json` 为 `0600`，上传状态目录为 `0700`。

因此结论不是“能不能做”，而是“**哪些值得做、哪些应该明确不做**”。

## 2. 目标与边界

### 2.1 重写目标

1. **去掉进程与文本解析层**：不再启动外部 `pcs`，不再解析 PTY 输出。
2. **保留高效交互**：列表导航、过滤、搜索、Yazi、危险操作确认、序号跳转。
3. **补齐结构性能力**：`fs_id` 模型、传输队列、断点续传、MD5 校验、结构化错误。
4. **自包含与可移植**：源码/可执行文件不含凭据，一个配置文件即可换机。
5. **可测试**：协议、签名、路径、分块、配置导入导出均有单元测试；CLI 可做无头冒烟。

### 2.2 明确不做（MVP）

| 不做 | 原因 |
| --- | --- |
| 密码 / 验证码 / 短信登录 | 验证码识别、风控、二次验证维护成本高；Cookie 导入已覆盖个人使用 |
| 分享链接创建 / 转存 | 接口与风控策略变化频繁，且与“个人文件管理”主路径弱相关 |
| 离线下载 | 需要轮询任务、配额与资源策略，独立产品级工作量 |
| 回收站管理 | 删除已可用；恢复/彻底删除可作为下一阶段 |
| 多账号 UI | 单账号配置更简单；多账号应先做配置模型迁移 |
| 单文件分片并发上传 | 先保证状态文件与重试语义正确；`upload_connections` 已用于多任务并发 |
| 队列暂停/重排/优先级 | 当前只做持久化恢复与取消 |
| 加密下载链接的 pan-home 签名破解 | 脆弱且易失效；回退到 Cookie 认证的 PCS 下载端点 |
| 旧式 `rtype=3` 秒传 | 只使用 `precreate` 返回的秒传结果，避免维护旧协议 |

## 3. 协议可行性验证

### 3.1 认证

BaiduPCS-Go 的登录态核心是 Cookie：

```text
BDUSS=<...>; STOKEN=<...>
```

`pcstui` 直接将其放入请求头，不依赖 `access_token`。`uid` 用于 `locatedownload` 签名，可省略：签名接受 `0`，登录后由 `whoami` 自动回填。`STOKEN` 用于需要登录态的 PCS 接口。`baiduid` / `access_token` 为可选字段。

验证结果：

- `whoami` 返回账号名、`uk`、VIP 类型；
- `quota` 返回总容量、已用、剩余；
- 过期/错误 Cookie 返回结构化 `errno`，可被 `doctor` 识别。

### 3.2 目录与搜索

| 接口 | 用途 | 关键参数 | 验证 |
| --- | --- | --- | --- |
| `GET https://pan.baidu.com/api/list` | 列目录 | `dir, order=name, desc=0, num=1000, page=N` | 通过；按 1000 条分页，按 `fs_id` 去重 |
| `GET https://pan.baidu.com/rest/2.0/xpan/file?method=search` | 搜索 | `key, dir, recursion=0/1, web=1` | 通过；支持当前目录递归 |
| `GET https://pan.baidu.com/api/quota` | 配额 | `checkfree=1, checkexpire=1` | 通过；`free` 字段与 `total-used` 不一致，展示时以 `total-used` 为准 |
| `GET https://pan.baidu.com/rest/2.0/xpan/nas?method=uinfo` | 账号 | 无 | 通过 |

目录条目包含 `fs_id`、`server_filename`、`path`、`size`、`isdir`、`server_mtime`、`md5`、`block_list`。`pcstui` 将其转换为 `RemoteItem`，并在列表/搜索/操作中使用绝对路径 + `fs_id`，避免纯序号身份。

### 3.3 文件操作

| 操作 | 接口 | 形式 | 验证 |
| --- | --- | --- | --- |
| mkdir | `POST /rest/2.0/pcs/file?method=mkdir` | `path` | 通过；已存在视为成功 |
| delete | `POST /rest/2.0/pcs/file?method=delete` | `param={"list":[{"path":...}]}` | 通过 |
| move / rename | `POST /rest/2.0/pcs/file?method=move` | `param={"list":[{"from":...,"to":...}]}` | 通过 |
| copy | `POST /rest/2.0/pcs/file?method=copy` | 同上 | 通过 |

`app_id` 使用配置值（默认 `266719`）。这些 PCS 接口对 Cookie 登录态友好，返回 JSON `errno`。

### 3.4 下载

下载链路有两条：

1. **`locatedownload` 签名直链**
   - `POST https://pcs.baidu.com/rest/2.0/pcs/file?method=locatedownload`
   - 参数含 `app_id=250528`、`path`、`time`、`rand`、`devuid`、`cuid` 等；
   - `rand` / `devuid` 由 BDUSS + uid + 时间生成，算法已按 BaiduPCS-Go 移植并用固定向量验证；
   - 返回 `urls[]`，优先选择 `encrypt=0` 的 URL。

2. **Cookie 认证的 PCS 回退**
   - `GET https://pcs.baidu.com/rest/2.0/pcs/file?method=download&path=...`
   - 当直链全部为 `encrypt=1` 时使用；
   - 支持 `Range`，因此断点续传仍然可用。

关键限制（实测）：

- 大文件不能用开放 Range（`bytes=offset-`）或无 Range 请求，下载主机会返回 HTTP 403 `31326 user is not authorized`；
- 闭合 Range 的单个请求也不能过大，约 5 MiB 以上会被拒绝；因此单流和多连接都使用 4 MiB 闭合分片；
- 下载请求优先使用 PCS UA + Cookie；浏览器 UA 的无 Cookie 客户端只作为 401/403 兜底；
- 目录列表对“分片上传合并”的文件返回 block-list JSON 的 MD5，不是文件内容 MD5；下载主机响应头 `Content-MD5` 才是权威内容 MD5，`ETag` 通常与其一致。

验证结果：

- 1 MiB 文件完整下载；70 MiB 文件 4 连接下载完成，MD5 与源一致；
- 预置 300 KiB `.part` 后从 300 KiB 继续，最终 MD5 与源一致；
- 服务端不支持 Range 时自动从头开始；
- 70 MiB 文件下载完成后，目录 md5 与内容 md5 不一致，但服务端 `Content-MD5` 校验通过；
- 697 MiB 与 1.21 GiB 文件用 1 字节闭合 Range 探测均返回 `206` 和 `Content-MD5`；
- 完成后按远端 mtime 设置本地文件时间。

### 3.5 上传

上传是全链路中最复杂的部分，已验证：

```text
POST /api/precreate
  path, size, isdir=0, block_list, autoinit=1,
  content-md5, slice-md5, contentCrc32, rtype=2
        │
        ├─ return_type=2 → 秒传完成
        │
        └─ 返回 uploadid
                │
POST /rest/2.0/pcs/superfile2?method=upload&type=tmpfile
  app_id=266719, path, partseq, partoffset, uploadid, vip=1
  multipart file=<该分片>
                │
POST /api/create
  uploadid, path, size, isdir=0, rtype=2,
  block_list, target_path
```

关键结论：

- `superfile2` 必须使用上传专用 `app_id=266719`，不能复用下载的 `250528`；
- `block_list` 是每个分片的 MD5 数组，不是整文件 MD5；
- `slice-md5` 是前 256 KiB 的 MD5；
- `contentCrc32` 为十进制无符号整数；
- 秒传只在 `precreate` 的 `return_type=2` 时直接成功；
- 分片上传响应返回的 `md5` 必须与本地计算值一致，否则应视为失败并重试；当前实现会比对并在不一致时重传该分片。
- 分片 multipart 字段名按上游使用 `uploadedfile`；`path` 与 `partseq` 必须对应同一个 `uploadid`。
- `/api/create` 合并成功后，目录列表里的 `md5` 是 block-list JSON 的 MD5（实测 10 MiB 文件为 `82c8ff6f...`），不是文件内容 MD5；不能用它判断上传是否成功。
- 尝试修复目录元数据的路径均不可行：PCS `rapidupload` 返回 `31023 param error`，xpan `create` 返回 `9019 need verify`，旧 `createsuperfile` 流程返回 `block miss`/`file already exists`。
- 因此上传完成后通过下载主机的 1 字节 Range 读取 `Content-MD5`，与本地整文件 MD5 比对；头部缺失时容忍，明确不一致时重试后失败。10 MiB 上传实测通过。
- 上传断点状态记录 `uploadid` 与每个分片的服务端 MD5；重跑同一文件会跳过已完成分片。

### 3.6 MD5 混淆解码

列表接口返回的 `md5` 不是标准 32 位十六进制，而是百度自定义混淆串。已实现并测试两组向量：

```text
05bf4f422q09b2afb5d21a4deda0ab62 → a3a27f40049c0a25640b668db4f15f2a
5a556e453k40d18c0795019694d2afa8 → bdeb1c635b762b221d79624706b644f1
```

解码失败时保留原串；下载校验只在 `is_md5()` 为真时启用，避免把不可校验值误判为损坏。

## 4. 与 BaiduPCS-Go 的架构映射

| BaiduPCS-Go 层 | 职责 | pcstui 对应 |
| --- | --- | --- |
| `baidupcs` / `requester` | HTTP、Cookie、重试、限速 | `src/api/client.rs`（`reqwest`） |
| `pcsutil` / 签名工具 | `locatedownload` 签名、MD5 解码 | `src/api/sign.rs` |
| `internal/pcscommand` | 命令实现（ls/download/upload/...） | `src/cli.rs` + `src/api/client.rs` |
| `internal/pcsfunctions/pcsdownload` | 多连接下载、Range、状态 | `src/transfer/download.rs`（4 MiB 多连接 + 位图状态） |
| `internal/pcsfunctions/pcsupload` | 分片、秒传、上传数据库 | `src/transfer/upload.rs` + `config/upload-state/*.json` |
| `internal/pcsconfig` | 配置、账号、工作目录 | `src/config.rs` |
| `main.go` + urfave/cli | CLI 命令树 | `src/cli.rs`（`clap`） |
| 无 | 双栏 TUI | `src/tui/`（`ratatui` + `crossterm`） |

### 4.1 为什么不是逐行移植

BaiduPCS-Go 的下载器/上传器为多连接、限速、断点数据库做了大量工程；直接逐行移植会引入：

- 与 Go 并发模型强耦合的状态机；
- 历史兼容负担（旧配置、旧秒传、旧分享接口）；
- 难以裁剪的依赖与平台代码。

`pcstui` 选择“协议对等、交互重做、传输先正确后并发”的路线：

- 协议层按 BaiduPCS-Go 的知识实现并验证；
- 传输层先保证断点、重试、校验正确，再接入多连接下载与任务级并发；
- 当前已实现 4 MiB 多连接 Range、下载/上传独立信号量、持久化待办队列；
- 单文件分片并发上传仍保持串行，避免状态文件竞态；后续可在不改变队列格式的前提下增加 worker。

## 5. 工作量与成本

### 5.1 MVP 实际构成

| 模块 | 规模（约） | 内容 |
| --- | --- | --- |
| `config.rs` | 620 行 | 路径发现、可移植配置、导入/导出、权限、原子保存 |
| `api/` | 1,390 行 | 客户端、签名、模型、错误 |
| `transfer/` | 2,570 行 | 下载、上传、持久化队列、并发、进度 |
| `tui/` | 3,300 行 | 登录页、状态、键位、渲染、事件循环 |
| `cli.rs` | 990 行 | 16 个子命令 |
| 测试/文档 | — | 34 个单测 + 3 份文档 |

### 5.2 粗略人日估算

| 阶段 | 单人估算 | 说明 |
| --- | --- | --- |
| 协议验证 + 签名/上传打通 | 3–5 天 | 最容易被低估；需要真实账号与反复抓包 |
| 核心 API + 配置隔离 | 2–3 天 | 列表、搜索、配额、文件操作、错误模型 |
| 下载（Range/MD5/目录） | 2–3 天 | 断点、重命名、mtime、取消 |
| 上传（分片/秒传/状态） | 4–6 天 | 哈希、状态机、重试、合并 |
| TUI | 4–6 天 | 双栏、对话框、队列、Yazi、窄屏 |
| CLI + 测试 + 文档 | 3–4 天 | 冒烟与边界 |
| **MVP 合计** | **18–27 人日** | 熟悉百度协议与 Rust 异步的开发者 |
| 阶段 2：多连接/并发/队列/TUI 增强 | 4–7 天 | Range 分片位图、信号量、持久化队列、序号/补全/元数据/光标编辑 |

若要接近 BaiduPCS-Go 的功能覆盖（分享、离线、回收站、多账号、单文件分片并发上传、限速、跨平台打包），建议再预留 **3–6 周**，且应分阶段交付，而不是一次性重写。

## 6. 风险与缓解

| 风险 | 影响 | 缓解 |
| --- | --- | --- |
| 百度接口/字段变更 | 列表、上传、下载失败 | 结构化错误 + `doctor`；协议集中在 `api/`；保留 PCS 回退 |
| 反盗链/风控 | 直链 403、上传被限 | 双客户端（带/不带 Cookie）、Referer、UA、失败重试；避免高频请求 |
| Cookie 过期 | 所有操作 401/errno | `doctor` 明确提示重新导入；错误信息含 `errno`/`request_id` |
| `locatedownload` 签名失效 | 无法获取直链 | 回退到 Cookie 认证的 `method=download` |
| 加密直链 | 直链不可用 | 不破解 pan-home 签名，直接走 PCS 回退 |
| 大文件哈希耗时 | 上传前等待 | 单遍计算所有校验；`spawn_blocking` 避免阻塞 TUI；进度显示“计算校验”；发布版必须使用 `--release` |
| 上传分片失败 | 大文件重传 | 分片级重试 + 持久化 `uploadid`/已完成分片 |
| Range 不被支持/闭合区间过大 | 断点或多连接失效 | 检测 200/401/403 后回退单流；统一使用 4 MiB 闭合 Range |
| 并发过高触发限流 | 失败率上升 | 下载/上传独立信号量，默认 3/2；多连接默认 4 |
| 上传后目录 MD5 不是内容 MD5 | 误判上传失败/下载损坏 | 以下载主机 `Content-MD5` 为权威；目录 MD5 只作参考 |
| 多连接状态与 `.part` 不一致 | 拼接出稀疏/损坏文件 | 状态校验总大小/分片数；不匹配时删除 `.part` 从头开始 |
| 本地路径穿越/非法字符 | 写入意外位置 | `sanitize_component` + `safe_local_path`，拒绝 `..` 逃逸 |
| 配置泄露 | 账号被盗 | 配置 `0600`、目录 `0700`、日志脱敏、`doctor` 只显示掩码 |
| Windows 路径/终端差异 | 无法运行 | 当前只保证 Linux；路径层已用 `PathBuf`，后续补 Windows 测试 |

## 7. 验收清单

### 7.1 已完成的自动验证

```bash
cd /home/mat/pansync/pcstui
cargo fmt --check
cargo test          # 34 passed
cargo build --release
```

单元测试覆盖：

- `locatedownload` 签名固定向量；
- MD5 混淆解码两组向量；
- `sign2`；
- 远程路径规范化、父子目录、`fs_id` 模型；
- 本地路径安全与重名策略；
- 配置导入/导出往返、脱敏、Cookie 解析、密钥掩码；
- 分块大小、空文件、整文件/分片哈希；
- 上传计划、持久化队列往返；
- 输入光标/Unicode 编辑、单词删除、Tab 补全目标解析；
- CLI 参数、Unicode 截断、排序/过滤、控制字符清理。

### 7.2 已完成的真实 API 冒烟

测试使用专用目录 `/apps/pcsTui/upload/`，不触碰正式数据。

```bash
pcstui doctor
pcstui whoami
pcstui quota
pcstui ls /
pcstui upload /tmp/opencode/pcstui-10m.bin --dest /apps/pcsTui/upload
pcstui download /apps/pcsTui/upload/pcstui-70m.bin \
  --dest /tmp/opencode/pcstui-70m-dl2 --connections 4 --retries 2
pcstui mkdir /apps/pcsTui/upload/_smoke_dir
pcstui cp /apps/pcsTui/upload/pcstui-10m.bin /apps/pcsTui/upload/_smoke_dir
pcstui rename /apps/pcsTui/upload/_smoke_dir/pcstui-10m.bin \
  /apps/pcsTui/upload/_smoke_dir/pcstui-renamed.bin
pcstui rm /apps/pcsTui/upload/_smoke_dir --yes
```

多连接下载验证：

1. 70 MiB 文件，`--connections 4`，18 个 4 MiB 分片；
2. 分片位图持续保存，进度单调；
3. 完成后目录 md5 为 block-list MD5（`7512c7ba1tf...`），与服务端 `Content-MD5`（`603303a04f...`）不同；
4. 下载器读取服务端 `Content-MD5` 校验通过，本地文件 MD5 与源一致。

上传验证：

1. 10 MiB 文件（3 × 4 MiB 分片）上传成功；
2. 目录 md5 为 `82c8ff6f...`（block-list JSON 的 MD5），不是内容 MD5 `cc502f54...`；
3. 上传器通过下载主机 `Content-MD5` 确认内容一致后报告成功。

大文件 Range 探测：

- 697 MiB 视频与 1.21 GiB 压缩包，1 字节闭合 Range 均返回 `206`；
- 响应含 `Content-MD5`、`Content-Range`、`x-bs-meta-crc32`；
- 开放 Range 或无 Range 请求会被拒绝（403 `31326`），因此实现只使用闭合区间。

断点与性能（历史验证）：

- 预置 300 KiB `.part` 后从 300 KiB 继续，最终 MD5 与源一致；
- 128 MiB 上传中断后，`config/upload-state/*.json` 保留 `uploadid` 与已完成分片，重跑后跳过；
- release 构建 64 MiB 上传约 15 秒；debug 构建哈希计算明显更慢。

首次使用与配置生成：

- 在空的 `PCSTUI_HOME` 下运行 `pcstui config init`，生成 `config/pcstui.json`（0600）与 `config/upload-state/`（0700）；
- 同一空目录下运行 `pcstui login --bduss ... --stoken ...`（不传 `--uid`），自动回填 uid 并在线验证；
- `pcstui whoami`、`pcstui doctor` 随后均正常；
- 未配置账号时运行 `whoami` 会给出 TUI 登录页、`pcstui login` 与 `--from-config` 的提示；
- 无配置启动 TUI 会进入登录页，而不是直接报错退出。

TUI 冒烟：

- 140×40 与 90×30 下渲染正常，双栏/上下分栏切换、帮助浮层、传输面板、确认框、退出均可用；
- 序号跳转、底部元数据、输入光标、队列恢复在 tmux 中人工验证。

### 7.3 仍需人工确认

- Yazi 在 TUI 内的挂起/恢复（`yazi` 已安装，未做自动化交互测试）；
- 长时间大文件传输的内存与进度稳定性；
- 极端文件名（控制字符、超长中文名）在真实终端中的显示；
- 上传服务端返回 MD5 不一致时的重试路径（代码已实现，尚无故障注入测试）；
- 多连接下载在弱网/限流账号下频繁 403 时的回退体验；
- TUI 被强制终止后，持久化队列与 `.part`/上传状态的组合恢复。

### 7.4 可移植性验收

```bash
# 源目录
pcstui config export /tmp/pcstui-portable.json

# 全新的 PCSTUI_HOME
PCSTUI_HOME=/tmp/pcstui-fresh pcstui login --from-config /tmp/pcstui-portable.json
PCSTUI_HOME=/tmp/pcstui-fresh pcstui whoami
PCSTUI_HOME=/tmp/pcstui-fresh pcstui doctor
```

要求：

- 新目录下无需手工编辑即可完成账号验证；
- 导入后的 `config/pcstui.json` 为 `0600`，配置目录为 `0700`；
- `--redacted` 导出的文件不含 `account`，导入时应被拒绝；
- 源码和 release 可执行文件中检索不到 BDUSS/STOKEN 明文。

## 8. 换机与恢复策略

### 8.1 推荐路径

1. **首次使用**：启动 TUI 进入登录页，粘贴浏览器 Cookie；或执行 `pcstui login`。
2. **换机**：在原机器执行 `pcstui config export <FILE>`，把文件带到新机器，执行 `pcstui login --from-config <FILE>`。
3. **共享模板**：执行 `pcstui config export --redacted <FILE>`，接收方手工填写账号。
4. **恢复**：导入的配置同时恢复传输参数、下载目录与最近远程目录；断点队列与上传状态不随配置迁移，需按需复制 `config/queue.json` 与 `config/upload-state/`。

### 8.2 回滚

- 删除 `pcstui/config/pcstui.json` 即可回到“未配置”状态，下次启动会进入登录页；
- 上传断点状态可单独删除 `pcstui/config/upload-state/`；
- 下载临时文件为 `.<name>.pcstui.part`，可直接删除；
- 旧的可移植配置副本可直接用于恢复账号。

## 9. 最终建议

**Go / No-Go：Go，但限定范围。**

- 以原生 Rust 直接调用百度 API 是可行的，且已经在真实账号上验证了最关键的下载/上传链路。
- 不应把“重写 BaiduPCS-Go 全部功能”作为第一目标；那会把一个可维护的 MVP 变成长期追赶接口的项目。
- 最合理的产品形态是：**pcstui 聚焦“原生、可恢复、可测试、可移植的文件管理与传输”**，分享/离线/回收站等按需增补。
- 下一阶段优先级：
  1. 破坏性操作的 `fs_id`/mtime 复核；
  2. 单文件分片并发上传与队列暂停/重排；
  3. 上传断点、多连接回退与 TUI 的端到端自动化测试；
  4. 分享/离线/回收站按需增补；
  5. 跨平台与打包。

## 10. 附：关键协议注意事项

- `locatedownload` 使用 `app_id=250528`；上传 `superfile2` 使用配置 `app_id=266719`。
- 下载直链优先 PCS UA + Cookie；401/403 时用浏览器 UA 的无 Cookie 客户端兜底。
- 下载必须使用闭合 Range，单段不超过 4 MiB；开放 Range/无 Range 在大文件上会返回 403 `31326`。
- 目录列表对分片上传合并出的文件返回 block-list JSON 的 MD5；内容 MD5 以下载主机 `Content-MD5` 为准。
- `precreate` 的 `block_list` 是 JSON 数组字符串，不是逗号分隔。
- `create` 的 `target_path` 是目标文件的父目录。
- 空文件也要计算 MD5：`d41d8cd98f00b204e9800998ecf8427e`。
- 空文件的 `precreate` 可用 `block_list=[""]` 或 `["<空串MD5>"]`；但 `/api/create` 只接受 `["<空串MD5>"]`，空数组和 `[""]` 都会返回 `errno=2`。当前实现把空文件的块列表固定为 `[EMPTY_MD5]`。
- 上传状态文件按“本地路径 + 远端路径 + 大小 + mtime”哈希命名；文件变化后不会误用旧状态。
- 下载 `.part` 与目标文件同目录，完成后原子 `rename`，避免半成品被误认为完整文件。
- 多连接下载状态为 `.<name>.pcstui.part.state.json`，记录总大小、4 MiB 分片大小与完成位图；状态与 `.part` 不一致时丢弃重下。
- 上传合并后目录 MD5 不可信，因此上传成功前会再读一次服务端 `Content-MD5`；缺失时容忍，明确不一致则失败。
