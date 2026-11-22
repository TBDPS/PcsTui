use serde_json::Value;
use url::Url;

use crate::api::client::{ApiError, BaiduClient, PAN_BASE};
use crate::api::models::{ApiListItem, RemoteItem, normalize_remote};
use crate::api::share::{parse_api_list, value_to_u64};

#[derive(Debug, Clone)]
pub struct GroupInfo {
    pub gid: String,
    pub name: String,
    pub member_count: u64,
    pub status: i64,
}

#[derive(Debug, Clone)]
pub struct GroupShareEntry {
    pub msg_id: String,
    pub from_uk: u64,
    pub ctime: i64,
    pub item: RemoteItem,
}

/// Fetch all groups the current account belongs to (`/mbox/group/list`).
pub async fn list_groups(client: &BaiduClient) -> Result<Vec<GroupInfo>, ApiError> {
    let mut groups = Vec::new();
    let mut start = 0usize;
    for _ in 0..50 {
        let mut url = Url::parse(&format!("{PAN_BASE}/mbox/group/list"))?;
        url.query_pairs_mut()
            .append_pair("type", "0")
            .append_pair("start", &start.to_string())
            .append_pair("limit", "100")
            .append_pair("clienttype", "0")
            .append_pair("web", "1");
        let value = client.get_value(url, "获取群组列表").await?;
        let (batch, has_more) = parse_group_list(&value);
        let count = batch.len();
        groups.extend(batch);
        if !has_more || count == 0 {
            break;
        }
        start += count;
    }
    Ok(groups)
}

pub fn parse_group_list(value: &Value) -> (Vec<GroupInfo>, bool) {
    let records = value
        .get("records")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let has_more = value
        .get("has_more")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        != 0;
    let mut groups = Vec::with_capacity(records.len());
    for record in records {
        let gid = match record.get("gid") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => continue,
        };
        if gid.is_empty() {
            continue;
        }
        let name = record
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(&gid)
            .to_string();
        let member_count = record.get("gnum").and_then(value_to_u64).unwrap_or(0);
        let status = record.get("status").and_then(Value::as_i64).unwrap_or(0);
        groups.push(GroupInfo {
            gid,
            name,
            member_count,
            status,
        });
    }
    (groups, has_more)
}

/// Fetch shared file/directory entries inside a group (`/mbox/group/listshare`).
pub async fn list_group_shares(
    client: &BaiduClient,
    gid: &str,
) -> Result<Vec<GroupShareEntry>, ApiError> {
    let entries = fetch_group_shares_with_type(client, gid, Some("2")).await?;
    if !entries.is_empty() {
        return Ok(entries);
    }
    // Some group file-library views omit `type` or use `type=1`.
    for fallback_type in [None, Some("1")] {
        if let Ok(fallback) = fetch_group_shares_with_type(client, gid, fallback_type).await {
            if !fallback.is_empty() {
                return Ok(fallback);
            }
        }
    }
    Ok(entries)
}

async fn fetch_group_shares_with_type(
    client: &BaiduClient,
    gid: &str,
    share_type: Option<&str>,
) -> Result<Vec<GroupShareEntry>, ApiError> {
    let mut entries = Vec::new();
    let mut to_msg_id: Option<String> = None;
    for _ in 0..40 {
        let mut url = Url::parse(&format!("{PAN_BASE}/mbox/group/listshare"))?;
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("gid", gid)
                .append_pair("limit", "50")
                .append_pair("desc", "1")
                .append_pair("clienttype", "0")
                .append_pair("web", "1");
            if let Some(t) = share_type {
                query.append_pair("type", t);
            }
            if let Some(cursor) = &to_msg_id {
                query.append_pair("to_msg_id", cursor);
            }
        }
        let value = client.get_value(url, "获取群组文件列表").await?;
        let top_keys: Vec<&str> = value
            .as_object()
            .map(|obj| obj.keys().map(String::as_str).collect())
            .unwrap_or_default();
        tracing::debug!(
            gid,
            share_type = ?share_type,
            ?top_keys,
            records_is_obj = value.get("records").is_some_and(|v| v.is_object()),
            records_is_arr = value.get("records").is_some_and(|v| v.is_array()),
            "mbox/group/listshare 响应结构"
        );
        let (batch, next_cursor, has_more) = parse_group_shares(&value)?;
        entries.extend(batch);
        if !has_more {
            break;
        }
        let Some(cursor) = next_cursor else {
            break;
        };
        if to_msg_id.as_deref() == Some(cursor.as_str()) {
            break;
        }
        to_msg_id = Some(cursor);
    }
    Ok(entries)
}

pub fn parse_group_shares(
    value: &Value,
) -> Result<(Vec<GroupShareEntry>, Option<String>, bool), ApiError> {
    let records_val = value.get("records");
    let records = records_val
        .and_then(|r| {
            r.as_array()
                .or_else(|| r.get("msg_list").and_then(Value::as_array))
                .or_else(|| r.get("list").and_then(Value::as_array))
        })
        .or_else(|| value.get("msg_list").and_then(Value::as_array))
        .or_else(|| value.get("list").and_then(Value::as_array))
        .cloned()
        .unwrap_or_default();
    let has_more = records_val
        .and_then(|r| r.get("has_more"))
        .or_else(|| value.get("has_more"))
        .and_then(Value::as_i64)
        .unwrap_or(0)
        != 0;
    let mut entries = Vec::new();
    let mut last_msg_id = None;
    for record in records {
        let msg_id = match record.get("msg_id").or_else(|| record.get("id")) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => String::new(),
        };
        if !msg_id.is_empty() {
            last_msg_id = Some(msg_id.clone());
        }
        let from_uk = record
            .get("from_uk")
            .or_else(|| record.get("uk"))
            .and_then(value_to_u64)
            .unwrap_or(0);
        let ctime = record
            .get("msg_ctime")
            .or_else(|| record.get("ctime"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if let Some(file_list) = record.get("file_list").and_then(Value::as_array) {
            for file_val in file_list {
                let api: ApiListItem =
                    serde_json::from_value(file_val.clone()).map_err(|err| ApiError::Parse {
                        context: "解析群组文件条目".to_string(),
                        message: err.to_string(),
                    })?;
                entries.push(GroupShareEntry {
                    msg_id: msg_id.clone(),
                    from_uk,
                    ctime,
                    item: api.into(),
                });
            }
        } else if record.get("fs_id").is_some() {
            let api: ApiListItem =
                serde_json::from_value(record).map_err(|err| ApiError::Parse {
                    context: "解析群组文件条目".to_string(),
                    message: err.to_string(),
                })?;
            entries.push(GroupShareEntry {
                msg_id,
                from_uk,
                ctime,
                item: api.into(),
            });
        }
    }
    Ok((entries, last_msg_id, has_more))
}

/// List a subdirectory inside a shared group folder (`/mbox/msg/shareinfo`,
/// falling back to `/mbox/msg/sessioninfo` if needed).
pub async fn list_group_dir(
    client: &BaiduClient,
    gid: &str,
    from_uk: u64,
    msg_id: &str,
    fs_id: u64,
    dir: &str,
) -> Result<Vec<RemoteItem>, ApiError> {
    let items = fetch_group_dir_shareinfo(client, gid, from_uk, msg_id, fs_id, None).await?;
    if !items.is_empty() {
        return Ok(items);
    }
    let normalized = normalize_remote(dir);
    if normalized != "/" {
        let with_dir =
            fetch_group_dir_shareinfo(client, gid, from_uk, msg_id, fs_id, Some(&normalized))
                .await?;
        if !with_dir.is_empty() {
            return Ok(with_dir);
        }
    }
    // Fallback to `/mbox/msg/sessioninfo` for older group share variants.
    fetch_group_dir_sessioninfo(client, gid, from_uk, msg_id, fs_id, &normalized).await
}

async fn fetch_group_dir_shareinfo(
    client: &BaiduClient,
    gid: &str,
    from_uk: u64,
    msg_id: &str,
    fs_id: u64,
    dir: Option<&str>,
) -> Result<Vec<RemoteItem>, ApiError> {
    let mut items = Vec::new();
    for page in 1..=100 {
        let mut url = Url::parse(&format!("{PAN_BASE}/mbox/msg/shareinfo"))?;
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("type", "2")
                .append_pair("gid", gid)
                .append_pair("from_uk", &from_uk.to_string())
                .append_pair("msg_id", msg_id)
                .append_pair("fs_id", &fs_id.to_string())
                .append_pair("page", &page.to_string())
                .append_pair("num", "100")
                .append_pair("clienttype", "0")
                .append_pair("web", "1");
            if let Some(dir) = dir {
                query.append_pair("dir", dir);
            }
        }
        let value = client
            .get_value_with_cookie(url, "列出群组目录", None)
            .await?;
        let top_keys: Vec<&str> = value
            .as_object()
            .map(|obj| obj.keys().map(String::as_str).collect())
            .unwrap_or_default();
        tracing::debug!(
            gid,
            msg_id,
            fs_id,
            dir = ?dir,
            page,
            ?top_keys,
            "mbox/msg/shareinfo 响应结构"
        );
        let (batch, has_more) = extract_group_dir_items(&value)?;
        let count = batch.len();
        items.extend(batch);
        if !has_more.unwrap_or(count >= 100) || count == 0 {
            break;
        }
    }
    Ok(items)
}

async fn fetch_group_dir_sessioninfo(
    client: &BaiduClient,
    gid: &str,
    from_uk: u64,
    msg_id: &str,
    fs_id: u64,
    normalized_dir: &str,
) -> Result<Vec<RemoteItem>, ApiError> {
    let mut items = Vec::new();
    let self_uk = client.account().uid;
    for page in 1..=100 {
        let mut url = Url::parse(&format!("{PAN_BASE}/mbox/msg/sessioninfo"))?;
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("type", "2")
                .append_pair("gid", gid)
                .append_pair("from_uk", &from_uk.to_string())
                .append_pair("to_uk", &self_uk.to_string())
                .append_pair("msg_id", msg_id)
                .append_pair("fs_id", &fs_id.to_string())
                .append_pair("page", &page.to_string())
                .append_pair("num", "100")
                .append_pair("clienttype", "0")
                .append_pair("web", "1");
            if normalized_dir != "/" {
                query.append_pair("dir", normalized_dir);
            }
        }
        let value = client
            .get_value_with_cookie(url, "列出群组目录", None)
            .await?;
        tracing::debug!(
            gid,
            msg_id,
            fs_id,
            dir = %normalized_dir,
            has_records = value.get("records").is_some(),
            has_list = value.get("list").is_some(),
            "mbox/msg/sessioninfo 响应结构"
        );
        let (batch, has_more) = extract_group_dir_items(&value)?;
        let count = batch.len();
        items.extend(batch);
        if !has_more.unwrap_or(count >= 100) || count == 0 {
            break;
        }
    }
    Ok(items)
}

pub fn extract_group_dir_items(value: &Value) -> Result<(Vec<RemoteItem>, Option<bool>), ApiError> {
    let records_val = value.get("records");
    let has_more = records_val
        .and_then(|r| r.get("has_more"))
        .or_else(|| value.get("has_more"))
        .and_then(|v| {
            v.as_bool()
                .or_else(|| v.as_i64().map(|n| n != 0))
        });
    let raw_list = value
        .get("list")
        .and_then(Value::as_array)
        .or_else(|| {
            records_val.and_then(|r| {
                r.as_array()
                    .or_else(|| r.get("list").and_then(Value::as_array))
                    .or_else(|| r.get("file_list").and_then(Value::as_array))
            })
        })
        .cloned()
        .unwrap_or_default();
    let items = parse_api_list(raw_list, "解析群组目录")?;
    Ok((items, has_more))
}

/// Transfer selected files (`fs_ids`) from a group message into `dest_dir`.
pub async fn transfer_group_files(
    client: &BaiduClient,
    gid: &str,
    from_uk: u64,
    msg_id: &str,
    fs_ids: &[u64],
    dest_dir: &str,
) -> Result<(), ApiError> {
    if fs_ids.is_empty() {
        return Ok(());
    }
    let dest = normalize_remote(dest_dir);
    let bdstoken = client.bdstoken().await?;
    let mut url = Url::parse(&format!("{PAN_BASE}/mbox/msg/transfer"))?;
    url.query_pairs_mut()
        .append_pair("bdstoken", &bdstoken)
        .append_pair("channel", "chunlei")
        .append_pair("web", "1")
        .append_pair("app_id", "250528")
        .append_pair("clienttype", "0");

    let fs_ids_json = serde_json::to_string(fs_ids).unwrap_or_else(|_| "[]".to_string());
    let fields = vec![
        ("from_uk", from_uk.to_string()),
        ("msg_id", msg_id.to_string()),
        ("gid", gid.to_string()),
        ("type", "2".to_string()),
        ("fs_ids", fs_ids_json),
        ("path", dest),
        ("ondup", "newcopy".to_string()),
    ];
    client
        .post_form_value_with_cookie(url, "转存群组文件", &fields, None)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_group_list_with_business_status() {
        let payload = json!({
            "errno": 0,
            "has_more": 0,
            "records": [
                { "gid": "889900", "name": "研发资料群", "gnum": 24, "status": 1 },
                { "gid": 776655, "name": "备份交流群", "gnum": 5, "status": 0 }
            ]
        });
        let (groups, has_more) = parse_group_list(&payload);
        assert!(!has_more);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].gid, "889900");
        assert_eq!(groups[0].name, "研发资料群");
        assert_eq!(groups[0].member_count, 24);
        assert_eq!(groups[1].gid, "776655");
    }

    #[test]
    fn parses_group_shares_into_entries() {
        let payload = json!({
            "errno": 0,
            "has_more": 1,
            "records": [
                {
                    "msg_id": "11223344",
                    "from_uk": "10001",
                    "msg_ctime": 1700001111,
                    "file_list": [
                        {
                            "fs_id": 5001,
                            "server_filename": "设计文档.docx",
                            "path": "/设计文档.docx",
                            "size": 4096,
                            "isdir": 0
                        }
                    ]
                }
            ]
        });
        let (entries, cursor, has_more) = parse_group_shares(&payload).unwrap();
        assert!(has_more);
        assert_eq!(cursor.as_deref(), Some("11223344"));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].msg_id, "11223344");
        assert_eq!(entries[0].from_uk, 10001);
        assert_eq!(entries[0].item.fs_id, 5001);
        assert_eq!(entries[0].item.name, "设计文档.docx");
    }

    #[test]
    fn parses_nested_records_msg_list_and_stringified_item_fields() {
        let payload = json!({
            "errno": 0,
            "records": {
                "has_more": 0,
                "msg_list": [
                    {
                        "msg_id": 998877,
                        "uk": 10002,
                        "ctime": 1700002222,
                        "file_list": [
                            {
                                "fs_id": "6002",
                                "server_filename": "群资料目录",
                                "path": "/群资料目录",
                                "size": "0",
                                "isdir": "1"
                            }
                        ]
                    }
                ]
            }
        });
        let (entries, cursor, has_more) = parse_group_shares(&payload).unwrap();
        assert!(!has_more);
        assert_eq!(cursor.as_deref(), Some("998877"));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].from_uk, 10002);
        assert_eq!(entries[0].item.fs_id, 6002);
        assert!(entries[0].item.is_dir);
    }

    #[test]
    fn extracts_shareinfo_directory_records() {
        let payload = json!({
            "errno": 0,
            "has_more": 0,
            "records": [
                {
                    "fs_id": 8801,
                    "server_filename": "子文件.pdf",
                    "path": "/父目录/子文件.pdf",
                    "size": 12345,
                    "isdir": 0
                }
            ]
        });
        let (items, has_more) = extract_group_dir_items(&payload).unwrap();
        assert_eq!(has_more, Some(false));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].fs_id, 8801);
        assert_eq!(items[0].name, "子文件.pdf");
    }
}
