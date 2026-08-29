use std::{
    collections::HashMap,
    io::ErrorKind,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use chrono::Utc;
use reqwest::{StatusCode, Url};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::{sync::Mutex, task::JoinHandle, time::sleep};
use tracing::{info, warn};
use uuid::Uuid;

use crate::{
    cluster::{AppState, auth_headers},
    models::{ActionResponse, MetricsSnapshot, ShutdownRequest, SpeedtestResult},
    power,
};

const USERINFO_RATE_LIMIT: Duration = Duration::from_secs(10);
const MONITOR_INTERVAL: Duration = Duration::from_secs(1);
const MONITOR_MAX_DURATION: Duration = Duration::from_secs(10 * 60);
const BAR_WIDTH: usize = 12;
const DEFAULT_SHUTDOWN_DELAY_SECONDS: u64 = 0;
const TELEGRAM_OFFSET_FILE: &str = "telegram-offset";

const TELEGRAM_BOT_COMMANDS: &[BotCommand] = &[
    BotCommand {
        command: "status",
        description: "Fleet status",
    },
    BotCommand {
        command: "leader",
        description: "Current leader",
    },
    BotCommand {
        command: "monitor",
        description: "Live host metrics",
    },
    BotCommand {
        command: "speedtest",
        description: "Internet or peer speed",
    },
    BotCommand {
        command: "on",
        description: "Wake a host",
    },
    BotCommand {
        command: "off",
        description: "Shut down a host",
    },
    BotCommand {
        command: "update",
        description: "Update nodes",
    },
    BotCommand {
        command: "screenshot",
        description: "Get a screenshot",
    },
    BotCommand {
        command: "userinfo",
        description: "Show Telegram IDs",
    },
    BotCommand {
        command: "help",
        description: "Command help",
    },
];

#[derive(Debug, Serialize)]
struct BotCommand {
    command: &'static str,
    description: &'static str,
}

#[derive(Debug)]
struct TelegramRuntime {
    bot_username: Option<String>,
    monitors: Mutex<HashMap<String, MonitorHandle>>,
    userinfo_last_seen: Mutex<HashMap<i64, Instant>>,
    speedtest_running: Mutex<bool>,
    screenshot_running: Mutex<bool>,
    update_running: Mutex<bool>,
}

impl TelegramRuntime {
    fn new(bot_username: Option<String>) -> Self {
        Self {
            bot_username,
            monitors: Mutex::new(HashMap::new()),
            userinfo_last_seen: Mutex::new(HashMap::new()),
            speedtest_running: Mutex::new(false),
            screenshot_running: Mutex::new(false),
            update_running: Mutex::new(false),
        }
    }
}

#[derive(Debug)]
struct MonitorHandle {
    chat_id: i64,
    message_id: i64,
    handle: JoinHandle<()>,
}

pub async fn run(state: Arc<AppState>) -> anyhow::Result<()> {
    let Some(token) = state.config.telegram_token() else {
        return Ok(());
    };
    if state.config.telegram_chat_ids().is_empty() {
        warn!("TELEGRAM_CHAT_ID is not set; only /userinfo will be accepted");
    }

    let client = TelegramClient::new(token, state.config.telegram.polling_timeout_seconds)?;
    let bot_username = client
        .get_me()
        .await
        .map(|me| me.username)
        .inspect_err(|err| warn!(error = %err, "failed to fetch Telegram bot identity"))
        .ok()
        .flatten();
    if let Err(err) = client.set_my_commands().await {
        warn!(error = %err, "failed to register Telegram bot commands");
    }
    let runtime = Arc::new(TelegramRuntime::new(bot_username));
    let mut offset = load_update_offset(&state).await?;

    tokio::spawn(alert_loop(client.clone(), Arc::clone(&state)));

    loop {
        if !state.is_leader().await {
            sleep(Duration::from_secs(2)).await;
            continue;
        }

        if offset.is_none() {
            offset = initialize_update_offset(&client, &state).await?;
            if !state.is_leader().await {
                continue;
            }
        }

        match client
            .get_updates(offset, state.config.telegram.polling_timeout_seconds)
            .await
        {
            Ok(updates) => {
                for update in updates {
                    if !state.is_leader().await {
                        break;
                    }

                    let next_offset = update.update_id + 1;
                    if let Err(err) = client.commit_update(next_offset).await {
                        warn!(
                            update_id = update.update_id,
                            error = %err,
                            "failed to commit Telegram update before handling"
                        );
                        continue;
                    }
                    store_update_offset(&state, next_offset).await?;
                    offset = Some(next_offset);

                    if !state.is_leader().await {
                        continue;
                    }

                    if let Some(message) = update.message {
                        handle_message(&client, Arc::clone(&runtime), Arc::clone(&state), message)
                            .await;
                    }
                    if let Some(callback) = update.callback_query {
                        handle_callback(
                            &client,
                            Arc::clone(&runtime),
                            Arc::clone(&state),
                            callback,
                        )
                        .await;
                    }
                }
            }
            Err(err) => {
                if is_telegram_poll_conflict(&err) {
                    let backoff = telegram_conflict_backoff(state.config.node.priority);
                    warn!(
                        error = %err,
                        backoff_seconds = backoff.as_secs(),
                        "another Telegram poller is active; backing off"
                    );
                    sleep(backoff).await;
                } else {
                    warn!(error = %err, "Telegram polling failed");
                    sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }
}

async fn alert_loop(client: TelegramClient, state: Arc<AppState>) {
    loop {
        sleep(Duration::from_secs(1)).await;
        if !state.is_leader().await {
            continue;
        }

        let chat_ids = state.config.telegram_chat_ids();
        if chat_ids.is_empty() {
            continue;
        }

        let mut retry = Vec::new();
        for event in state.drain_alerts().await {
            let text = event.render();
            let mut failed = false;
            for chat_id in &chat_ids {
                if let Err(err) = client.send_message(*chat_id, &text).await {
                    failed = true;
                    warn!(error = %err, "failed to send Telegram alert");
                }
            }
            if failed {
                retry.push(event);
            }
        }
        if !retry.is_empty() {
            state.requeue_alerts(retry).await;
        }
    }
}

async fn initialize_update_offset(
    client: &TelegramClient,
    state: &Arc<AppState>,
) -> anyhow::Result<Option<i64>> {
    let updates = client.get_updates(None, 0).await?;
    let Some(next_offset) = updates.iter().map(|update| update.update_id + 1).max() else {
        return Ok(None);
    };
    client.commit_update(next_offset).await?;
    store_update_offset(state, next_offset).await?;
    Ok(Some(next_offset))
}

async fn load_update_offset(state: &Arc<AppState>) -> anyhow::Result<Option<i64>> {
    let path = telegram_offset_path(state);
    match tokio::fs::read_to_string(&path).await {
        Ok(text) => text
            .trim()
            .parse::<i64>()
            .map(Some)
            .with_context(|| format!("invalid Telegram offset in {}", path.display())),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
    }
}

async fn store_update_offset(state: &Arc<AppState>, next_offset: i64) -> anyhow::Result<()> {
    let dir = &state.config.cluster.state_dir;
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("failed to create {}", dir.display()))?;
    let path = telegram_offset_path(state);
    tokio::fs::write(&path, format!("{next_offset}\n"))
        .await
        .with_context(|| format!("failed to write {}", path.display()))
}

fn telegram_offset_path(state: &Arc<AppState>) -> PathBuf {
    state.config.cluster.state_dir.join(TELEGRAM_OFFSET_FILE)
}

async fn handle_message(
    client: &TelegramClient,
    runtime: Arc<TelegramRuntime>,
    state: Arc<AppState>,
    message: Message,
) {
    let Some(text) = message.text.as_deref() else {
        return;
    };
    let Some(parsed_command) = command_name(text, runtime.bot_username.as_deref()) else {
        return;
    };
    let command = parsed_command.name;
    let chat_id = message.chat.id;

    if command == "/userinfo" {
        let response = render_userinfo(&state, Arc::clone(&runtime), &message).await;
        if let Err(err) = client.send_message(chat_id, &response).await {
            warn!(error = %err, "failed to send /userinfo response");
        }
        return;
    }

    if !is_authorized(&state, chat_id) {
        warn!(chat_id, "rejected unauthorized Telegram chat");
        let response = if state.config.telegram_chat_ids().is_empty() {
            "Setup needed\nSend /userinfo here, then set TELEGRAM_CHAT_ID to this chat ID."
        } else {
            "Not authorized\nThis chat cannot control the cluster. Send /userinfo here to check the chat ID."
        };
        if let Err(err) = client.send_message(chat_id, response).await {
            warn!(error = %err, "failed to send unauthorized response");
        }
        return;
    }

    if command_requires_private_chat(command) {
        if message.chat.kind != "private" && !parsed_command.addressed_to_bot {
            let response = match runtime.bot_username.as_deref() {
                Some(username) => format!(
                    "Mention me in groups\nUse {command}@{username} so the command is intentional."
                ),
                None => "Mention me in groups\nUse the bot mention on control commands so they are intentional.".to_string(),
            };
            if let Err(err) = client.send_message(chat_id, &response).await {
                warn!(error = %err, "failed to send group mention rejection");
            }
            return;
        }

        if !is_sensitive_actor_authorized(&state, &message.chat, message.from.as_ref()) {
            let response = "Not allowed here\nUse the authorized private chat. Group control also needs allow_group_control and your Telegram user ID in config.";
            if let Err(err) = client.send_message(chat_id, response).await {
                warn!(error = %err, "failed to send sensitive command rejection");
            }
            return;
        }
    }

    if !state.is_leader().await {
        if let Err(err) = client
            .send_message(
                chat_id,
                "Standby node\nAnother node is controller now. Send the command again.",
            )
            .await
        {
            warn!(error = %err, "failed to send stale leader response");
        }
        return;
    }

    if command == "/monitor" {
        let parts: Vec<_> = text.split_whitespace().collect();
        if let Err(err) = start_live_monitor(client, runtime, state, chat_id, &parts[1..]).await {
            let response = format!("Monitor failed\n{}", compact_error(&err.to_string()));
            if let Err(send_err) = client.send_message(chat_id, &response).await {
                warn!(error = %send_err, "failed to send monitor error");
            }
        }
        return;
    }

    if command == "/speedtest" {
        let parts: Vec<_> = text.split_whitespace().collect();
        start_speedtest(
            client.clone(),
            Arc::clone(&runtime),
            state,
            chat_id,
            &parts[1..],
        )
        .await;
        return;
    }

    if command == "/screenshot" {
        let parts: Vec<_> = text.split_whitespace().collect();
        let Some(host) = parts.get(1) else {
            if let Err(err) = client.send_message(chat_id, &usage_screenshot()).await {
                warn!(error = %err, "failed to send screenshot usage");
            }
            return;
        };
        start_screenshot(
            client.clone(),
            Arc::clone(&runtime),
            state,
            chat_id,
            (*host).to_string(),
        )
        .await;
        return;
    }

    if command == "/update" {
        let parts: Vec<_> = text.split_whitespace().collect();
        start_update(
            client.clone(),
            Arc::clone(&runtime),
            state,
            chat_id,
            &parts[1..],
        )
        .await;
        return;
    }

    let response = handle_command(&state, text, command).await;
    if let Err(err) = client.send_message(chat_id, &response).await {
        warn!(error = %err, "failed to send Telegram response");
    }
}

async fn handle_callback(
    client: &TelegramClient,
    runtime: Arc<TelegramRuntime>,
    state: Arc<AppState>,
    callback: CallbackQuery,
) {
    let Some(data) = callback.data.as_deref() else {
        let _ = client
            .answer_callback_query(&callback.id, Some("Nothing to do"))
            .await;
        return;
    };

    let Some(message) = callback.message.as_ref() else {
        let _ = client
            .answer_callback_query(&callback.id, Some("Message is no longer available"))
            .await;
        return;
    };

    if !is_authorized(&state, message.chat.id) {
        let _ = client
            .answer_callback_query(&callback.id, Some("Not authorized"))
            .await;
        return;
    }

    if !is_sensitive_actor_authorized(&state, &message.chat, Some(&callback.from)) {
        let _ = client
            .answer_callback_query(&callback.id, Some("Not allowed here"))
            .await;
        return;
    }

    if let Some(monitor_id) = data.strip_prefix("monitor.stop:") {
        let stopped = stop_monitor(
            Arc::clone(&runtime),
            monitor_id,
            message.chat.id,
            message.message_id,
        )
        .await;
        let text = if stopped {
            format!(
                "Monitor stopped\nTime: {}",
                Utc::now().format("%H:%M:%S UTC")
            )
        } else {
            "Monitor stopped\nAlready ended.".to_string()
        };
        if let Err(err) = client
            .edit_message_text(message.chat.id, message.message_id, &text, None)
            .await
        {
            warn!(error = %err, "failed to edit stopped monitor message");
        }
        let _ = client
            .answer_callback_query(&callback.id, Some("Stopped"))
            .await;
        return;
    }

    let _ = client
        .answer_callback_query(&callback.id, Some("Unknown action"))
        .await;
}

async fn stop_monitor(
    runtime: Arc<TelegramRuntime>,
    monitor_id: &str,
    chat_id: i64,
    message_id: i64,
) -> bool {
    let mut monitors = runtime.monitors.lock().await;
    let Some(monitor) = monitors.get(monitor_id) else {
        return false;
    };
    if monitor.chat_id != chat_id || monitor.message_id != message_id {
        return false;
    }
    if let Some(monitor) = monitors.remove(monitor_id) {
        monitor.handle.abort();
        true
    } else {
        false
    }
}

async fn render_userinfo(
    state: &AppState,
    runtime: Arc<TelegramRuntime>,
    message: &Message,
) -> String {
    if !is_authorized(state, message.chat.id)
        && let Some(remaining) = rate_limit(
            &runtime.userinfo_last_seen,
            message.chat.id,
            USERINFO_RATE_LIMIT,
        )
        .await
    {
        return format!("Slow down\nTry /userinfo again in {remaining}s.");
    }

    let user_id = message
        .from
        .as_ref()
        .map(|user| user.id.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let username = message
        .from
        .as_ref()
        .and_then(|user| user.username.as_ref())
        .map(|name| format!("@{name}"))
        .unwrap_or_else(|| "none".to_string());

    format!(
        "Telegram IDs\nUser: {user_id}\nUsername: {username}\nChat: {}\nType: {}\n\n.env\nTELEGRAM_CHAT_ID={}",
        message.chat.id, message.chat.kind, message.chat.id
    )
}

fn is_authorized(state: &AppState, chat_id: i64) -> bool {
    state.config.telegram_chat_ids().contains(&chat_id)
}

fn is_sensitive_actor_authorized(state: &AppState, chat: &Chat, user: Option<&User>) -> bool {
    if !is_authorized(state, chat.id) {
        return false;
    }
    if chat.kind == "private" {
        return true;
    }
    if !state.config.telegram.allow_group_control {
        return false;
    }
    let Some(user) = user else {
        return false;
    };
    state.config.telegram.authorized_user_ids.contains(&user.id)
}

fn command_requires_private_chat(command: &str) -> bool {
    matches!(
        command,
        "/monitor" | "/on" | "/off" | "/screenshot" | "/speedtest" | "/update"
    )
}

async fn handle_command(state: &AppState, text: &str, command: &str) -> String {
    let parts: Vec<_> = text.split_whitespace().collect();

    match command {
        "/status" => render_status(state).await,
        "/leader" => render_leader(state).await,
        "/on" => {
            let Some(host) = parts.get(1) else {
                return usage_on();
            };
            wake_host(state, host).await
        }
        "/off" => {
            let Some(host) = parts.get(1) else {
                return usage_off();
            };
            shutdown_host(state, host).await
        }
        "/update" => usage_update(),
        "/speedtest" => usage_speedtest(),
        "/screenshot" => usage_screenshot(),
        "/help" | "/start" => help_text(),
        _ => help_text(),
    }
}

async fn start_speedtest(
    client: TelegramClient,
    runtime: Arc<TelegramRuntime>,
    state: Arc<AppState>,
    chat_id: i64,
    hosts: &[&str],
) {
    if !try_begin_operation(&runtime.speedtest_running).await {
        if let Err(err) = client
            .send_message(chat_id, "Speed test already running.")
            .await
        {
            warn!(error = %err, "failed to send speedtest busy response");
        }
        return;
    }

    let hosts: Vec<String> = hosts.iter().map(|host| (*host).to_string()).collect();
    let status = client
        .send_message(chat_id, "Speed test\nRunning...")
        .await
        .ok();

    tokio::spawn(async move {
        let response = if state.is_leader().await {
            handle_speedtest_command(&state, &hosts).await
        } else {
            "Standby node\nAnother node is controller now. Send /speedtest again.".to_string()
        };
        if let Some(message) = status {
            if let Err(err) = client
                .edit_message_text(chat_id, message.message_id, &response, None)
                .await
            {
                warn!(error = %err, "failed to edit speedtest response");
            }
        } else if let Err(err) = client.send_message(chat_id, &response).await {
            warn!(error = %err, "failed to send speedtest response");
        }
        finish_operation(&runtime.speedtest_running).await;
    });
}

async fn handle_speedtest_command(state: &AppState, hosts: &[String]) -> String {
    match hosts {
        [host] => match state.speedtest_internet_for_host(host, None).await {
            Ok(result) => render_speedtest(&result),
            Err(err) => format!("Speed test failed\n{}", compact_error(&err.to_string())),
        },
        [source, target] => match state.speedtest_between_hosts(source, target, None).await {
            Ok(result) => render_speedtest(&result),
            Err(err) => format!("Speed test failed\n{}", compact_error(&err.to_string())),
        },
        _ => usage_speedtest(),
    }
}

async fn start_screenshot(
    client: TelegramClient,
    runtime: Arc<TelegramRuntime>,
    state: Arc<AppState>,
    chat_id: i64,
    host: String,
) {
    if !try_begin_operation(&runtime.screenshot_running).await {
        if let Err(err) = client
            .send_message(chat_id, "Screenshot already running.")
            .await
        {
            warn!(error = %err, "failed to send screenshot busy response");
        }
        return;
    }

    let notice = format!("Screenshot\nHost: {host}\nCapturing...");
    let status_message = match client.send_message(chat_id, &notice).await {
        Ok(message) => Some(message),
        Err(err) => {
            warn!(error = %err, "failed to send screenshot status");
            None
        }
    };

    tokio::spawn(async move {
        if !state.is_leader().await {
            let text = "Standby node\nAnother node is controller now. Send /screenshot again.";
            if let Some(message) = status_message {
                let _ = client
                    .edit_message_text(chat_id, message.message_id, text, None)
                    .await;
            } else {
                let _ = client.send_message(chat_id, text).await;
            }
            finish_operation(&runtime.screenshot_running).await;
            return;
        }

        match state.screenshot_for_host(&host).await {
            Ok(screenshot) => {
                let caption = format!("Screenshot: {host}");
                if let Err(err) = client.send_document(chat_id, screenshot, &caption).await {
                    warn!(error = %err, "failed to send screenshot");
                    let _ = client
                        .send_message(
                            chat_id,
                            &format!(
                                "Screenshot send failed\n{}",
                                compact_error(&err.to_string())
                            ),
                        )
                        .await;
                } else if let Some(message) = status_message {
                    let _ = client
                        .edit_message_text(
                            chat_id,
                            message.message_id,
                            &format!("Screenshot\nHost: {host}\nCaptured."),
                            None,
                        )
                        .await;
                }
            }
            Err(err) => {
                let text = format!(
                    "Screenshot failed\nHost: {host}\n{}",
                    compact_error(&err.to_string())
                );
                if let Some(message) = status_message {
                    let _ = client
                        .edit_message_text(chat_id, message.message_id, &text, None)
                        .await;
                } else {
                    let _ = client.send_message(chat_id, &text).await;
                }
            }
        }
        finish_operation(&runtime.screenshot_running).await;
    });
}

async fn start_update(
    client: TelegramClient,
    runtime: Arc<TelegramRuntime>,
    state: Arc<AppState>,
    chat_id: i64,
    hosts: &[&str],
) {
    if !try_begin_operation(&runtime.update_running).await {
        if let Err(err) = client
            .send_message(chat_id, "Update already running.")
            .await
        {
            warn!(error = %err, "failed to send update busy response");
        }
        return;
    }

    let hosts: Vec<String> = hosts.iter().map(|host| (*host).to_string()).collect();
    let status = client
        .send_message(chat_id, "Update\nScheduling...")
        .await
        .ok();

    tokio::spawn(async move {
        let response = if state.is_leader().await {
            handle_update_command(&state, &hosts).await
        } else {
            "Standby node\nAnother node is controller now. Send /update again.".to_string()
        };
        if let Some(message) = status {
            if let Err(err) = client
                .edit_message_text(chat_id, message.message_id, &response, None)
                .await
            {
                warn!(error = %err, "failed to edit update response");
            }
        } else if let Err(err) = client.send_message(chat_id, &response).await {
            warn!(error = %err, "failed to send update response");
        }
        finish_operation(&runtime.update_running).await;
    });
}

async fn handle_update_command(state: &AppState, hosts: &[String]) -> String {
    let targets = match state.resolve_update_targets(hosts).await {
        Ok(targets) if !targets.is_empty() => targets,
        Ok(_) => return usage_update(),
        Err(err) => {
            return format!("Update failed\n{}", compact_error(&err.to_string()));
        }
    };

    let operation_id = state.new_update_operation_id();
    let mut queued = Vec::new();
    let mut failed = Vec::new();
    for target in targets {
        match state.update_for_host(&target, &operation_id).await {
            Ok(result) => queued.push(format!("- {}: {}", result.node_id, result.message)),
            Err(err) => failed.push(format!("- {target}: {}", safe_update_error(&err))),
        }
    }

    let mut lines = vec!["Update".to_string()];
    if !queued.is_empty() {
        lines.push("Queued:".to_string());
        lines.extend(queued);
    }
    if !failed.is_empty() {
        lines.push("Failed:".to_string());
        lines.extend(failed);
    }

    truncate(lines.join("\n"))
}

fn safe_update_error(err: &anyhow::Error) -> String {
    let raw = err.to_string();
    match raw.as_str() {
        "updates are disabled on this node" => "updates disabled".to_string(),
        "update.command is not configured" => "no updater configured".to_string(),
        "update.command program is empty" => "bad updater config".to_string(),
        "update launcher timed out" => "launcher timed out".to_string(),
        "update launcher failed" => "launcher failed; check update.log".to_string(),
        "update launcher could not start" => "launcher could not start".to_string(),
        "duplicate update operation" => "duplicate request ignored".to_string(),
        "update already running" => "already running".to_string(),
        "leadership state is not initialized" => "waiting for cluster state".to_string(),
        "refusing to restart active leader without a viable successor" => {
            "needs a healthy backup first".to_string()
        }
        "this node is not the active leader" => "not the current controller".to_string(),
        "update cluster mismatch" | "update target mismatch" => "request mismatch".to_string(),
        "stale update request" => "request expired".to_string(),
        "update request was not issued by the current leader" => {
            "request not from current controller".to_string()
        }
        "invalid update signature" => "request signature invalid".to_string(),
        "invalid update operation id" => "bad request id".to_string(),
        _ => "not scheduled; check local update.log".to_string(),
    }
}

fn render_speedtest(result: &SpeedtestResult) -> String {
    let seconds = result.elapsed_millis as f64 / 1000.0;
    let mut lines = if result.mode == "peer_download" {
        vec![
            "Speed test".to_string(),
            format!("Path: {} <- {}", result.source_node, result.target),
            "Mode: peer download".to_string(),
        ]
    } else {
        vec![
            "Speed test".to_string(),
            format!("Path: {} -> internet", result.source_node),
            "Mode: internet download".to_string(),
        ]
    };

    lines.extend([
        format!("Rate: {:.1} Mbps [{}]", result.mbps, speed_bar(result.mbps)),
        format!("Data: {}", bytes(result.bytes)),
        format!("Time: {:.2}s", seconds),
    ]);

    if result.mode == "peer_download" && result.mbps < 200.0 {
        lines.push("Check link speed: this looks closer to 100M than 1G.".to_string());
    }

    lines.join("\n")
}

async fn start_live_monitor(
    client: &TelegramClient,
    runtime: Arc<TelegramRuntime>,
    state: Arc<AppState>,
    chat_id: i64,
    hosts: &[&str],
) -> anyhow::Result<()> {
    let targets = monitor_targets(&state, hosts);
    let monitor_id = Uuid::new_v4().to_string();
    let markup = stop_monitor_markup(&monitor_id);
    let sent = client
        .send_message_with_markup(chat_id, "Monitor\nStarting...", Some(markup.clone()))
        .await?;
    let message_id = sent.message_id;

    let client = client.clone();
    let loop_runtime = Arc::clone(&runtime);
    let loop_state = Arc::clone(&state);
    let loop_monitor_id = monitor_id.clone();
    let handle = tokio::spawn(async move {
        live_monitor_loop(
            client,
            loop_runtime,
            loop_state,
            chat_id,
            message_id,
            loop_monitor_id,
            targets,
        )
        .await;
    });

    runtime.monitors.lock().await.insert(
        monitor_id,
        MonitorHandle {
            chat_id,
            message_id,
            handle,
        },
    );
    Ok(())
}

async fn live_monitor_loop(
    client: TelegramClient,
    runtime: Arc<TelegramRuntime>,
    state: Arc<AppState>,
    chat_id: i64,
    message_id: i64,
    monitor_id: String,
    targets: Vec<String>,
) {
    let markup = stop_monitor_markup(&monitor_id);
    let started = Instant::now();
    loop {
        if started.elapsed() >= MONITOR_MAX_DURATION {
            let text = format!(
                "Monitor ended\nLimit: {} minutes\nTime: {}",
                MONITOR_MAX_DURATION.as_secs() / 60,
                Utc::now().format("%H:%M:%S UTC")
            );
            let _ = client
                .edit_message_text(chat_id, message_id, &text, None)
                .await;
            break;
        }

        if !state.is_leader().await {
            let _ = client
                .edit_message_text(
                    chat_id,
                    message_id,
                    "Monitor stopped\nAnother node is controller now.",
                    None,
                )
                .await;
            break;
        }

        let text = render_monitor_live(&state, &targets).await;
        if let Err(err) = client
            .edit_message_text(chat_id, message_id, &text, Some(markup.clone()))
            .await
        {
            warn!(error = %err, "failed to update live monitor");
        }
        sleep(MONITOR_INTERVAL).await;
    }

    runtime.monitors.lock().await.remove(&monitor_id);
    info!(monitor_id, "live monitor ended");
}

fn monitor_targets(state: &AppState, hosts: &[&str]) -> Vec<String> {
    if hosts.is_empty() {
        let mut all = vec![state.config.node.id.clone()];
        all.extend(state.config.peers.iter().map(|peer| peer.id.clone()));
        all
    } else {
        hosts.iter().map(|host| (*host).to_string()).collect()
    }
}

async fn render_monitor_live(state: &AppState, targets: &[String]) -> String {
    let mut lines = vec![
        format!("Monitor {}", Utc::now().format("%H:%M:%S UTC")),
        format!(
            "Controller: {}",
            state
                .leader_id()
                .await
                .unwrap_or_else(|| "unknown".to_string())
        ),
        format!("Limit: {} minutes", MONITOR_MAX_DURATION.as_secs() / 60),
        String::new(),
    ];

    for host in targets {
        match state.collect_metrics_for_host(host).await {
            Some(metrics) => lines.extend(render_metrics(host, &metrics)),
            None => lines.push(format!("{host}\n  Waiting for metrics")),
        }
        lines.push(String::new());
    }

    truncate(lines.join("\n").trim().to_string())
}

async fn render_status(state: &AppState) -> String {
    let status = state.cluster_status().await;
    let online_peers = status.peers.iter().filter(|peer| peer.online).count();
    let total_hosts = status.peers.len() + 1;
    let online_hosts = online_peers + 1;
    let mut lines = vec![
        "Status".to_string(),
        format!("Cluster: {}", state.config.cluster.name),
        format!(
            "Controller: {}",
            status
                .leader_id
                .clone()
                .unwrap_or_else(|| "unknown".to_string())
        ),
        format!(
            "This node: {} ({})",
            status.self_node.node_id, status.self_node.display_name
        ),
        format!("Hosts: {online_hosts}/{total_hosts} online"),
        String::new(),
    ];

    lines.push(format!(
        "- OK  {}  priority {}",
        state.config.node.id, state.config.node.priority
    ));
    for peer in status.peers {
        let state_text = if peer.online { "OK " } else { "OFF" };
        let mut line = format!("- {state_text} {}  priority {}", peer.id, peer.priority);
        if let Some(err) = peer.last_error {
            line.push_str(&format!(" ({})", compact_error(&err)));
        }
        lines.push(line);
    }

    truncate(lines.join("\n"))
}

async fn render_leader(state: &AppState) -> String {
    let status = state.cluster_status().await;
    format!(
        "Controller\nActive: {}\nThis node: {}\nEligible: {}",
        status.leader_id.unwrap_or_else(|| "unknown".to_string()),
        state.config.node.id,
        yes_no(state.config.node.eligible_leader)
    )
}

fn render_metrics(host: &str, metrics: &MetricsSnapshot) -> Vec<String> {
    let mut lines = vec![
        host.to_string(),
        format!(
            "  CPU  [{}] {:>5.1}%  {} cores",
            percent_bar(metrics.cpu.usage_percent, BAR_WIDTH),
            metrics.cpu.usage_percent,
            metrics.cpu.logical_cores
        ),
        format!(
            "  RAM  [{}] {:>5.1}%  {} / {}",
            percent_bar(metrics.memory.usage_percent, BAR_WIDTH),
            metrics.memory.usage_percent,
            bytes(metrics.memory.used_bytes),
            bytes(metrics.memory.total_bytes)
        ),
    ];

    if let Some(hottest) = metrics.temperatures.iter().max_by(|left, right| {
        left.temperature_celsius
            .total_cmp(&right.temperature_celsius)
    }) {
        lines.push(format!(
            "  Temp {} {:.1}C",
            hottest.label, hottest.temperature_celsius
        ));
    }

    if let Some(busiest_disk) = metrics
        .disks
        .iter()
        .max_by(|left, right| left.usage_percent.total_cmp(&right.usage_percent))
    {
        lines.push(format!(
            "  Disk [{}] {:>5.1}%  {} free on {}",
            percent_bar(busiest_disk.usage_percent, BAR_WIDTH),
            busiest_disk.usage_percent,
            bytes(busiest_disk.available_bytes),
            busiest_disk.mount_point,
        ));
    }

    let active_networks: Vec<_> = metrics
        .networks
        .iter()
        .filter(|network| {
            network.received_bytes_per_second.unwrap_or(0.0) > 0.0
                || network.transmitted_bytes_per_second.unwrap_or(0.0) > 0.0
        })
        .take(3)
        .collect();
    if !active_networks.is_empty() {
        lines.push("  Network".to_string());
        for network in active_networks {
            let rx = network.received_bytes_per_second.unwrap_or(0.0);
            let tx = network.transmitted_bytes_per_second.unwrap_or(0.0);
            lines.push(format!(
                "    {}  Down [{}] {}/s  Up [{}] {}/s",
                network.name,
                throughput_bar(rx),
                bytes_f64(rx),
                throughput_bar(tx),
                bytes_f64(tx)
            ));
        }
    }

    if metrics.gpus.is_empty() {
        lines.push("  GPU  none reported".to_string());
    } else {
        lines.push(format!("  GPU  {} device(s)", metrics.gpus.len()));
        for gpu in &metrics.gpus {
            let usage = gpu
                .usage_percent
                .map(|value| {
                    format!(
                        "[{}] {:>5.1}%",
                        percent_bar(value, BAR_WIDTH),
                        value.clamp(0.0, 100.0)
                    )
                })
                .unwrap_or_else(|| "n/a".to_string());
            let temp = gpu
                .temperature_celsius
                .map(|value| format!("{value:.0}C"))
                .unwrap_or_else(|| "n/a".to_string());
            let mem = match (gpu.memory_used_bytes, gpu.memory_total_bytes) {
                (Some(used), Some(total)) => format!("{} / {}", bytes(used), bytes(total)),
                (_, Some(total)) => format!("{} total", bytes(total)),
                _ => "n/a".to_string(),
            };
            lines.push(format!(
                "    #{} {}  {}  {}  {}",
                gpu.index, gpu.name, usage, temp, mem
            ));
        }
    }

    lines
}

async fn wake_host(state: &AppState, host: &str) -> String {
    let Some(peer) = state.peer_config(host).await else {
        return format!("Unknown host\nHost: {host}");
    };
    let Some(mac) = peer.wol_mac.as_deref() else {
        return format!(
            "Wake not configured\nHost: {}\nMissing Wake-on-LAN MAC.",
            peer.id
        );
    };
    let Some(broadcast) = peer.wol_broadcast.as_deref() else {
        return format!(
            "Wake not configured\nHost: {}\nMissing Wake-on-LAN broadcast.",
            peer.id
        );
    };

    match power::wake_on_lan(mac, broadcast) {
        Ok(()) => format!("Wake sent\nHost: {}", peer.id),
        Err(err) => format!(
            "Wake failed\nHost: {}\n{}",
            peer.id,
            compact_error(&err.to_string())
        ),
    }
}

async fn shutdown_host(state: &AppState, host: &str) -> String {
    if host.eq_ignore_ascii_case(&state.config.node.id) {
        if !state.config.node.allow_shutdown {
            return format!("Shutdown disabled\nHost: {}", state.config.node.id);
        }
        if let Err(err) = state.ensure_restart_allowed().await {
            return format!(
                "Shutdown blocked\nHost: {}\n{}",
                state.config.node.id,
                compact_error(&err.to_string())
            );
        }
        return match power::shutdown_local(
            DEFAULT_SHUTDOWN_DELAY_SECONDS,
            "requested from Telegram through distributed-watchdog",
        ) {
            Ok(actual_delay_seconds) => {
                render_shutdown_message(&state.config.node.id, actual_delay_seconds)
            }
            Err(err) => format!(
                "Shutdown failed\nHost: {}\n{}",
                state.config.node.id,
                compact_error(&err.to_string())
            ),
        };
    }

    let Some(peer) = state.peer_config(host).await else {
        return format!("Unknown host\nHost: {host}");
    };
    if !peer.allow_shutdown {
        return format!("Shutdown disabled\nHost: {}", peer.id);
    }

    let urls = peer.urls();
    if urls.is_empty() {
        return format!(
            "Shutdown not configured\nHost: {}\nNo API URL is configured.",
            peer.id
        );
    }

    let body = ShutdownRequest {
        delay_seconds: Some(DEFAULT_SHUTDOWN_DELAY_SECONDS),
        reason: Some("requested from Telegram through distributed-watchdog".to_string()),
    };

    let mut errors = Vec::new();
    for url in urls {
        let headers = match auth_headers(&state.shared_secret) {
            Ok(headers) => headers,
            Err(err) => {
                return format!("Shutdown failed\n{}", compact_error(&err.to_string()));
            }
        };
        let endpoint = format!("{}/power/shutdown", url.trim_end_matches('/'));
        match state
            .http
            .post(endpoint)
            .headers(headers)
            .json(&body)
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => {
                let message = response
                    .json::<ActionResponse>()
                    .await
                    .map(|action| action.message)
                    .unwrap_or_else(|_| "shutdown requested".to_string());
                return format!("Shutdown requested\nHost: {}\n{message}", peer.id);
            }
            Ok(response) => errors.push(format!("request rejected: {}", response.status())),
            Err(err) => errors.push(format!("host unreachable: {err}")),
        }
    }

    format!(
        "Shutdown failed\nHost: {}\n{}",
        peer.id,
        errors
            .first()
            .cloned()
            .unwrap_or_else(|| "no API URL responded".to_string())
    )
}

fn render_shutdown_message(host: &str, delay_seconds: u64) -> String {
    if delay_seconds == 0 {
        format!("Shutdown requested\nHost: {host}")
    } else {
        format!("Shutdown scheduled\nHost: {host}\nWait: {delay_seconds}s")
    }
}

fn help_text() -> String {
    [
        "Watchdog",
        "/status - cluster overview",
        "/leader - current controller",
        "/monitor [hosts...] - live metrics for up to 10 minutes",
        "/speedtest <host> - internet speed from one host",
        "/speedtest <source> <target> - speed between two hosts",
        "/on <host> - wake a host",
        "/off <host> - shut down a host now",
        "/update [all|host ...] - update opted-in hosts",
        "/screenshot <host> - capture the display",
        "/userinfo - show Telegram user and chat IDs",
    ]
    .join("\n")
}

fn usage_update() -> String {
    "Update\n/update\n/update all\n/update <host> [host ...]".to_string()
}

fn usage_speedtest() -> String {
    "Speed test\n/speedtest <host>\n/speedtest <source> <target>".to_string()
}

fn usage_screenshot() -> String {
    "Screenshot\n/screenshot <host>".to_string()
}

fn usage_on() -> String {
    "Wake\n/on <host>".to_string()
}

fn usage_off() -> String {
    "Shutdown\n/off <host>".to_string()
}

async fn rate_limit(
    seen: &Mutex<HashMap<i64, Instant>>,
    chat_id: i64,
    window: Duration,
) -> Option<u64> {
    let now = Instant::now();
    let mut seen = seen.lock().await;
    if let Some(last_seen) = seen.get(&chat_id) {
        let elapsed = now.duration_since(*last_seen);
        if elapsed < window {
            return Some(window.saturating_sub(elapsed).as_secs());
        }
    }
    seen.insert(chat_id, now);
    None
}

async fn try_begin_operation(running: &Mutex<bool>) -> bool {
    let mut running = running.lock().await;
    if *running {
        return false;
    }
    *running = true;
    true
}

async fn finish_operation(running: &Mutex<bool>) {
    *running.lock().await = false;
}

#[derive(Debug, Clone)]
struct TelegramClient {
    http: reqwest::Client,
    base_url: Url,
}

impl TelegramClient {
    fn new(token: String, polling_timeout_seconds: u64) -> anyhow::Result<Self> {
        let base_url = Url::parse(&format!("https://api.telegram.org/bot{token}/"))
            .context("invalid Telegram token")?;
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(polling_timeout_seconds + 15))
            .build()
            .context("failed to create Telegram HTTP client")?;
        Ok(Self { http, base_url })
    }

    async fn get_updates(
        &self,
        offset: Option<i64>,
        timeout_seconds: u64,
    ) -> anyhow::Result<Vec<Update>> {
        #[derive(Debug, Serialize)]
        struct Params {
            timeout: u64,
            allowed_updates: [&'static str; 2],
            #[serde(skip_serializing_if = "Option::is_none")]
            offset: Option<i64>,
        }

        self.post_json(
            "getUpdates",
            &Params {
                timeout: timeout_seconds,
                allowed_updates: ["message", "callback_query"],
                offset,
            },
        )
        .await
        .map(|result| result.unwrap_or_default())
    }

    async fn commit_update(&self, next_offset: i64) -> anyhow::Result<()> {
        #[derive(Debug, Serialize)]
        struct Params {
            timeout: u64,
            allowed_updates: [&'static str; 2],
            offset: i64,
        }

        let _ = self
            .post_json::<Vec<Update>>(
                "getUpdates",
                &Params {
                    timeout: 0,
                    allowed_updates: ["message", "callback_query"],
                    offset: next_offset,
                },
            )
            .await?;
        Ok(())
    }

    async fn send_message(&self, chat_id: i64, text: &str) -> anyhow::Result<SentMessage> {
        self.send_message_with_markup(chat_id, text, None).await
    }

    async fn get_me(&self) -> anyhow::Result<User> {
        self.post_json("getMe", &serde_json::json!({}))
            .await?
            .ok_or_else(|| anyhow::anyhow!("Telegram getMe had no result"))
    }

    async fn set_my_commands(&self) -> anyhow::Result<()> {
        #[derive(Debug, Serialize)]
        struct Params<'a> {
            commands: &'a [BotCommand],
        }

        match self
            .post_json::<bool>(
                "setMyCommands",
                &Params {
                    commands: TELEGRAM_BOT_COMMANDS,
                },
            )
            .await?
        {
            Some(true) => Ok(()),
            _ => bail!("Telegram setMyCommands was not accepted"),
        }
    }

    async fn send_document(
        &self,
        chat_id: i64,
        screenshot: crate::screenshot::Screenshot,
        caption: &str,
    ) -> anyhow::Result<()> {
        let part = reqwest::multipart::Part::bytes(screenshot.bytes)
            .file_name(screenshot.filename)
            .mime_str(screenshot.content_type)?;
        let form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .text("caption", telegram_caption_html(caption))
            .text("parse_mode", TELEGRAM_PARSE_MODE)
            .part("document", part);

        let url = self.url("sendDocument")?;
        let response = self
            .http
            .post(url)
            .multipart(form)
            .send()
            .await
            .map_err(|err| telegram_request_error("sendDocument", err))?;
        let _ = telegram_response::<serde_json::Value>("sendDocument", response).await?;

        Ok(())
    }

    async fn send_message_with_markup(
        &self,
        chat_id: i64,
        text: &str,
        reply_markup: Option<InlineKeyboardMarkup>,
    ) -> anyhow::Result<SentMessage> {
        #[derive(Debug, Serialize)]
        struct Params<'a> {
            chat_id: i64,
            text: &'a str,
            parse_mode: &'static str,
            disable_web_page_preview: bool,
            #[serde(skip_serializing_if = "Option::is_none")]
            reply_markup: Option<InlineKeyboardMarkup>,
        }

        let text = telegram_html(text);
        self.post_json(
            "sendMessage",
            &Params {
                chat_id,
                text: &text,
                parse_mode: TELEGRAM_PARSE_MODE,
                disable_web_page_preview: true,
                reply_markup,
            },
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("Telegram sendMessage had no result"))
    }

    async fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
        reply_markup: Option<InlineKeyboardMarkup>,
    ) -> anyhow::Result<()> {
        #[derive(Debug, Serialize)]
        struct Params<'a> {
            chat_id: i64,
            message_id: i64,
            text: &'a str,
            parse_mode: &'static str,
            disable_web_page_preview: bool,
            #[serde(skip_serializing_if = "Option::is_none")]
            reply_markup: Option<InlineKeyboardMarkup>,
        }

        let text = telegram_html(text);
        let url = self.url("editMessageText")?;
        let response = self
            .http
            .post(url)
            .json(&Params {
                chat_id,
                message_id,
                text: &text,
                parse_mode: TELEGRAM_PARSE_MODE,
                disable_web_page_preview: true,
                reply_markup,
            })
            .send()
            .await
            .map_err(|err| telegram_request_error("editMessageText", err))?;

        if response.status() == StatusCode::BAD_REQUEST {
            let body = response.text().await.unwrap_or_default();
            if body.contains("message is not modified") {
                return Ok(());
            }
            bail!(
                "Telegram editMessageText HTTP {}: {}",
                StatusCode::BAD_REQUEST,
                telegram_body_description(&body)
            );
        }

        let _ = telegram_response::<serde_json::Value>("editMessageText", response).await?;

        Ok(())
    }

    async fn answer_callback_query(
        &self,
        callback_query_id: &str,
        text: Option<&str>,
    ) -> anyhow::Result<()> {
        #[derive(Debug, Serialize)]
        struct Params<'a> {
            callback_query_id: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            text: Option<&'a str>,
        }

        let _ = self
            .post_json::<serde_json::Value>(
                "answerCallbackQuery",
                &Params {
                    callback_query_id,
                    text,
                },
            )
            .await?;

        Ok(())
    }

    async fn post_json<T: DeserializeOwned>(
        &self,
        method: &'static str,
        params: &impl Serialize,
    ) -> anyhow::Result<Option<T>> {
        let url = self.url(method)?;
        let response = self
            .http
            .post(url)
            .json(params)
            .send()
            .await
            .map_err(|err| telegram_request_error(method, err))?;
        telegram_response(method, response).await
    }

    fn url(&self, method: &str) -> anyhow::Result<Url> {
        self.base_url
            .join(method)
            .with_context(|| format!("invalid Telegram method {method}"))
    }
}

async fn telegram_response<T: DeserializeOwned>(
    method: &'static str,
    response: reqwest::Response,
) -> anyhow::Result<Option<T>> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!(
            "Telegram {method} HTTP {status}: {}",
            telegram_body_description(&body)
        );
    }

    let response = response
        .json::<TelegramResponse<T>>()
        .await
        .with_context(|| format!("Telegram {method} returned invalid JSON"))?;
    if !response.ok {
        bail!(
            "Telegram {method} failed: {}",
            telegram_failure_description(response.description)
        );
    }

    Ok(response.result)
}

fn telegram_request_error(method: &'static str, err: reqwest::Error) -> anyhow::Error {
    let reason = if let Some(status) = err.status() {
        format!("HTTP {status}")
    } else if err.is_timeout() {
        "request timed out".to_string()
    } else if err.is_connect() {
        "connection failed".to_string()
    } else if err.is_decode() {
        "invalid response".to_string()
    } else if err.is_request() {
        "request could not be built".to_string()
    } else {
        "request failed".to_string()
    };
    anyhow::anyhow!("Telegram {method} request failed: {reason}")
}

fn telegram_body_description(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "empty response".to_string();
    }

    if let Ok(response) = serde_json::from_str::<TelegramResponse<serde_json::Value>>(trimmed) {
        let description = response
            .description
            .unwrap_or_else(|| "unknown error".to_string());
        return redact_telegram_tokens(&description);
    }

    redact_telegram_tokens(&truncate(trimmed.to_string()))
}

fn telegram_failure_description(description: Option<String>) -> String {
    redact_telegram_tokens(&description.unwrap_or_else(|| "unknown error".to_string()))
}

fn redact_telegram_tokens(text: &str) -> String {
    let mut output = text.to_string();
    let mut search_start = 0;
    while let Some(relative_start) = output[search_start..].find("bot") {
        let start = search_start + relative_start;
        let mut end = start + "bot".len();
        let bytes = output.as_bytes();
        let mut saw_digit = false;

        while end < bytes.len() && bytes[end].is_ascii_digit() {
            saw_digit = true;
            end += 1;
        }

        if !saw_digit || end >= bytes.len() || bytes[end] != b':' {
            search_start = start + "bot".len();
            continue;
        }

        end += 1;
        let token_start = end;
        while end < bytes.len()
            && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_' || bytes[end] == b'-')
        {
            end += 1;
        }

        if end == token_start {
            search_start = start + "bot".len();
            continue;
        }

        output.replace_range(start..end, "bot<redacted>");
        search_start = start + "bot<redacted>".len();
    }

    output
}

fn is_telegram_poll_conflict(err: &anyhow::Error) -> bool {
    let message = err.to_string();
    message.contains("Telegram getUpdates HTTP 409")
        || message.contains("terminated by other getUpdates request")
}

fn telegram_conflict_backoff(priority: i64) -> Duration {
    let penalty = (100_i64.saturating_sub(priority)).clamp(0, 60) as u64;
    Duration::from_secs(30 + penalty)
}

#[derive(Debug, Serialize, Clone)]
struct InlineKeyboardMarkup {
    inline_keyboard: Vec<Vec<InlineKeyboardButton>>,
}

#[derive(Debug, Serialize, Clone)]
struct InlineKeyboardButton {
    text: String,
    callback_data: String,
}

fn stop_monitor_markup(monitor_id: &str) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup {
        inline_keyboard: vec![vec![InlineKeyboardButton {
            text: "Stop".to_string(),
            callback_data: format!("monitor.stop:{monitor_id}"),
        }]],
    }
}

#[derive(Debug, Deserialize)]
struct TelegramResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Update {
    update_id: i64,
    message: Option<Message>,
    callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    id: String,
    from: User,
    message: Option<CallbackMessage>,
    data: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Message {
    chat: Chat,
    from: Option<User>,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CallbackMessage {
    message_id: i64,
    chat: Chat,
}

#[derive(Debug, Deserialize)]
struct SentMessage {
    message_id: i64,
}

#[derive(Debug, Deserialize)]
struct Chat {
    id: i64,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Deserialize)]
struct User {
    id: i64,
    username: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedCommand<'a> {
    name: &'a str,
    addressed_to_bot: bool,
}

fn command_name<'a>(text: &'a str, bot_username: Option<&str>) -> Option<ParsedCommand<'a>> {
    let raw = text.split_whitespace().next()?;
    let Some((command, suffix)) = raw.split_once('@') else {
        return Some(ParsedCommand {
            name: raw,
            addressed_to_bot: false,
        });
    };
    if bot_username
        .map(|username| suffix.eq_ignore_ascii_case(username))
        .unwrap_or(false)
    {
        Some(ParsedCommand {
            name: command,
            addressed_to_bot: true,
        })
    } else {
        None
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn bytes(bytes: u64) -> String {
    bytes_f64(bytes as f64)
}

fn bytes_f64(bytes: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes.max(0.0);
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn percent_bar(percent: f32, width: usize) -> String {
    let clamped = percent.clamp(0.0, 100.0);
    let filled = ((clamped / 100.0) * width as f32).round() as usize;
    format!(
        "{}{}",
        "#".repeat(filled.min(width)),
        "-".repeat(width.saturating_sub(filled))
    )
}

fn throughput_bar(bytes_per_second: f64) -> String {
    const ONE_GIGABIT_BYTES_PER_SECOND: f64 = 1_000_000_000.0 / 8.0;
    let percent =
        (bytes_per_second / ONE_GIGABIT_BYTES_PER_SECOND * 100.0).clamp(0.0, 100.0) as f32;
    percent_bar(percent, BAR_WIDTH)
}

fn speed_bar(mbps: f64) -> String {
    let percent = (mbps / 1000.0 * 100.0).clamp(0.0, 100.0) as f32;
    percent_bar(percent, BAR_WIDTH)
}

fn compact_error(error: &str) -> String {
    let first_line = error.lines().next().unwrap_or(error);
    if first_line.len() > 80 {
        format!("{}...", truncate_at_char_boundary(first_line, 80))
    } else {
        first_line.to_string()
    }
}

fn truncate(text: String) -> String {
    const MAX: usize = 3900;
    if text.len() <= MAX {
        return text;
    }
    let mut truncated = text;
    let end = truncate_at_char_boundary(&truncated, MAX).len();
    truncated.truncate(end);
    truncated.push_str("\n...");
    truncated
}

fn truncate_at_char_boundary(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_name_strips_bot_suffix() {
        let command = command_name("/status@example_bot more", Some("example_bot")).unwrap();
        assert_eq!(command.name, "/status");
        assert!(command.addressed_to_bot);
        assert_eq!(
            command_name("/status@other_bot more", Some("example_bot")),
            None
        );
        let command = command_name("/status more", Some("example_bot")).unwrap();
        assert_eq!(command.name, "/status");
        assert!(!command.addressed_to_bot);
    }

    #[test]
    fn bot_commands_are_valid_for_telegram_autocomplete() {
        for command in TELEGRAM_BOT_COMMANDS {
            assert!((1..=32).contains(&command.command.len()));
            assert!(command.command.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'
            }));
            assert!((1..=256).contains(&command.description.len()));
        }
    }

    #[test]
    fn telegram_body_description_extracts_json_description() {
        let description =
            telegram_body_description(r#"{"ok":false,"description":"Conflict: terminated"}"#);
        assert_eq!(description, "Conflict: terminated");
    }

    #[test]
    fn telegram_body_description_redacts_bot_tokens() {
        let description = telegram_body_description(
            r#"{"ok":false,"description":"failed at https://api.telegram.org/bot123456:ABC_def-ghi/getUpdates"}"#,
        );
        assert!(description.contains("bot<redacted>"));
        assert!(!description.contains("123456:ABC"));
        assert!(!description.contains("ABC_def"));
    }

    #[test]
    fn telegram_failure_description_redacts_bot_tokens() {
        let description = telegram_failure_description(Some(
            "failed at https://api.telegram.org/bot123456:ABC_def-ghi/getUpdates".to_string(),
        ));
        assert!(description.contains("bot<redacted>"));
        assert!(!description.contains("ABC_def"));
    }

    #[test]
    fn percent_bar_is_fixed_width() {
        assert_eq!(percent_bar(0.0, 5), "-----");
        assert_eq!(percent_bar(50.0, 5), "###--");
        assert_eq!(percent_bar(100.0, 5), "#####");
        assert_eq!(percent_bar(250.0, 5), "#####");
    }

    #[test]
    fn render_speedtest_uses_readable_labels() {
        let text = render_speedtest(&SpeedtestResult {
            mode: "peer_download".to_string(),
            source_node: "source".to_string(),
            target: "target".to_string(),
            bytes: 1024 * 1024,
            elapsed_millis: 1000,
            mbps: 150.0,
            completed_at: Utc::now(),
        });

        assert!(text.contains("Path: source <- target"));
        assert!(text.contains("Mode: peer download"));
        assert!(text.contains("Rate: 150.0 Mbps"));
        assert!(text.contains("Data: 1.0 MiB"));
        assert!(text.contains("Time: 1.00s"));
        assert!(text.contains("Check link speed:"));
    }

    #[test]
    fn truncate_keeps_utf8_boundaries() {
        let text = "a".repeat(3899) + "ééé";
        let truncated = truncate(text);
        assert!(truncated.ends_with("\n..."));
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    }
}
