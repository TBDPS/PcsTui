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

## ⚡ Quickstart

### 1. 下载或编译

- **方式 A：直接下载二进制（推荐）**  
  前往 [GitHub Releases](https://github.com/TBDPS/PcsTui/releases/latest) 下载对应平台（Linux / macOS / Windows）编译好的压缩包，解压后即可直接运行。

- **方式 B：本地源码编译**  
  ```bash
  git clone https://github.com/TBDPS/PcsTui.git
  cd PcsTui
  cargo build --release
  # 可执行文件位于 target/release/pcstui
  ```

### 2. 运行与登录

首次在终端运行程序：

```bash
./pcstui
```

首次启动将直接进入双栏登录界面：
1. 打开浏览器登录百度网盘，在开发者工具（F12 -> Network / Application）中复制 Cookie；
2. 在 TUI 的 `BDUSS` 输入框直接**粘贴完整 Cookie 字符串**，程序会自动解析提取 `BDUSS` 与 `STOKEN`；
3. 回车后程序将自动在线验证，核验成功即进入主界面。

*也可以通过命令行直接登录：*
```bash
./pcstui login --cookie 'BDUSS=...; STOKEN=...'
```

### 3. 日常使用

- 默认直接运行 `./pcstui` 进入交互式双栏界面；
- 也可以通过命令行脚本化执行简单任务：
  ```bash
  ./pcstui ls /               # 列出网盘根目录
  ./pcstui download /远程/文件.zip --dest ./downloads # 下载
  ./pcstui upload ./本地文件.pdf --dest /我的网盘      # 上传
  ./pcstui quota              # 查看容量
  ./pcstui doctor             # 诊断本地与网络状态
  ```

---

## 🎮 常用快捷键速查

| 按键 | 功能说明 |
| :--- | :--- |
| `Tab` | 在本地面板、远程面板、传输队列之间切换焦点 |
| `↑` `↓` `PgUp` `PgDn` | 列表上下移动光标 |
| `1`–`99` | 快速输入数字跳转项 |
| `Enter` / `→` | 进入目录；在本地面板回车上传；在远程面板回车下载 |
| `Backspace` / `←` | 返回上级目录 |
| `Space` | 标记 / 取消标记（支持批量操作） |
| `d` / `u` | 快速触发下载（远程面板） / 上传（本地面板） |
| `x` | 删除选中项（需二次确认，默认取消，键入 `y` 确认） |
| `m` / `c` | 移动 / 复制选中的远程项目（输入框支持 `Tab` 补全路径） |
| `R` / `n` | 重命名远程文件 / 新建远程目录 |
| `g` / `/` / `f` | 路径跳转 / 本地过滤 / 递归搜索网盘 |
| `s` / `h` | 排序切换 / 显示或隐藏本地点文件（dotfiles） |
| `r` / `p` | 刷新当前列表 / 显示或折叠传输队列 |
| `K` | 取消传输面板中当前选中的任务 |
| `C` | 打开配置与诊断面板（导入、导出、重新登录、状态自检） |
| `?` / `q` | 打开帮助 / 退出程序 |

---

## 🔒 配置说明与注意事项

1. **配置自包含**：所有配置文件默认存放在执行目录同级的 `config/` 目录下（`config/pcstui.json`、`config/queue.json` 等）。
2. **凭据安全**：配置文件以 `0600` 私有权限写入，切勿将其上传到公开平台。换机时可复制该文件，或在 TUI 中按 `C` 导出/导入。
3. **传输机制**：
   - 大文件（≥64 MiB）下载支持 Range 闭合区间断点续传（默认 4 并发连接，每片 4 MiB）；
   - 上传支持分块断点续传与秒传；
   - 未完成任务保存在 `config/queue.json`，重启后自动恢复；
   - 未对包含极多文件的目录下载进行优化，注意使用场景。
4. **单实例保护**：内置基于系统内核的排他锁（`config/pcstui.lock`），避免多个实例并发写导致配置或队列损坏。

---

## 开源协议

本项目采用 [Apache-2.0 许可证](LICENSE) 分发。
第三方协议引用归属见 [NOTICE](NOTICE) 文件。
