# 验收与边界

本文记录 pcstui 的协议验证范围、验收清单、可移植性要求与已知风险。产品定位与功能取舍见 [`FEATURE-MATRIX.md`](FEATURE-MATRIX.md)。

## 1. 已验证能力

| 能力域 | 状态 | 说明 |
| --- | --- | --- |
| Cookie 登录态 | 已验证 | `BDUSS` + `STOKEN` 调用网盘 Web/PCS 接口 |
| 目录浏览 / 搜索 / 配额 / 账号 | 已验证 | `/api/list`、`xpan/file?method=search`、`/api/quota`、`xpan/nas?method=uinfo` |
| mkdir / rm / mv / cp / rename | 已验证 | PCS `method=mkdir/delete/move/copy` |
| 下载（Range 续传、MD5、mtime） | 已验证 | `locatedownload` 签名 + PCS `download` 回退 |
| 上传（秒传、分片、断点） | 已验证 | `precreate → superfile2 → create` |
| 双栏 TUI / 队列 / 取消 | 已验证 | `ratatui` + `tokio` 事件循环 |

真实账号冒烟覆盖：`doctor`、`whoami`、`quota`、`ls`、`search`、上传、下载、断点续传、`mkdir`、`cp`、`rename`、`rm`；70 MiB 多连接下载与服务端 `Content-MD5` 校验；10 MiB 上传；TUI 在 140×40 与 90×30 下的渲染、序号跳转、元数据、输入光标、传输面板与队列恢复。

## 2. 明确不做

| 不做 | 原因 |
| --- | --- |
| 密码 / 验证码 / 短信登录 | 验证码、风控、二次验证维护成本高；Cookie 导入已覆盖个人使用 |
| 分享链接创建 / 转存 | 接口与风控策略变化频繁，与个人文件管理主路径弱相关 |
| 离线下载 | 需要任务轮询、配额与资源策略，独立产品级工作量 |
| 回收站管理 | 删除已可用；恢复/彻底删除留待后续 |
| 多账号 UI | 单账号配置更简单；多账号应先做配置模型迁移 |
| 单文件分片并发上传 | 先保证状态文件与重试语义正确；`upload_connections` 已用于多任务并发 |
| 队列暂停/重排/优先级 | 当前只做持久化恢复与取消 |
| 加密下载链接的 pan-home 签名破解 | 脆弱且易失效；回退到 Cookie 认证的 PCS 下载端点 |
| 旧式 `rtype=3` 秒传 | 只使用 `precreate` 返回的秒传结果 |
| 凭据加密/解密 | 明文配置 + 用户自行使用 age/gpg；产品不保存密码 |

## 3. 验收清单

### 3.1 首次使用与配置

- 空 `PCSTUI_HOME` 下 `pcstui config init` 生成 `config/pcstui.json`（`0600`）与 `config/upload-state/`（`0700`）；
- 空目录启动 TUI 进入登录页，而不是直接报错退出；
- `pcstui login --bduss ... --stoken ...`（不传 `--uid`）自动回填 uid 并在线验证；
- 登录/导入前旧配置备份为 `pcstui.json.bak-<时间戳>`，权限 `0600`，保留策略最多保存最近 3 份历史备份；
- 导入加密/二进制文件被拒绝，且旧配置保持不变；
- `config export` 拒绝覆盖当前活动配置文件；
- `whoami`、`doctor` 在配置完成后正常。

### 3.2 传输

- 预置 300 KiB `.part` 后继续下载，最终 MD5 与源一致；
- 同名不同 `fs_id` 的远程文件不会共用 `.part`；
- 截断/缺失 `.part` 时断点状态被丢弃，不拼接稀疏文件；
- 128 MiB 上传中断后，`config/upload-state/*.json` 保留 `uploadid` 与已完成分片，重跑后跳过；
- 本地文件内容变化后，旧上传状态不会被误用；
- 队列中相同任务身份不会重复入队；重启后去重并恢复。

### 3.3 TUI

- 140×40 与 90×30 下双栏/上下分栏、帮助（双栏自适应）、传输面板、确认框、配置面板（固定底部按键行与滚动）均可完整渲染且无按键截断；
- 主界面与登录界面均启用 bracketed paste，避免多行粘贴导致按键穿透误触提交；
- 危险操作确认默认取消，`Enter` 不执行删除；
- 远程项目在确认后被修改时，整批操作中止；
- `C` 面板按键不泄漏到主界面；`e/E` 导出、`i` 导入、`r` 重新登录、`d` 刷新可用；
- 导入成功后重启客户端与引擎，未完成任务从队列恢复；
- Yazi 挂起/恢复后终端状态正常。

### 3.4 可移植性与权限

```bash
# 源目录
pcstui config export /tmp/pcstui-portable.json

# 全新的 PCSTUI_HOME（CLI 次级入口）
PCSTUI_HOME=/tmp/pcstui-fresh pcstui config import /tmp/pcstui-portable.json
PCSTUI_HOME=/tmp/pcstui-fresh pcstui whoami
PCSTUI_HOME=/tmp/pcstui-fresh pcstui doctor
```

- 新目录无需手工编辑即可完成账号验证；
- 导入后的 `config/pcstui.json` 为 `0600`，配置目录为 `0700`；
- `--redacted` 导出不含 `account`，导入时被拒绝；
- 源码与 release 可执行文件中检索不到 BDUSS/STOKEN 明文。

## 4. 仍需人工确认

- Yazi 在 TUI 内的挂起/恢复（未做自动化交互测试）；
- 长时间大文件传输的内存与进度稳定性；
- 极端文件名（控制字符、超长中文名）在真实终端中的显示；
- 上传服务端返回 MD5 不一致时的重试路径（代码已实现，尚无故障注入测试）；
- 多连接下载在弱网/限流账号下频繁 403 时的回退体验；
- TUI 被强制终止后，持久化队列与 `.part`/上传状态的组合恢复。

## 5. 换机与恢复

1. **首次使用**：启动 TUI 进入登录页，粘贴浏览器 Cookie；或执行 `pcstui login`。
2. **换机**：原机器 TUI 面板 `C → e` 导出明文配置（或 `pcstui config export`），新机器 `C → i` 导入（或 `pcstui config import`）。
3. **共享模板**：`C → E` 或 `pcstui config export --redacted`；接收方手工填写账号。
4. **恢复**：导入的配置同时恢复传输参数、下载目录与最近远程目录；断点队列与上传状态不随配置迁移，需按需复制 `config/queue.json` 与 `config/upload-state/`。

回滚：

- 删除 `config/pcstui.json` 回到“未配置”状态，下次启动进入登录页；
- 导入失败不会替换当前配置；成功导入可用同目录的 `pcstui.json.bak-*` 恢复旧凭据；
- 上传断点可删除 `config/upload-state/`；下载临时文件可删除 `.<name>.<fs_id>.pcstui.part` 及其 `.state.json`。

## 6. 风险

| 风险 | 影响 | 缓解 |
| --- | --- | --- |
| 百度接口变更 | 局部功能失效 | 保留 `errno`/`request_id`，`doctor` 与 TUI 面板快速定位 |
| Cookie 过期 | API 返回认证错误 | 登录页重新登录；导入前在线验证 |
| 慢网/限流 | 120s 总超时内重试不足 | 拆分连接/读取超时属于后续工作；当前会失败并保留断点 |
| 同名文件 | 覆盖或断点串用 | TUI 默认自动改名；`.part` 含 `fs_id`；删除前按 `fs_id` 复核 |
| 强制终止 | 状态与数据不一致 | 状态在 `sync_data` 后写入；缺失/截断 `.part` 拒绝续传；队列重启恢复 |
