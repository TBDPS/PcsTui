use std::collections::HashSet;
use std::io::SeekFrom;
use std::path::Path;
use std::time::Duration;

use chrono::Utc;
use reqwest::header::{COOKIE, RANGE, REFERER, USER_AGENT};
use reqwest::{Client, Response, StatusCode, Url};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::api::models::{ApiListItem, Quota, RemoteItem, UserInfo, remote_join, remote_parent};
use crate::api::sign;
use crate::config::{Account, RuntimeConfig};

const PAN_BASE: &str = "https://pan.baidu.com";
const PCS_BASE: &str = "https://pcs.baidu.com";

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("网络请求失败: {0}")]
    Network(#[from] reqwest::Error),
    #[error("URL 解析失败: {0}")]
    Url(#[from] url::ParseError),
    #[error("百度接口返回错误 {errno}: {message}{}", request_suffix(.request_id))]
    Remote {
        errno: i64,
        message: String,
        request_id: Option<String>,
        context: String,
    },
    #[error("响应解析失败（{context}）: {message}")]
    Parse { context: String, message: String },
    #[error("{0}")]
    Other(String),
}

fn request_suffix(request_id: &Option<String>) -> String {
    request_id
        .as_ref()
        .map(|id| format!("（request_id={id}）"))
        .unwrap_or_default()
}

impl ApiError {
    pub fn is_not_found(&self) -> bool {
        match self {
            ApiError::Remote { errno, message, .. } => {
                matches!(*errno, -9 | 31066 | 31297)
                    || message.contains("文件不存在")
                    || message.contains("路径不存在")
            }
            _ => false,
        }
    }

    pub fn is_already_exists(&self) -> bool {
        match self {
            ApiError::Remote { errno, message, .. } => {
                matches!(*errno, -8 | 31061 | 114514)
                    || message.contains("已存在")
                    || message.contains("already exists")
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PrecreateResult {
    #[serde(default)]
    pub return_type: i32,
    #[serde(default, rename = "uploadid")]
    pub upload_id: String,
}

#[derive(Clone)]
pub struct BaiduClient {
    http: Client,
    download_http: Client,
    account: Account,
    app_id: u32,
    pan_ua: String,
}

impl BaiduClient {
    pub fn new(config: &RuntimeConfig) -> Result<Self, ApiError> {
        let account = config
            .account()
            .map_err(|err| ApiError::Other(err.to_string()))?
            .clone();
        let pan_ua = config.file.pan_ua.clone();
        let cookie = account.cookie_header();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            COOKIE,
            cookie
                .parse()
                .map_err(|_| ApiError::Other("Cookie 含有非法字符".to_string()))?,
        );
        headers.insert(REFERER, "https://pan.baidu.com/".parse().unwrap());
        headers.insert(
            USER_AGENT,
            pan_ua
                .parse()
                .map_err(|_| ApiError::Other("User-Agent 含有非法字符".to_string()))?,
        );
        let http = Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::limited(5))
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            .build()?;
        // Signed download hosts are picky about browser-style user agents.
        // Keep the PCS UA for the cookie-authenticated fallback.
        let download_http = Client::builder()
            .user_agent(&config.file.user_agent)
            .redirect(reqwest::redirect::Policy::limited(10))
            .connect_timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self {
            http,
            download_http,
            account,
            app_id: config.file.app_id,
            pan_ua,
        })
    }

    pub fn account(&self) -> &Account {
        &self.account
    }

    pub fn app_id(&self) -> u32 {
        self.app_id
    }

    pub fn pan_ua(&self) -> &str {
        &self.pan_ua
    }

    /// A cookie-free client for signed `baidupcs.com` download URLs.
    pub fn download_client(&self) -> Client {
        self.download_http.clone()
    }

    pub async fn whoami(&self) -> Result<UserInfo, ApiError> {
        let mut url = Url::parse(&format!("{PAN_BASE}/rest/2.0/xpan/nas"))?;
        url.query_pairs_mut().append_pair("method", "uinfo");
        let value = self.get_value(url, "获取账号信息").await?;
        Ok(UserInfo {
            baidu_name: string_field(&value, "baidu_name"),
            netdisk_name: string_field(&value, "netdisk_name"),
            uk: u64_field(&value, "uk"),
            vip_type: value.get("vip_type").and_then(Value::as_u64).unwrap_or(0) as u32,
            avatar_url: string_field(&value, "avatar_url"),
        })
    }

    pub async fn quota(&self) -> Result<Quota, ApiError> {
        let mut url = Url::parse(&format!("{PAN_BASE}/api/quota"))?;
        url.query_pairs_mut()
            .append_pair("checkfree", "1")
            .append_pair("checkexpire", "1");
        let value = self.get_value(url, "获取网盘配额").await?;
        let total = u64_field(&value, "total");
        let used = u64_field(&value, "used");
        // The API's `free` field does not always equal `total - used`
        // (promotional/expiring capacity is accounted differently), so derive
        // the value we display to keep the header and doctor consistent.
        let free = total.saturating_sub(used);
        Ok(Quota { total, used, free })
    }

    /// List a remote directory, following the web API's 1000-entry pages.
    pub async fn list_dir(&self, path: &str) -> Result<Vec<RemoteItem>, ApiError> {
        let path = if path.trim().is_empty() { "/" } else { path };
        let mut items = Vec::new();
        let mut seen = HashSet::new();
        for page in 1..=2000 {
            let mut url = Url::parse(&format!("{PAN_BASE}/api/list"))?;
            url.query_pairs_mut()
                .append_pair("dir", path)
                .append_pair("order", "name")
                .append_pair("desc", "0")
                .append_pair("clienttype", "0")
                .append_pair("num", "1000")
                .append_pair("page", &page.to_string());
            let value = self.get_value(url, "获取目录列表").await?;
            let list = value
                .get("list")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if list.is_empty() {
                break;
            }
            let page_len = list.len();
            let mut added = 0usize;
            for entry in list {
                let api: ApiListItem =
                    serde_json::from_value(entry).map_err(|err| ApiError::Parse {
                        context: "解析目录条目".to_string(),
                        message: err.to_string(),
                    })?;
                let key = if api.fs_id != 0 {
                    format!("id:{}", api.fs_id)
                } else {
                    format!("path:{}", api.path)
                };
                if seen.insert(key) {
                    items.push(api.into());
                    added += 1;
                }
            }
            if page_len < 1000 || added == 0 {
                break;
            }
        }
        Ok(items)
    }

    /// Resolve a remote path through its parent directory. Baidu has no stable
    /// cross-client `stat` endpoint, and listing the parent is cheap enough
    /// for interactive use.
    pub async fn stat(&self, path: &str) -> Result<RemoteItem, ApiError> {
        let path = if path.trim().is_empty() { "/" } else { path };
        if path == "/" {
            return Ok(RemoteItem {
                name: "/".to_string(),
                path: "/".to_string(),
                is_dir: true,
                ..Default::default()
            });
        }
        let parent = remote_parent(path);
        let name = path.rsplit('/').next().unwrap_or(path);
        let items = self.list_dir(&parent).await?;
        items
            .into_iter()
            .find(|item| item.name == name || item.path == path)
            .ok_or_else(|| ApiError::Remote {
                errno: -9,
                message: format!("文件或目录不存在: {path}"),
                request_id: None,
                context: "获取文件信息".to_string(),
            })
    }

    pub async fn search(
        &self,
        keyword: &str,
        directory: &str,
        recursive: bool,
    ) -> Result<Vec<RemoteItem>, ApiError> {
        let mut url = Url::parse(&format!("{PAN_BASE}/rest/2.0/xpan/file"))?;
        url.query_pairs_mut()
            .append_pair("method", "search")
            .append_pair("key", keyword)
            .append_pair("dir", directory)
            .append_pair("recursion", if recursive { "1" } else { "0" })
            .append_pair("web", "1");
        let value = self.get_value(url, "搜索网盘").await?;
        let list = value
            .get("list")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut items = Vec::with_capacity(list.len());
        for entry in list {
            let api: ApiListItem =
                serde_json::from_value(entry).map_err(|err| ApiError::Parse {
                    context: "解析搜索结果".to_string(),
                    message: err.to_string(),
                })?;
            items.push(api.into());
        }
        Ok(items)
    }

    pub async fn mkdir(&self, path: &str) -> Result<(), ApiError> {
        let mut url = self.pcs_url("mkdir")?;
        url.query_pairs_mut().append_pair("path", path);
        let value = self.post_value(url, "创建目录", None).await?;
        check_remote(&value, "创建目录")?;
        Ok(())
    }

    pub async fn ensure_dir(&self, path: &str) -> Result<(), ApiError> {
        match self.mkdir(path).await {
            Ok(()) => Ok(()),
            Err(err) if err.is_already_exists() => Ok(()),
            Err(err) => Err(err),
        }
    }

    pub async fn remove(&self, paths: &[String]) -> Result<(), ApiError> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut url = self.pcs_url("delete")?;
        url.query_pairs_mut();
        let param = json!({
            "list": paths.iter().map(|path| json!({"path": path})).collect::<Vec<_>>()
        });
        let value = self
            .post_value(url, "删除文件", Some(("param", param.to_string())))
            .await?;
        check_remote(&value, "删除文件")?;
        Ok(())
    }

    pub async fn rename(&self, from: &str, to: &str) -> Result<(), ApiError> {
        self.cp_mv("move", &[json!({"from": from, "to": to})], "重命名")
            .await
    }

    pub async fn move_paths(&self, paths: &[String], target_dir: &str) -> Result<(), ApiError> {
        let list = paths
            .iter()
            .map(|path| {
                let name = path.rsplit('/').next().unwrap_or(path);
                json!({"from": path, "to": remote_join(target_dir, name)})
            })
            .collect::<Vec<_>>();
        self.cp_mv("move", &list, "移动文件").await
    }

    pub async fn copy_paths(&self, paths: &[String], target_dir: &str) -> Result<(), ApiError> {
        let list = paths
            .iter()
            .map(|path| {
                let name = path.rsplit('/').next().unwrap_or(path);
                json!({"from": path, "to": remote_join(target_dir, name)})
            })
            .collect::<Vec<_>>();
        self.cp_mv("copy", &list, "复制文件").await
    }

    async fn cp_mv(&self, method: &str, list: &[Value], context: &str) -> Result<(), ApiError> {
        if list.is_empty() {
            return Ok(());
        }
        let mut url = self.pcs_url(method)?;
        url.query_pairs_mut();
        let param = json!({ "list": list });
        let value = self
            .post_value(url, context, Some(("param", param.to_string())))
            .await?;
        check_remote(&value, context)?;
        Ok(())
    }

    /// Get a signed direct download URL from the Android-client endpoint.
    /// This endpoint was verified against the live API and does not need the
    /// brittle `pan.baidu.com/disk/home` signature scrape.
    pub async fn download_link(&self, path: &str) -> Result<Url, ApiError> {
        let time = Utc::now().timestamp();
        let (rand, dev_uid) =
            sign::locate_download_sign(self.account.uid, &self.account.bduss, time);
        let mut url = Url::parse(&format!("{PCS_BASE}/rest/2.0/pcs/file"))?;
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("ant", "1")
                .append_pair("check_blue", "1")
                .append_pair("es", "1")
                .append_pair("esl", "1")
                .append_pair("app_id", "250528")
                .append_pair("method", "locatedownload")
                .append_pair("path", path)
                .append_pair("ver", "4.0")
                .append_pair("clienttype", "17")
                .append_pair("channel", "0")
                .append_pair("apn_id", "1_0")
                .append_pair("freeisp", "0")
                .append_pair("queryfree", "0")
                .append_pair("use", "0")
                .append_pair("time", &time.to_string())
                .append_pair("rand", &rand)
                .append_pair("devuid", &dev_uid)
                .append_pair("cuid", &dev_uid);
        }
        let value = self
            .post_value(url, "获取下载链接", None)
            .await
            .map_err(|err| match err {
                ApiError::Remote {
                    errno,
                    message,
                    request_id,
                    ..
                } => ApiError::Remote {
                    errno,
                    message: format!("locatedownload: {message}"),
                    request_id,
                    context: "获取下载链接".to_string(),
                },
                other => other,
            })?;
        let urls = value
            .get("urls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut fallback = None;
        for entry in urls {
            let Some(raw) = entry.get("url").and_then(Value::as_str) else {
                continue;
            };
            let Ok(parsed) = Url::parse(raw) else {
                continue;
            };
            let encrypt = entry.get("encrypt").and_then(Value::as_i64).unwrap_or(0);
            if encrypt == 0 {
                return Ok(parsed);
            }
            fallback.get_or_insert(parsed);
        }
        // Encrypted URLs require the brittle pan-home signature scrape. The
        // legacy PCS download endpoint works with the account cookie and
        // supports ranges, so prefer it over an unusable encrypted URL.
        let _ = fallback;
        let mut url = self.pcs_url("download")?;
        url.query_pairs_mut().append_pair("path", path);
        Ok(url)
    }

    /// Request a closed byte range. Baidu's signed download hosts reject
    /// open-ended ranges for large files (HTTP 403), but accept closed ranges
    /// up to about 5 MiB. The PCS user agent and account cookie are the most
    /// reliable combination here; the browser-style client is kept as a
    /// fallback for hosts that behave differently.
    pub async fn download_get_range(
        &self,
        url: &Url,
        start: u64,
        end: u64,
    ) -> Result<Response, ApiError> {
        let build = |client: &Client| {
            client
                .get(url.clone())
                .header(REFERER, "https://pan.baidu.com/")
                .header(RANGE, format!("bytes={start}-{end}"))
        };
        let response = build(&self.http).send().await?;
        if response.status() == StatusCode::FORBIDDEN
            || response.status() == StatusCode::UNAUTHORIZED
        {
            return Ok(build(&self.download_http).send().await?);
        }
        Ok(response)
    }

    pub async fn download_get(&self, url: &Url, offset: Option<u64>) -> Result<Response, ApiError> {
        let build = |client: &Client| {
            let mut request = client.get(url.clone());
            request = request.header(REFERER, "https://pan.baidu.com/");
            if let Some(offset) = offset {
                request = request.header(RANGE, format!("bytes={offset}-"));
            }
            request
        };
        let response = build(&self.http).send().await?;
        if response.status() == StatusCode::FORBIDDEN
            || response.status() == StatusCode::UNAUTHORIZED
        {
            return Ok(build(&self.download_http).send().await?);
        }
        Ok(response)
    }

    /// Ask the download host for the real content MD5 of a remote file.
    ///
    /// Baidu's directory listing reports the MD5 of the block-list JSON for
    /// files assembled from uploaded parts, not the MD5 of the file content.
    /// A one-byte range request exposes the authoritative `Content-MD5`
    /// header without downloading the file again.
    pub async fn content_md5(&self, path: &str) -> Result<Option<String>, ApiError> {
        let url = self.download_link(path).await?;
        let response = self.download_get_range(&url, 0, 0).await?;
        if !response.status().is_success() {
            return Ok(None);
        }
        let value = response
            .headers()
            .get(reqwest::header::HeaderName::from_static("content-md5"))
            .and_then(|value| value.to_str().ok())
            .map(|value| value.trim().to_ascii_lowercase());
        if let Some(value) = value.filter(|value| is_md5(value)) {
            return Ok(Some(value));
        }
        // Some hosts mirror the content MD5 in ETag.
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.trim_matches('"').to_ascii_lowercase());
        Ok(etag.filter(|value| is_md5(value)))
    }

    pub async fn precreate(
        &self,
        path: &str,
        size: u64,
        content_md5: &str,
        slice_md5: &str,
        crc32: u32,
        block_list: &str,
    ) -> Result<PrecreateResult, ApiError> {
        let url = Url::parse(&format!("{PAN_BASE}/api/precreate"))?;
        let fields = vec![
            ("path", path.to_string()),
            ("size", size.to_string()),
            ("isdir", "0".to_string()),
            ("block_list", block_list.to_string()),
            ("autoinit", "1".to_string()),
            ("content-md5", content_md5.to_string()),
            ("slice-md5", slice_md5.to_string()),
            ("contentCrc32", crc32.to_string()),
            ("rtype", "2".to_string()),
        ];
        let value = self.post_form_value(url, "上传预创建", &fields).await?;
        let return_type = value
            .get("return_type")
            .and_then(Value::as_i64)
            .unwrap_or(0) as i32;
        let upload_id = value
            .get("uploadid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        Ok(PrecreateResult {
            return_type,
            upload_id,
        })
    }

    /// Stream one upload block with a multipart request. The file is read
    /// lazily so even a 64 MiB block does not inflate memory use.
    pub async fn upload_part(
        &self,
        upload_id: &str,
        path: &str,
        partseq: usize,
        partoffset: u64,
        local: &Path,
        length: u64,
    ) -> Result<String, ApiError> {
        let mut upload_url = Url::parse(&format!("{PCS_BASE}/rest/2.0/pcs/superfile2"))?;
        {
            let mut query = upload_url.query_pairs_mut();
            query
                .append_pair("app_id", &self.app_id.to_string())
                .append_pair("method", "upload")
                .append_pair("type", "tmpfile")
                .append_pair("path", path)
                .append_pair("partseq", &partseq.to_string())
                .append_pair("partoffset", &partoffset.to_string())
                .append_pair("uploadid", upload_id)
                .append_pair("vip", "1");
        }
        let mut file = tokio::fs::File::open(local)
            .await
            .map_err(|err| ApiError::Other(format!("打开上传文件失败: {err}")))?;
        file.seek(SeekFrom::Start(partoffset))
            .await
            .map_err(|err| ApiError::Other(format!("定位上传分片失败: {err}")))?;
        let file_name = local
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| "blob".to_string());
        let stream = ReaderStream::new(file.take(length));
        let body = reqwest::Body::wrap_stream(stream);
        let part = reqwest::multipart::Part::stream_with_length(body, length)
            .file_name(file_name)
            .mime_str("application/octet-stream")
            .map_err(ApiError::Network)?;
        let form = reqwest::multipart::Form::new().part("uploadedfile", part);
        let response = self.http.post(upload_url).multipart(form).send().await?;
        let value = self.parse_response(response, "上传分片").await?;
        let md5 = value
            .get("md5")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if md5.is_empty() {
            return Err(ApiError::Other("上传分片响应中没有 md5".to_string()));
        }
        Ok(md5)
    }

    pub async fn create_file(
        &self,
        upload_id: &str,
        path: &str,
        size: u64,
        block_list: &str,
    ) -> Result<(), ApiError> {
        let url = Url::parse(&format!("{PAN_BASE}/api/create"))?;
        let fields = vec![
            ("uploadid", upload_id.to_string()),
            ("path", path.to_string()),
            ("size", size.to_string()),
            ("isdir", "0".to_string()),
            ("rtype", "2".to_string()),
            ("block_list", block_list.to_string()),
            ("target_path", remote_parent(path)),
        ];
        let value = self.post_form_value(url, "合并上传分片", &fields).await?;
        check_remote(&value, "合并上传分片")?;
        Ok(())
    }

    fn pcs_url(&self, method: &str) -> Result<Url, ApiError> {
        let mut url = Url::parse(&format!("{PCS_BASE}/rest/2.0/pcs/file"))?;
        url.query_pairs_mut()
            .append_pair("app_id", &self.app_id.to_string())
            .append_pair("method", method);
        Ok(url)
    }

    async fn get_value(&self, url: Url, context: &str) -> Result<Value, ApiError> {
        let response = self.http.get(url).send().await?;
        self.parse_response(response, context).await
    }

    async fn post_value(
        &self,
        url: Url,
        context: &str,
        form_field: Option<(&str, String)>,
    ) -> Result<Value, ApiError> {
        let request = self.http.post(url);
        let response = match form_field {
            Some((key, value)) => {
                let form = reqwest::multipart::Form::new().text(key.to_string(), value);
                request.multipart(form).send().await?
            }
            None => request.send().await?,
        };
        self.parse_response(response, context).await
    }

    async fn post_form_value(
        &self,
        url: Url,
        context: &str,
        fields: &[(&str, String)],
    ) -> Result<Value, ApiError> {
        let response = self.http.post(url).form(fields).send().await?;
        self.parse_response(response, context).await
    }

    async fn parse_response(&self, response: Response, context: &str) -> Result<Value, ApiError> {
        let status = response.status();
        let text = response.text().await?;
        let value: Value = serde_json::from_str(&text).map_err(|err| ApiError::Parse {
            context: format!("{context}（HTTP {status}）"),
            message: format!("{err}: {}", truncate(&text, 300)),
        })?;
        if !status.is_success() {
            let errno = value.get("errno").and_then(Value::as_i64).unwrap_or(-1);
            let message = error_message(&value);
            return Err(ApiError::Remote {
                errno,
                message: format!("HTTP {status}: {message}"),
                request_id: value
                    .get("request_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                context: context.to_string(),
            });
        }
        check_remote(&value, context)?;
        Ok(value)
    }
}

fn check_remote(value: &Value, context: &str) -> Result<(), ApiError> {
    if let Some(errno) = value.get("errno").and_then(Value::as_i64) {
        if errno != 0 {
            return Err(ApiError::Remote {
                errno,
                message: error_message(value),
                request_id: value
                    .get("request_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                context: context.to_string(),
            });
        }
    }
    if let Some(status) = value.get("status").and_then(Value::as_i64) {
        if status != 0 {
            return Err(ApiError::Remote {
                errno: status,
                message: error_message(value),
                request_id: value
                    .get("request_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                context: context.to_string(),
            });
        }
    }
    if let Some(code) = value.get("error_code").and_then(Value::as_i64) {
        if code != 0 {
            return Err(ApiError::Remote {
                errno: code,
                message: error_message(value),
                request_id: value
                    .get("request_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                context: context.to_string(),
            });
        }
    }
    Ok(())
}

fn error_message(value: &Value) -> String {
    for key in ["errmsg", "error_msg", "message", "msg"] {
        if let Some(message) = value.get(key).and_then(Value::as_str) {
            if !message.trim().is_empty() {
                return message.to_string();
            }
        }
    }
    "未知错误".to_string()
}

fn string_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn u64_field(value: &Value, key: &str) -> u64 {
    value
        .get(key)
        .and_then(Value::as_u64)
        .or_else(|| {
            value
                .get(key)
                .and_then(Value::as_i64)
                .map(|v| v.max(0) as u64)
        })
        .unwrap_or(0)
}

fn is_md5(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

// Keep these imports used in tests / future fallbacks without warnings.
#[allow(dead_code)]
fn _status_ok(status: StatusCode) -> bool {
    status.is_success()
}

#[allow(dead_code)]
fn _deserialize<T: DeserializeOwned>(value: Value) -> Result<T, serde_json::Error> {
    serde_json::from_value(value)
}
