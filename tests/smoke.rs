//! Optional live-account smoke test.
//!
//! Normal `cargo test` skips this test (`#[ignore]`). Run explicitly with:
//! `PCSTUI_SMOKE=1 cargo test --test smoke -- --ignored --nocapture`
//!
//! Optional env vars:
//! - `PCSTUI_CONFIG=/path/to/pcstui.json`
//! - `PCSTUI_SMOKE_SHARE='https://pan.baidu.com/s/1xxxx 提取码: abcd'`
//! - `PCSTUI_SMOKE_GROUP='<gid>'`

use std::path::PathBuf;

use pcstui::api::BaiduClient;
use pcstui::api::group::{list_group_shares, list_groups};
use pcstui::api::share::{open_share, parse_share_input};
use pcstui::config::RuntimeConfig;

#[tokio::test]
#[ignore = "requires live Baidu Netdisk credentials and network access (set PCSTUI_SMOKE=1)"]
async fn live_smoke() {
    if std::env::var("PCSTUI_SMOKE").as_deref() != Ok("1") {
        eprintln!("跳过：请设置 PCSTUI_SMOKE=1 后再运行");
        return;
    }

    let config_path = std::env::var_os("PCSTUI_CONFIG").map(PathBuf::from);
    let config = RuntimeConfig::load_from(config_path).expect("加载配置失败");
    let client = BaiduClient::new(&config).expect("初始化 BaiduClient 失败（请先登录）");

    let user = client.whoami().await.expect("whoami 失败");
    println!("whoami: {} (uk={})", user.best_name(), user.uk);

    let quota = client.quota().await.expect("quota 失败");
    println!("quota: {}", quota.summary());

    let root = client.list_dir("/").await.expect("list_dir(/) 失败");
    println!("list_dir(/): {} items", root.len());

    let groups = list_groups(&client).await.expect("list_groups 失败");
    println!("groups: {} 个", groups.len());
    let target_gid = std::env::var("PCSTUI_SMOKE_GROUP")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| groups.first().map(|g| g.gid.clone()));
    if let Some(gid) = target_gid {
        let shares = list_group_shares(&client, &gid)
            .await
            .expect("list_group_shares 失败");
        println!("group {gid} shares: {} 项", shares.len());
    }

    if let Ok(share_raw) = std::env::var("PCSTUI_SMOKE_SHARE") {
        if !share_raw.trim().is_empty() {
            let input = parse_share_input(&share_raw).expect("解析分享链接失败");
            let (session, items) = open_share(&client, &input).await.expect("打开分享失败");
            println!(
                "share '{}': shareid={}, uk={}, {} 项",
                session.title,
                session.shareid,
                session.uk,
                items.len()
            );
        }
    }
}
