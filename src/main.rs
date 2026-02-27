// ════════════════════════════════════════════════════════════
// port-sentinel-rs v2.2.6 - Port Sentinel Monitoring System
// Features: Async Concurrent Detection | WeCom Alert | Graceful Shutdown
//           Auto Config Initialization | Permission Protection | Env Var Injection
// ════════════════════════════════════════════════════════════

#![warn(rust_2018_idioms)]
#![allow(dead_code)]

use chrono::Local;
use serde::Deserialize;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, Semaphore};
use tokio::time::{sleep, timeout};
use tracing::{error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;

// ────────────────────────────────────────────────────────────
// Configuration Structure (Strictly matches config.toml)
// ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Clone)]
struct Config {
    settings: Settings,
    #[serde(rename = "device")]
    devices: Vec<Device>,
}

#[derive(Debug, Deserialize, Clone)]
struct Settings {
    interval: u64,
    timeout: u64,
    alert_cooldown: u64,
    webhook: String,
    #[serde(default = "default_log_level")]
    log_level: String,
    #[serde(default = "default_max_concurrent")]
    max_concurrent_connections: usize,
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_max_concurrent() -> usize {
    100
}

#[derive(Debug, Deserialize, Clone)]
struct Device {
    id: String,
    name: String,
    group: String,
    priority: String,
    ips: Vec<String>,
    os: String,
    location: String,
    checks: Vec<CheckItem>,
}

#[derive(Debug, Deserialize, Clone)]
struct CheckItem {
    port: u16,
    #[serde(default)]
    name: String,
}

// ────────────────────────────────────────────────────────────
// Alert State Management (Thread-safe + Cooldown Control + State Recovery)
// ────────────────────────────────────────────────────────────

struct AlertState {
    last_alert: HashMap<String, i64>,
    is_failed: HashMap<String, bool>,
}

impl AlertState {
    fn new() -> Self {
        Self {
            last_alert: HashMap::new(),
            is_failed: HashMap::new(),
        }
    }

    /// Determine if alert should be sent (supports failure recovery detection + cooldown control)
    fn should_alert(
        &mut self,
        device_id: &str,
        currently_failed: bool,
        now_ts: i64,
        cooldown: u64,
    ) -> bool {
        let prev_failed = self.is_failed.get(device_id).copied().unwrap_or(false);

        // If previously failed but now recovered, clear state without alert
        if prev_failed && !currently_failed {
            self.is_failed.remove(device_id);
            return false;
        }

        // Only alert if failed and cooldown period has passed
        if !currently_failed {
            return false;
        }

        let last = self.last_alert.get(device_id).copied().unwrap_or(0);
        if now_ts - last >= cooldown as i64 {
            self.last_alert.insert(device_id.to_string(), now_ts);
            self.is_failed
                .insert(device_id.to_string(), currently_failed);
            true
        } else {
            false
        }
    }

    /// Mark device as recovered, return whether recovery actually occurred (for statistics)
    fn mark_recovered(&mut self, device_id: &str) -> bool {
        self.is_failed.remove(device_id).is_some()
    }
}

// ────────────────────────────────────────────────────────────
// Core Detection Logic (Three-level Concurrency + Semaphore Rate Limiting + Resource Reuse)
// ────────────────────────────────────────────────────────────

async fn check_port_with_semaphore(
    ip: &str,
    port: u16,
    timeout_sec: u64,
    semaphore: Arc<Semaphore>,
) -> bool {
    let _permit = semaphore.acquire().await.unwrap();
    let addr = format!("{}:{}", ip, port);
    let timeout_dur = Duration::from_secs(timeout_sec);

    matches!(
        timeout(timeout_dur, TcpStream::connect(&addr)).await,
        Ok(Ok(_))
    )
}

async fn check_item_with_parallel_ip(
    check: &CheckItem,
    ips: &[String],
    timeout_sec: u64,
    semaphore: Arc<Semaphore>,
) -> (bool, Vec<String>) {
    let mut tasks = tokio::task::JoinSet::new();

    for ip in ips {
        let ip_clone = ip.clone();
        let sem_clone = semaphore.clone();
        let port = check.port;
        let to_sec = timeout_sec;

        tasks.spawn(async move {
            let success = check_port_with_semaphore(&ip_clone, port, to_sec, sem_clone).await;
            (ip_clone, success)
        });
    }

    let mut failed_ips = Vec::new();
    let mut any_success = false;

    while let Some(result) = tasks.join_next().await {
        if let Ok((ip, success)) = result {
            if success {
                any_success = true;
            } else {
                failed_ips.push(ip);
            }
        }
    }
    (any_success, failed_ips)
}

async fn check_device_parallel(
    device: &Device,
    timeout_sec: u64,
    semaphore: Arc<Semaphore>,
) -> (bool, Vec<CheckFailure>) {
    let mut tasks = tokio::task::JoinSet::new();

    for check in &device.checks {
        let check_clone = check.clone();
        let ips_clone = device.ips.clone();
        let sem_clone = semaphore.clone();
        let to_sec = timeout_sec;

        tasks.spawn(async move {
            let (success, failed_ips) =
                check_item_with_parallel_ip(&check_clone, &ips_clone, to_sec, sem_clone).await;
            (check_clone, success, failed_ips)
        });
    }

    let mut failures = Vec::new();

    while let Some(result) = tasks.join_next().await {
        if let Ok((check, success, failed_ips)) = result {
            if !success {
                failures.push(CheckFailure {
                    check_name: if check.name.is_empty() {
                        format!("port:{}", check.port)
                    } else {
                        check.name.clone()
                    },
                    port: check.port,
                    attempted_ips: failed_ips,
                });
            }
        }
    }
    (failures.is_empty(), failures)
}

#[derive(Clone)]
struct CheckFailure {
    check_name: String,
    port: u16,
    attempted_ips: Vec<String>,
}

// ────────────────────────────────────────────────────────────
// Alert Sending (WeCom Markdown - Clear Vertical Layout + Silent Mode + Retry Mechanism)
// ────────────────────────────────────────────────────────────

async fn send_wechat_alert(webhook: &str, device: &Device, failures: &[CheckFailure]) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to create HTTP client: {}", e);
            return;
        }
    };

    let mut detail = String::from("```\n");

    for (idx, failure) in failures.iter().enumerate() {
        detail.push_str(&format!(
            "┌─ 🔴 {} (Port：{})\n",
            failure.check_name, failure.port
        ));

        let display_ips: Vec<&String> = failure.attempted_ips.iter().take(10).collect();
        for (ip_idx, ip) in display_ips.iter().enumerate() {
            let connector = if ip_idx == display_ips.len() - 1 {
                "│  └─"
            } else {
                "│  ├─"
            };
            detail.push_str(&format!("{} ❌ {}\n", connector, ip));
        }

        if failure.attempted_ips.len() > 10 {
            detail.push_str(&format!(
                "│  └─ ... {} more IPs\n",
                failure.attempted_ips.len() - 10
            ));
        }

        if idx < failures.len() - 1 {
            detail.push_str("│\n");
        }
    }

    detail.push_str(&format!(
        "└─ 📊 Stats：{} checks failed | {} IPs affected\n",
        failures.len(),
        failures
            .iter()
            .map(|f| f.attempted_ips.len())
            .sum::<usize>()
    ));
    detail.push_str("```\n");

    let priority_emoji = match device.priority.as_str() {
        "critical" => "🔴",
        "high" => "🟠",
        "medium" => "🟡",
        _ => "🔵",
    };

    let content = format!(
        "{} **{}** Failure Alert\n\n\
        > 📍 Location：{}\n\
        > 💻 OS：{} | 🏷️ Group：{}\n\
        > ⚠️ Priority：{}\n\n\
        **Failure Details**：\n{}\n\
        ---\n\
        <font color=\"warning\">Recommendation：Check device power/network/service status</font>",
        priority_emoji,
        device.name,
        device.location,
        device.os,
        device.group,
        device.priority,
        detail
    );

    let payload = serde_json::json!({
        "msgtype": "markdown",
        "markdown": { "content": content }
    });

    // Silent sending + simple retry (avoid alert loss due to network jitter)
    for attempt in 1..=3 {
        match client.post(webhook).json(&payload).send().await {
            Ok(resp) if resp.status().is_success() => return,
            Ok(resp) => warn!(
                "WeCom alert failed (attempt {}): HTTP {}",
                attempt,
                resp.status()
            ),
            Err(e) => warn!("WeCom alert failed (attempt {}): {}", attempt, e),
        }

        if attempt < 3 {
            sleep(Duration::from_millis(500 * attempt as u64)).await;
        }
    }
}

// ────────────────────────────────────────────────────────────
// Default Config Generation (Auto-create if config.toml not exists + Permission Protection)
// ────────────────────────────────────────────────────────────

fn create_default_config(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    use std::fs::File;
    use std::io::Write;

    let default_content = r#"# ════════════════════════════════════════════════════════════
# Port-Sentinel-RS Config - Default Template (v2.2.6)
# Note: Copy [[device]] block to add new devices without modifying code
# Sensitive info should be injected via env vars: export WEBHOOK_URL="xxx"
# ════════════════════════════════════════════════════════════

# ── Global Settings ────────────────────────────────────────────────
[settings]
# Polling interval (seconds), min 5, recommended 15-60
interval = 15
# Single TCP connection timeout (seconds), range 1-30, 3 for intranet, 10 for public network
timeout = 3
# Alert cooldown for same device (seconds), avoid spamming, recommended 300 (5min)
alert_cooldown = 300
# WeCom robot webhook, supports ${WEBHOOK_URL} env var substitution
# Production recommendation: webhook = "${WEBHOOK_URL}"
webhook = "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=YOUR_KEY_HERE"
# Log level: debug | info | warn | error
log_level = "info"
# Max concurrent connections, adjust based on server performance, recommended = CPU cores * 10
max_concurrent_connections = 100

# ── Device Monitoring List (Flat structure, copy [[device]] to add) ───

[[device]]
id = "example-server-01"
name = "Example Server"
group = "example"
priority = "medium"           # critical | high | medium | low
ips = ["192.168.1.100"]
os = "linux"
location = "Room A/Rack 01"
checks = [
    { port = 22, name = "SSH Service" },
    { port = 80, name = "HTTP Service" },
    { port = 443, name = "HTTPS Service" }
]

# ── More Device Examples (Uncomment to use) ─────────────────────────

# [[device]]
# id = "redis-master-01"
# name = "Redis Master Node"
# group = "database"
# priority = "critical"
# ips = ["192.168.1.133", "192.168.1.128"]
# os = "linux"
# location = "Core Rack"
# checks = [{ port = 6379, name = "Redis Service" }]

# [[device]]
# id = "web-frontend-01"
# name = "Web Frontend Server"
# group = "web"
# priority = "high"
# ips = ["10.0.0.10", "10.0.0.11"]
# os = "windows"
# location = "Cloud/Shenzhen Region"
# checks = [
#     { port = 80, name = "HTTP" },
#     { port = 443, name = "HTTPS" },
#     { port = 3389, name = "RDP" }
# ]
"#;

    let mut file = File::create(path)?;
    file.write_all(default_content.as_bytes())?;

    // 🔐 Set config file permission to 600 on Unix systems (read/write only for owner)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = fs::metadata(path) {
            let mut perms = metadata.permissions();
            perms.set_mode(0o600);
            if let Err(e) = fs::set_permissions(path, perms) {
                warn!("Failed to set file permissions for {}: {}", path, e);
            }
        }
    }

    Ok(())
}

/// Ensure config file exists, create default config and exit to prompt user edit if not exists
fn ensure_config_exists(config_path: &str) -> Result<String, Box<dyn std::error::Error>> {
    let path = Path::new(config_path);

    // Config file exists, return directly
    if path.exists() {
        info!("✓ Config file detected: {}", config_path);
        return Ok(config_path.to_string());
    }

    // Create default config if not exists
    warn!(
        "⚙️  Config file not found, creating default config: {}",
        config_path
    );
    create_default_config(config_path)?;

    #[cfg(unix)]
    {
        println!("🔐 Config file permission set to 600 (read/write only for owner)");
    }

    println!("✅ Default config generated: {}", config_path);
    println!("📝 Please edit config file and restart program:");
    println!("   1. Modify webhook to WeCom robot address");
    println!("   2. Add devices to monitor");
    println!("   3. Optional: Inject sensitive info via export WEBHOOK_URL=xxx");
    println!();

    // 🔧 Fix: Use ${{}} escape to avoid Rust macro parsing errors
    println!("💡 Tip: ${{WEBHOOK_URL}} in config will be automatically replaced by env var");

    std::process::exit(0);
}

// ────────────────────────────────────────────────────────────
// Config Loading + Env Var Substitution + URL Auto-cleanup + Parameter Validation
// ────────────────────────────────────────────────────────────

fn load_config(path: &str) -> Result<Config, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(path)?;

    // 🔹 Env var substitution: support ${WEBHOOK_URL} syntax
    let content = env::var("WEBHOOK_URL")
        .map(|val| content.replace("${WEBHOOK_URL}", &val))
        .unwrap_or(content);

    let mut config: Config = toml::from_str(&content)?;

    // 🔹 Clean up whitespace around webhook
    config.settings.webhook = config.settings.webhook.trim().to_string();

    // 🔹 Parameter validation (detect config errors on startup)
    if config.settings.interval < 5 {
        return Err("interval cannot be less than 5 seconds".into());
    }
    if config.settings.timeout < 1 || config.settings.timeout > 30 {
        return Err("timeout should be between 1-30 seconds".into());
    }

    // 🔧 Fix: webhook empty check with friendly error message
    if config.settings.webhook.is_empty() || !config.settings.webhook.starts_with("http") {
        return Err(format!(
            "webhook URL is required and must start with http/https. Current value: '{}'. \
             Please set WEBHOOK_URL environment variable or edit config.toml",
            config.settings.webhook
        )
        .into());
    }

    if config.settings.max_concurrent_connections == 0 {
        return Err("max_concurrent_connections must be greater than 0".into());
    }

    Ok(config)
}

// ────────────────────────────────────────────────────────────
// Logging Initialization (ChronoLocal Compatibility + Clean Output)
// ────────────────────────────────────────────────────────────

fn init_logging(log_level: &str) {
    // Priority: env var LOG_LEVEL > config file log_level > default info
    let level = env::var("LOG_LEVEL")
        .ok()
        .or_else(|| Some(log_level.to_string()))
        .and_then(|l| match l.to_lowercase().as_str() {
            "debug" => Some(Level::DEBUG),
            "info" => Some(Level::INFO),
            "warn" => Some(Level::WARN),
            "error" => Some(Level::ERROR),
            _ => None,
        })
        .unwrap_or(Level::INFO);

    let timer = tracing_subscriber::fmt::time::ChronoLocal::new("%Y-%m-%dT%H:%M:%S%:z".to_string());

    let subscriber = FmtSubscriber::builder()
        .with_max_level(level)
        .with_thread_ids(false)
        .with_target(false)
        .with_file(false)
        .with_line_number(false)
        .with_level(true)
        .with_timer(timer)
        .finish();

    if let Err(e) = tracing::subscriber::set_global_default(subscriber) {
        eprintln!("Failed to initialize logging: {}", e);
    }
}

// ────────────────────────────────────────────────────────────
// Main Program Entry (Graceful Startup + Monitoring Loop + Signal Handling)
// ────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    // 🔹 1. Ensure config exists first (to get log level from config)
    let config_path = match ensure_config_exists("config.toml") {
        Ok(path) => path,
        Err(e) => {
            eprintln!("✗ Config initialization failed: {}", e);
            std::process::exit(1);
        }
    };

    // 🔹 2. Load config to get log level
    let config_for_log = match load_config(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("✗ Failed to load config for logging: {}", e);
            std::process::exit(1);
        }
    };

    // 🔹 3. Initialize logging system with config log level
    init_logging(&config_for_log.settings.log_level);

    // 🔹 4. Print startup banner
    println!();
    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║     🚀 port-sentinel-rs v2.2.6 (High Performance)         ║");
    println!("║        Keep Your Network Heartbeat Steady                ║");
    println!("╚══════════════════════════════════════════════════════════╝");
    println!();

    info!(
        "📅 Startup time: {}",
        Local::now().format("%Y-%m-%d %H:%M:%S")
    );

    // 🔹 5. Reload full config (with validation)
    let config = match load_config(&config_path) {
        Ok(c) => {
            info!("✓ Config loaded successfully");
            info!("  ├─ Device count: {}", c.devices.len());
            info!("  ├─ Polling interval: {}s", c.settings.interval);
            info!("  ├─ Connection timeout: {}s", c.settings.timeout);
            info!("  ├─ Alert cooldown: {}s", c.settings.alert_cooldown);
            info!(
                "  └─ Concurrent limit: {} connections",
                c.settings.max_concurrent_connections
            );
            println!();
            c
        }
        Err(e) => {
            error!("✗ Config load failed: {}", e);
            std::process::exit(1);
        }
    };

    // 🔹 6. Initialize shared resources
    let semaphore = Arc::new(Semaphore::new(config.settings.max_concurrent_connections));
    let alert_state = Arc::new(Mutex::new(AlertState::new()));
    let webhook = config.settings.webhook.clone();
    let timeout_sec = config.settings.timeout;
    let interval_sec = config.settings.interval;
    let cooldown_sec = config.settings.alert_cooldown;

    // 🔹 7. Graceful shutdown signal handling
    let shutdown_signal = async {
        let _ = tokio::signal::ctrl_c().await;
        println!();
        warn!("🛑 Shutdown signal received, exiting gracefully...");
    };

    // 🔹 8. Main monitoring loop (supports long-term stable operation)
    let monitor_loop = async {
        let mut round = 0u64;
        let mut total_alerts = 0u64;
        let mut recovered_count = 0u64;

        loop {
            round += 1;
            let round_start = Instant::now();

            // Print statistics separator every 10 rounds
            if round.is_multiple_of(10) {
                info!("╔══════════════════════════════════════════════════════════╗");
            }

            let mut tasks = tokio::task::JoinSet::new();

            // 🔹 Submit all device detection tasks concurrently
            for device in &config.devices {
                let dev = device.clone();
                let to_sec = timeout_sec;
                let sem = semaphore.clone();

                tasks.spawn(async move {
                    let (ok, failures) = check_device_parallel(&dev, to_sec, sem).await;
                    (dev, ok, failures)
                });
            }

            // 🔹 Collect detection results and aggregate failures by group
            let mut group_failures: HashMap<String, Vec<(Device, Vec<CheckFailure>)>> =
                HashMap::new();

            while let Some(result) = tasks.join_next().await {
                if let Ok((device, is_ok, failures)) = result {
                    if !is_ok {
                        group_failures
                            .entry(device.group.clone())
                            .or_default()
                            .push((device, failures));
                    } else {
                        // Device recovered: clear alert state and count
                        let mut state = alert_state.lock().await;
                        if state.mark_recovered(&device.id) {
                            recovered_count += 1;
                            info!("✅ Device recovered: {} ({})", device.name, device.id);
                        }
                    }
                }
            }

            // 🔹 Send alerts (with cooldown control + silent mode)
            let now_ts = Local::now().timestamp();
            let mut state = alert_state.lock().await;
            let mut new_alerts = 0u64;

            for failed_list in group_failures.values() {
                for (device, failures) in failed_list {
                    // Pass actual failure state to should_alert (fixed core bug)
                    if state.should_alert(&device.id, true, now_ts, cooldown_sec) {
                        new_alerts += 1;
                        total_alerts += 1;

                        // Temporarily release lock to avoid deadlock, re-acquire after sending
                        let webhook_clone = webhook.clone();
                        let dev_clone = device.clone();
                        let failures_clone = failures.clone();

                        drop(state);
                        send_wechat_alert(&webhook_clone, &dev_clone, &failures_clone).await;
                        state = alert_state.lock().await;
                    }
                }
            }

            let elapsed = round_start.elapsed().as_secs();

            // 🔹 Output current round results
            if group_failures.is_empty() {
                info!(
                    "✓ Round {:>3} | All devices normal | Elapsed: {}s",
                    round, elapsed
                );
            } else {
                warn!(
                    "⚠ Round {:>3} | {} devices failed | {} alerts sent | Elapsed: {}s",
                    round,
                    group_failures.values().map(|v| v.len()).sum::<usize>(),
                    new_alerts,
                    elapsed
                );
            }

            // 🔹 Output cumulative statistics every 10 rounds
            if round.is_multiple_of(10) {
                info!("╚══════════════════════════════════════════════════════════╝");
                info!(
                    "📊 Cumulative: {} rounds | {} alerts | {} recoveries",
                    round, total_alerts, recovered_count
                );
                println!();
            }

            // 🔹 Smart wait: ensure stable polling interval (subtract detection time)
            if elapsed < interval_sec {
                sleep(Duration::from_secs(interval_sec - elapsed)).await;
            }
            // If detection takes longer than interval, start next round immediately (avoid backlog)
        }
    };

    // 🔹 9. Wait concurrently: monitoring loop or shutdown signal (whichever comes first)
    tokio::select! {
        _ = monitor_loop => {},
        _ = shutdown_signal => {
            println!();
            info!("👋 System exited gracefully");
            println!("╔══════════════════════════════════════════════════════════╗");
            println!("║              Port-Sentinel-RS - Thank You!               ║");
            println!("╚══════════════════════════════════════════════════════════╝");
            println!();
        }
    }
}
