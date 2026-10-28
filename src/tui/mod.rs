pub mod app;
pub(crate) mod config_panel;
pub(crate) mod login;
pub mod ui;

use std::io::{Stdout, stdout};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;
use tokio::time;

use crate::api::BaiduClient;
use crate::config::RuntimeConfig;
use crate::transfer::TransferEngine;
use app::App;

pub async fn run(mut config: RuntimeConfig) -> Result<()> {
    install_panic_hook();
    if !config.has_account() {
        match login::run(config.clone()).await? {
            Some(next) => config = next,
            None => return Ok(()),
        }
    }
    loop {
        let client = BaiduClient::new(&config).map_err(|err| anyhow!(err))?;
        let (engine, mut transfer_rx) = TransferEngine::new(client.clone(), config.clone());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::new(config.clone(), client, engine, tx);
        app.start();

        let mut terminal = init_terminal().context("初始化终端失败")?;
        let result = event_loop(&mut terminal, &mut app, &mut rx, &mut transfer_rx).await;
        app.engine.cancel_all();
        app.save_if_dirty();
        let restart = app.restart_requested;
        let relogin = app.relogin_requested;
        let next_config = app.config.clone();
        let restore = restore_terminal(&mut terminal);
        restore?;
        result?;

        if relogin {
            // Rebuild the client and engine after the login screen saves the
            // new credentials. Cancelling here leaves unfinished queue entries
            // intact, so the rebuilt app resumes them.
            match login::run(next_config.clone()).await? {
                Some(updated) => config = updated,
                None => config = next_config,
            }
            continue;
        }
        if restart {
            config = next_config;
            continue;
        }
        return Ok(());
    }
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    rx: &mut mpsc::UnboundedReceiver<app::AppEvent>,
    transfer_rx: &mut mpsc::UnboundedReceiver<crate::transfer::TransferEvent>,
) -> Result<()> {
    let mut events = EventStream::new();
    let mut ticker = time::interval(Duration::from_millis(120));
    ticker.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    loop {
        terminal
            .draw(|frame| ui::draw(frame, app))
            .context("渲染 TUI 失败")?;

        tokio::select! {
            event = events.next() => {
                match event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        app.handle_key(key);
                    }
                    Some(Ok(Event::Resize(_, _))) => {}
                    Some(Ok(_)) => {}
                    Some(Err(err)) => return Err(anyhow!("读取终端事件失败: {err}")),
                    None => break,
                }
            }
            Some(event) = rx.recv() => app.handle_event(event),
            Some(event) = transfer_rx.recv() => app.handle_transfer(event),
            _ = ticker.tick() => app.on_tick(),
        }

        if app.take_yazi_request() {
            restore_terminal(terminal)?;
            let result = launch_yazi(&app.local.cwd);
            *terminal = init_terminal().context("重新初始化终端失败")?;
            app.on_yazi_done(result);
        }

        if app.should_quit || app.restart_requested || app.relogin_requested {
            break;
        }
    }
    Ok(())
}

pub(crate) fn init_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    init_terminal_with_paste(false)
}

/// The login form needs bracketed paste so a whole cookie arrives as one
/// event. The main TUI keeps the old key-by-key behavior because it does not
/// have a paste handler.
pub(crate) fn init_login_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    init_terminal_with_paste(true)
}

fn init_terminal_with_paste(bracketed_paste: bool) -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("启用 raw mode 失败")?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen).context("进入备用屏幕失败")?;
    if bracketed_paste {
        execute!(out, EnableBracketedPaste).context("启用粘贴模式失败")?;
    }
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend).context("创建终端失败")?;
    terminal.clear().context("清屏失败")?;
    Ok(terminal)
}

pub(crate) fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        LeaveAlternateScreen
    )
    .ok();
    terminal.show_cursor().ok();
    Ok(())
}

pub(crate) fn install_panic_hook() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let original = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = execute!(stdout(), DisableBracketedPaste, LeaveAlternateScreen);
            original(info);
        }));
    });
}

fn launch_yazi(cwd: &Path) -> Result<(), String> {
    if !command_exists("yazi") {
        return Err("未找到 yazi，请先安装或在终端中手动使用".to_string());
    }
    let status = std::process::Command::new("yazi")
        .current_dir(cwd)
        .status()
        .map_err(|err| format!("启动 yazi 失败: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("yazi 退出状态: {status}"))
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
