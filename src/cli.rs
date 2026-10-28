use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use clap::{Parser, Subcommand};

use crate::api::BaiduClient;
use crate::api::models::{
    format_bytes, format_speed, format_timestamp, remote_join, remote_parent,
};
use crate::config::{Account, RuntimeConfig, expand_tilde, normalize_secret, parse_cookie_input};
use crate::transfer::download::{download_plan, plan_download};
use crate::transfer::types::{
    Direction, DownloadOptions, ProgressFn, ProgressReporter, TransferProgress, TransferStatus,
    UploadOptions,
};
use crate::transfer::upload::{UploadRequest, ensure_remote_dir, plan_upload, upload_one};

#[derive(Debug, Parser)]
#[command(
    name = "pcstui",
    version,
    about = "原生 Rust 百度网盘 TUI / CLI（无需 BaiduPCS-Go 二进制）",
    long_about = None
)]
pub struct Cli {
    /// 使用指定配置文件（默认 <项目>/config/pcstui.json）
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// 启动双栏 TUI（默认行为）
    Tui,
    /// 显示当前账号信息
    Whoami {
        #[arg(long)]
        json: bool,
    },
    /// 显示网盘容量
    Quota {
        #[arg(long)]
        json: bool,
    },
    /// 列出远程目录
    Ls {
        #[arg(default_value = "/")]
        path: String,
        #[arg(long)]
        json: bool,
        /// 同时显示 fs_id 与完整路径
        #[arg(long)]
        long: bool,
    },
    /// 搜索网盘文件
    Search {
        keyword: String,
        #[arg(long, default_value = "/")]
        dir: String,
        #[arg(long)]
        no_recursive: bool,
        #[arg(long)]
        json: bool,
    },
    /// 下载文件或目录
    Download {
        remote: String,
        /// 本地保存目录（默认 config/pcstui.json 的 download_dir）
        #[arg(short, long)]
        dest: Option<PathBuf>,
        #[arg(long)]
        overwrite: bool,
        /// 目标已存在时自动改名，而不是报错
        #[arg(long)]
        rename: bool,
        #[arg(long)]
        no_verify: bool,
        /// 单文件并发 Range 连接数（默认使用配置文件）
        #[arg(long)]
        connections: Option<usize>,
        /// 下载失败重试次数（默认使用配置文件）
        #[arg(long)]
        retries: Option<usize>,
    },
    /// 上传本地文件或目录
    Upload {
        local: Vec<PathBuf>,
        /// 远程目标目录（默认上次使用的目录）
        #[arg(short, long)]
        dest: Option<String>,
        /// 强制分片大小（MiB，0 = 自动 4/16/64）
        #[arg(long)]
        chunk_mib: Option<u64>,
        #[arg(long)]
        quiet: bool,
    },
    /// 递归创建远程目录
    Mkdir { path: String },
    /// 删除远程文件或目录
    Rm {
        paths: Vec<String>,
        /// 确认执行（脚本中必须显式传入）
        #[arg(long)]
        yes: bool,
    },
    /// 移动远程文件或目录
    Mv { source: String, dest: String },
    /// 复制远程文件或目录
    Cp { source: String, dest: String },
    /// 重命名远程文件或目录
    Rename { from: String, to: String },
    /// 诊断配置、权限、网络与 API
    Doctor,
    /// 登录并保存凭据（支持浏览器 Cookie；不实现密码/验证码登录）
    Login {
        /// 从已保存的 pcstui.json 导入账号（换机或恢复备份）
        #[arg(long, value_name = "FILE")]
        from_config: Option<PathBuf>,
        /// 完整 Cookie，或单独的 BDUSS 值
        #[arg(long)]
        cookie: Option<String>,
        /// BDUSS 值（也可直接粘贴完整 Cookie）
        #[arg(long)]
        bduss: Option<String>,
        /// STOKEN 值；缺少时会交互输入
        #[arg(long)]
        stoken: Option<String>,
        /// 可选；登录成功后会自动获取
        #[arg(long)]
        uid: Option<u64>,
        /// 可选显示名；默认使用网盘账号名
        #[arg(long)]
        name: Option<String>,
    },
    /// 查看或修改配置
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    Show,
    Path,
    Init,
    /// 导出可移植配置（默认包含凭据；--redacted 生成脱敏模板）
    Export {
        path: Option<PathBuf>,
        #[arg(long)]
        redacted: bool,
    },
    /// 导入另一台机器上的 pcstui.json
    Import {
        path: PathBuf,
    },
    SetDownloadDir {
        path: PathBuf,
    },
    SetRemoteDir {
        path: String,
    },
}

pub async fn run(cli: Cli) -> Result<()> {
    let config = RuntimeConfig::load_from(cli.config.clone())?;
    match cli.command.unwrap_or(Command::Tui) {
        Command::Tui => {
            let _lock = crate::lock::InstanceLock::try_acquire(&config.paths.config_dir, "tui")?;
            crate::tui::run(config).await
        }
        Command::Whoami { json } => whoami(config, json).await,
        Command::Quota { json } => quota(config, json).await,
        Command::Ls { path, json, long } => ls(config, &path, json, long).await,
        Command::Search {
            keyword,
            dir,
            no_recursive,
            json,
        } => search(config, &keyword, &dir, !no_recursive, json).await,
        Command::Download {
            remote,
            dest,
            overwrite,
            rename,
            no_verify,
            connections,
            retries,
        } => {
            let _lock =
                crate::lock::InstanceLock::try_acquire(&config.paths.config_dir, "cli-download")?;
            download(
                config,
                &remote,
                dest,
                overwrite,
                rename,
                no_verify,
                connections,
                retries,
            )
            .await
        }
        Command::Upload {
            local,
            dest,
            chunk_mib,
            quiet,
        } => {
            let _lock =
                crate::lock::InstanceLock::try_acquire(&config.paths.config_dir, "cli-upload")?;
            upload(config, local, dest, chunk_mib, quiet).await
        }
        Command::Mkdir { path } => mkdir(config, &path).await,
        Command::Rm { paths, yes } => rm(config, &paths, yes).await,
        Command::Mv { source, dest } => mv(config, &source, &dest).await,
        Command::Cp { source, dest } => cp(config, &source, &dest).await,
        Command::Rename { from, to } => rename(config, &from, &to).await,
        Command::Doctor => doctor(config).await,
        Command::Login {
            from_config,
            cookie,
            bduss,
            stoken,
            uid,
            name,
        } => {
            let _lock =
                crate::lock::InstanceLock::try_acquire(&config.paths.config_dir, "cli-login")?;
            login(config, from_config, cookie, bduss, stoken, uid, name).await
        }
        Command::Config {
            action: ConfigAction::Import { path },
        } => {
            let _lock = crate::lock::InstanceLock::try_acquire(
                &config.paths.config_dir,
                "cli-config-import",
            )?;
            config_command(config, ConfigAction::Import { path }).await
        }
        Command::Config {
            action: ConfigAction::Init,
        } => {
            let _lock = crate::lock::InstanceLock::try_acquire(
                &config.paths.config_dir,
                "cli-config-init",
            )?;
            config_command(config, ConfigAction::Init).await
        }
        Command::Config { action } => config_command(config, action).await,
    }
}

fn client(config: &RuntimeConfig) -> Result<BaiduClient> {
    BaiduClient::new(config).map_err(|err| anyhow!(err))
}

async fn whoami(config: RuntimeConfig, json_output: bool) -> Result<()> {
    let client = client(&config)?;
    let user = client.whoami().await?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&user)?);
    } else {
        println!("账号: {}", user.best_name());
        println!("百度名称: {}", user.baidu_name);
        println!("网盘名称: {}", user.netdisk_name);
        println!("uk: {}", user.uk);
        println!("VIP 类型: {}", user.vip_type);
        let stored_uid = config.account()?.uid;
        if stored_uid != 0 {
            println!("本地 uid: {stored_uid}");
        }
    }
    Ok(())
}

async fn quota(config: RuntimeConfig, json_output: bool) -> Result<()> {
    let client = client(&config)?;
    let quota = client.quota().await?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&quota)?);
    } else {
        println!("{}", quota.summary());
        println!("使用率: {:.1}%", quota.used_ratio() * 100.0);
    }
    Ok(())
}

async fn ls(config: RuntimeConfig, path: &str, json_output: bool, long: bool) -> Result<()> {
    let client = client(&config)?;
    let mut items = client.list_dir(path).await?;
    items.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    if json_output {
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }
    println!("{}  ({} 项)", path, items.len());
    for item in &items {
        let kind = if item.is_dir { "d" } else { "-" };
        let size = if item.is_dir {
            "-".to_string()
        } else {
            format_bytes(item.size)
        };
        if long {
            println!(
                "{kind} {:>10}  {}  {:>16}  {}",
                size,
                format_timestamp(item.mtime),
                item.fs_id,
                item.name
            );
        } else {
            println!(
                "{kind} {:>10}  {}  {}",
                size,
                format_timestamp(item.mtime),
                item.name
            );
        }
    }
    Ok(())
}

async fn search(
    config: RuntimeConfig,
    keyword: &str,
    dir: &str,
    recursive: bool,
    json_output: bool,
) -> Result<()> {
    let client = client(&config)?;
    let items = client.search(keyword, dir, recursive).await?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }
    println!("搜索 “{keyword}” @ {dir}: {} 项", items.len());
    for item in &items {
        let kind = if item.is_dir { "d" } else { "-" };
        println!(
            "{kind} {:>10}  {}  {}",
            if item.is_dir {
                "-".to_string()
            } else {
                format_bytes(item.size)
            },
            format_timestamp(item.mtime),
            item.path
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn download(
    config: RuntimeConfig,
    remote: &str,
    dest: Option<PathBuf>,
    overwrite: bool,
    rename: bool,
    no_verify: bool,
    connections: Option<usize>,
    retries: Option<usize>,
) -> Result<()> {
    let client = client(&config)?;
    let item = client.stat(remote).await?;
    let base = dest.unwrap_or_else(|| config.download_dir());
    let cwd = remote_parent(remote);
    let plan = plan_download(&client, &item, &cwd, &base).await?;
    let local_path = plan
        .files
        .first()
        .map(|file| file.local.clone())
        .or_else(|| plan.directories.first().cloned())
        .unwrap_or_else(|| base.clone());
    let progress = TransferProgress {
        id: 0,
        direction: Direction::Download,
        name: item.name.clone(),
        remote_path: remote.to_string(),
        local_path: local_path.clone(),
        total: plan.total_bytes,
        done: 0,
        status: TransferStatus::Running,
        speed_bps: 0.0,
        message: "准备下载".to_string(),
        files_done: 0,
        files_total: plan.files.len(),
    };
    let mut reporter = ProgressReporter::new(progress, headless_callback());
    let options = DownloadOptions {
        overwrite,
        rename_existing: rename,
        verify_md5: !no_verify,
        connections: connections
            .unwrap_or(config.file.transfer.download_connections)
            .clamp(1, 16),
        retries: retries.unwrap_or(config.file.transfer.retries).min(10),
    };
    match download_plan(
        &client,
        &plan,
        &options,
        &tokio_util::sync::CancellationToken::new(),
        &mut reporter,
    )
    .await
    {
        Ok(()) => {
            reporter.finish(TransferStatus::Completed, "下载完成");
            if item.is_dir {
                println!("已保存到: {}", base.display());
            } else {
                println!("已保存到: {}", local_path.display());
            }
            Ok(())
        }
        Err(err) => {
            let message = err.to_string();
            reporter.finish(TransferStatus::Failed, message.clone());
            Err(err)
        }
    }
}

async fn upload(
    config: RuntimeConfig,
    locals: Vec<PathBuf>,
    dest: Option<String>,
    chunk_mib: Option<u64>,
    quiet: bool,
) -> Result<()> {
    if locals.is_empty() {
        bail!("请至少指定一个本地文件或目录");
    }
    let client = client(&config)?;
    let remote_dir = dest.unwrap_or_else(|| config.file.last_remote_dir.clone());
    let options = UploadOptions {
        chunk_mib: chunk_mib.unwrap_or(config.file.transfer.upload_chunk_mib),
        retries: config.file.transfer.retries,
    };
    let cancel = tokio_util::sync::CancellationToken::new();
    for local in locals {
        let plan = plan_upload(&local, &remote_dir)?;
        let files = plan
            .iter()
            .filter(|(path, _, _)| !path.as_os_str().is_empty())
            .count();
        let total_bytes: u64 = plan
            .iter()
            .filter(|(path, _, _)| !path.as_os_str().is_empty())
            .filter_map(|(path, _, _)| std::fs::metadata(path).ok())
            .map(|meta| meta.len())
            .sum();
        let progress = TransferProgress {
            id: 0,
            direction: Direction::Upload,
            name: local
                .file_name()
                .map(|value| value.to_string_lossy().to_string())
                .unwrap_or_else(|| "upload".to_string()),
            remote_path: remote_dir.clone(),
            local_path: local.clone(),
            total: total_bytes,
            done: 0,
            status: TransferStatus::Running,
            speed_bps: 0.0,
            message: "准备上传".to_string(),
            files_done: 0,
            files_total: files,
        };
        let mut reporter = ProgressReporter::new(
            progress,
            if quiet {
                Arc::new(|_| {})
            } else {
                headless_callback()
            },
        );
        let result = async {
            let mut base_done = 0_u64;
            let mut file_index = 0usize;
            for (path, dir, _) in &plan {
                if path.as_os_str().is_empty() {
                    ensure_remote_dir(&client, dir).await?;
                    continue;
                }
                let size = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
                reporter.begin_file(base_done, file_index);
                let remote = upload_one(
                    &client,
                    UploadRequest {
                        local: path.as_path(),
                        remote_dir: dir,
                        state_dir: &config.paths.upload_state_dir,
                    },
                    &options,
                    &cancel,
                    &mut reporter,
                )
                .await?;
                if !quiet {
                    eprintln!("  → {remote}");
                }
                base_done = base_done.saturating_add(size);
                file_index += 1;
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        match result {
            Ok(()) => {
                reporter.finish(TransferStatus::Completed, "上传完成");
            }
            Err(err) => {
                let message = err.to_string();
                reporter.finish(TransferStatus::Failed, message.clone());
                return Err(err);
            }
        }
    }
    Ok(())
}

async fn mkdir(config: RuntimeConfig, path: &str) -> Result<()> {
    let client = client(&config)?;
    ensure_remote_dir(&client, path).await?;
    println!("已创建: {path}");
    Ok(())
}

async fn rm(config: RuntimeConfig, paths: &[String], yes: bool) -> Result<()> {
    if paths.is_empty() {
        bail!("请至少指定一个远程路径");
    }
    if !yes {
        bail!("删除操作不可恢复，请确认后添加 --yes");
    }
    let client = client(&config)?;
    client.remove(paths).await?;
    println!("已删除 {} 项", paths.len());
    Ok(())
}

async fn mv(config: RuntimeConfig, source: &str, dest: &str) -> Result<()> {
    let client = client(&config)?;
    client.move_paths(&[source.to_string()], dest).await?;
    println!("已移动: {source} → {dest}");
    Ok(())
}

async fn cp(config: RuntimeConfig, source: &str, dest: &str) -> Result<()> {
    let client = client(&config)?;
    client.copy_paths(&[source.to_string()], dest).await?;
    println!("已复制: {source} → {dest}");
    Ok(())
}

async fn rename(config: RuntimeConfig, from: &str, to: &str) -> Result<()> {
    let client = client(&config)?;
    client.rename(from, to).await?;
    println!("已重命名: {from} → {to}");
    Ok(())
}

async fn login(
    mut config: RuntimeConfig,
    from_config: Option<PathBuf>,
    cookie: Option<String>,
    bduss: Option<String>,
    stoken: Option<String>,
    uid: Option<u64>,
    name: Option<String>,
) -> Result<()> {
    if let Some(path) = from_config {
        let path = expand_tilde(&path);
        // Adopt in memory first: a failed online check must not replace the
        // working credentials.
        config.adopt_file(&path)?;
        let client = client(&config)?;
        let user = client
            .whoami()
            .await
            .map_err(|err| anyhow!("导入配置在线验证失败: {err}"))?;
        if let Some(account) = config.file.account.as_mut() {
            account.uid = user.uk;
            if account.name.is_empty() {
                account.name = user.best_name().to_string();
            }
        }
        let backup = config.save_with_backup()?;
        println!("已导入配置: {}", path.display());
        println!("{}", config.redacted_summary());
        if let Some(backup) = backup {
            println!("旧配置已备份: {}", backup.display());
        }
        return Ok(());
    }

    let interactive_tui = cookie.is_none()
        && bduss.is_none()
        && stoken.is_none()
        && uid.is_none()
        && name.is_none()
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal();
    if interactive_tui {
        crate::tui::install_panic_hook();
        return match crate::tui::login::run(config).await? {
            Some(config) => {
                println!("登录成功: {}", config.redacted_summary());
                println!("配置文件: {}", config.paths.config_file.display());
                Ok(())
            }
            None => {
                println!("已取消登录");
                Ok(())
            }
        };
    }

    let mut bduss = bduss.unwrap_or_default();
    let mut stoken = stoken.unwrap_or_default();
    if let Some(cookie) = cookie {
        let (cookie_bduss, cookie_stoken) = parse_cookie_input(&cookie);
        if bduss.trim().is_empty() {
            bduss = cookie_bduss.unwrap_or_default();
        }
        if stoken.trim().is_empty() {
            stoken = cookie_stoken.unwrap_or_default();
        }
    }

    // A pasted full cookie in either field is common; split it before use.
    let (parsed_bduss, parsed_stoken) = parse_cookie_input(&bduss);
    if let Some(value) = parsed_bduss {
        bduss = value;
    }
    if stoken.trim().is_empty() {
        if let Some(value) = parsed_stoken {
            stoken = value;
        }
    }

    if bduss.trim().is_empty() {
        bduss = read_prompt("BDUSS", true)?;
        let (parsed_bduss, parsed_stoken) = parse_cookie_input(&bduss);
        if let Some(value) = parsed_bduss {
            bduss = value;
        }
        if stoken.trim().is_empty() {
            if let Some(value) = parsed_stoken {
                stoken = value;
            }
        }
    }
    if stoken.trim().is_empty() {
        stoken = read_prompt("STOKEN", true)?;
        if let (Some(value), _) = parse_cookie_input(&stoken) {
            stoken = value;
        }
    }

    let bduss = normalize_secret(&bduss, "BDUSS")?;
    let stoken = normalize_secret(&stoken, "STOKEN")?;
    let account = Account {
        uid: uid.unwrap_or(0),
        name: name.unwrap_or_default(),
        bduss,
        stoken,
        baiduid: String::new(),
        access_token: String::new(),
    };
    config.set_account(account);
    config.save()?;
    println!("凭据已保存: {}", config.paths.config_file.display());
    verify_login(config).await
}

/// Validate the saved account and enrich it with the online name/UID. The
/// credential is already on disk, so a transient network failure does not
/// destroy it; it only reports that verification could not complete.
async fn verify_login(mut config: RuntimeConfig) -> Result<()> {
    let client = client(&config)?;
    match client.whoami().await {
        Ok(user) => {
            if let Some(account) = config.file.account.as_mut() {
                account.uid = user.uk;
                if account.name.is_empty() {
                    account.name = user.best_name().to_string();
                }
            }
            config.save()?;
            println!("登录成功: {}", config.redacted_summary());
            println!("配置文件: {}", config.paths.config_file.display());
            Ok(())
        }
        Err(err) => {
            eprintln!("凭据已保存，但在线验证失败: {err}");
            eprintln!("请检查 BDUSS/STOKEN 是否过期，或稍后重试 `pcstui whoami`。");
            Err(anyhow!("登录验证失败: {err}"))
        }
    }
}

/// Read one line from the terminal. Secrets are hidden with `stty -echo` when
/// stdin is a TTY; piped input is read verbatim, which keeps scripting easy.
fn read_prompt(label: &str, secret: bool) -> Result<String> {
    use std::io::BufRead;

    let terminal = std::io::stdin().is_terminal();
    if !terminal && !secret {
        // Non-interactive callers must provide the value explicitly.
        bail!("缺少 {label}：请使用 --bduss/--stoken/--cookie，或从配置文件导入");
    }
    eprint!("{label}: ");
    let _ = std::io::stderr().flush();
    let mut echo_off = false;
    if secret && terminal {
        echo_off = std::process::Command::new("stty")
            .arg("-echo")
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
    }
    let mut line = String::new();
    let result = std::io::stdin().lock().read_line(&mut line);
    if echo_off {
        let _ = std::process::Command::new("stty").arg("echo").status();
        eprintln!();
    }
    result?;
    let line = line.trim().to_string();
    if line.is_empty() {
        bail!("{label} 不能为空");
    }
    Ok(line)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

async fn config_command(config: RuntimeConfig, action: ConfigAction) -> Result<()> {
    match action {
        ConfigAction::Show => {
            println!(
                "配置文件: {} ({})",
                config.paths.config_file.display(),
                if config.paths.config_file.is_file() {
                    "存在"
                } else {
                    "不存在"
                }
            );
            println!("项目根目录: {}", config.paths.root.display());
            println!("下载目录: {}", config.download_dir().display());
            println!("临时目录: {}", config.paths.tmp_dir.display());
            println!("队列文件: {}", config.paths.queue_file.display());
            println!("上传断点: {}", config.paths.upload_state_dir.display());
            println!("远程目录: {}", config.file.last_remote_dir);
            println!("账号: {}", config.redacted_summary());
            println!("app_id: {}", config.file.app_id);
            println!("传输: {:?}", config.file.transfer);
            Ok(())
        }
        ConfigAction::Path => {
            println!("{}", config.paths.config_file.display());
            Ok(())
        }
        ConfigAction::Init => {
            if config.paths.config_file.exists() {
                bail!("配置文件已存在: {}", config.paths.config_file.display());
            }
            config.save()?;
            println!("已创建: {}", config.paths.config_file.display());
            println!("下一步: 在 TUI 登录页登录，或运行 `pcstui login`");
            Ok(())
        }
        ConfigAction::Export { path, redacted } => {
            let path = expand_tilde(&path.unwrap_or_else(|| {
                PathBuf::from(if redacted {
                    "pcstui-redacted.json"
                } else {
                    "pcstui-portable.json"
                })
            }));
            if same_file(&path, &config.paths.config_file) {
                bail!("导出路径不能是当前配置文件；请换一个路径");
            }
            config.export_to(&path, redacted)?;
            println!("已导出: {}", path.display());
            if redacted {
                println!("已移除账号凭据，可直接分享或作为模板。");
            } else {
                println!(
                    "包含账号凭据，请按 0600 权限保管；需要加密时请自行使用 age/gpg。换机后可在 TUI 配置面板（C）导入。"
                );
            }
            Ok(())
        }
        ConfigAction::Import { path } => {
            let path = expand_tilde(&path);
            let mut config = config;
            // Validate before replacing: a bad file must leave the active
            // config untouched. The old file is backed up on success.
            config.adopt_file(&path)?;
            let client = client(&config)?;
            let user = client
                .whoami()
                .await
                .map_err(|err| anyhow!("导入配置在线验证失败: {err}"))?;
            if let Some(account) = config.file.account.as_mut() {
                account.uid = user.uk;
                if account.name.is_empty() {
                    account.name = user.best_name().to_string();
                }
            }
            let backup = config.save_with_backup()?;
            println!("已导入: {}", path.display());
            println!("{}", config.redacted_summary());
            if let Some(backup) = backup {
                println!("旧配置已备份: {}", backup.display());
            }
            Ok(())
        }
        ConfigAction::SetDownloadDir { path } => {
            let mut config = config;
            config.file.download_dir = Some(path.clone());
            config.save()?;
            println!("下载目录已设置为: {}", path.display());
            Ok(())
        }
        ConfigAction::SetRemoteDir { path } => {
            if !path.starts_with('/') {
                bail!("远程目录必须以 / 开头");
            }
            let mut config = config;
            config.file.last_remote_dir = path.clone();
            config.save()?;
            println!("远程目录已设置为: {path}");
            Ok(())
        }
    }
}

async fn doctor(config: RuntimeConfig) -> Result<()> {
    println!("pcstui {} 诊断", crate::VERSION);
    println!("项目根目录: {}", config.paths.root.display());
    println!(
        "配置文件: {} ({})",
        config.paths.config_file.display(),
        if config.paths.config_file.is_file() {
            "存在"
        } else {
            "不存在"
        }
    );
    check(
        "配置文件权限",
        file_is_private(&config.paths.config_file),
        "应为 0600",
    );
    check(
        "配置目录权限",
        dir_is_private(&config.paths.config_dir),
        "应为 0700",
    );
    check(
        "下载目录可写",
        ensure_writable(&config.download_dir()),
        "检查磁盘空间/权限",
    );
    check(
        "上传断点目录可写",
        ensure_writable(&config.paths.upload_state_dir),
        "检查磁盘空间/权限",
    );
    println!(
        "外部工具: yazi={}, xdg-open={}",
        if command_exists("yazi") { "是" } else { "否" },
        if command_exists("xdg-open") {
            "是"
        } else {
            "否"
        }
    );

    if !config.has_account() {
        if !config.paths.config_file.is_file() {
            println!("配置文件不存在：启动 TUI 会进入登录页，或运行 `pcstui login`");
        }
        println!("账号: 未配置（启动 TUI 或运行 `pcstui login` 交互登录）");
        return Ok(());
    }
    println!("账号: {}", config.redacted_summary());
    let client = match BaiduClient::new(&config) {
        Ok(client) => client,
        Err(err) => {
            println!("客户端初始化失败: {err}");
            return Ok(());
        }
    };
    match client.whoami().await {
        Ok(user) => println!("在线账号: {} (uk={})", user.best_name(), user.uk),
        Err(err) => println!("在线账号检查失败: {err}"),
    }
    match client.quota().await {
        Ok(quota) => println!("容量: {}", quota.summary()),
        Err(err) => println!("容量检查失败: {err}"),
    }
    match client.list_dir("/").await {
        Ok(items) => println!("根目录读取: 正常（{} 项）", items.len()),
        Err(err) => println!("根目录读取失败: {err}"),
    }
    Ok(())
}

fn check(label: &str, ok: bool, hint: &str) {
    if ok {
        println!("✓ {label}");
    } else {
        println!("✗ {label}（{hint}）");
    }
}

#[cfg(unix)]
fn file_is_private(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|meta| meta.permissions().mode() & 0o077 == 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn file_is_private(_path: &Path) -> bool {
    true
}

#[cfg(unix)]
fn dir_is_private(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|meta| meta.permissions().mode() & 0o077 == 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn dir_is_private(_path: &Path) -> bool {
    true
}

fn ensure_writable(path: &Path) -> bool {
    if std::fs::create_dir_all(path).is_err() {
        return false;
    }
    let probe = path.join(".pcstui-write-test");
    match std::fs::write(&probe, b"ok") {
        Ok(()) => {
            let _ = std::fs::remove_file(probe);
            true
        }
        Err(_) => false,
    }
}

fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                let candidate = dir.join(name);
                candidate.is_file()
            })
        })
        .unwrap_or(false)
}

fn headless_callback() -> ProgressFn {
    Arc::new(|progress| {
        let terminal = progress.status.is_terminal();
        let mut stderr = std::io::stderr().lock();
        if terminal {
            let _ = writeln!(
                stderr,
                "\r[{}] {} — {}",
                progress.direction.label(),
                progress.name,
                progress.message
            );
            return;
        }
        let line = format!(
            "[{}] {} {:>5.1}%  {}/{}  {}  {}",
            progress.direction.label(),
            truncate(&progress.name, 28),
            progress.ratio() * 100.0,
            format_bytes(progress.done),
            format_bytes(progress.total),
            format_speed(progress.speed_bps),
            progress.message
        );
        if std::io::stderr().is_terminal() {
            let _ = write!(stderr, "\r\x1b[2K{line}");
        } else {
            let _ = writeln!(stderr, "{line}");
        }
        let _ = stderr.flush();
    })
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut result: String = text.chars().take(max.saturating_sub(1)).collect();
    result.push('…');
    result
}

/// Public helper used by the TUI's conflict dialog and tests.
pub fn default_remote_for(local: &Path, remote_dir: &str) -> String {
    let name = local
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "upload".to_string());
    remote_join(remote_dir, &name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_default_tui() {
        let cli = Cli::try_parse_from(["pcstui"]).unwrap();
        assert!(cli.command.is_none());
    }

    #[test]
    fn cli_parses_download_flags() {
        let cli = Cli::try_parse_from([
            "pcstui",
            "download",
            "/apps/bypy/a",
            "--dest",
            "/tmp",
            "--overwrite",
        ])
        .unwrap();
        match cli.command.unwrap() {
            Command::Download {
                remote,
                dest,
                overwrite,
                ..
            } => {
                assert_eq!(remote, "/apps/bypy/a");
                assert_eq!(dest.unwrap(), PathBuf::from("/tmp"));
                assert!(overwrite);
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn builds_default_remote_path() {
        assert_eq!(
            default_remote_for(Path::new("/tmp/hello.txt"), "/apps/bypy/saved"),
            "/apps/bypy/saved/hello.txt"
        );
    }

    #[test]
    fn truncates_unicode_safely() {
        assert_eq!(truncate("一二三四五", 3), "一二…");
    }
}
