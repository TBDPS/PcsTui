# PcsTui

[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Release](https://img.shields.io/github/v/release/TBDPS/PcsTui)](https://github.com/TBDPS/PcsTui/releases)

原生 Rust 实现的百度网盘双栏终端客户端（TUI）与轻量命令行工具（CLI）。

> ⚠️ **声明与风险提示**：
> 1. **Vibe Coding 产物**：本项目为个人 Vibe Coding 探索实践产物，仅供学习、研究与个人日常测试使用。
> 2. **未经充分测试**：代码未经过大规模、全场景以及严苛边界情况的完整测试，可能潜藏未知的 Bug 或逻辑缺陷。
> 3. **数据风险提示**：在执行**删除、移动、重命名**等破坏性操作前，请务必提前对网盘核心数据做好备份。作者不对因使用本程序导致的任何数据丢失、文件损坏或误操作承担任何责任。
> 4. **账号与风控风险**：本地配置文件未加密，请妥善保管个人敏感信息。不要盲目过大调整并发连接数，以免触碰网盘服务端的调用频控或风控限制。

---

## ✨ 主要特性（v0.2.0）

- **双栏终端界面（TUI）**：本地与网盘同屏浏览、多选批量传输、序号快速跳转（`1`–`99`）、窄屏自适应布局、Yazi 集成；
- **可靠断点传输**：大文件（≥64 MiB）默认 4 连接 4 MiB 闭合 Range 并行下载，支持 `.pcstui.part.state.json` 分片校验、自动续传与 `Content-MD5` 校验；分片上传支持秒传与断点续传；队列持久化至 `queue.json`；
- **分享链接浏览、转存与下载（`S`）**：支持粘贴分享链接或整段分享口令，自动解析提取码与目录结构，支持仅转存到网盘（`t`）或转存并自动加入本地下载队列（`d`）；
- **群组文件浏览、转存与下载（`G`）**：支持查看已加入的网盘群组、逐级浏览群内共享文件夹，支持批量转存到网盘（`t`）或转存并下载到本地（`d`）；
- **单实例保护与便携配置**：基于系统内核建议锁防止并发写损坏状态；支持明文导入/导出与脱敏模板导出。

---

## ⚡ Quickstart

### 1. 下载或安装

- **方式 A：直接下载预编译二进制**  
  前往 [GitHub Releases](https://github.com/TBDPS/PcsTui/releases/latest) 下载对应平台（Linux / macOS / Windows）压缩包，解压后即可运行。

- **方式 B：源码编译并安装到本机（含图标与 Linux 开始菜单入口）**  
  ```bash
  git clone https://github.com/TBDPS/PcsTui.git
  cd PcsTui
  ./scripts/install.sh
  ```
  安装脚本会编译 Release 版本并完成：
  - 可执行文件安装至 `~/.local/bin/pcstui`
  - 图标安装至 `~/.local/share/icons/hicolor/scalable/apps/pcstui.svg`
  - 桌面启动器安装至 `~/.local/share/applications/pcstui.desktop`（可在开始菜单搜索 **PcsTui** 直接点击启动）

- **方式 C：仅在仓库内编译运行**
  ```bash
  cargo build --release
  ./target/release/pcstui
  ```

### 2. 登录与配置

首次运行将直接进入 TUI 登录页：
1. 在浏览器登录 `pan.baidu.com`，从开发者工具（F12 -> Network / Application）复制 Cookie；
2. 在 `BDUSS` 输入框**粘贴完整 Cookie 字符串**（支持 Bracketed paste），程序会自动拆分 `BDUSS` 与 `STOKEN`；
3. 回车完成在线核验后自动进入主界面。

也可以在命令行导入或登录：
```bash
pcstui login --cookie 'BDUSS=...; STOKEN=...'
# 或从已有明文配置导入：
pcstui config import /path/to/pcstui.json
```

### 3. 日常使用与常用命令

- **日常交互使用**：在终端输入 `pcstui`，或从桌面开始菜单启动 **PcsTui**；
- **常用只读/脚本命令**：
  ```bash
  pcstui doctor                                     # 检查配置、权限、账号与网络状态
  pcstui ls /                                       # 列出网盘根目录
  pcstui download /远程/文件.zip --dest ./downloads # 下载文件或目录
  pcstui upload ./本地文件.pdf --dest /我的网盘     # 上传文件或目录
  pcstui share "https://pan.baidu.com/s/1xxx" --pwd abcd  # 命令行只读查看分享链接
  pcstui group [gid]                                # 命令行只读查看群组列表或群共享文件
  ```

---

## 🎮 快捷键速查

| 按键 | 功能说明 |
| :--- | :--- |
| `Tab` | 在本地面板、远程面板、传输队列之间切换焦点 |
| `↑` `↓` `PgUp` `PgDn` | 列表上下移动光标 |
| `1`–`99` | 快速输入数字跳转项 |
| `Enter` / `→` | 进入目录；本地面板回车上传；远程面板回车下载 |
| `Backspace` / `←` | 返回上级目录 |
| `Space` | 标记 / 取消标记（支持批量操作） |
| `d` / `u` | 快速触发下载（远程面板） / 上传（本地面板） |
| `S` / `G` | 打开分享链接 / 浏览群组文件（远程面板） |
| `t` | 在分享或群组视图中转存选中项到个人网盘 |
| `x` | 删除选中项（需二次确认，默认取消，键入 `y` 确认） |
| `m` / `c` | 移动 / 复制选中的远程项目（输入框支持 `Tab` 补全路径） |
| `R` / `n` | 重命名远程文件 / 新建远程目录 |
| `g` / `/` / `f` | 路径跳转 / 本地过滤 / 递归搜索网盘 |
| `s` / `h` | 排序切换 / 显示或隐藏本地点文件（dotfiles） |
| `r` / `p` | 刷新当前列表 / 显示或折叠传输队列 |
| `K` / `R` / `c` | 传输面板：取消选中任务 / 重试失败或取消任务 / 清理已结束任务 |
| `C` | 打开配置与诊断面板（导入、导出、重新登录、状态自检） |
| `?` / `q` | 打开帮助 / 退出程序 |

---

## 🔒 配置路径与注意事项

1. **配置与路径规则**：
   - **安装到 `~/.local/bin` 或系统 `bin` 目录时**（如通过 `./scripts/install.sh` 安装）：遵循 XDG 标准，配置文件位于 `~/.config/pcstui/pcstui.json`，队列与上传断点位于 `~/.config/pcstui/`，默认下载目录为 `~/Downloads`；
   - **在源码目录或独立便携目录运行**（或设置了 `PCSTUI_HOME`）：所有配置与状态自包含在运行目录关联的 `config/` 下（`config/pcstui.json`、`config/queue.json`）；
   - 可随时运行 `pcstui config path` 查看当前生效的配置文件绝对路径，或用 `pcstui config show` 查看全部目录与配置。
2. **自定义目录与传输参数**：
   - 支持命令行设置默认下载目录：`pcstui config set-download-dir ~/Downloads`；
   - 也可直接编辑 `pcstui.json` 中的 `download_dir`、`local_dir`（TUI 左侧初始目录）、`transfer`（并发连接数、重试次数、MD5 校验等）。
3. **凭据安全、单实例保护与适用边界**：
   - 配置文件与断点状态均以 `0600` / `0700` 私有权限保存（明文未加密），切勿上传至公开仓库；
   - 内置基于系统内核的排他锁（`pcstui.lock`），避免多实例并发写损坏队列或配置（只读命令可免锁并发）；
   - 未对包含极多文件的目录下载进行特别优化，注意使用场景。
4. **排障与冒烟测试**：
   - 遇到接口异常时可加 `RUST_LOG=debug` 运行对应命令查看请求细节；
   - 源码仓库内置可选的真实账号冒烟测试（默认忽略，不会在普通 `cargo test` 中执行）：
     ```bash
     PCSTUI_SMOKE=1 cargo test --test smoke -- --ignored --nocapture
     ```

---

## 开源协议

本项目采用 [Apache-2.0 许可证](LICENSE) 分发。
第三方协议引用归属见 [NOTICE](NOTICE) 文件。
