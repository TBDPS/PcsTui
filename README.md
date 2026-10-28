# pcstui

[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](Cargo.toml)

原生 Rust 实现的现代化百度网盘双栏终端客户端（TUI），提供高效的图形化双栏文件管理体验，并保留轻量级 CLI 作为脚本与自动化入口。客户端直接基于百度网盘原生 HTTP API 构建。

---

## 核心特性

- **双栏直观浏览**：本地文件系统与远程网盘同屏双栏对照，导航、标记、批量上传/下载一气呵成。
- **纯原生高性能**：基于 Rust `tokio` 异步运行时与 `ratatui` 构建，内存占用极小，启动瞬时完成。
- **可靠的断点续传**：
  - **下载**：大文件（≥64 MiB）自动启用多连接 Range 闭合区间并发（默认 4 连接，每片 4 MiB），维护分片位图与每片独立 MD5 校验；下载完成后校验权威 `Content-MD5` 并保留远端修改时间。
  - **上传**：支持分块计算、`precreate` 秒传、分片断点续传（`superfile2`）与分片 MD5 校验，防止上传坏块合并。
- **持久化任务队列**：传输意图持久化保存在 `config/queue.json` 中，按任务身份强去重；意外退出或重启后自动恢复未完成的任务。
- **破坏性操作安全防御**：
  - 远程删除、移动、复制、重命名操作均深度绑定百度网盘不可变对象标识 `fs_id`。
  - 操作提交前自动向服务端复核远端状态，若项目已被外部修改或删除，整批操作立即主动中止，杜绝列表脏数据导致的误删。
  - 删除确认默认“取消”，必须显式键入 `y` 确认。
- **单实例内核互斥锁**：
  - 依托内核级建议锁（Unix `flock` / Windows `LockFileEx`），严防多个 TUI 实例或并发写命令损坏配置文件与传输状态。
  - 读写解耦：只读 CLI 指令（`whoami`、`quota`、`ls`、`doctor` 等）完全免锁，可在 TUI 运行的同时自由并发查询。
  - 进程崩溃时内核自动释放锁，无遗留死锁风险。
- **自包含与安全配置**：
  - 零远程遥测、零外部凭据中转，源码与编译产物不包含任何账号硬编码。
  - 本地配置（`config/pcstui.json`）与待办队列严格采用 `0600` 私有权限，并支持脱敏导出（可安全分享模板）与完整明文导出（配合 `age`/`gpg` 加密）。
- **灵活的终端适配与跨平台**：
  - 支持 Linux、macOS 与 Windows。
  - 支持 90×30 等短屏/窄屏终端自适应（自适应双栏快捷键帮助、配置面板固定按键行与平滑滚动）。
  - 全局支持括号化粘贴模式（Bracketed Paste），避免多行粘贴文本意外触发行提交。

---

## 快速开始

### 编译与安装

确保系统已安装 Rust 1.85+（edition 2024）：

```bash
# 克隆仓库
git clone https://github.com/<your-username>/pcstui.git
cd pcstui

# 编译 Release 二进制（推荐始终使用 release，Debug 下哈希计算显著变慢）
cargo build --release

# 可选：安装到系统 PATH
sudo install -m 755 target/release/pcstui /usr/local/bin/pcstui
```

### 首次登录

pcstui 支持浏览器 Cookie 中的 `BDUSS` / `STOKEN` 快速登录（不处理验证码与短信风控）：

1. **直接启动 TUI 交互登录**：
   ```bash
   ./target/release/pcstui
   ```
   首次运行将直接进入登录界面：
   - 支持直接将浏览器 DevTools 中截取的完整 Cookie 字符串粘贴到 `BDUSS` 输入框，程序将自动解析提取 `BDUSS` 与 `STOKEN`；
   - 输入完成回车后，程序会先向百度服务端进行在线身份核验，核验通过后才会安全落盘并进入主界面。

2. **命令行登录**：
   ```bash
   # 直接传入 Cookie
   pcstui login --cookie 'BDUSS=...; STOKEN=...'

   # 或分别指定凭据
   pcstui login --bduss '<BDUSS>' --stoken '<STOKEN>'
   ```

---

## 终端界面指南（TUI）

默认不带参数启动 `pcstui` 即可进入双栏界面：

```text
 ┌── 本地文件系统 ──────────┐  ┌── 远程网盘 [/] ────────────┐
 │ ..                       │  │ 📁 文档/                   │
 │ 📄 backup.tar.gz         │  │ 📁 照片备份/               │
 │ 📄 project.rs            │  │ 📄 dataset.zip             │
 └──────────────────────────┘  └────────────────────────────┘
 ┌── 传输队列 (按 p 展开) ──────────────────────────────────┐
 │ ↓ dataset.zip  [████████░░] 80.2%  12.4 MB/s  (4 连接)   │
 └──────────────────────────────────────────────────────────┘
 [选中项元数据: 路径/大小/修改时间/fs_id/MD5]
 [快捷操作提示栏]
```

### 常用快捷键

| 按键 | 功能说明 |
| :--- | :--- |
| `Tab` / `Shift+Tab` | 切换焦点（本地面板 / 远程面板 / 传输队列） |
| `↑` `↓` `PgUp` `PgDn` | 列表上下移动光标 |
| `1`–`99` | 快速数字跳转（快速定位当前面板第 N 项） |
| `Enter` / `→` | 进入目录；本地文件触发上传；远程文件触发下载 |
| `Backspace` / `←` | 返回上级目录 |
| `Space` | 标记 / 取消标记选中项（支持跨文件多选批量操作） |
| `d` | 下载选中的远程项目到本地当前目录 |
| `u` | 上传选中的本地项目到远程当前目录 |
| `x` | 删除选中项目（需弹出确认，默认取消，键入 `y` 执行） |
| `m` / `c` | 移动 / 复制选中的远程项目（输入框支持 `Tab` 补全路径） |
| `R` | 重命名选中的远程文件/目录（支持行内自由移动光标） |
| `n` | 在当前远程目录下新建文件夹 |
| `g` | 绝对路径跳转（支持 `Tab` 自动补全远端路径） |
| `/` | 快速过滤当前面板列表（本地过滤无需网络请求） |
| `f` | 在当前远程目录下递归搜索关键字 |
| `s` | 远程面板排序切换；本地面板切换是否显示隐藏文件 |
| `h` | 本地面板显示 / 隐藏点文件（dotfiles） |
| `r` | 刷新当前面板数据 |
| `p` | 展开 / 折叠底部实时传输任务面板 |
| `K` | 取消传输面板中当前选中的传输任务 |
| `y` | 在本地当前目录就地唤起 Yazi 文件管理器（退出后自动刷新） |
| `o` | 使用系统关联默认程序打开本地文件（如 `xdg-open`） |
| `C` | 打开“配置与诊断面板”（查看账号/配额/路径、导出、导入、重新登录） |
| `?` | 打开 / 关闭帮助浮层 |
| `q` | 退出程序（若有正在进行的传输任务，会要求二次确认） |

---

## 命令行用法（CLI）

除了丰富的 TUI 界面，`pcstui` 还提供了符合 Unix 管道习惯的轻量命令行接口：

```bash
# 查询当前登录账号与网盘容量
pcstui whoami
pcstui quota

# 列出网盘目录（支持 --json 与详细模式 --long）
pcstui ls /apps/notes
pcstui ls /apps/notes --long

# 搜索网盘文件
pcstui search 学习资料 --dir /

# 下载文件或整个目录
pcstui download /远程路径/file.zip --dest ./downloads
pcstui download /远程路径/folder/ --dest ./downloads

# 上传文件或整个目录
pcstui upload ./local_file.pdf --dest /我的资料
pcstui upload ./folder/ --dest /我的资料

# 远程管理操作
pcstui mkdir /远程路径/新目录
pcstui mv /远程路径/a.txt /远程路径/b.txt
pcstui cp /远程路径/a.txt /远程路径/backup/
pcstui rename /远程路径/old.txt /远程路径/new.txt
pcstui rm /远程路径/temp/ --yes

# 运行本地与网络诊断自检
pcstui doctor
```

---

## 配置与迁移

### 配置文件位置

默认情况下，`pcstui` 的全部运行时文件保存在执行程序同级的 `config/` 目录下（便于自包含便携运行）：

```text
config/
├── pcstui.json          # 核心配置文件（账号凭据与传输参数，权限 0600）
├── queue.json           # 待办传输队列存储
├── upload-state/        # 大文件断点续传状态目录
└── pcstui.lock          # 单实例互斥文件锁
```

可通过环境变量自定义配置位置：

| 环境变量 | 说明 |
| :--- | :--- |
| `PCSTUI_HOME` | 自定义项目主工作目录（所有配置与队列均存放在其下） |
| `PCSTUI_CONFIG` | 显式指定 `pcstui.json` 文件路径 |
| `PCSTUI_CONFIG_DIR` | 显式指定配置存储目录 |
| `PCSTUI_DOWNLOAD_DIR` | 显式指定默认下载落盘目录 |

### 换机与备份导出

- **在 TUI 中换机**：按 `C` 打开配置面板，键入 `e` 可导出包含凭据的明文副本；在新机器上按 `C` 键入 `i` 选择该文件，程序将在线验证无误后原子替换并重登。
- **脱敏分享**：在 TUI 中按 `C` 键入 `E`，或使用 `pcstui config export --redacted template.json`，可导出剥离所有个人账号与 Cookie 的纯净配置模板，供安全分享。
- **外部加密安全建议**：`pcstui` 遵循“职责单一”原则，不在二进制内部集成不透明的加解密算法。如需备份凭据，建议使用标准的开源工具（如 `age` 或 `gpg`）进行加密保管：
  ```bash
  # 加密备份
  age -p -o pcstui.json.age config/pcstui.json

  # 解密还原
  age -d -o pcstui.json pcstui.json.age
  ```

---

## 技术架构与实现规范

详细的设计实现、通信协议规范与自动化验收清单，请参考文档目录：

- [架构设计详解（ARCHITECTURE.md）](docs/ARCHITECTURE.md)：包含 API 客户端分层、Range 下载状态机、分块上传状态机、任务排他锁机制等深度剖析。
- [功能与场景矩阵（FEATURE-MATRIX.md）](docs/FEATURE-MATRIX.md)：TUI 与 CLI 细项功能对比及路线图。
- [测试与验收标准（ACCEPTANCE.md）](docs/ACCEPTANCE.md)：全量安全与边缘场景验收清单。

---

## 开源协议

本项目采用 [Apache-2.0 许可证](LICENSE) 分发。
第三方协议引用归属见 [NOTICE](NOTICE) 文件。
