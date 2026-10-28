use std::env;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const DEFAULT_APP_ID: u32 = 266_719;
pub const DEFAULT_PAN_UA: &str = "netdisk;P2SP;3.0.0.8;netdisk;11.12.3;ANG-AN00;android-android;10.0;JSbridge4.4.0;jointBridge;1.1.0;";
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_13_2) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/63.0.3239.132 Safari/537.36";
pub const DEFAULT_REMOTE_DIR: &str = "/";

#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
    pub config_dir: PathBuf,
    pub config_file: PathBuf,
    pub download_dir: PathBuf,
    pub tmp_dir: PathBuf,
    pub upload_state_dir: PathBuf,
    pub queue_file: PathBuf,
}

impl Paths {
    pub fn discover() -> Self {
        let root = project_root();
        let config_dir = env_path("PCSTUI_CONFIG_DIR").unwrap_or_else(|| root.join("config"));
        let config_file =
            env_path("PCSTUI_CONFIG").unwrap_or_else(|| config_dir.join("pcstui.json"));
        let config_dir = config_file
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or(config_dir);
        let download_dir =
            env_path("PCSTUI_DOWNLOAD_DIR").unwrap_or_else(|| root.join("downloads"));
        let tmp_dir = env_path("PCSTUI_TMP_DIR").unwrap_or_else(|| root.join("tmp"));
        let upload_state_dir = config_dir.join("upload-state");
        let queue_file = config_dir.join("queue.json");
        Self {
            root,
            config_dir,
            config_file,
            download_dir,
            tmp_dir,
            upload_state_dir,
            queue_file,
        }
    }
}

fn env_path(key: &str) -> Option<PathBuf> {
    env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Find the portable application root.
///
/// The binary is self-contained: when it lives in `<root>/target/...` or
/// `<root>/bin/`, `<root>` is used; otherwise the executable's own directory
/// is used. `PCSTUI_HOME` overrides everything, which is useful for read-only
/// install locations or tests.
pub fn project_root() -> PathBuf {
    if let Some(home) = env_path("PCSTUI_HOME") {
        return home;
    }
    if let Ok(exe) = env::current_exe() {
        let exe = fs::canonicalize(&exe).unwrap_or(exe);
        if let Some(parent) = exe.parent() {
            if parent.file_name().and_then(|n| n.to_str()) == Some("bin") {
                if let Some(root) = parent.parent() {
                    return root.to_path_buf();
                }
            }
            if let Some(target) = parent.parent() {
                if target.file_name().and_then(|n| n.to_str()) == Some("target") {
                    if let Some(root) = target.parent() {
                        return root.to_path_buf();
                    }
                }
            }
            // A standalone binary in an arbitrary directory owns its config
            // and downloads next to itself.
            if parent.join("Cargo.toml").is_file() || !parent.as_os_str().is_empty() {
                return parent.to_path_buf();
            }
        }
    }
    env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Account {
    /// Optional. The download signature accepts `0`, and `login` fills this
    /// from the online account when possible, so users never have to type it.
    #[serde(default)]
    pub uid: u64,
    #[serde(default)]
    pub name: String,
    pub bduss: String,
    pub stoken: String,
    #[serde(default)]
    pub baiduid: String,
    #[serde(default)]
    pub access_token: String,
}

/// Never expose credentials through `{:?}` logging. The serialized form still
/// contains them because the config file is the portable credential bundle.
impl fmt::Debug for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Account")
            .field("uid", &self.uid)
            .field("name", &self.name)
            .field("bduss", &mask(&self.bduss))
            .field("stoken", &mask(&self.stoken))
            .field("baiduid", &mask(&self.baiduid))
            .field("access_token", &mask(&self.access_token))
            .finish()
    }
}

impl Account {
    pub fn cookie_header(&self) -> String {
        let mut cookie = format!("BDUSS={}; STOKEN={}", self.bduss, self.stoken);
        if !self.baiduid.is_empty() {
            cookie.push_str("; BAIDUID=");
            cookie.push_str(&self.baiduid);
        }
        cookie
    }

    pub fn display_name(&self) -> &str {
        if self.name.is_empty() {
            "（未命名账号）"
        } else {
            &self.name
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferConfig {
    /// Range connections used by one large download.
    pub download_connections: usize,
    /// Maximum number of upload jobs running at the same time.
    pub upload_connections: usize,
    /// Maximum number of download jobs running at the same time.
    #[serde(default = "default_max_active_downloads")]
    pub max_active_downloads: usize,
    pub retries: usize,
    pub verify_md5: bool,
    /// 0 means auto (4/16/64 MiB depending on file size).
    pub upload_chunk_mib: u64,
}

impl Default for TransferConfig {
    fn default() -> Self {
        Self {
            download_connections: 4,
            upload_connections: 2,
            max_active_downloads: 3,
            retries: 3,
            verify_md5: true,
            upload_chunk_mib: 0,
        }
    }
}

impl TransferConfig {
    pub fn sanitized(mut self) -> Self {
        self.download_connections = self.download_connections.clamp(1, 16);
        self.upload_connections = self.upload_connections.clamp(1, 8);
        self.max_active_downloads = self.max_active_downloads.clamp(1, 8);
        self.retries = self.retries.clamp(0, 10);
        self.upload_chunk_mib = self.upload_chunk_mib.clamp(0, 64);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigFile {
    pub version: u32,
    #[serde(default)]
    pub account: Option<Account>,
    #[serde(default = "default_app_id")]
    pub app_id: u32,
    #[serde(default = "default_pan_ua")]
    pub pan_ua: String,
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
    #[serde(default)]
    pub download_dir: Option<PathBuf>,
    #[serde(default = "default_remote_dir")]
    pub last_remote_dir: String,
    #[serde(default)]
    pub transfer: TransferConfig,
}

fn default_max_active_downloads() -> usize {
    3
}

fn default_app_id() -> u32 {
    DEFAULT_APP_ID
}

fn default_pan_ua() -> String {
    DEFAULT_PAN_UA.to_string()
}

fn default_user_agent() -> String {
    DEFAULT_USER_AGENT.to_string()
}

fn default_remote_dir() -> String {
    DEFAULT_REMOTE_DIR.to_string()
}

impl Default for ConfigFile {
    fn default() -> Self {
        Self {
            version: 1,
            account: None,
            app_id: DEFAULT_APP_ID,
            pan_ua: DEFAULT_PAN_UA.to_string(),
            user_agent: DEFAULT_USER_AGENT.to_string(),
            download_dir: None,
            last_remote_dir: DEFAULT_REMOTE_DIR.to_string(),
            transfer: TransferConfig::default(),
        }
    }
}

impl ConfigFile {
    /// Read a saved `pcstui.json`. This is the portable import format; no
    /// external application or other project directory is consulted.
    pub fn read(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("读取配置失败: {}", path.display()))?;
        let file: Self = serde_json::from_str(&raw)
            .with_context(|| format!("解析配置失败: {}", path.display()))?;
        Ok(file.normalized())
    }

    pub fn normalized(mut self) -> Self {
        self.transfer = self.transfer.sanitized();
        if self.last_remote_dir.trim().is_empty() {
            self.last_remote_dir = DEFAULT_REMOTE_DIR.to_string();
        }
        self
    }

    pub fn has_account(&self) -> bool {
        self.account.as_ref().is_some_and(|account| {
            !account.bduss.trim().is_empty() && !account.stoken.trim().is_empty()
        })
    }

    pub fn account_or_err(&self) -> Result<&Account> {
        match &self.account {
            Some(account)
                if !account.bduss.trim().is_empty() && !account.stoken.trim().is_empty() =>
            {
                Ok(account)
            }
            Some(_) => {
                bail!("配置中的账号凭据不完整（BDUSS/STOKEN 为空）：请运行 `pcstui login` 重新登录")
            }
            None => bail!(
                "尚未配置账号：可在 TUI 登录页登录，或运行 `pcstui login --bduss <BDUSS> --stoken <STOKEN>`；也可用 `pcstui login --from-config <FILE>` 导入已保存的配置"
            ),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub paths: Paths,
    pub file: ConfigFile,
}

impl RuntimeConfig {
    pub fn load() -> Result<Self> {
        Self::load_from(None)
    }

    /// Load configuration, optionally overriding the config file path.
    /// The file is the only credential source; there is no automatic import.
    pub fn load_from(config_file: Option<PathBuf>) -> Result<Self> {
        let mut paths = Paths::discover();
        if let Some(file) = config_file {
            let file = expand_tilde(&file);
            paths.config_file = file.clone();
            if let Some(parent) = file.parent() {
                paths.config_dir = parent.to_path_buf();
                paths.upload_state_dir = parent.join("upload-state");
                paths.queue_file = parent.join("queue.json");
            }
        }
        Self::load_paths(paths)
    }

    fn load_paths(paths: Paths) -> Result<Self> {
        let file = if paths.config_file.is_file() {
            ConfigFile::read(&paths.config_file)?
        } else {
            ConfigFile::default()
        };
        let _ = fs::create_dir_all(&paths.upload_state_dir);
        set_dir_private(&paths.upload_state_dir);
        Ok(Self { paths, file })
    }

    pub fn account(&self) -> Result<&Account> {
        self.file.account_or_err()
    }

    pub fn has_account(&self) -> bool {
        self.file.has_account()
    }

    pub fn download_dir(&self) -> PathBuf {
        let dir = self
            .file
            .download_dir
            .clone()
            .unwrap_or_else(|| self.paths.download_dir.clone());
        let dir = expand_tilde(&dir);
        if dir.is_relative() {
            self.paths.root.join(dir)
        } else {
            dir
        }
    }

    pub fn save(&self) -> Result<()> {
        write_config_file(&self.paths.config_file, &self.file, true)
    }

    /// Write a portable copy of the current config. The default keeps the
    /// account so the file can be copied to another machine; `redacted`
    /// removes credentials for safe sharing.
    pub fn export_to(&self, path: &Path, redacted: bool) -> Result<()> {
        let mut file = self.file.clone().normalized();
        if let Some(dir) = file.download_dir.take() {
            // Relative paths keep working after the folder is copied.
            file.download_dir = match dir.strip_prefix(&self.paths.root) {
                Ok(relative) => Some(relative.to_path_buf()),
                Err(_) => Some(dir),
            };
        }
        if redacted {
            file.account = None;
        }
        write_config_file(path, &file, false)
    }

    /// Load another machine's config into memory without saving. The caller
    /// can validate the credentials first (TUI login does exactly that).
    pub fn adopt_file(&mut self, path: &Path) -> Result<()> {
        let mut file = ConfigFile::read(path)?;
        if !file.has_account() {
            bail!(
                "配置文件 {} 不包含完整账号（需要 BDUSS 和 STOKEN）；请先在其中登录，或使用 --redacted 导出的模板",
                path.display()
            );
        }
        if let Some(dir) = &file.download_dir {
            if let Ok(relative) = dir.strip_prefix(&self.paths.root) {
                file.download_dir = Some(relative.to_path_buf());
            }
        }
        self.file = file;
        Ok(())
    }

    /// Import a saved `pcstui.json` from another machine or backup. This is
    /// the portable path: the file contains the account and all settings.
    pub fn import_from(&mut self, path: &Path) -> Result<()> {
        self.adopt_file(path)?;
        self.save()
    }

    pub fn set_account(&mut self, account: Account) {
        self.file.account = Some(account);
    }

    pub fn redacted_summary(&self) -> String {
        match &self.file.account {
            Some(account) => format!(
                "{} (uid={}, BDUSS={}…, STOKEN={}…)",
                account.display_name(),
                account.uid,
                mask(&account.bduss),
                mask(&account.stoken)
            ),
            None => "未配置账号".to_string(),
        }
    }
}

fn write_config_file(path: &Path, file: &ConfigFile, private_dir: bool) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("创建配置目录失败: {}", parent.display()))?;
    if private_dir {
        set_dir_private(parent);
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("pcstui.json");
    let tmp = parent.join(format!(".{name}.tmp"));
    let data = serde_json::to_vec_pretty(file)?;
    {
        let mut options = fs::OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut handle = options
            .open(&tmp)
            .with_context(|| format!("写入配置失败: {}", tmp.display()))?;
        handle.write_all(&data)?;
        handle.write_all(b"\n")?;
        handle.sync_all()?;
    }
    set_file_private(&tmp);
    fs::rename(&tmp, path).with_context(|| format!("替换配置失败: {}", path.display()))?;
    Ok(())
}

/// Trim a pasted credential and reject accidental cookie syntax in a single
/// value. Kept next to `parse_cookie_input` so CLI and TUI behave the same.
pub fn normalize_secret(value: &str, key: &str) -> Result<String> {
    let mut value = value.trim().trim_matches('"').trim_matches('\'');
    if let Some(rest) = value.strip_prefix(key) {
        if let Some(rest) = rest.strip_prefix('=') {
            value = rest.trim();
        }
    }
    if value.is_empty() {
        bail!("{key} 不能为空");
    }
    if value.contains(|ch: char| ch.is_whitespace() || ch == ';') {
        bail!("{key} 含有空白或分号，请只粘贴值本身或完整 Cookie");
    }
    Ok(value.to_string())
}

/// Accept a raw value, `BDUSS=value`, or a full cookie string and return the
/// BDUSS/STOKEN pair. This is what both the CLI prompt and the TUI login form
/// use, so pasting a browser cookie works without manual splitting.
pub fn parse_cookie_input(raw: &str) -> (Option<String>, Option<String>) {
    let mut bduss = None;
    let mut stoken = None;
    for part in raw.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((key, value)) = part.split_once('=') {
            let value = value.trim().trim_matches('"').trim_matches('\'');
            if value.is_empty() {
                continue;
            }
            match key.trim().to_ascii_uppercase().as_str() {
                "BDUSS" => bduss = Some(value.to_string()),
                "STOKEN" => stoken = Some(value.to_string()),
                _ => {}
            }
        }
    }
    if bduss.is_none() && stoken.is_none() {
        let value = raw
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .trim_start_matches("BDUSS=")
            .trim();
        if !value.is_empty() && !value.contains(';') {
            bduss = Some(value.to_string());
        }
    }
    (bduss, stoken)
}

pub fn expand_tilde(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        return home_dir();
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    path.to_path_buf()
}

pub fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn mask(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 8 {
        "***".to_string()
    } else {
        let head: String = chars[..4].iter().collect();
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("{head}…{tail}")
    }
}

#[cfg(unix)]
fn set_file_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_file_private(_path: &Path) {}

#[cfg(unix)]
fn set_dir_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn set_dir_private(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_paths(root: &Path) -> Paths {
        Paths {
            root: root.to_path_buf(),
            config_dir: root.join("config"),
            config_file: root.join("config/pcstui.json"),
            download_dir: root.join("downloads"),
            tmp_dir: root.join("tmp"),
            upload_state_dir: root.join("config/upload-state"),
            queue_file: root.join("config/queue.json"),
        }
    }

    #[test]
    fn config_round_trips_and_debug_is_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pcstui.json");
        let mut file = ConfigFile::default();
        file.account = Some(Account {
            uid: 42,
            name: "tester".to_string(),
            bduss: "BDUSS_SECRET_1234".to_string(),
            stoken: "STOKEN_SECRET_5678".to_string(),
            baiduid: String::new(),
            access_token: String::new(),
        });
        write_config_file(&path, &file, true).unwrap();
        let loaded = ConfigFile::read(&path).unwrap();
        let account = loaded.account.as_ref().unwrap();
        assert_eq!(account.bduss, "BDUSS_SECRET_1234");
        assert_eq!(account.stoken, "STOKEN_SECRET_5678");
        let debug = format!("{account:?}");
        assert!(!debug.contains("SECRET"));
        assert!(debug.contains("BDUS"));
        assert!(debug.contains("STOK"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn redacted_export_has_no_account() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = ConfigFile::default();
        file.account = Some(Account {
            uid: 42,
            name: "tester".to_string(),
            bduss: "BDUSS_SECRET_1234".to_string(),
            stoken: "STOKEN_SECRET_5678".to_string(),
            baiduid: String::new(),
            access_token: String::new(),
        });
        let config = RuntimeConfig {
            paths: test_paths(dir.path()),
            file,
        };
        let export = dir.path().join("redacted.json");
        config.export_to(&export, true).unwrap();
        let imported = ConfigFile::read(&export).unwrap();
        assert!(imported.account.is_none());
        assert_eq!(imported.last_remote_dir, "/");
    }

    #[test]
    fn portable_export_imports_on_another_machine() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let mut file = ConfigFile::default();
        file.account = Some(Account {
            uid: 42,
            name: "tester".to_string(),
            bduss: "BDUSS_SECRET_1234".to_string(),
            stoken: "STOKEN_SECRET_5678".to_string(),
            baiduid: String::new(),
            access_token: String::new(),
        });
        file.download_dir = Some(source.path().join("downloads"));
        let config = RuntimeConfig {
            paths: test_paths(source.path()),
            file,
        };
        let export = source.path().join("portable.json");
        config.export_to(&export, false).unwrap();
        let exported = ConfigFile::read(&export).unwrap();
        assert_eq!(
            exported.download_dir.as_deref(),
            Some(Path::new("downloads"))
        );

        let mut imported = RuntimeConfig {
            paths: test_paths(target.path()),
            file: ConfigFile::default(),
        };
        imported.adopt_file(&export).unwrap();
        let account = imported.file.account.as_ref().unwrap();
        assert_eq!(account.bduss, "BDUSS_SECRET_1234");
        assert_eq!(
            imported.file.download_dir.as_deref(),
            Some(Path::new("downloads"))
        );
        assert_eq!(imported.download_dir(), target.path().join("downloads"));
    }

    #[test]
    fn redacted_export_is_rejected_as_import() {
        let dir = tempfile::tempdir().unwrap();
        let config = RuntimeConfig {
            paths: test_paths(dir.path()),
            file: ConfigFile::default(),
        };
        let export = dir.path().join("redacted.json");
        config.export_to(&export, true).unwrap();
        let mut imported = RuntimeConfig {
            paths: test_paths(dir.path()),
            file: ConfigFile::default(),
        };
        assert!(imported.adopt_file(&export).is_err());
    }

    #[test]
    fn incomplete_account_is_rejected() {
        let mut file = ConfigFile::default();
        file.account = Some(Account {
            uid: 0,
            name: String::new(),
            bduss: "only-bduss".to_string(),
            stoken: String::new(),
            baiduid: String::new(),
            access_token: String::new(),
        });
        assert!(!file.has_account());
        assert!(file.account_or_err().is_err());
    }

    #[test]
    fn account_without_uid_loads_as_zero() {
        let raw = r#"{"bduss":"BDUSS_VALUE","stoken":"STOKEN_VALUE"}"#;
        let account: Account = serde_json::from_str(raw).unwrap();
        assert_eq!(account.uid, 0);
        assert_eq!(account.bduss, "BDUSS_VALUE");
    }

    #[test]
    fn parses_raw_and_cookie_input() {
        assert_eq!(
            parse_cookie_input("BDUSS=abc; STOKEN=def; BAIDUID=ignored"),
            (Some("abc".to_string()), Some("def".to_string()))
        );
        assert_eq!(parse_cookie_input("abc"), (Some("abc".to_string()), None));
        assert_eq!(
            parse_cookie_input("BDUSS=abc"),
            (Some("abc".to_string()), None)
        );
        assert_eq!(parse_cookie_input("   "), (None, None));
    }

    #[test]
    fn default_remote_dir_is_root() {
        assert_eq!(DEFAULT_REMOTE_DIR, "/");
        assert_eq!(ConfigFile::default().last_remote_dir, "/");
    }

    #[test]
    fn masks_secrets() {
        assert_eq!(mask("abcdefghijklmnop"), "abcd…mnop");
        assert_eq!(mask("short"), "***");
        assert_eq!(mask("一二三四五六七八九十"), "一二三四…七八九十");
    }
}
