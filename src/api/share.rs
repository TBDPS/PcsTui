use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crate::api::client::{ApiError, BaiduClient, PAN_BASE};
use crate::api::models::{ApiListItem, RemoteItem, normalize_remote, remote_join};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareInput {
    pub shorturl: String,
    pub verify_surl: String,
    pub pwd: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ShareSession {
    pub shorturl: String,
    pub verify_surl: String,
    pub shareid: u64,
    pub uk: u64,
    pub title: String,
    pub randsk: String,
    pub pwd: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ShareTransferResult {
    pub saved_paths: Vec<String>,
}

/// Parse a Baidu Netdisk share URL or pasted share snippet (including
/// extraction codes like `提取码: abcd`, `?pwd=abcd`, or a trailing 4-char code).
pub fn parse_share_input(raw: &str) -> Result<ShareInput, ApiError> {
    let text = raw.trim();
    if text.is_empty() {
        return Err(ApiError::Other("分享链接不能为空".to_string()));
    }

    let url_token = extract_url_token(text).ok_or_else(|| {
        ApiError::Other(
            "未识别到百度网盘分享链接（示例: https://pan.baidu.com/s/1xxxx）".to_string(),
        )
    })?;
    let parsed = if url_token.starts_with("http://") || url_token.starts_with("https://") {
        Url::parse(&url_token)
    } else {
        Url::parse(&format!("https://{url_token}"))
    }
    .map_err(ApiError::Url)?;

    let mut pwd = parsed
        .query_pairs()
        .find(|(k, _)| k.eq_ignore_ascii_case("pwd"))
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty());

    let (shorturl, verify_surl) = extract_surl_pair(&parsed)?;

    if pwd.is_none() {
        pwd = extract_password_from_text(text, &url_token);
    }

    Ok(ShareInput {
        shorturl,
        verify_surl,
        pwd,
    })
}

fn extract_url_token(text: &str) -> Option<String> {
    for token in text.split_whitespace() {
        let cleaned = token.trim_matches(|ch: char| {
            matches!(ch, '<' | '>' | '"' | '\'' | '(' | ')' | '，' | ',' | '。' | ';')
        });
        if cleaned.contains("pan.baidu.com/") || cleaned.contains("yun.baidu.com/") {
            return Some(cleaned.to_string());
        }
    }
    None
}

fn extract_surl_pair(url: &Url) -> Result<(String, String), ApiError> {
    let path = url.path().trim_end_matches('/');
    if let Some(rest) = path.strip_prefix("/s/") {
        let token = rest.split('/').next().unwrap_or("").trim();
        if token.is_empty() {
            return Err(ApiError::Other("分享链接缺少标识符".to_string()));
        }
        let verify_surl = token.strip_prefix('1').unwrap_or(token).to_string();
        if verify_surl.is_empty() {
            return Err(ApiError::Other("分享链接格式无效".to_string()));
        }
        return Ok((token.to_string(), verify_surl));
    }
    if path == "/share/init" || path == "/wap/init" {
        if let Some((_, surl)) = url
            .query_pairs()
            .find(|(k, _)| k.eq_ignore_ascii_case("surl"))
        {
            let token = surl.trim();
            if !token.is_empty() {
                let verify_surl = token.strip_prefix('1').unwrap_or(token).to_string();
                let shorturl = if token.starts_with('1') {
                    token.to_string()
                } else {
                    format!("1{token}")
                };
                return Ok((shorturl, verify_surl));
            }
        }
    }
    Err(ApiError::Other(
        "暂不支持该分享链接格式，请使用 /s/1xxxx 或 /share/init?surl=xxxx 链接".to_string(),
    ))
}

fn decode_sekey(raw: &str) -> String {
    url::form_urlencoded::parse(format!("k={raw}").as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_else(|| raw.to_string())
}

fn extract_password_from_text(text: &str, url_token: &str) -> Option<String> {
    for marker in ["提取码", "密码", "pwd"] {
        if let Some(pos) = text.to_ascii_lowercase().find(marker) {
            let after = &text[pos + marker.len()..];
            let trimmed = after.trim_start_matches(|ch: char| {
                ch.is_whitespace() || matches!(ch, ':' | '：' | '=' | '为')
            });
            let code: String = trimmed
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric())
                .collect();
            if (4..=8).contains(&code.len()) {
                return Some(code);
            }
        }
    }
    // Check for a standalone 4-character alphanumeric token after the URL.
    if let Some(pos) = text.find(url_token) {
        let rest = &text[pos + url_token.len()..];
        for token in rest.split_whitespace() {
            let cleaned = token.trim_matches(|ch: char| !ch.is_ascii_alphanumeric());
            if cleaned.len() == 4 && cleaned.chars().all(|ch| ch.is_ascii_alphanumeric()) {
                return Some(cleaned.to_string());
            }
        }
    }
    None
}

/// Verify an extraction code and return `randsk` (the `BDCLND` / `sekey` value).
pub async fn verify_share_password(
    client: &BaiduClient,
    verify_surl: &str,
    pwd: &str,
) -> Result<String, ApiError> {
    let mut url = Url::parse(&format!("{PAN_BASE}/share/verify"))?;
    let now_ms = chrono::Utc::now().timestamp_millis().to_string();
    url.query_pairs_mut()
        .append_pair("surl", verify_surl)
        .append_pair("t", &now_ms)
        .append_pair("channel", "chunlei")
        .append_pair("web", "1")
        .append_pair("app_id", "250528")
        .append_pair("clienttype", "0");
    let fields = vec![
        ("pwd", pwd.trim().to_string()),
        ("vcode", String::new()),
        ("vcode_str", String::new()),
    ];
    let value = client
        .post_form_value_with_cookie(url, "校验分享提取码", &fields, None)
        .await
        .map_err(map_share_error)?;
    let randsk = value
        .get("randsk")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if randsk.is_empty() {
        return Err(ApiError::Parse {
            context: "校验分享提取码".to_string(),
            message: "响应缺少 randsk".to_string(),
        });
    }
    Ok(randsk)
}

/// Open a share link and list its root items.
/// Uses `share/wxlist` first (pure JSON, returns `shareid`/`uk`/`seckey`/`list`),
/// falling back to HTML metadata (`locals.mset` / `yunData`) + `share/list`.
pub async fn open_share(
    client: &BaiduClient,
    input: &ShareInput,
) -> Result<(ShareSession, Vec<RemoteItem>), ApiError> {
    let pwd = input.pwd.clone().unwrap_or_default();
    let mut randsk = if !pwd.is_empty() {
        verify_share_password(client, &input.verify_surl, &pwd).await?
    } else {
        String::new()
    };

    let mut wx_err: Option<ApiError> = None;
    for candidate in [&input.verify_surl, &input.shorturl] {
        match open_via_wxlist(client, input, candidate, &pwd, &randsk).await {
            Ok((session, items)) => return Ok((session, items)),
            Err(err) => {
                tracing::debug!(shorturl = %candidate, error = %err, "share/wxlist 尝试失败");
                if wx_err.is_none() {
                    wx_err = Some(err);
                }
            }
        }
    }

    // Fallback: load the share page HTML to read `locals.mset` / `yunData` metadata.
    let page_url = Url::parse(&format!("{PAN_BASE}/s/{}", input.shorturl))?;
    let extra_cookie = (!randsk.is_empty()).then(|| format!("BDCLND={randsk}"));
    let html = client
        .get_text_with_cookie(page_url, "读取分享页面", extra_cookie.as_deref())
        .await?;
    if (html.contains("/share/init") || html.contains("请输入提取码")) && randsk.is_empty() {
        return Err(ApiError::Other("该分享需要提取码".to_string()));
    }
    let meta = match parse_share_html_meta(&html) {
        Ok(meta) => meta,
        Err(html_err) => {
            if let Some(wx_err) = wx_err {
                return Err(ApiError::Other(format!(
                    "{wx_err}；页面回退解析失败: {html_err}"
                )));
            }
            return Err(html_err);
        }
    };
    if randsk.is_empty() && !meta.randsk.is_empty() {
        randsk = meta.randsk;
    }
    let session = ShareSession {
        shorturl: input.shorturl.clone(),
        verify_surl: input.verify_surl.clone(),
        shareid: meta.shareid,
        uk: meta.uk,
        title: if meta.title.is_empty() {
            input.shorturl.clone()
        } else {
            meta.title
        },
        randsk,
        pwd: input.pwd.clone(),
    };
    let items = if !meta.root_items.is_empty() {
        meta.root_items
    } else {
        list_share_dir(client, &session, "/").await?
    };
    Ok((session, items))
}

async fn open_via_wxlist(
    client: &BaiduClient,
    input: &ShareInput,
    shorturl_param: &str,
    pwd: &str,
    randsk: &str,
) -> Result<(ShareSession, Vec<RemoteItem>), ApiError> {
    let mut url = Url::parse(&format!("{PAN_BASE}/share/wxlist"))?;
    url.query_pairs_mut()
        .append_pair("channel", "weixin")
        .append_pair("version", "2.2.2")
        .append_pair("clienttype", "25")
        .append_pair("web", "1");
    let fields = vec![
        ("shorturl", shorturl_param.to_string()),
        ("dir", String::new()),
        ("root", "1".to_string()),
        ("pwd", pwd.to_string()),
        ("page", "1".to_string()),
        ("num", "1000".to_string()),
        ("order", "time".to_string()),
    ];
    let extra_cookie = (!randsk.is_empty()).then(|| format!("BDCLND={randsk}"));
    let value = client
        .post_form_value_with_cookie(url, "获取分享列表", &fields, extra_cookie.as_deref())
        .await
        .map_err(map_share_error)?;
    parse_wxlist_response(input, randsk, &value)
}

pub fn parse_wxlist_response(
    input: &ShareInput,
    existing_randsk: &str,
    value: &Value,
) -> Result<(ShareSession, Vec<RemoteItem>), ApiError> {
    let data = value.get("data").unwrap_or(value);
    let shareid = data
        .get("shareid")
        .or_else(|| data.get("share_id"))
        .and_then(value_to_u64)
        .unwrap_or(0);
    let uk = data.get("uk").and_then(value_to_u64).unwrap_or(0);
    if shareid == 0 || uk == 0 {
        return Err(ApiError::Parse {
            context: "解析分享信息".to_string(),
            message: "缺少 shareid 或 uk".to_string(),
        });
    }
    let seckey = data
        .get("seckey")
        .or_else(|| data.get("randsk"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(existing_randsk)
        .to_string();
    let title = data
        .get("title")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(&input.shorturl)
        .to_string();
    let list = data
        .get("list")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let items = parse_api_list(list, "解析分享列表")?;
    Ok((
        ShareSession {
            shorturl: input.shorturl.clone(),
            verify_surl: input.verify_surl.clone(),
            shareid,
            uk,
            title,
            randsk: seckey,
            pwd: input.pwd.clone(),
        },
        items,
    ))
}

/// List a directory inside an opened share link (`/share/list`).
pub async fn list_share_dir(
    client: &BaiduClient,
    session: &ShareSession,
    dir: &str,
) -> Result<Vec<RemoteItem>, ApiError> {
    let normalized = normalize_remote(dir);
    let is_root = normalized == "/";
    let mut items = Vec::new();
    let extra_cookie = (!session.randsk.is_empty()).then(|| format!("BDCLND={}", session.randsk));
    for page in 1..=200 {
        let mut url = Url::parse(&format!("{PAN_BASE}/share/list"))?;
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("shareid", &session.shareid.to_string())
                .append_pair("uk", &session.uk.to_string())
                .append_pair("order", "name")
                .append_pair("desc", "0")
                .append_pair("num", "1000")
                .append_pair("page", &page.to_string())
                .append_pair("root", if is_root { "1" } else { "0" })
                .append_pair("web", "1")
                .append_pair("channel", "chunlei")
                .append_pair("app_id", "250528")
                .append_pair("clienttype", "0");
            if !is_root {
                query.append_pair("dir", &normalized);
            }
            if !session.randsk.is_empty() {
                query.append_pair("sekey", &decode_sekey(&session.randsk));
            }
        }
        let value = client
            .get_value_with_cookie(url, "列出分享目录", extra_cookie.as_deref())
            .await
            .map_err(map_share_error)?;
        let list = value
            .get("list")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if list.is_empty() {
            break;
        }
        let page_len = list.len();
        items.extend(parse_api_list(list, "解析分享目录")?);
        if page_len < 1000 {
            break;
        }
    }
    Ok(items)
}

/// Transfer selected items (`fs_ids`) from a share link into `dest_dir` in the
/// user's own Netdisk.
pub async fn transfer_share(
    client: &BaiduClient,
    session: &ShareSession,
    fs_ids: &[u64],
    dest_dir: &str,
) -> Result<ShareTransferResult, ApiError> {
    if fs_ids.is_empty() {
        return Ok(ShareTransferResult {
            saved_paths: Vec::new(),
        });
    }
    let dest = normalize_remote(dest_dir);
    let bdstoken = client.bdstoken().await?;
    let mut url = Url::parse(&format!("{PAN_BASE}/share/transfer"))?;
    url.query_pairs_mut()
        .append_pair("shareid", &session.shareid.to_string())
        .append_pair("from", &session.uk.to_string())
        .append_pair("bdstoken", &bdstoken)
        .append_pair("ondup", "newcopy")
        .append_pair("async", "1")
        .append_pair("channel", "chunlei")
        .append_pair("web", "1")
        .append_pair("app_id", "250528")
        .append_pair("clienttype", "0");

    let fsidlist = serde_json::to_string(fs_ids).unwrap_or_else(|_| "[]".to_string());
    let mut fields = vec![
        ("fsidlist", fsidlist),
        ("path", dest.clone()),
    ];
    if !session.randsk.is_empty() {
        fields.push(("sekey", decode_sekey(&session.randsk)));
    }
    let extra_cookie = (!session.randsk.is_empty()).then(|| format!("BDCLND={}", session.randsk));
    let value = client
        .post_form_value_with_cookie(url, "转存分享文件", &fields, extra_cookie.as_deref())
        .await
        .map_err(map_share_error)?;
    Ok(parse_transfer_result(&value, &dest))
}

fn parse_transfer_result(value: &Value, dest: &str) -> ShareTransferResult {
    let mut saved_paths = Vec::new();
    if let Some(info) = value.get("info").and_then(Value::as_array) {
        for entry in info {
            if let Some(path) = entry.get("path").and_then(Value::as_str) {
                if path.starts_with('/') {
                    saved_paths.push(path.to_string());
                } else {
                    saved_paths.push(remote_join(dest, path));
                }
            }
        }
    }
    ShareTransferResult { saved_paths }
}

#[derive(Default)]
struct ShareHtmlMeta {
    shareid: u64,
    uk: u64,
    title: String,
    randsk: String,
    root_items: Vec<RemoteItem>,
}

fn parse_share_html_meta(html: &str) -> Result<ShareHtmlMeta, ApiError> {
    let mut meta = ShareHtmlMeta::default();
    for marker in [
        "locals.mset(",
        "yunData.setData(",
        "window.yunData =",
        "window.yunData=",
        "var yunData =",
        "var yunData=",
    ] {
        let mut search_from = 0usize;
        while let Some(pos) = html[search_from..].find(marker) {
            let rest = &html[search_from + pos + marker.len()..];
            if let Some(json_str) = extract_balanced_json(rest) {
                if let Ok(value) = serde_json::from_str::<Value>(json_str) {
                    apply_meta_value(&mut meta, &value);
                    if meta.shareid != 0 && meta.uk != 0 {
                        return Ok(meta);
                    }
                }
            }
            search_from += pos + marker.len();
        }
    }
    if meta.shareid == 0 {
        meta.shareid = extract_json_u64_field(html, "shareid").unwrap_or(0);
    }
    if meta.uk == 0 {
        meta.uk = extract_json_u64_field(html, "share_uk")
            .or_else(|| extract_json_u64_field(html, "uk"))
            .unwrap_or(0);
    }
    if meta.shareid == 0 || meta.uk == 0 {
        if html.contains("分享的文件已经被取消") || html.contains("啊哦，来晚了") {
            return Err(ApiError::Other("分享链接已失效或被取消".to_string()));
        }
        if html.contains("百度安全验证") || html.contains("wappass.baidu.com") {
            return Err(ApiError::Other("触发百度安全验证，请稍后重试或刷新 Cookie".to_string()));
        }
        return Err(ApiError::Parse {
            context: "解析分享页面".to_string(),
            message: format!(
                "未找到 shareid/uk（页面长度 {} 字节，请用 RUST_LOG=debug 查看详情）",
                html.len()
            ),
        });
    }
    Ok(meta)
}

fn apply_meta_value(meta: &mut ShareHtmlMeta, value: &Value) {
    if meta.shareid == 0 {
        meta.shareid = value
            .get("shareid")
            .or_else(|| value.get("share_id"))
            .and_then(value_to_u64)
            .unwrap_or(0);
    }
    if meta.uk == 0 {
        meta.uk = value
            .get("uk")
            .or_else(|| value.get("share_uk"))
            .and_then(value_to_u64)
            .unwrap_or(0);
    }
    if meta.title.is_empty() {
        meta.title = value
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
    }
    if meta.randsk.is_empty() {
        meta.randsk = value
            .get("seckey")
            .or_else(|| value.get("randsk"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
    }
    if meta.root_items.is_empty() {
        let list = value
            .get("file_list")
            .and_then(|fl| {
                fl.as_array()
                    .or_else(|| fl.get("list").and_then(Value::as_array))
            })
            .cloned();
        if let Some(list) = list {
            if let Ok(parsed) = parse_api_list(list, "解析分享页面文件列表") {
                meta.root_items = parsed;
            }
        }
    }
}

fn extract_json_u64_field(text: &str, key: &str) -> Option<u64> {
    let pattern = format!("\"{key}\"");
    let pos = text.find(&pattern)?;
    let rest = text[pos + pattern.len()..]
        .trim_start_matches(|ch: char| ch.is_whitespace() || ch == ':' || ch == '"');
    let digits: String = rest.chars().take_while(|ch| ch.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn extract_balanced_json(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escape = false;
    for (offset, ch) in text[start..].char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            match ch {
                '\\' => escape = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=start + offset]);
                }
            }
            _ => {}
        }
    }
    None
}

pub(crate) fn parse_api_list(list: Vec<Value>, context: &str) -> Result<Vec<RemoteItem>, ApiError> {
    let mut items = Vec::with_capacity(list.len());
    for entry in list {
        let api: ApiListItem = serde_json::from_value(entry).map_err(|err| ApiError::Parse {
            context: context.to_string(),
            message: err.to_string(),
        })?;
        items.push(api.into());
    }
    Ok(items)
}

#[derive(Deserialize)]
#[serde(untagged)]
enum U64OrStr {
    Num(u64),
    Str(String),
}

pub(crate) fn value_to_u64(value: &Value) -> Option<u64> {
    match serde_json::from_value::<U64OrStr>(value.clone()).ok()? {
        U64OrStr::Num(v) => Some(v),
        U64OrStr::Str(s) => s.trim().parse().ok(),
    }
}

fn map_share_error(err: ApiError) -> ApiError {
    match err {
        ApiError::Remote {
            errno,
            message,
            request_id,
            context,
        } => {
            let friendly = match errno {
                -9 | -12 => "提取码错误或分享已失效".to_string(),
                -7 | -10 => "分享链接已过期或被取消".to_string(),
                -62 => "触发百度验证码限制，请稍后重试".to_string(),
                12 | 120 => "转存文件数量超出限制或空间不足".to_string(),
                4 => "分享链接不存在".to_string(),
                _ => message,
            };
            ApiError::Remote {
                errno,
                message: friendly,
                request_id,
                context,
            }
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_standard_and_snippet_share_links() {
        let s1 = parse_share_input("https://pan.baidu.com/s/1AbCdEfG?pwd=xy12").unwrap();
        assert_eq!(s1.shorturl, "1AbCdEfG");
        assert_eq!(s1.verify_surl, "AbCdEfG");
        assert_eq!(s1.pwd.as_deref(), Some("xy12"));

        let s2 = parse_share_input(
            "链接: https://pan.baidu.com/s/1HelloWorld 提取码: 9a8b 复制这段内容后打开",
        )
        .unwrap();
        assert_eq!(s2.shorturl, "1HelloWorld");
        assert_eq!(s2.verify_surl, "HelloWorld");
        assert_eq!(s2.pwd.as_deref(), Some("9a8b"));

        let s3 = parse_share_input("https://pan.baidu.com/share/init?surl=ZyxWvU").unwrap();
        assert_eq!(s3.shorturl, "1ZyxWvU");
        assert_eq!(s3.verify_surl, "ZyxWvU");
        assert!(s3.pwd.is_none());

        let s4 = parse_share_input("https://yun.baidu.com/s/1QuickTest q1w2").unwrap();
        assert_eq!(s4.pwd.as_deref(), Some("q1w2"));
    }

    #[test]
    fn parses_wxlist_response_into_session_and_items() {
        let input = ShareInput {
            shorturl: "1TestShare".to_string(),
            verify_surl: "TestShare".to_string(),
            pwd: Some("1234".to_string()),
        };
        let payload = json!({
            "errno": 0,
            "data": {
                "shareid": "99887766",
                "uk": 123456,
                "seckey": "RANDSK_TOKEN",
                "title": "资料合集",
                "list": [
                    {
                        "fs_id": 42,
                        "server_filename": "手册.pdf",
                        "path": "/手册.pdf",
                        "size": 1024,
                        "isdir": 0,
                        "server_mtime": 1700000000
                    },
                    {
                        "fs_id": 43,
                        "server_filename": "子目录",
                        "path": "/子目录",
                        "size": 0,
                        "isdir": 1
                    }
                ]
            }
        });
        let (session, items) = parse_wxlist_response(&input, "", &payload).unwrap();
        assert_eq!(session.shareid, 99887766);
        assert_eq!(session.uk, 123456);
        assert_eq!(session.randsk, "RANDSK_TOKEN");
        assert_eq!(session.title, "资料合集");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "手册.pdf");
        assert!(items[1].is_dir);
    }

    #[test]
    fn parses_locals_mset_html_metadata() {
        let html = r#"<script>locals.mset({"shareid":555,"uk":666,"title":"分享包","file_list":[{"fs_id":9,"server_filename":"a.zip","path":"/a.zip","size":2048,"isdir":0}]});</script>"#;
        let meta = parse_share_html_meta(html).unwrap();
        assert_eq!(meta.shareid, 555);
        assert_eq!(meta.uk, 666);
        assert_eq!(meta.title, "分享包");
        assert_eq!(meta.root_items.len(), 1);
        assert_eq!(meta.root_items[0].name, "a.zip");
    }

    #[test]
    fn parses_yundata_setdata_and_nested_file_list() {
        let html = r#"<script>yunData.setData({"shareid":"777","share_uk":"888","file_list":{"list":[{"fs_id":"10","server_filename":"b.pdf","path":"/b.pdf","size":"4096","isdir":"0"}]}});</script>"#;
        let meta = parse_share_html_meta(html).unwrap();
        assert_eq!(meta.shareid, 777);
        assert_eq!(meta.uk, 888);
        assert_eq!(meta.root_items.len(), 1);
        assert_eq!(meta.root_items[0].fs_id, 10);
        assert_eq!(meta.root_items[0].size, 4096);
        assert_eq!(decode_sekey("abc%2Bdef%3D"), "abc+def=");
    }
}
