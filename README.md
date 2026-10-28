# pcstui

原生 Rust 百度网盘双栏 TUI，CLI 作为脚本与冒烟测试入口。所有操作直接走百度网盘 HTTP API。

- 项目位置：`/home/mat/pansync/pcstui`
- 可执行文件：`target/release/pcstui`
- 许可证：Apache-2.0（`NOTICE` 保留协议实现的必要归属）
- 自包含：源码和可执行文件不含任何账号信息，凭据只保存在可移植的 `config/pcstui.json`

## 设计目标

- 全部操作直接走百度网盘 HTTP API；
- 结构化错误处理、持久化传输队列、断点续传和 MD5 校验；
- 用 `fs_id` 标识远程对象，避免缓存列表与真实网盘状态不一致导致误操作；
- TUI 为配置与诊断主入口；CLI 保持最小脚本面，不追求功能对等；
- 换机只需一个文件：复制 `config/pcstui.json`，或在 TUI 里按 `C` 导入明文配置。

实现说明与取舍见：

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
- [`docs/ACCEPTANCE.md`](docs/ACCEPTANCE.md)
- [`docs/FEATURE-MATRIX.md`](docs/FEATURE-MATRIX.md)

## 功能

### TUI

- 双栏：本地文件系统 / 远程网盘
- 远程目录浏览、跳转、过滤、排序、搜索（当前目录递归）
- 本地过滤、隐藏文件切换、父目录导航
- 批量标记（空格）后下载 / 上传 / 删除 / 移动 / 复制
- 传输队列面板：进度、速度、状态、取消
- 远程 mkdir / rename / move / copy / delete
- 远程路径 Tab 补全（跳转、移动、复制、新建目录、重命名）
- 序号快速跳转：`1`–`99`，单位数稍候生效，两位数立即跳转
- 底部元数据行：当前选中项的路径、大小、mtime/ctime、`fs_id`、MD5
- 输入框支持自由光标编辑：`←/→`、`Home/End`、`Delete`、`Ctrl+W`、`Ctrl+U/K`、`Ctrl+A/E`
- 持久化待办队列：未完成的传输在重启 TUI 后自动恢复
- 下载：多连接 Range 断点续传、`.pcstui.part`、服务端内容 MD5 校验、保留 mtime
- 上传：MD5 / slice-MD5 / CRC32 / 分块 MD5、`precreate → superfile2 → create`、持久化断点、分片重试
- 全局传输并发限制：下载与上传分别限流，避免一次标记大量文件时打满网络/句柄
- Yazi：在本地当前目录直接启动，退出后刷新本地列表
- 窄终端自动改为上下分栏；帮助浮层、输入浮层、危险操作确认

### CLI（脚本/冒烟测试）

```bash
pcstui whoami
pcstui quota
pcstui ls /apps/bypy/saved
pcstui search 关键字 --dir /apps/bypy/saved
pcstui download /apps/bypy/saved/a.bin --dest ./downloads
pcstui download /apps/bypy/saved/a.bin --dest ./downloads --connections 8 --retries 5
pcstui upload ./a.bin --dest /apps/bypy/saved
pcstui mkdir /apps/bypy/saved/new
pcstui mv /apps/bypy/saved/a /apps/bypy/saved/b
pcstui cp /apps/bypy/saved/a /apps/bypy/saved/b
pcstui rename /apps/bypy/saved/a /apps/bypy/saved/c
pcstui rm /apps/bypy/saved/a --yes
pcstui doctor
```

默认没有子命令时启动 TUI。

## 安装与构建

要求 Rust 1.85+（edition 2024）。

```bash
cd /home/mat/pansync/pcstui
cargo build --release
install -m 755 target/release/pcstui bin/pcstui   # 可选
```

运行：

```bash
cargo run --release
# 或
./target/release/pcstui
```

> 请始终使用 release 构建。debug 构建下上传前的 MD5/CRC32 计算会慢一个数量级（实测 64 MiB 文件：release 约 15 秒完成上传，debug 需要数分钟）。

## 首次登录与配置

不支持密码/验证码登录（避免处理验证码、短信、二次验证和风控），使用浏览器 Cookie 中的 `BDUSS` / `STOKEN`。

### TUI 登录页

首次启动（`config/pcstui.json` 不存在或不含账号）会直接进入登录页：

- `BDUSS` / `STOKEN` 输入框默认掩码显示；
- 可直接把浏览器里的完整 Cookie 粘贴到 `BDUSS` 字段，会自动拆出 `STOKEN`；
- 第三项可填另一台机器导出的明文 `pcstui.json` 路径，直接导入账号；
- 按 `Enter` 后先在线验证，验证通过才写入配置；失败会停留在登录页并显示原因；
- 写入前会把旧配置备份为 `pcstui.json.bak-<时间戳>`；
- `Esc` 退出。

### 命令行登录

在 TTY 下直接运行 `pcstui login` 会打开同一个登录页；带参数时走无头流程。`uid` 可省略，登录后自动获取。

```bash
# 打开 TUI 登录页
pcstui login

# 直接传入凭据；--cookie 可粘贴完整 Cookie
pcstui login --cookie 'BDUSS=...; STOKEN=...'
pcstui login --bduss '<BDUSS>' --stoken '<STOKEN>'

# 换机/恢复：在 TUI 中按 C，再用 i 导入明文 pcstui.json
pcstui

# 只生成空配置，随后手工编辑
pcstui config init
$EDITOR config/pcstui.json
```

`uid` 不是必填项：下载签名接受 `0`，登录成功后会从在线账号自动回填。`--uid` 仅作为可选覆盖。

### 可移植配置

`config/pcstui.json` 是唯一的凭据载体，复制它即可换机。TUI 内的“配置与诊断”面板（`C`）是主入口：

| 键 | 操作 |
| --- | --- |
| `e` | 导出含账号凭据的明文副本（`0600`） |
| `E` | 导出脱敏模板（不含账号，可安全分享） |
| `i` | 导入明文 `pcstui.json`：先在线验证账号，再确认替换 |
| `r` | 重新登录（重建客户端与传输引擎，未完成任务保留在队列中） |
| `d` | 刷新本地权限检查与在线诊断 |

导入不会静默覆盖当前配置：验证失败会保留旧配置；验证成功也要再次确认，替换前自动备份旧文件。导出目标已存在时拒绝写入，且永远不能覆盖当前活动配置。

pcstui 不做凭据加密/解密。需要加密保管时，请自行使用 `age` / `gpg`：

```bash
age -p -o pcstui.json.age config/pcstui.json
age -d -o pcstui.json pcstui.json.age
```

加密文件不能直接导入；面板会明确提示“先解密为明文 pcstui.json”。

CLI 仅保留次级入口，不追求与 TUI 功能对等：

```bash
pcstui config export ~/pcstui-portable.json
pcstui config export --redacted ~/pcstui-template.json
pcstui config import ~/pcstui-portable.json
```

导出的相对下载目录会保留为相对路径，换项目目录后仍可用。

配置默认写入：

```text
pcstui/config/pcstui.json          # 0600，账号 + 全部设置
pcstui/config/queue.json           # 0600，持久化待办队列
pcstui/config/upload-state/*.json  # 0600
pcstui/downloads/                  # CLI 默认下载目录（TUI 下载到本地栏当前目录）
pcstui/tmp/                        # 预留临时目录
```

可用环境变量覆盖：

| 变量 | 含义 |
| --- | --- |
| `PCSTUI_HOME` | 项目根目录 |
| `PCSTUI_CONFIG` | 配置文件路径 |
| `PCSTUI_CONFIG_DIR` | 配置目录 |
| `PCSTUI_DOWNLOAD_DIR` | 默认下载目录 |
| `PCSTUI_TMP_DIR` | 临时目录 |

查看当前路径与脱敏账号：

```bash
pcstui config show
pcstui config path
pcstui doctor
```

## TUI 快捷键

| 键 | 作用 |
| --- | --- |
| `Tab` / `Shift+Tab` | 本地 / 远程 / 传输面板 |
| `↑ ↓ PgUp PgDn Home End` | 移动选择 |
| `1`–`99` | 快速跳转到当前面板第 N 项（单位数稍候生效） |
| `Enter` / `→` | 进入目录；本地文件上传；远程文件下载 |
| `Backspace` / `←` | 返回上级目录 |
| `Space` | 标记/取消标记 |
| `d` | 下载远程选中项到本地当前目录 |
| `u` | 上传本地选中项到远程当前目录 |
| `x` | 删除（确认） |
| `m` / `c` | 移动 / 复制到指定远程目录 |
| `R` | 重命名远程选中项 |
| `n` | 新建远程目录 |
| `g` | 跳转远程绝对路径 |
| `/` | 过滤当前面板 |
| `f` | 搜索网盘 |
| `s` | 远程排序；本地切换隐藏文件 |
| `h` | 本地显示/隐藏点文件 |
| `r` | 刷新 |
| `p` | 显示/隐藏传输队列 |
| `K` | 取消选中的传输 |
| `y` | 在本地当前目录启动 Yazi |
| `o` | `xdg-open` 打开本地文件 |
| `C` | 配置与诊断：导出 / 导入 / 重新登录 / 检查 |
| `?` | 帮助 |
| `q` | 退出（活动传输时确认） |

输入框（跳转 / 新建 / 移动 / 复制 / 重命名）：

- `Tab`：请求远程路径补全；只有一个候选时直接补全，多个候选时补全公共前缀或显示候选；
- `←/→`：移动光标；`Home/End`：行首/行尾；
- `Delete`：删除光标下字符；`Backspace`：删除光标前字符；
- `Ctrl+W`：删除左侧单词；`Ctrl+U`：删除到行首；`Ctrl+K`：删除到行尾；
- `Ctrl+A/E`：行首/行尾。

## 断点、并发与校验

### 下载

- 临时文件：目标目录下 `.<文件名>.<fs_id>.pcstui.part`；`fs_id` 缺失时使用远程路径指纹，避免同名不同文件共用断点
- 单流重试从 `.part` 长度继续；服务端不支持 Range 时自动从头开始
- 大文件（≥64 MiB）默认多连接 Range：每个分片 4 MiB，`download_connections` 控制单文件连接数
- 多连接断点状态：`.<文件名>.<fs_id>.pcstui.part.state.json`，记录分片位图、每个已完成分片的 MD5 和 `.part` 指纹；缺失或截断的 `.part` 会整段丢弃，指纹变化时重新校验已完成分片，避免拼出稀疏文件
- 分片数据 `sync_data` 成功后才写入断点状态；最终 rename 前再次 `sync_all`，降低崩溃后“状态说完成、数据却没落盘”的风险
- 服务端返回 403/401 时自动回退到单流下载；单流仍使用闭合 Range，兼容百度大文件限制
- 下载完成后先校验再原子重命名；保留远端 mtime
- 百度目录列表对“分片上传合并”的文件会返回 `block_list` JSON 的 MD5，而不是文件内容 MD5；下载器会用下载主机的 `Content-MD5` 响应头做权威校验，因此不会误删正确文件
- 已存在的同名文件：CLI 默认校验后跳过；TUI 默认自动改名，避免误覆盖
- `--overwrite` 覆盖，`--rename` 自动改名，`--no-verify` 关闭 MD5
- `--connections N` / `--retries N` 可覆盖本次下载的配置

### 上传

- 上传前单遍计算：整文件 MD5、前 256 KiB slice-MD5、CRC32、分块 MD5
- 分块大小自动 4/16/64 MiB（`<8 GiB` / `<32 GiB` / 更大），可用 `--chunk-mib` 覆盖
- 状态文件：`config/upload-state/<hash>.json`，记录 `uploadid`、已确认分片 MD5 和本地整文件 MD5；本地内容变化时不会错误复用旧分片
- 中断后重新运行同一上传，会复用 `uploadid` 并跳过已完成分片
- 每个分片最多重试 `transfer.retries` 次（默认 3）
- 服务端返回的分片 MD5 会与本地计算值比对，不一致按失败重试，避免把坏分片合并进文件
- 合并完成后再次读取下载主机的 `Content-MD5`，与本地整文件 MD5 比对；明确不一致时上传失败，不会静默报告成功
- `upload_connections` 控制同时执行的上传任务数（不是单个文件的分片并发）
- 空文件使用空串 MD5 作为唯一分块，兼容 `precreate`/`create` 的校验要求
- 秒传（`precreate` 返回 `return_type=2`）直接完成

### 持久化队列

- 路径：`config/queue.json`（0600，原子写入）
- 记录下载/上传任务及其选项；TUI 启动时自动恢复未完成任务
- 入队时按任务身份去重：下载使用 `fs_id + 远程路径 + 本地根目录`，上传使用 `本地路径 + 远程目录`；重复任务不会并行写入同一 `.part` 或上传状态
- 启动恢复时会清理队列中的重复项
- 任务完成或失败后从队列移除；应用被强制退出时，未完成任务会在下次启动继续
- 队列只记录任务意图，真正的断点仍由 `.pcstui.part` / `upload-state` 管理

## 配置示例

```json
{
  "version": 1,
  "account": {
    "uid": 123456,
    "name": "your-name",
    "bduss": "...",
    "stoken": "...",
    "baiduid": "",
    "access_token": ""
  },
  "app_id": 266719,
  "pan_ua": "netdisk;P2SP;3.0.0.8;...",
  "user_agent": "Mozilla/5.0 ...",
  "download_dir": null,
  "last_remote_dir": "/",
  "transfer": {
    "download_connections": 4,
    "upload_connections": 2,
    "max_active_downloads": 3,
    "retries": 3,
    "verify_md5": true,
    "upload_chunk_mib": 0
  }
}
```

字段说明：

- `download_connections`：单个大文件的多连接 Range 连接数（1–16，默认 4）；
- `upload_connections`：同时执行的上传任务数（1–8，默认 2）；
- `max_active_downloads`：同时执行的下载任务数（1–8，默认 3）；
- `retries`：下载请求/分片和上传分片的重试次数（0–10，默认 3）；
- `verify_md5`：下载完成后是否校验内容 MD5（默认 true）；
- `upload_chunk_mib`：上传分块大小，0 表示自动 4/16/64 MiB。

## 已知限制（有意收敛）

- 不支持密码/验证码登录、分享链接、离线下载、回收站、多账号切换；
- 不支持秒传的“旧接口”路径（`rtype=3`），只使用 `precreate` 的秒传返回；
- 加密下载链接（`encrypt=1`）不尝试破解 pan-home 签名，回退到带 Cookie 的 PCS `download` 端点；
- 上传分片仍按文件内顺序执行；`upload_connections` 只并发多个文件；
- 百度接口可能变更，`doctor` 用于快速定位认证/配额/根目录问题；
- 仅保证 Linux 终端体验（Yazi/`xdg-open` 为 Linux 习惯），Rust 代码本身可移植。

## 测试

```bash
cargo fmt --check
cargo test
cargo build --release
./target/release/pcstui doctor
./target/release/pcstui ls /
```

单元测试覆盖：签名向量、MD5 混淆解码、sign2、路径规范化/本地路径安全、配置导入/导出与脱敏、加密/二进制配置拒绝、备份替换、导出不覆盖活动配置、Cookie 解析、分块哈希、空文件、上传计划、队列去重/往返、断点状态校验、输入光标/单词删除、Tab 补全路径、TUI 配置面板按键隔离、CLI 参数、排序/过滤。

真实 API 冒烟测试需要有效 BDUSS/STOKEN，见 `docs/ACCEPTANCE.md` 的验收清单。

## 目录结构

```text
pcstui/
├── src/
│   ├── api/          # HTTP 客户端、签名、模型
│   ├── transfer/     # 下载/上传引擎、持久化队列、断点状态、进度事件
│   ├── tui/          # App 状态、事件循环、ratatui 渲染
│   ├── cli.rs        # clap 子命令与 headless 流程
│   ├── config.rs     # 可移植配置、导入/导出、权限、凭据掩码
│   ├── lib.rs
│   └── main.rs
├── docs/
├── config/           # 运行时生成，已 gitignore
├── downloads/        # 运行时生成，已 gitignore
├── LICENSE
└── NOTICE
```

## 可移植性

- 源码和 release 可执行文件不含任何账号、Cookie 或本机路径；全部凭据只在运行时从配置文件读取；
- `config/pcstui.json` 是自包含的：复制到新机器后，在 TUI 配置面板（`C`）用 `i` 导入即可；
- 配置文件和队列文件以 `0600` 写入，配置目录为 `0700`，写入采用临时文件 + rename 的原子方式；
- 日志、`Debug` 输出和 `config show` 只显示掩码后的凭据；
- 不依赖外部百度网盘客户端，也不需要额外目录或环境变量。
