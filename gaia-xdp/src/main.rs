use std::{
    collections::{HashMap, VecDeque},
    fs,
    mem::size_of,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use std::convert::Infallible;

use anyhow::{Context, Result, anyhow};
use axum::{
    Json, Router,
    extract::{Multipart, Path as AxumPath, State},
    http::{HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response, sse::{Event, KeepAlive, Sse}},
    routing::{get, post},
};
use aya::{
    Ebpf,
    maps::{Array as BpfArray, HashMap as BpfHashMap, MapData, ring_buf::RingBuf},
    programs::{CgroupAttachMode, CgroupSockAddr, KProbe, TracePoint, UProbe},
};
use clap::Parser;
use gaia_xdp_common::{
    EVENT_ACTION_ALERT, EVENT_ACTION_BLOCKED, EVENT_ACTION_RATE_LIMITED,
    EVENT_KIND_FILE_IO, EVENT_KIND_HOTPATCH, EVENT_KIND_NETWORK, EVENT_KIND_PRIVILEGE,
    EVENT_KIND_PROCESS, HOTPATCH_ACTION_MONITOR, HOTPATCH_ACTION_OVERRIDE_RETURN,
    HOTPATCH_ACTION_REPLACE_FUNCTION, HOTPATCH_ACTION_SKIP_CALL,
    HotpatchPidEntry, HotpatchRuleEntry, KernelEvent, RateLimitEntry,
};
use log::{info, warn};
use object::{Object, ObjectSymbol};
use rust_embed::Embed;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{Interest, unix::AsyncFd},
    net::TcpListener,
    process::Command,
    signal,
    sync::{RwLock, broadcast},
};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt as TokioStreamExt;
use tower_http::cors::CorsLayer;

mod ai;
mod db;
mod livepatch;
mod process;
mod web;

/// WebUI static assets embedded at compile time.
#[derive(Embed)]
#[folder = "../gaia-webui/dist/"]
struct WebAssets;

const DEFAULT_CONFIG: &str = "gaia.toml";
const MAX_EVENT_HISTORY: usize = 512;
const MAX_ALERT_HISTORY: usize = 256;
/// How long (seconds) to suppress repeated baseline alerts for the same event kind.
const BASELINE_ALERT_COOLDOWN_SECS: u64 = 10;
/// Capacity of the broadcast channel for AI alert notifications.
const AI_ALERT_CHANNEL_CAP: usize = 64;
/// Maximum number of recent events to include as context in AI chat.
const AI_CONTEXT_EVENTS: usize = 20;

// ── AI Analysis System ──

/// Supported LLM providers.
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
enum AiProvider {
    #[default]
    OpenAi,
    Ollama,
    Custom,
}

/// Persisted AI configuration (stored alongside the main policy).
#[derive(Debug, Clone, Deserialize, Serialize)]
struct AiConfig {
    /// Whether the AI analysis feature is enabled.
    #[serde(default)]
    enabled: bool,
    /// LLM provider.
    #[serde(default)]
    provider: AiProvider,
    /// API base URL (overrides default for the provider).
    /// For Ollama this defaults to "http://localhost:11434".
    #[serde(default)]
    base_url: String,
    /// Model name, e.g. "gpt-4o", "llama3", "qwen2.5".
    #[serde(default = "default_ai_model")]
    model: String,
    /// API key (empty for Ollama / local models).
    #[serde(default)]
    api_key: String,
}

fn default_ai_model() -> String {
    "gpt-4o-mini".to_string()
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: AiProvider::OpenAi,
            base_url: String::new(),
            model: default_ai_model(),
            api_key: String::new(),
        }
    }
}

/// A high-severity alert notification pushed to AI event stream subscribers.
#[derive(Debug, Clone, Serialize)]
struct AiAlertNotification {
    /// ISO-8601 timestamp string.
    timestamp: String,
    /// Alert level: "critical" | "high".
    level: String,
    /// Human-readable reason.
    reason: String,
    /// Abbreviated event context.
    event_kind: String,
    event_action: String,
    pid: u32,
    comm: String,
    detail: String,
}

// ── CLI ──

#[derive(Debug, Parser)]
struct Opt {
    #[arg(long, default_value = DEFAULT_CONFIG)]
    config: PathBuf,
    #[arg(long, default_value = "0.0.0.0:17890")]
    web_listen: String,
    #[arg(long, default_value = "gaia-events.db")]
    db_path: String,
}

// ── Policy config (YAML / TOML) ──

#[derive(Debug, Clone, Deserialize, Serialize)]
struct MonitorPolicy {
    #[serde(default)]
    sensitive_prefixes: Vec<ToggleItem>,
    #[serde(default)]
    monitored_services: Vec<ToggleItem>,
    #[serde(default)]
    exec_whitelist_prefixes: Vec<ToggleItem>,
    #[serde(default)]
    blocked_ports: Vec<TogglePort>,
    #[serde(default)]
    baseline_thresholds: HashMap<String, u32>,
    #[serde(default)]
    hotpatch: HotpatchPolicy,
    #[serde(default)]
    rate_limit_rules: Vec<RateLimitRule>,
}

/// A simple string item with an enabled/disabled toggle.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct ToggleItem {
    value: String,
    #[serde(default = "default_true")]
    enabled: bool,
}

/// A port number with an enabled/disabled toggle.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct TogglePort {
    port: u16,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
struct HotpatchPolicy {
    #[serde(default)]
    targets: Vec<HotpatchTarget>,
    /// Kernel livepatch targets (replace kernel functions via klp_patch API).
    #[serde(default)]
    kernel_livepatch: Vec<livepatch::KernelLivepatchTarget>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct HotpatchTarget {
    binary: String,
    symbol: String,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default = "default_true")]
    enabled: bool,
    /// What the uprobe should do: "monitor", "override_return", "skip_call", or "replace_function".
    #[serde(default = "default_patch_action")]
    patch_action: PatchAction,
    /// The return value to force when patch_action is override_return or skip_call.
    /// Interpreted as a signed i64 (e.g. -1 for -EPERM, -22 for -EINVAL, 0 for success).
    #[serde(default)]
    override_return_value: i64,
    /// Path to the replacement shared library (.so) for replace_function.
    /// The .so must export a function with the same signature as the original.
    #[serde(default)]
    replace_lib: Option<String>,
    /// Symbol name in the replacement .so to use. Defaults to the same as `symbol` if omitted.
    #[serde(default)]
    replace_symbol: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
enum PatchAction {
    /// Just observe function entry/exit, don't interfere.
    #[default]
    Monitor,
    /// Force the function to return a specific value immediately (skip function body).
    OverrideReturn,
    /// Skip the function call entirely (semantic alias for override_return).
    SkipCall,
    /// Replace the function implementation with one from a shared library.
    /// Requires `replace_lib` to be set on the target.
    ReplaceFunction,
    /// Patch a kernel function using the Linux livepatch API (klp_patch).
    /// Requires root, CONFIG_LIVEPATCH=y, and kernel-devel headers.
    KernelLivepatch,
}

fn default_patch_action() -> PatchAction {
    PatchAction::Monitor
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct RateLimitRule {
    cidr: String,
    max_conn_per_sec: u32,
    #[serde(default)]
    action: RateLimitAction,
    #[serde(default)]
    enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
enum RateLimitAction {
    #[default]
    Log,
    Block,
    Throttle,
}

impl Default for MonitorPolicy {
    fn default() -> Self {
        Self {
            sensitive_prefixes: vec![
                ToggleItem { value: "/etc/shadow".into(), enabled: true },
                ToggleItem { value: "/etc/ssl".into(), enabled: true },
                ToggleItem { value: "/root/.ssh".into(), enabled: true },
            ],
            monitored_services: vec![
                ToggleItem { value: "sshd.service".into(), enabled: true },
                ToggleItem { value: "nginx.service".into(), enabled: true },
            ],
            exec_whitelist_prefixes: vec![
                ToggleItem { value: "/usr/bin".into(), enabled: true },
                ToggleItem { value: "/usr/sbin".into(), enabled: true },
            ],
            blocked_ports: vec![
                TogglePort { port: 4444, enabled: true },
                TogglePort { port: 31337, enabled: true },
            ],
            baseline_thresholds: HashMap::from([
                ("file_io".to_string(), 50),
                ("process".to_string(), 20),
                ("privilege".to_string(), 5),
                ("network".to_string(), 40),
                ("hotpatch".to_string(), 10),
            ]),
            hotpatch: HotpatchPolicy::default(),
            rate_limit_rules: Vec::new(),
        }
    }
}

// ── Data types ──

#[derive(Debug, Clone, Serialize)]
struct EventRecord {
    timestamp_ns: u64,
    kind: String,
    action: String,
    pid: u32,
    tgid: u32,
    uid: u32,
    gid: u32,
    comm: String,
    detail: String,
    network: Option<NetworkView>,
    /// The systemd service this event belongs to, if the PID is tracked.
    #[serde(skip_serializing_if = "Option::is_none")]
    service: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct NetworkView {
    port: u16,
    address: String,
}

#[derive(Debug, Clone, Serialize)]
struct AlertRecord {
    level: String,
    reason: String,
    event: EventRecord,
}

#[derive(Debug, Clone, Serialize)]
struct Snapshot {
    features: FeatureStatus,
    counters: HashMap<String, u64>,
    services: HashMap<String, ServiceStatus>,
    events: Vec<EventRecord>,
    alerts: Vec<AlertRecord>,
    traffic: TrafficSnapshot,
}

/// Systemd service status as reported by `systemctl show`.
#[derive(Debug, Clone, Serialize)]
struct ServiceStatus {
    /// e.g. "active", "inactive", "failed"
    active_state: String,
    /// e.g. "running", "dead", "exited", "waiting"
    sub_state: String,
    /// Combined human-readable form, e.g. "active (running)"
    state: String,
    /// PIDs belonging to this service's cgroup.
    pids: Vec<u32>,
}

/// Real-time network traffic data (bytes per second, sampled every 1s).
#[derive(Debug, Clone, Serialize, Default)]
struct TrafficSnapshot {
    /// Per-interface traffic rates.
    interfaces: Vec<NetIfTraffic>,
    /// Aggregate inbound bytes/sec across all non-lo interfaces.
    total_rx_bytes_per_sec: u64,
    /// Aggregate outbound bytes/sec across all non-lo interfaces.
    total_tx_bytes_per_sec: u64,
}

#[derive(Debug, Clone, Serialize)]
struct NetIfTraffic {
    name: String,
    rx_bytes_per_sec: u64,
    tx_bytes_per_sec: u64,
    rx_packets_per_sec: u64,
    tx_packets_per_sec: u64,
    /// Cumulative counters (for reference).
    rx_bytes_total: u64,
    tx_bytes_total: u64,
}

#[derive(Debug, Clone, Serialize)]
struct FeatureStatus {
    file_io_agent: bool,
    process_agent: bool,
    network_agent: bool,
    hotpatch_agent: bool,
    anomaly_engine: bool,
    symbol_resolver: bool,
}

// ── Runtime state ──

#[derive(Debug)]
struct BaselineState {
    /// Per-kind: (window_start, count_in_current_1s_window)
    windows: HashMap<String, (Instant, u32)>,
    /// Per-kind: when the last baseline alert was fired (for cooldown).
    last_alert: HashMap<String, Instant>,
}

#[derive(Debug)]
struct RuntimeState {
    counters: HashMap<String, u64>,
    service_map: HashMap<String, ServiceStatus>,
    /// Reverse lookup: PID → service name, rebuilt by the service tracker.
    pid_to_service: HashMap<u32, String>,
    /// Reverse lookup: comm name (e.g. "sshd") → service name, for matching
    /// short-lived child processes that may not yet appear in pid_to_service.
    comm_to_service: HashMap<String, String>,
    events: VecDeque<EventRecord>,
    alerts: VecDeque<AlertRecord>,
    baseline: BaselineState,
    traffic: TrafficSnapshot,
}

impl RuntimeState {
    fn new() -> Self {
        Self {
            counters: HashMap::new(),
            service_map: HashMap::new(),
            pid_to_service: HashMap::new(),
            comm_to_service: HashMap::new(),
            events: VecDeque::new(),
            alerts: VecDeque::new(),
            baseline: BaselineState {
                windows: HashMap::new(),
                last_alert: HashMap::new(),
            },
            traffic: TrafficSnapshot::default(),
        }
    }
}

/// Anchor for converting bpf_ktime_get_ns() (CLOCK_BOOTTIME, nanoseconds since boot)
/// to Unix epoch milliseconds. Sampled once at startup by reading both clocks as close
/// together as possible.
#[derive(Clone, Copy)]
struct KtimeAnchor {
    /// Unix epoch milliseconds at the moment the anchor was taken.
    unix_epoch_ms: u64,
    /// bpf_ktime_get_ns() value read from /proc/timer_list (or estimated via
    /// /proc/uptime) at the same moment.
    ktime_ns: u64,
}

impl KtimeAnchor {
    /// Sample the anchor by reading /proc/uptime (seconds since boot) and
    /// SystemTime::now() together. This avoids needing root or a BPF call.
    fn sample() -> Self {
        let uptime_content = fs::read_to_string("/proc/uptime").unwrap_or_default();
        let uptime_secs: f64 = uptime_content
            .split_whitespace()
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let unix_epoch_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let ktime_ns = (uptime_secs * 1_000_000_000.0) as u64;
        Self { unix_epoch_ms, ktime_ns }
    }

    /// Convert a bpf_ktime_get_ns() timestamp to Unix epoch milliseconds.
    fn to_epoch_ms(self, ktime_ns: u64) -> u64 {
        let delta_ms = (ktime_ns as i64 - self.ktime_ns as i64) / 1_000_000;
        (self.unix_epoch_ms as i64 + delta_ms).max(0) as u64
    }
}

#[derive(Clone)]
struct Shared {
    policy: Arc<RwLock<MonitorPolicy>>,
    config_path: PathBuf,
    runtime: Arc<RwLock<RuntimeState>>,
    bpf: Arc<Mutex<Ebpf>>,
    hotpatch_active: Arc<Mutex<bool>>,
    symbol_resolver_ok: Arc<Mutex<bool>>,
    /// Tracks code patches applied via process_vm_writev so they can be restored.
    code_patches: Arc<Mutex<Vec<PatchedFunction>>>,
    /// AI analysis system configuration.
    ai_config: Arc<RwLock<AiConfig>>,
    /// Broadcast channel for pushing high-severity alert notifications to AI event stream.
    ai_alert_tx: broadcast::Sender<AiAlertNotification>,
    /// Anchor for ktime → Unix epoch conversion.
    ktime_anchor: KtimeAnchor,
    /// SQLite event history database.
    db: Arc<db::EventDb>,
    /// Kernel livepatch module manager.
    livepatch_mgr: Arc<tokio::sync::Mutex<livepatch::LivepatchManager>>,
}

// ── main ──

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let opt = Opt::parse();
    let policy = load_policy(&opt.config).context("load policy")?;
    info!("policy loaded from {}", opt.config.display());

    let mut bpf = Ebpf::load(aya::include_bytes_aligned!(concat!(
        env!("OUT_DIR"),
        "/gaia-xdp"
    )))
    .context("load eBPF object")?;
    info!("eBPF object loaded");

    attach_agents(&mut bpf).context("attach eBPF agents")?;
    apply_blocked_ports(&mut bpf, &policy.blocked_ports).context("configure blocked ports")?;
    apply_rate_limit_rules(&mut bpf, &policy.rate_limit_rules).context("configure rate limits")?;

    let (ai_alert_tx, _) = broadcast::channel::<AiAlertNotification>(AI_ALERT_CHANNEL_CAP);
    // Load AI config from a sidecar file next to the main config.
    let ai_config = ai::load_ai_config(&opt.config);
    info!("AI analysis system: enabled={}", ai_config.enabled);

    // Sample ktime anchor before the event loop starts so timestamps can be
    // converted from CLOCK_BOOTTIME to Unix epoch milliseconds.
    let ktime_anchor = KtimeAnchor::sample();
    info!(
        "ktime anchor: unix_epoch_ms={}, ktime_ns={}",
        ktime_anchor.unix_epoch_ms, ktime_anchor.ktime_ns
    );

    // Open SQLite event history database.
    let event_db = db::EventDb::open(&opt.db_path)
        .await
        .with_context(|| format!("open event db at {}", opt.db_path))?;
    info!("event history db opened: {}", opt.db_path);

    let livepatch_build_dir = std::env::temp_dir().join("gaia-livepatch");
    let shared = Shared {
        policy: Arc::new(RwLock::new(policy.clone())),
        config_path: opt.config.clone(),
        runtime: Arc::new(RwLock::new(RuntimeState::new())),
        bpf: Arc::new(Mutex::new(bpf)),
        hotpatch_active: Arc::new(Mutex::new(false)),
        symbol_resolver_ok: Arc::new(Mutex::new(true)),
        code_patches: Arc::new(Mutex::new(Vec::new())),
        ai_config: Arc::new(RwLock::new(ai_config)),
        ai_alert_tx,
        ktime_anchor,
        db: Arc::new(event_db),
        livepatch_mgr: Arc::new(tokio::sync::Mutex::new(
            livepatch::LivepatchManager::new(livepatch_build_dir),
        )),
    };

    // The kprobe guard was already attached in attach_agents(), so the
    // hot-patch agent is active even when no uprobe targets are configured.
    if let Ok(mut h) = shared.hotpatch_active.lock() {
        *h = true;
    }

    // Hotpatch uprobe targets are best-effort: failure to attach should not crash.
    {
        let mut bpf_guard = shared.bpf.lock().unwrap();
        match attach_hotpatch_targets(&mut bpf_guard, &policy.hotpatch.targets, &shared) {
            Ok(()) => {
                if !policy.hotpatch.targets.is_empty() {
                    info!(
                        "hot-patch agent attached {} uprobe target(s)",
                        policy.hotpatch.targets.len()
                    );
                }
            }
            Err(err) => warn!("hot-patch uprobe attach skipped: {err:#}"),
        }
    }

    // Sync hotpatch rules to BPF maps (must happen after bpf_guard is dropped above)
    sync_bpf_hotpatch_pids(&shared, &policy.hotpatch.targets);
    sync_bpf_hotpatch_rules(&shared, &policy.hotpatch.targets);

    // Apply runtime code patches for override_return / skip_call targets.
    // This uses process_vm_writev to directly modify the target function's code.
    {
        let mut patches = shared.code_patches.lock().unwrap();
        apply_hotpatch_code_patches(&policy.hotpatch.targets, &mut patches);
    }

    {
        let mut bpf_guard = shared.bpf.lock().unwrap();
        let events = bpf_guard.take_map("EVENTS").context("missing EVENTS map")?;
        let events = RingBuf::<MapData>::try_from(events).context("open EVENTS ring buffer")?;
        spawn_event_collector(events, shared.clone());
    }

    spawn_service_tracker(shared.clone());
    spawn_traffic_sampler(shared.clone());

    let web_state = shared.clone();
    let web_listen = opt.web_listen.clone();
    tokio::spawn(async move {
        if let Err(err) = web::run_http_server(web_state, web_listen).await {
            warn!("web api stopped: {err:#}");
        }
    });

    // Clean up any gaia_klp_* modules left in the kernel from a previous run.
    {
        let mut mgr = shared.livepatch_mgr.lock().await;
        mgr.cleanup_stale_modules().await;
    }

    // Apply kernel livepatches from config (best-effort; failures are logged but don't abort).
    apply_kernel_livepatches(&shared, &policy.hotpatch.kernel_livepatch).await;

    info!("gaia controller started — press Ctrl-C to stop");
    signal::ctrl_c().await.context("waiting for ctrl-c")?;
    info!("gaia controller exiting");

    // Clean up kernel livepatches on exit.
    {
        let mut mgr = shared.livepatch_mgr.lock().await;
        mgr.remove_all().await;
    }

    Ok(())
}

/// Apply all enabled kernel livepatch targets from the policy.
async fn apply_kernel_livepatches(
    shared: &Shared,
    targets: &[livepatch::KernelLivepatchTarget],
) {
    let enabled: Vec<_> = targets.iter().filter(|t| t.enabled).cloned().collect();
    if enabled.is_empty() {
        return;
    }

    match livepatch::check_livepatch_support().await {
        Ok(()) => info!("kernel livepatch: support confirmed"),
        Err(e) => {
            warn!("kernel livepatch: support check failed — skipping: {e:#}");
            return;
        }
    }

    let mut mgr = shared.livepatch_mgr.lock().await;
    match mgr.apply(&enabled).await {
        Ok(name) => info!("kernel livepatch: module {name} applied ({} func(s))", enabled.len()),
        Err(e) => warn!("kernel livepatch: failed to apply: {e:#}"),
    }
}

// ── eBPF attach ──

fn attach_agents(bpf: &mut Ebpf) -> Result<()> {
    // File I/O Agent
    attach_tracepoint(bpf, "tp_sys_enter_openat", "syscalls", "sys_enter_openat")?;
    attach_tracepoint(bpf, "tp_sys_exit_openat", "syscalls", "sys_exit_openat")?;
    info!("file I/O agent attached");

    // Process & Privilege Agent
    attach_tracepoint(bpf, "tp_sys_enter_execve", "syscalls", "sys_enter_execve")?;
    attach_tracepoint(bpf, "tp_sys_enter_setuid", "syscalls", "sys_enter_setuid")?;
    attach_tracepoint(bpf, "tp_sys_enter_setgid", "syscalls", "sys_enter_setgid")?;
    info!("process & privilege agent attached");

    // Network Telemetry Agent
    attach_tracepoint(bpf, "tp_sys_enter_connect", "syscalls", "sys_enter_connect")?;
    attach_tracepoint(bpf, "tp_sys_enter_bind", "syscalls", "sys_enter_bind")?;
    info!("network telemetry agent attached");

    // Hot-patching Agent (kprobe guard)
    let prog: &mut KProbe = bpf
        .program_mut("kprobe_hotpatch_guard")
        .context("missing kprobe_hotpatch_guard")?
        .try_into()
        .context("kprobe cast")?;
    prog.load().context("load kprobe_hotpatch_guard")?;
    prog.attach("tcp_connect", 0)
        .context("attach kprobe tcp_connect")?;
    info!("hot-patch kprobe guard attached on tcp_connect");

    // Port Blocking Agent (cgroup/connect4 + cgroup/bind4)
    // Attach to root cgroup so it applies system-wide.
    let cgroup_path = find_cgroup_root()?;
    let cgroup_fd = std::fs::File::open(&cgroup_path)
        .with_context(|| format!("open cgroup root {}", cgroup_path.display()))?;

    let prog: &mut CgroupSockAddr = bpf
        .program_mut("cgroup_connect4")
        .context("missing cgroup_connect4")?
        .try_into()
        .context("cgroup_connect4 cast")?;
    prog.load().context("load cgroup_connect4")?;
    prog.attach(&cgroup_fd, CgroupAttachMode::Single)
        .context("attach cgroup_connect4")?;
    info!("port blocking agent attached: cgroup/connect4 on {}", cgroup_path.display());

    let prog: &mut CgroupSockAddr = bpf
        .program_mut("cgroup_bind4")
        .context("missing cgroup_bind4")?
        .try_into()
        .context("cgroup_bind4 cast")?;
    prog.load().context("load cgroup_bind4")?;
    prog.attach(&cgroup_fd, CgroupAttachMode::Single)
        .context("attach cgroup_bind4")?;
    info!("port blocking agent attached: cgroup/bind4 on {}", cgroup_path.display());

    let prog: &mut CgroupSockAddr = bpf
        .program_mut("cgroup_connect6")
        .context("missing cgroup_connect6")?
        .try_into()
        .context("cgroup_connect6 cast")?;
    prog.load().context("load cgroup_connect6")?;
    prog.attach(&cgroup_fd, CgroupAttachMode::Single)
        .context("attach cgroup_connect6")?;
    info!("port blocking agent attached: cgroup/connect6 on {}", cgroup_path.display());

    let prog: &mut CgroupSockAddr = bpf
        .program_mut("cgroup_bind6")
        .context("missing cgroup_bind6")?
        .try_into()
        .context("cgroup_bind6 cast")?;
    prog.load().context("load cgroup_bind6")?;
    prog.attach(&cgroup_fd, CgroupAttachMode::Single)
        .context("attach cgroup_bind6")?;
    info!("port blocking agent attached: cgroup/bind6 on {}", cgroup_path.display());

    Ok(())
}

/// Find the root cgroup v2 mount point.
///
/// Supports three common configurations:
/// 1. Pure cgroup v2: `/sys/fs/cgroup` is a cgroup2 mount.
/// 2. Hybrid mode:   `/sys/fs/cgroup/unified` is the cgroup2 mount.
/// 3. Fallback:      parse `/proc/mounts` for any `cgroup2` type entry.
fn find_cgroup_root() -> Result<PathBuf> {
    // Well-known candidate paths (pure v2 first, then hybrid unified mount)
    for path in &["/sys/fs/cgroup", "/sys/fs/cgroup/unified"] {
        let p = Path::new(path);
        if p.join("cgroup.controllers").exists() {
            return Ok(p.to_path_buf());
        }
    }

    // Fallback: scan /proc/mounts for a cgroup2 filesystem
    if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
        for line in mounts.lines() {
            // Format: <device> <mountpoint> <fstype> <options> <dump> <pass>
            let mut parts = line.splitn(6, ' ');
            let _device = parts.next();
            let mountpoint = parts.next().unwrap_or("");
            let fstype = parts.next().unwrap_or("");
            if fstype == "cgroup2" {
                let p = Path::new(mountpoint);
                if p.join("cgroup.controllers").exists() {
                    return Ok(p.to_path_buf());
                }
            }
        }
    }

    anyhow::bail!(
        "could not find cgroup v2 root mount point; \
         ensure cgroup v2 is mounted (e.g. `mount -t cgroup2 none /sys/fs/cgroup` \
         or set systemd.unified_cgroup_hierarchy=1 in kernel cmdline)"
    )
}

fn attach_tracepoint(bpf: &mut Ebpf, prog_name: &str, category: &str, name: &str) -> Result<()> {
    let prog: &mut TracePoint = bpf
        .program_mut(prog_name)
        .with_context(|| format!("missing {prog_name}"))?
        .try_into()
        .with_context(|| format!("tracepoint cast {prog_name}"))?;
    prog.load()
        .with_context(|| format!("load tracepoint {prog_name}"))?;
    prog.attach(category, name)
        .with_context(|| format!("attach tracepoint {category}/{name}"))?;
    Ok(())
}

fn apply_blocked_ports(bpf: &mut Ebpf, blocked_ports: &[TogglePort]) -> Result<()> {
    let map = bpf
        .map_mut("BLOCKED_PORTS")
        .context("missing BLOCKED_PORTS map")?;
    let mut ports = BpfHashMap::<_, u16, u8>::try_from(map).context("blocked ports map cast")?;
    // Clear existing entries
    let existing: Vec<u16> = ports.keys().filter_map(|k| k.ok()).collect();
    for k in existing {
        let _ = ports.remove(&k);
    }
    // Only insert enabled ports
    let active: Vec<u16> = blocked_ports.iter().filter(|p| p.enabled).map(|p| p.port).collect();
    for port in &active {
        ports
            .insert(port, &1u8, 0)
            .with_context(|| format!("insert blocked port {port}"))?;
    }
    info!("blocked ports configured: {active:?} ({} enabled / {} total)", active.len(), blocked_ports.len());
    Ok(())
}

fn apply_rate_limit_rules(bpf: &mut Ebpf, rules: &[RateLimitRule]) -> Result<()> {
    // Update rule count
    {
        let map = bpf
            .map_mut("RATE_LIMIT_RULE_COUNT")
            .context("missing RATE_LIMIT_RULE_COUNT map")?;
        let mut arr = BpfArray::<_, u32>::try_from(map).context("rule count array cast")?;
        arr.set(0, &(rules.len() as u32), 0)
            .context("set rule count")?;
    }

    // Update rules map
    {
        let map = bpf
            .map_mut("RATE_LIMIT_RULES")
            .context("missing RATE_LIMIT_RULES map")?;
        let mut rule_map =
            BpfHashMap::<_, u32, RateLimitEntry>::try_from(map).context("rules map cast")?;
        // Clear existing
        let existing: Vec<u32> = rule_map.keys().filter_map(|k| k.ok()).collect();
        for k in existing {
            let _ = rule_map.remove(&k);
        }
        // Insert new rules
        for (idx, rule) in rules.iter().enumerate() {
            if let Some(entry) = parse_rate_limit_to_entry(rule) {
                rule_map
                    .insert(&(idx as u32), &entry, 0)
                    .with_context(|| format!("insert rate limit rule {idx}"))?;
            }
        }
    }

    info!("rate limit rules configured: {} rule(s)", rules.len());
    Ok(())
}

fn parse_rate_limit_to_entry(rule: &RateLimitRule) -> Option<RateLimitEntry> {
    let (addr, prefix_len) = parse_cidr(&rule.cidr)?;
    let mut network = [0u8; 16];
    let octets = addr.octets();
    network[0] = octets[0];
    network[1] = octets[1];
    network[2] = octets[2];
    network[3] = octets[3];

    let action = match rule.action {
        RateLimitAction::Log => gaia_xdp_common::RATE_ACTION_LOG,
        RateLimitAction::Block => gaia_xdp_common::RATE_ACTION_BLOCK,
        RateLimitAction::Throttle => gaia_xdp_common::RATE_ACTION_THROTTLE,
    };

    Some(RateLimitEntry {
        network,
        prefix_len,
        action,
        enabled: if rule.enabled { 1 } else { 0 },
        _pad: 0,
        max_conn_per_sec: rule.max_conn_per_sec,
    })
}

fn parse_cidr(cidr: &str) -> Option<(Ipv4Addr, u8)> {
    if let Some((addr_str, prefix_str)) = cidr.split_once('/') {
        let addr: Ipv4Addr = addr_str.parse().ok()?;
        let prefix: u8 = prefix_str.parse().ok()?;
        if prefix > 32 {
            return None;
        }
        Some((addr, prefix))
    } else {
        // Treat as /32
        let addr: Ipv4Addr = cidr.parse().ok()?;
        Some((addr, 32))
    }
}

fn sync_bpf_blocked_ports(shared: &Shared, blocked_ports: &[TogglePort]) {
    if let Ok(mut bpf) = shared.bpf.lock() {
        if let Err(err) = apply_blocked_ports(&mut bpf, blocked_ports) {
            warn!("failed to sync blocked ports to BPF: {err:#}");
        }
    }
}

fn sync_bpf_rate_limits(shared: &Shared, rules: &[RateLimitRule]) {
    if let Ok(mut bpf) = shared.bpf.lock() {
        if let Err(err) = apply_rate_limit_rules(&mut bpf, rules) {
            warn!("failed to sync rate limit rules to BPF: {err:#}");
        }
    }
}

fn sync_bpf_hotpatch_pids(shared: &Shared, targets: &[HotpatchTarget]) {
    if let Ok(mut bpf) = shared.bpf.lock() {
        let map = match bpf.map_mut("HOTPATCH_PIDS") {
            Some(map) => map,
            None => {
                warn!("missing HOTPATCH_PIDS map");
                return;
            }
        };
        let mut pid_map = match BpfHashMap::<_, u32, HotpatchPidEntry>::try_from(map) {
            Ok(m) => m,
            Err(err) => {
                warn!("hotpatch pids map cast failed: {err:#}");
                return;
            }
        };
        // Clear existing
        let existing: Vec<u32> = pid_map.keys().filter_map(|k| k.ok()).collect();
        for k in existing {
            let _ = pid_map.remove(&k);
        }
        // Insert PIDs from enabled targets only
        for target in targets.iter().filter(|t| t.enabled) {
            if let Some(pid) = target.pid {
                let entry = HotpatchPidEntry {
                    active: 1,
                    _pad: [0; 3],
                };
                let _ = pid_map.insert(&pid, &entry, 0);
            }
        }
        let enabled = targets.iter().filter(|t| t.enabled).count();
        info!("hotpatch PID filter updated: {} enabled / {} total", enabled, targets.len());
    }
}

/// Sync hotpatch rules to the HOTPATCH_RULES BPF map.
/// Each enabled target with a non-monitor patch_action gets a rule entry
/// that the eBPF uprobe reads to decide whether to override the function return.
fn sync_bpf_hotpatch_rules(shared: &Shared, targets: &[HotpatchTarget]) {
    if let Ok(mut bpf) = shared.bpf.lock() {
        // Update rule count
        {
            let map = match bpf.map_mut("HOTPATCH_RULE_COUNT") {
                Some(m) => m,
                None => {
                    warn!("missing HOTPATCH_RULE_COUNT map");
                    return;
                }
            };
            let mut arr = match BpfArray::<_, u32>::try_from(map) {
                Ok(a) => a,
                Err(err) => {
                    warn!("HOTPATCH_RULE_COUNT cast failed: {err:#}");
                    return;
                }
            };
            let count = targets.iter().filter(|t| t.enabled).count() as u32;
            let _ = arr.set(0, &count, 0);
        }

        // Update rules map
        {
            let map = match bpf.map_mut("HOTPATCH_RULES") {
                Some(m) => m,
                None => {
                    warn!("missing HOTPATCH_RULES map");
                    return;
                }
            };
            let mut rule_map =
                match BpfHashMap::<_, u32, HotpatchRuleEntry>::try_from(map) {
                    Ok(m) => m,
                    Err(err) => {
                        warn!("HOTPATCH_RULES cast failed: {err:#}");
                        return;
                    }
                };
            // Clear existing
            let existing: Vec<u32> = rule_map.keys().filter_map(|k| k.ok()).collect();
            for k in existing {
                let _ = rule_map.remove(&k);
            }
            // Insert rules for enabled targets
            let mut idx: u32 = 0;
            for target in targets {
                let action = match target.patch_action {
                    PatchAction::Monitor => HOTPATCH_ACTION_MONITOR,
                    PatchAction::OverrideReturn => HOTPATCH_ACTION_OVERRIDE_RETURN,
                    PatchAction::SkipCall => HOTPATCH_ACTION_SKIP_CALL,
                    PatchAction::ReplaceFunction => HOTPATCH_ACTION_REPLACE_FUNCTION,
                    // Kernel livepatches are managed separately; skip in eBPF rules.
                    PatchAction::KernelLivepatch => continue,
                };
                let entry = HotpatchRuleEntry {
                    action,
                    enabled: if target.enabled { 1 } else { 0 },
                    _pad: [0; 2],
                    override_return_value: target.override_return_value,
                    target_pid: target.pid.unwrap_or(0),
                    _pad2: [0; 4],
                };
                let _ = rule_map.insert(&idx, &entry, 0);
                idx += 1;
            }
        }

        let active_overrides = targets
            .iter()
            .filter(|t| t.enabled && t.patch_action != PatchAction::Monitor)
            .count();
        info!(
            "hotpatch rules synced: {} total, {} active override/skip rules",
            targets.len(),
            active_overrides
        );
    }
}

fn attach_hotpatch_targets(
    bpf: &mut Ebpf,
    targets: &[HotpatchTarget],
    shared: &Shared,
) -> Result<()> {
    let enabled_targets: Vec<&HotpatchTarget> = targets.iter().filter(|t| t.enabled).collect();
    if enabled_targets.is_empty() {
        return Ok(());
    }

    // Load uprobe programs (idempotent: skip if already loaded)
    {
        let entry: &mut UProbe = bpf
            .program_mut("uprobe_hotpatch_entry")
            .context("missing uprobe_hotpatch_entry")?
            .try_into()
            .context("cast uprobe_hotpatch_entry")?;
        // load() fails if already loaded — that's fine
        let _ = entry.load();
    }
    {
        let exit: &mut UProbe = bpf
            .program_mut("uretprobe_hotpatch_exit")
            .context("missing uretprobe_hotpatch_exit")?
            .try_into()
            .context("cast uretprobe_hotpatch_exit")?;
        let _ = exit.load();
    }

    for target in &enabled_targets {
        if target.pid == Some(0) {
            anyhow::bail!("hotpatch PID must not be 0");
        }
        let binary_path = Path::new(&target.binary);
        if !binary_path.exists() {
            warn!(
                "hotpatch target binary not found: {} — skipping",
                target.binary
            );
            continue;
        }

        // Attach uprobe entry
        if let Err(err) = (|| -> Result<()> {
            let entry: &mut UProbe = bpf
                .program_mut("uprobe_hotpatch_entry")
                .context("missing")?
                .try_into()
                .context("cast")?;
            entry
                .attach(target.symbol.as_str(), &target.binary, target.pid)
                .context("attach")?;
            Ok(())
        })() {
            warn!(
                "uprobe attach failed {}:{} — {err:#}",
                target.binary, target.symbol
            );
            continue;
        }

        // Attach uretprobe exit
        if let Err(err) = (|| -> Result<()> {
            let exit: &mut UProbe = bpf
                .program_mut("uretprobe_hotpatch_exit")
                .context("missing")?
                .try_into()
                .context("cast")?;
            exit.attach(target.symbol.as_str(), &target.binary, target.pid)
                .context("attach")?;
            Ok(())
        })() {
            warn!(
                "uretprobe attach failed {}:{} — {err:#}",
                target.binary, target.symbol
            );
            continue;
        }

        info!(
            "hotpatch uprobe/uretprobe attached: {}:{}",
            target.binary, target.symbol
        );

        // Symbol resolver: resolve runtime address (informational)
        if let Some(pid) = target.pid {
            match resolve_runtime_symbol(pid, binary_path, &target.symbol) {
                Ok(addr) => info!(
                    "symbol resolved {}:{} pid={} => 0x{addr:x}",
                    target.binary, target.symbol, pid
                ),
                Err(err) => {
                    if let Ok(mut guard) = shared.symbol_resolver_ok.lock() {
                        *guard = false;
                    }
                    warn!(
                        "symbol resolver failed {}:{} pid={}: {err:#}",
                        target.binary, target.symbol, pid
                    );
                }
            }
        }
    }

    Ok(())
}

// ── Runtime code patching via process_vm_writev ──
//
// bpf_override_return only works with kprobes on error-injectable kernel functions.
// For userspace function patching, we directly modify the target function's machine
// code using process_vm_writev. On aarch64, we replace the first two instructions
// with: `mov x0, #value; ret` (or `movn x0, #~value; ret` for negative values).
// The original instructions are saved so they can be restored later.

/// Saved original instructions for a patched function, keyed by (pid, address).
#[derive(Debug, Clone)]
struct PatchedFunction {
    pid: u32,
    address: u64,
    /// Original bytes at the patch site. Length depends on patch type:
    /// - override_return/skip_call: 8 bytes (2 aarch64 instructions)
    /// - replace_function: 16 bytes (4 aarch64 instructions for trampoline)
    original_bytes: Vec<u8>,
}

/// Apply runtime code patches for hotpatch targets that use override_return, skip_call,
/// or replace_function. This modifies the target process's function code in-place using
/// ptrace POKETEXT, which can write to read-only .text segment pages.
///
/// A PID must be explicitly specified in the target configuration. Targets without
/// a PID are skipped with a warning (hotpatching is only supported for long-running
/// processes like databases or systemd services).
fn apply_hotpatch_code_patches(
    targets: &[HotpatchTarget],
    patches: &mut Vec<PatchedFunction>,
) {
    for target in targets {
        if !target.enabled {
            continue;
        }
        if target.patch_action == PatchAction::Monitor {
            continue;
        }

        let Some(pid) = target.pid else {
            warn!(
                "hotpatch target {}:{} has no PID — skipping (hotpatch requires an explicit PID for long-running processes)",
                target.binary, target.symbol
            );
            continue;
        };

        let binary_path = Path::new(&target.binary);
        if !binary_path.exists() {
            warn!("hotpatch binary not found: {} — skipping patch", target.binary);
            continue;
        }

        if target.patch_action == PatchAction::ReplaceFunction {
            apply_replace_function_patch(pid, target, binary_path, patches);
        } else {
            let patch_bytes = generate_patch_instructions(target.override_return_value);
            apply_single_code_patch(pid, target, binary_path, &patch_bytes, patches);
        }
    }
}

/// Apply a code patch to a single (pid, target) pair.
fn apply_single_code_patch(
    pid: u32,
    target: &HotpatchTarget,
    binary_path: &Path,
    patch_bytes: &[u8; 8],
    patches: &mut Vec<PatchedFunction>,
) {
    // Resolve the runtime address of the target function
    let func_addr = match resolve_runtime_symbol(pid, binary_path, &target.symbol) {
        Ok(addr) => addr,
        Err(err) => {
            warn!(
                "cannot resolve {}:{} pid={} for code patch: {err:#}",
                target.binary, target.symbol, pid
            );
            return;
        }
    };

    // Check if already patched at this address
    if patches.iter().any(|p| p.pid == pid && p.address == func_addr) {
        info!(
            "hotpatch already applied at {}:{} pid={} addr=0x{:x}",
            target.binary, target.symbol, pid, func_addr
        );
        return;
    }

    // Read original instructions and write patch in a single ptrace session
    let result = ptrace_read_and_write(pid, func_addr, patch_bytes);
    match result {
        Ok(original_bytes) => {
            patches.push(PatchedFunction {
                pid,
                address: func_addr,
                original_bytes,
            });
            info!(
                "hotpatch code patch applied: {}:{} pid={} addr=0x{:x} override_return_value={}",
                target.binary, target.symbol, pid, func_addr, target.override_return_value
            );
        }
        Err(err) => {
            warn!(
                "failed to apply code patch at {}:{} pid={} addr=0x{:x}: {err:#}",
                target.binary, target.symbol, pid, func_addr
            );
        }
    }
}

// ── Live function replacement via dlopen injection + trampoline ──
//
// To replace a function in a running process with a new implementation from a .so:
//
// 1. Inject the .so into the target process by calling __libc_dlopen_mode() via ptrace.
//    We use the internal glibc symbol because the target may not link libdl.
//    The injection writes a small shellcode snippet into the process's stack, sets
//    PC to it, single-steps, then restores everything.
//
// 2. Resolve the replacement function's address in the target process by parsing
//    /proc/<pid>/maps to find where the .so was loaded, then adding the symbol offset.
//
// 3. Write a trampoline at the original function's entry point that jumps to the
//    replacement function. On aarch64 this is 4 instructions (16 bytes):
//      movz x16, #<addr_bits_0_15>
//      movk x16, #<addr_bits_16_31>, lsl #16
//      movk x16, #<addr_bits_32_47>, lsl #32
//      movk x16, #<addr_bits_48_63>, lsl #48  (usually 0 for userspace)
//    followed by: br x16
//    But since userspace addresses on aarch64 Linux fit in 48 bits, we can use
//    3 movz/movk + br = 4 instructions = 16 bytes.

/// Generate an aarch64 trampoline that jumps to an absolute 64-bit address.
/// Uses x16 (IP0) as the scratch register, which is the standard intra-procedure-call
/// scratch register on aarch64 and is caller-saved.
///
/// Produces 4 instructions (16 bytes):
///   movz x16, #<bits 0..15>
///   movk x16, #<bits 16..31>, lsl #16
///   movk x16, #<bits 32..47>, lsl #32
///   br   x16
#[cfg(target_arch = "aarch64")]
fn generate_trampoline(target_addr: u64) -> [u8; 16] {
    let mut buf = [0u8; 16];
    let imm0 = (target_addr & 0xFFFF) as u32;
    let imm1 = ((target_addr >> 16) & 0xFFFF) as u32;
    let imm2 = ((target_addr >> 32) & 0xFFFF) as u32;

    // movz x16, #imm0          — 0xD2800010 | (imm0 << 5)
    let insn0 = 0xD280_0010u32 | (imm0 << 5);
    // movk x16, #imm1, lsl #16 — 0xF2A00010 | (imm1 << 5)
    let insn1 = 0xF2A0_0010u32 | (imm1 << 5);
    // movk x16, #imm2, lsl #32 — 0xF2C00010 | (imm2 << 5)
    let insn2 = 0xF2C0_0010u32 | (imm2 << 5);
    // br x16                   — 0xD61F0200
    let insn3 = 0xD61F_0200u32;

    buf[0..4].copy_from_slice(&insn0.to_le_bytes());
    buf[4..8].copy_from_slice(&insn1.to_le_bytes());
    buf[8..12].copy_from_slice(&insn2.to_le_bytes());
    buf[12..16].copy_from_slice(&insn3.to_le_bytes());
    buf
}

#[cfg(target_arch = "x86_64")]
fn generate_trampoline(target_addr: u64) -> [u8; 16] {
    // x86_64: movabs rax, <addr>; jmp rax; padding
    // movabs rax = 48 B8 <8 bytes>  (10 bytes)
    // jmp rax    = FF E0             (2 bytes)
    // nop * 4                        (4 bytes padding to 16)
    let mut buf = [0x90u8; 16]; // NOP fill
    buf[0] = 0x48;
    buf[1] = 0xB8;
    buf[2..10].copy_from_slice(&target_addr.to_le_bytes());
    buf[10] = 0xFF;
    buf[11] = 0xE0;
    buf
}

/// Inject a shared library into a target process using ptrace.
///
/// This works by:
/// 1. Attaching to the process via ptrace
/// 2. Saving the current registers
/// 3. Finding __libc_dlopen_mode in the target's libc
/// 4. Writing the .so path string to the process's stack
/// 5. Setting up a call to __libc_dlopen_mode(path, RTLD_NOW)
/// 6. Single-stepping through the call
/// 7. Restoring original registers and detaching
///
/// Returns Ok(()) if the library was successfully loaded.
#[cfg(target_arch = "aarch64")]
fn inject_shared_library(pid: u32, lib_path: &Path) -> Result<()> {
    let pid_t = pid as libc::pid_t;
    let lib_path_str = lib_path
        .canonicalize()
        .unwrap_or_else(|_| lib_path.to_path_buf())
        .to_string_lossy()
        .to_string();
    let lib_path_bytes = lib_path_str.as_bytes();

    // Check if already loaded
    let maps = fs::read_to_string(format!("/proc/{pid}/maps"))
        .context("read /proc/pid/maps")?;
    if maps.contains(&lib_path_str) {
        info!("library {} already loaded in pid={}", lib_path_str, pid);
        return Ok(());
    }

    // Find __libc_dlopen_mode address in the target process.
    // We resolve it by finding libc's base in the target, then adding the offset
    // of __libc_dlopen_mode from our own libc (assuming same libc version).
    let dlopen_addr = resolve_libc_dlopen_in_target(pid)?;
    info!(
        "resolved __libc_dlopen_mode in pid={} at 0x{:x}",
        pid, dlopen_addr
    );

    // Attach
    let ret = unsafe { libc::ptrace(libc::PTRACE_ATTACH, pid_t, 0, 0) };
    if ret < 0 {
        return Err(anyhow!(
            "ptrace ATTACH failed for pid={}: {}",
            pid,
            std::io::Error::last_os_error()
        ));
    }
    let mut status: libc::c_int = 0;
    unsafe { libc::waitpid(pid_t, &mut status, 0) };

    // Save original registers
    let mut orig_regs = [0u64; 34]; // aarch64: x0-x30, sp, pc, pstate
    let mut iov = libc::iovec {
        iov_base: orig_regs.as_mut_ptr() as *mut libc::c_void,
        iov_len: std::mem::size_of_val(&orig_regs),
    };
    let ret = unsafe {
        libc::ptrace(
            libc::PTRACE_GETREGSET,
            pid_t,
            1 as *mut libc::c_void,
            &mut iov as *mut _ as *mut libc::c_void,
        )
    };
    if ret < 0 {
        unsafe { libc::ptrace(libc::PTRACE_DETACH, pid_t, 0, 0) };
        return Err(anyhow!(
            "GETREGSET failed for pid={}: {}",
            pid,
            std::io::Error::last_os_error()
        ));
    }

    // Write the library path string onto the stack (below current SP).
    // We need the path to be null-terminated and 8-byte aligned.
    let path_with_nul_len = lib_path_bytes.len() + 1;
    let aligned_len = (path_with_nul_len + 7) & !7;
    let mut path_buf = vec![0u8; aligned_len];
    path_buf[..lib_path_bytes.len()].copy_from_slice(lib_path_bytes);

    let sp = orig_regs[31]; // SP
    // Reserve space below SP for the path string + 16 bytes of stack alignment padding
    let string_addr = (sp - aligned_len as u64 - 16) & !0xF;

    // Write path string via POKETEXT
    let word_size = std::mem::size_of::<libc::c_long>();
    for i in (0..aligned_len).step_by(word_size) {
        let word = libc::c_long::from_ne_bytes(
            path_buf[i..i + word_size].try_into().unwrap(),
        );
        let ret = unsafe {
            libc::ptrace(
                libc::PTRACE_POKETEXT,
                pid_t,
                (string_addr + i as u64) as *mut libc::c_void,
                word as *mut libc::c_void,
            )
        };
        if ret < 0 {
            unsafe { libc::ptrace(libc::PTRACE_DETACH, pid_t, 0, 0) };
            return Err(anyhow!(
                "POKETEXT (path string) failed at offset {}: {}",
                i,
                std::io::Error::last_os_error()
            ));
        }
    }

    // Strategy: No shellcode needed. We set PC directly to __libc_dlopen_mode
    // and LR to 0 (an unmapped address). When dlopen returns, RET jumps to 0
    // which triggers SIGSEGV. We catch that, read x0 (the return value), then
    // restore registers and detach.
    //
    // This avoids the NX-stack problem entirely since we never execute code
    // from the stack.
    let new_sp = string_addr & !0xF; // 16-byte aligned, below the string
    let mut call_regs = orig_regs;
    call_regs[0] = string_addr;       // x0 = path to .so
    call_regs[1] = 0x2;               // x1 = RTLD_NOW
    call_regs[30] = 0;                // LR = 0 (will SIGSEGV on return)
    call_regs[31] = new_sp;           // SP (16-byte aligned)
    call_regs[32] = dlopen_addr;      // PC = __libc_dlopen_mode

    iov.iov_len = std::mem::size_of_val(&call_regs);
    iov.iov_base = call_regs.as_mut_ptr() as *mut libc::c_void;
    let ret = unsafe {
        libc::ptrace(
            libc::PTRACE_SETREGSET,
            pid_t,
            1 as *mut libc::c_void,
            &iov as *const _ as *mut libc::c_void,
        )
    };
    if ret < 0 {
        unsafe { libc::ptrace(libc::PTRACE_DETACH, pid_t, 0, 0) };
        return Err(anyhow!(
            "SETREGSET failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    // Continue execution — dlopen will run, then RET to address 0 → SIGSEGV
    let ret = unsafe { libc::ptrace(libc::PTRACE_CONT, pid_t, 0, 0) };
    if ret < 0 {
        unsafe { libc::ptrace(libc::PTRACE_DETACH, pid_t, 0, 0) };
        return Err(anyhow!(
            "PTRACE_CONT failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    // Wait for the trap signal (SIGSEGV from RET to address 0)
    let mut status: libc::c_int = 0;
    let ret = unsafe { libc::waitpid(pid_t, &mut status, 0) };
    if ret < 0 {
        unsafe { libc::ptrace(libc::PTRACE_DETACH, pid_t, 0, 0) };
        return Err(anyhow!(
            "waitpid after dlopen call failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    // Verify we got a signal stop (SIGSEGV or SIGTRAP)
    if libc::WIFSTOPPED(status) {
        let sig = libc::WSTOPSIG(status);
        if sig != libc::SIGSEGV && sig != libc::SIGTRAP {
            warn!(
                "inject_shared_library: unexpected stop signal {} in pid={}",
                sig, pid
            );
        }
    } else if libc::WIFSIGNALED(status) {
        unsafe { libc::ptrace(libc::PTRACE_DETACH, pid_t, 0, 0) };
        return Err(anyhow!(
            "target pid={} was killed by signal {} during dlopen injection",
            pid,
            libc::WTERMSIG(status)
        ));
    }

    // Read x0 to check dlopen return value (should be non-null handle)
    let mut result_regs = [0u64; 34];
    iov.iov_base = result_regs.as_mut_ptr() as *mut libc::c_void;
    iov.iov_len = std::mem::size_of_val(&result_regs);
    unsafe {
        libc::ptrace(
            libc::PTRACE_GETREGSET,
            pid_t,
            1 as *mut libc::c_void,
            &mut iov as *mut _ as *mut libc::c_void,
        )
    };
    let dlopen_result = result_regs[0];
    let pc_after = result_regs[32];

    info!(
        "inject_shared_library: after dlopen call, x0=0x{:x}, pc=0x{:x}, status=0x{:x}",
        dlopen_result, pc_after, status
    );

    // Restore original registers (this also restores PC to where the process was)
    iov.iov_base = orig_regs.as_mut_ptr() as *mut libc::c_void;
    iov.iov_len = std::mem::size_of_val(&orig_regs);
    unsafe {
        libc::ptrace(
            libc::PTRACE_SETREGSET,
            pid_t,
            1 as *mut libc::c_void,
            &iov as *const _ as *mut libc::c_void,
        )
    };

    // Detach (delivers no signal, process resumes normally from original PC)
    unsafe { libc::ptrace(libc::PTRACE_DETACH, pid_t, 0, 0) };

    // Verify: PC should be 0 (our trap address) if dlopen completed successfully
    if pc_after != 0 {
        warn!(
            "inject_shared_library: PC after dlopen is 0x{:x} (expected 0x0) — dlopen may not have completed",
            pc_after
        );
    }

    if dlopen_result == 0 {
        return Err(anyhow!(
            "dlopen failed in pid={} for library {} (returned NULL)",
            pid,
            lib_path_str
        ));
    }

    info!(
        "library {} injected into pid={} (handle=0x{:x})",
        lib_path_str, pid, dlopen_result
    );
    Ok(())
}

#[cfg(target_arch = "x86_64")]
fn inject_shared_library(pid: u32, lib_path: &Path) -> Result<()> {
    let pid_t = pid as libc::pid_t;
    let lib_path_str = lib_path
        .canonicalize()
        .unwrap_or_else(|_| lib_path.to_path_buf())
        .to_string_lossy()
        .to_string();
    let lib_path_bytes = lib_path_str.as_bytes();

    // Check if already loaded
    let maps = fs::read_to_string(format!("/proc/{pid}/maps"))
        .context("read /proc/pid/maps")?;
    if maps.contains(&lib_path_str) {
        info!("library {} already loaded in pid={}", lib_path_str, pid);
        return Ok(());
    }

    // Find __libc_dlopen_mode address in the target process.
    let dlopen_addr = resolve_libc_dlopen_in_target(pid)?;
    info!(
        "resolved __libc_dlopen_mode in pid={} at 0x{:x}",
        pid, dlopen_addr
    );

    // Attach
    let ret = unsafe { libc::ptrace(libc::PTRACE_ATTACH, pid_t, 0, 0) };
    if ret < 0 {
        return Err(anyhow!(
            "ptrace ATTACH failed for pid={}: {}",
            pid,
            std::io::Error::last_os_error()
        ));
    }
    let mut status: libc::c_int = 0;
    unsafe { libc::waitpid(pid_t, &mut status, 0) };

    // Save original registers
    let mut orig_regs: libc::user_regs_struct = unsafe { std::mem::zeroed() };
    let ret = unsafe {
        libc::ptrace(
            libc::PTRACE_GETREGS,
            pid_t,
            0,
            &mut orig_regs as *mut _ as *mut libc::c_void,
        )
    };
    if ret < 0 {
        unsafe { libc::ptrace(libc::PTRACE_DETACH, pid_t, 0, 0) };
        return Err(anyhow!(
            "GETREGS failed for pid={}: {}",
            pid,
            std::io::Error::last_os_error()
        ));
    }

    // Write the library path string onto the stack (below current RSP).
    // Null-terminated and 8-byte aligned.
    let path_with_nul_len = lib_path_bytes.len() + 1;
    let aligned_len = (path_with_nul_len + 7) & !7;
    let mut path_buf = vec![0u8; aligned_len];
    path_buf[..lib_path_bytes.len()].copy_from_slice(lib_path_bytes);

    let rsp = orig_regs.rsp;
    // Reserve space below RSP for the path string + 16 bytes padding
    let string_addr = (rsp - aligned_len as u64 - 16) & !0xF;

    // Write path string via POKETEXT
    let word_size = std::mem::size_of::<libc::c_long>();
    for i in (0..aligned_len).step_by(word_size) {
        let word = libc::c_long::from_ne_bytes(
            path_buf[i..i + word_size].try_into().unwrap(),
        );
        let ret = unsafe {
            libc::ptrace(
                libc::PTRACE_POKETEXT,
                pid_t,
                (string_addr + i as u64) as *mut libc::c_void,
                word as *mut libc::c_void,
            )
        };
        if ret < 0 {
            unsafe { libc::ptrace(libc::PTRACE_DETACH, pid_t, 0, 0) };
            return Err(anyhow!(
                "POKETEXT (path string) failed at offset {}: {}",
                i,
                std::io::Error::last_os_error()
            ));
        }
    }

    // Strategy: Set RIP = __libc_dlopen_mode, push return address 0 onto the stack.
    // On x86_64, CALL pushes the return address; since we're setting RIP directly
    // (not using CALL), we simulate this by writing 0 at the top of the stack.
    // When dlopen executes RET, it pops 0 into RIP → SIGSEGV.
    //
    // x86_64 System V ABI: RDI = 1st arg (path), RSI = 2nd arg (flags).
    // Stack must be 16-byte aligned BEFORE the CALL (i.e. RSP % 16 == 0 before
    // the return address is pushed). Since we push 8 bytes (the fake return addr),
    // we set RSP so that (RSP - 8) % 16 == 0, i.e. RSP % 16 == 8.
    let new_rsp = (string_addr - 8) & !0xF; // 16-byte aligned
    let fake_ret_rsp = new_rsp - 8; // push fake return address here

    // Write the fake return address (0) at the top of the stack
    let ret = unsafe {
        libc::ptrace(
            libc::PTRACE_POKETEXT,
            pid_t,
            fake_ret_rsp as *mut libc::c_void,
            0 as *mut libc::c_void,
        )
    };
    if ret < 0 {
        unsafe { libc::ptrace(libc::PTRACE_DETACH, pid_t, 0, 0) };
        return Err(anyhow!(
            "POKETEXT (fake return addr) failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    let mut call_regs = orig_regs;
    call_regs.rdi = string_addr;       // 1st arg: path to .so
    call_regs.rsi = 0x2;               // 2nd arg: RTLD_NOW
    call_regs.rsp = fake_ret_rsp;      // RSP points to the fake return address
    call_regs.rip = dlopen_addr;       // RIP = __libc_dlopen_mode
    // CRITICAL: Set orig_rax to -1 to prevent the kernel from restarting a
    // syscall that was interrupted by our PTRACE_ATTACH. Without this, the
    // kernel may subtract 2 from RIP on PTRACE_CONT (to re-execute the
    // `syscall` instruction), causing RIP to land 2 bytes before dlopen.
    call_regs.orig_rax = (-1i64) as u64;

    let ret = unsafe {
        libc::ptrace(
            libc::PTRACE_SETREGS,
            pid_t,
            0,
            &call_regs as *const _ as *mut libc::c_void,
        )
    };
    if ret < 0 {
        unsafe { libc::ptrace(libc::PTRACE_DETACH, pid_t, 0, 0) };
        return Err(anyhow!(
            "SETREGS failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    // Continue execution — dlopen will run, then RET pops 0 into RIP → SIGSEGV
    let ret = unsafe { libc::ptrace(libc::PTRACE_CONT, pid_t, 0, 0) };
    if ret < 0 {
        unsafe { libc::ptrace(libc::PTRACE_DETACH, pid_t, 0, 0) };
        return Err(anyhow!(
            "PTRACE_CONT failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    // Wait for the trap signal (SIGSEGV from RET to address 0)
    let mut status: libc::c_int = 0;
    let ret = unsafe { libc::waitpid(pid_t, &mut status, 0) };
    if ret < 0 {
        unsafe { libc::ptrace(libc::PTRACE_DETACH, pid_t, 0, 0) };
        return Err(anyhow!(
            "waitpid after dlopen call failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    // Verify we got a signal stop (SIGSEGV or SIGTRAP)
    if libc::WIFSTOPPED(status) {
        let sig = libc::WSTOPSIG(status);
        if sig != libc::SIGSEGV && sig != libc::SIGTRAP {
            warn!(
                "inject_shared_library: unexpected stop signal {} in pid={}",
                sig, pid
            );
        }
    } else if libc::WIFSIGNALED(status) {
        unsafe { libc::ptrace(libc::PTRACE_DETACH, pid_t, 0, 0) };
        return Err(anyhow!(
            "target pid={} was killed by signal {} during dlopen injection",
            pid,
            libc::WTERMSIG(status)
        ));
    }

    // Read RAX to check dlopen return value (should be non-null handle)
    let mut result_regs: libc::user_regs_struct = unsafe { std::mem::zeroed() };
    unsafe {
        libc::ptrace(
            libc::PTRACE_GETREGS,
            pid_t,
            0,
            &mut result_regs as *mut _ as *mut libc::c_void,
        )
    };
    let dlopen_result = result_regs.rax;
    let rip_after = result_regs.rip;

    info!(
        "inject_shared_library: after dlopen call, rax=0x{:x}, rip=0x{:x}, status=0x{:x}",
        dlopen_result, rip_after, status
    );

    // Restore original registers
    unsafe {
        libc::ptrace(
            libc::PTRACE_SETREGS,
            pid_t,
            0,
            &orig_regs as *const _ as *mut libc::c_void,
        )
    };

    // Detach
    unsafe { libc::ptrace(libc::PTRACE_DETACH, pid_t, 0, 0) };

    if dlopen_result == 0 {
        return Err(anyhow!(
            "dlopen failed in pid={} for library {} (returned NULL)",
            pid,
            lib_path_str
        ));
    }

    info!(
        "library {} injected into pid={} (handle=0x{:x})",
        lib_path_str, pid, dlopen_result
    );
    Ok(())
}

/// Resolve the address of __libc_dlopen_mode (or dlopen) in the target process.
///
/// Strategy: find libc's load base address in the target's /proc/pid/maps,
/// then find the symbol offset from the libc .so file on disk.
///
/// We look for the first mapping of libc with file offset 0 (the load base),
/// not the r-xp segment, because `resolve_symbol_offset` returns offsets relative
/// to the ELF load base (lowest LOAD segment vaddr, typically 0 for shared libs).
/// On newer glibc (2.35+), the first mapping is r--p (read-only data) at offset 0,
/// followed by r-xp at a non-zero offset, so using r-xp would produce a wrong address.
fn resolve_libc_dlopen_in_target(pid: u32) -> Result<u64> {
    let maps = fs::read_to_string(format!("/proc/{pid}/maps"))
        .context("read /proc/pid/maps")?;

    // Find the first mapping of libc (file offset 0 = load base).
    // Fall back to the first r-xp mapping if no offset-0 mapping is found.
    let mut libc_base: Option<u64> = None;
    let mut libc_path: Option<String> = None;
    let mut fallback_base: Option<u64> = None;
    let mut fallback_path: Option<String> = None;

    for line in maps.lines() {
        if !(line.contains("libc.so") || line.contains("libc-")) {
            continue;
        }
        let mut fields = line.split_whitespace();
        let range = match fields.next() {
            Some(r) => r,
            None => continue,
        };
        let _perms = fields.next();
        let offset_str = fields.next().unwrap_or("0");
        let path_field = line.split_whitespace().last().unwrap_or("").to_string();

        if let Some((start_str, _)) = range.split_once('-') {
            if let Ok(addr) = u64::from_str_radix(start_str, 16) {
                // Prefer the mapping with file offset 0 (true load base)
                if offset_str == "00000000" && libc_base.is_none() {
                    libc_base = Some(addr);
                    libc_path = Some(path_field.clone());
                }
                // Keep first r-xp as fallback (for older kernels/glibc)
                if line.contains("r-xp") && fallback_base.is_none() {
                    fallback_base = Some(addr);
                    fallback_path = Some(path_field);
                }
            }
        }

        // If we found the offset-0 base, we're done
        if libc_base.is_some() && fallback_base.is_some() {
            break;
        }
    }

    // Use offset-0 base if available, otherwise fall back to r-xp base
    let base = libc_base
        .or(fallback_base)
        .ok_or_else(|| anyhow!("cannot find libc mapping in pid={}", pid))?;
    let path = libc_path
        .or(fallback_path)
        .ok_or_else(|| anyhow!("cannot find libc path in pid={}", pid))?;

    // Resolve __libc_dlopen_mode offset from the libc binary
    let offset = resolve_symbol_offset(Path::new(&path), "__libc_dlopen_mode")
        .or_else(|_| resolve_symbol_offset(Path::new(&path), "dlopen"))
        .context("cannot find dlopen symbol in libc")?;

    Ok(base + offset)
}

/// Resolve the runtime address of a symbol in an injected .so within the target process.
/// Parses /proc/<pid>/maps to find the .so's load base, then adds the symbol offset.
///
/// We prefer the mapping with file offset 0 (the true load base) because
/// `resolve_symbol_offset` returns offsets relative to the ELF load base.
/// On newer kernels/glibc, the first mapping may be r--p (read-only) at offset 0,
/// with r-xp at a non-zero offset.
fn resolve_injected_symbol(pid: u32, lib_path: &Path, symbol: &str) -> Result<u64> {
    let canonical = lib_path
        .canonicalize()
        .unwrap_or_else(|_| lib_path.to_path_buf());
    let canonical_str = canonical.to_string_lossy();
    // Also extract just the filename for fallback matching
    let filename = lib_path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default();

    let maps = fs::read_to_string(format!("/proc/{pid}/maps"))
        .context("read /proc/pid/maps")?;

    // Strategy: prefer the mapping with file offset 0 (true load base).
    // Fall back to the first mapping (any permission) if no offset-0 mapping is found.
    let mut offset0_base: Option<u64> = None;
    let mut first_base: Option<u64> = None;
    for line in maps.lines() {
        // Check if this line references our library (by full path or filename)
        let path_field = line.split_whitespace().last().unwrap_or("");
        let matches = path_field.contains(canonical_str.as_ref())
            || (!filename.is_empty() && path_field.ends_with(&filename));
        if !matches {
            continue;
        }

        let mut fields = line.split_whitespace();
        let range = match fields.next() {
            Some(r) => r,
            None => continue,
        };
        let _perms = fields.next();
        let offset_str = fields.next().unwrap_or("0");

        if let Some((start_str, _)) = range.split_once('-') {
            if let Ok(addr) = u64::from_str_radix(start_str, 16) {
                if first_base.is_none() {
                    first_base = Some(addr);
                }
                if offset_str == "00000000" && offset0_base.is_none() {
                    offset0_base = Some(addr);
                    break; // offset-0 is the true load base, stop searching
                }
            }
        }
    }

    let base = offset0_base
        .or(first_base)
        .ok_or_else(|| {
            // Dump maps lines containing the filename for debugging
            let relevant: Vec<&str> = maps
                .lines()
                .filter(|l| {
                    l.contains(canonical_str.as_ref())
                        || (!filename.is_empty() && l.contains(&filename))
                })
                .collect();
            anyhow!(
                "injected library {} (filename={}) not found in pid={} maps. Relevant lines: {:?}",
                canonical_str,
                filename,
                pid,
                relevant
            )
        })?;

    info!(
        "resolve_injected_symbol: found {} at base=0x{:x} in pid={} maps",
        canonical_str, base, pid
    );

    let offset = resolve_symbol_offset(lib_path, symbol)?;
    Ok(base + offset)
}

/// Apply a replace_function patch: inject the .so, resolve the new function,
/// and write a trampoline at the original function's entry point.
fn apply_replace_function_patch(
    pid: u32,
    target: &HotpatchTarget,
    binary_path: &Path,
    patches: &mut Vec<PatchedFunction>,
) {
    let replace_lib = match &target.replace_lib {
        Some(lib) => lib.clone(),
        None => {
            warn!(
                "hotpatch replace_function target {}:{} has no replace_lib — skipping",
                target.binary, target.symbol
            );
            return;
        }
    };

    let lib_path = Path::new(&replace_lib);
    if !lib_path.exists() {
        warn!(
            "replacement library not found: {} — skipping",
            replace_lib
        );
        return;
    }

    // Resolve the original function address
    let func_addr = match resolve_runtime_symbol(pid, binary_path, &target.symbol) {
        Ok(addr) => addr,
        Err(err) => {
            warn!(
                "cannot resolve {}:{} pid={} for replace_function: {err:#}",
                target.binary, target.symbol, pid
            );
            return;
        }
    };

    // Check if already patched
    if patches.iter().any(|p| p.pid == pid && p.address == func_addr) {
        info!(
            "replace_function already applied at {}:{} pid={} addr=0x{:x}",
            target.binary, target.symbol, pid, func_addr
        );
        return;
    }

    // Step 1: Inject the .so into the target process
    if let Err(err) = inject_shared_library(pid, lib_path) {
        warn!(
            "failed to inject {} into pid={}: {err:#}",
            replace_lib, pid
        );
        return;
    }

    // Give the target process a moment to finalize the dlopen mmap
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Step 2: Resolve the replacement function's address in the target
    let replace_sym = target
        .replace_symbol
        .as_deref()
        .unwrap_or(&target.symbol);
    let new_func_addr = match resolve_injected_symbol(pid, lib_path, replace_sym) {
        Ok(addr) => addr,
        Err(err) => {
            warn!(
                "cannot resolve replacement symbol {} in {} pid={}: {err:#}",
                replace_sym, replace_lib, pid
            );
            return;
        }
    };

    info!(
        "replace_function: {}:{} pid={} original=0x{:x} replacement=0x{:x} (from {}:{})",
        target.binary, target.symbol, pid, func_addr, new_func_addr, replace_lib, replace_sym
    );

    // Step 3: Generate trampoline and write it at the original function entry
    let trampoline = generate_trampoline(new_func_addr);
    let result = ptrace_read_and_write(pid, func_addr, &trampoline);
    match result {
        Ok(original_bytes) => {
            patches.push(PatchedFunction {
                pid,
                address: func_addr,
                original_bytes,
            });
            info!(
                "replace_function patch applied: {}:{} pid={} addr=0x{:x} -> 0x{:x}",
                target.binary, target.symbol, pid, func_addr, new_func_addr
            );
        }
        Err(err) => {
            warn!(
                "failed to write trampoline at {}:{} pid={} addr=0x{:x}: {err:#}",
                target.binary, target.symbol, pid, func_addr
            );
        }
    }
}

/// Read original bytes and write new bytes at the given address in a single ptrace session.
/// Supports arbitrary lengths (must be word-aligned for full-word writes).
/// Returns the original bytes that were read before writing.
fn ptrace_read_and_write(pid: u32, addr: u64, new_data: &[u8]) -> Result<Vec<u8>> {
    let word_size = std::mem::size_of::<libc::c_long>();
    let pid_t = pid as libc::pid_t;

    assert!(
        new_data.len() % word_size == 0,
        "patch data length must be a multiple of {} bytes",
        word_size
    );

    // Attach
    let ret = unsafe { libc::ptrace(libc::PTRACE_ATTACH, pid_t, 0, 0) };
    if ret < 0 {
        return Err(anyhow!(
            "ptrace ATTACH failed for pid={}: {}",
            pid,
            std::io::Error::last_os_error()
        ));
    }

    // Wait for stop
    let mut status: libc::c_int = 0;
    let ret = unsafe { libc::waitpid(pid_t, &mut status, 0) };
    if ret < 0 {
        unsafe { libc::ptrace(libc::PTRACE_DETACH, pid_t, 0, 0) };
        return Err(anyhow!(
            "waitpid failed for pid={}: {}",
            pid,
            std::io::Error::last_os_error()
        ));
    }

    // Read original bytes (word at a time)
    let mut original_bytes = vec![0u8; new_data.len()];
    let num_words = new_data.len() / word_size;
    for i in 0..num_words {
        let offset = (i * word_size) as u64;
        unsafe { *libc::__errno_location() = 0 };
        let word = unsafe {
            libc::ptrace(
                libc::PTRACE_PEEKTEXT,
                pid_t,
                (addr + offset) as *mut libc::c_void,
                0,
            )
        };
        let errno = unsafe { *libc::__errno_location() };
        if errno != 0 {
            unsafe { libc::ptrace(libc::PTRACE_DETACH, pid_t, 0, 0) };
            return Err(anyhow!(
                "ptrace PEEKTEXT failed for pid={} addr=0x{:x}: {}",
                pid,
                addr + offset,
                std::io::Error::from_raw_os_error(errno)
            ));
        }
        let start = i * word_size;
        original_bytes[start..start + word_size].copy_from_slice(&word.to_ne_bytes());
    }

    // Write new data (word at a time)
    for i in 0..num_words {
        let offset = (i * word_size) as u64;
        let start = i * word_size;
        let new_word = libc::c_long::from_ne_bytes(
            new_data[start..start + word_size].try_into().unwrap(),
        );
        let ret = unsafe {
            libc::ptrace(
                libc::PTRACE_POKETEXT,
                pid_t,
                (addr + offset) as *mut libc::c_void,
                new_word as *mut libc::c_void,
            )
        };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            unsafe { libc::ptrace(libc::PTRACE_DETACH, pid_t, 0, 0) };
            return Err(anyhow!(
                "ptrace POKETEXT failed for pid={} addr=0x{:x}: {}",
                pid,
                addr + offset,
                err
            ));
        }
    }

    // Detach
    let ret = unsafe { libc::ptrace(libc::PTRACE_DETACH, pid_t, 0, 0) };
    if ret < 0 {
        warn!(
            "ptrace DETACH failed for pid={}: {}",
            pid,
            std::io::Error::last_os_error()
        );
    }

    Ok(original_bytes)
}

/// Restore all previously applied code patches.
fn restore_hotpatch_code_patches(patches: &mut Vec<PatchedFunction>) {
    for patch in patches.drain(..) {
        if let Err(err) = write_process_memory(patch.pid, patch.address, &patch.original_bytes) {
            warn!(
                "failed to restore original code at pid={} addr=0x{:x}: {err:#}",
                patch.pid, patch.address
            );
        } else {
            info!(
                "hotpatch code restored at pid={} addr=0x{:x}",
                patch.pid, patch.address
            );
        }
    }
}

/// Generate aarch64 instructions to replace a function with `mov x0, #value; ret`.
///
/// For values in range [0, 65535]: `movz x0, #value` + `ret`
/// For values in range [-65536, -1]: `movn x0, #(~value)` + `ret`
/// For other values: we use movz + movk sequences (up to 4 instructions),
/// but for simplicity we limit to 16-bit immediate values and fall back
/// to a 2-instruction sequence that covers most common return values.
#[cfg(target_arch = "aarch64")]
fn generate_patch_instructions(value: i64) -> [u8; 8] {
    let mut buf = [0u8; 8];
    let (mov_insn, ret_insn);

    if value >= 0 && value <= 0xFFFF {
        // movz x0, #value
        // Encoding: 1 10 100101 00 <imm16> <Rd=00000>
        // = 0xD2800000 | (imm16 << 5)
        mov_insn = 0xD280_0000u32 | ((value as u32 & 0xFFFF) << 5);
    } else if value >= -0x10000 && value < 0 {
        // movn x0, #(~value & 0xFFFF)
        // Encoding: 1 00 100101 00 <imm16> <Rd=00000>
        // = 0x92800000 | (imm16 << 5)
        let not_val = (!value) as u32 & 0xFFFF;
        mov_insn = 0x9280_0000u32 | (not_val << 5);
    } else {
        // For values outside 16-bit range, use movz for lower 16 bits.
        // This truncates to 16 bits — acceptable for most error codes and small values.
        warn!(
            "hotpatch override value {} exceeds 16-bit range, truncating to lower 16 bits",
            value
        );
        if value >= 0 {
            mov_insn = 0xD280_0000u32 | (((value as u32) & 0xFFFF) << 5);
        } else {
            let not_val = (!value) as u32 & 0xFFFF;
            mov_insn = 0x9280_0000u32 | (not_val << 5);
        }
    }

    // ret (return to LR/x30)
    // Encoding: 0xD65F03C0
    ret_insn = 0xD65F_03C0u32;

    buf[0..4].copy_from_slice(&mov_insn.to_le_bytes());
    buf[4..8].copy_from_slice(&ret_insn.to_le_bytes());
    buf
}

#[cfg(target_arch = "x86_64")]
fn generate_patch_instructions(value: i64) -> [u8; 8] {
    // x86_64: mov eax, imm32; ret; nop; nop
    // This covers 32-bit return values which is sufficient for most cases.
    let mut buf = [0x90u8; 8]; // fill with NOP
    // mov eax, imm32 = B8 <imm32>
    buf[0] = 0xB8;
    buf[1..5].copy_from_slice(&(value as u32).to_le_bytes());
    // ret = C3
    buf[5] = 0xC3;
    buf
}

/// Attach to a process via ptrace, execute a closure, then detach.
/// The target process is stopped while the closure runs.
fn with_ptrace_attach<F, T>(pid: u32, f: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    let pid_t = pid as libc::pid_t;

    // Attach — this sends SIGSTOP to the target
    let ret = unsafe { libc::ptrace(libc::PTRACE_ATTACH, pid_t, 0, 0) };
    if ret < 0 {
        return Err(anyhow!(
            "ptrace ATTACH failed for pid={}: {}",
            pid,
            std::io::Error::last_os_error()
        ));
    }

    // Wait for the target to actually stop
    let mut status: libc::c_int = 0;
    let ret = unsafe { libc::waitpid(pid_t, &mut status, 0) };
    if ret < 0 {
        // Try to detach even if waitpid failed
        unsafe { libc::ptrace(libc::PTRACE_DETACH, pid_t, 0, 0) };
        return Err(anyhow!(
            "waitpid failed for pid={}: {}",
            pid,
            std::io::Error::last_os_error()
        ));
    }

    let result = f();

    // Detach
    let ret = unsafe { libc::ptrace(libc::PTRACE_DETACH, pid_t, 0, 0) };
    if ret < 0 {
        warn!(
            "ptrace DETACH failed for pid={}: {}",
            pid,
            std::io::Error::last_os_error()
        );
    }

    result
}

/// Read memory from a target process using ptrace PEEKTEXT.
/// Reads one word (8 bytes on 64-bit) at a time.
#[allow(dead_code)]
fn read_process_memory(pid: u32, addr: u64, buf: &mut [u8]) -> Result<()> {
    let word_size = std::mem::size_of::<libc::c_long>();
    let pid_t = pid as libc::pid_t;

    with_ptrace_attach(pid, || {
        let mut offset = 0usize;
        while offset < buf.len() {
            // Clear errno before ptrace call (PEEKTEXT returns data, not error code)
            unsafe { *libc::__errno_location() = 0 };
            let word = unsafe {
                libc::ptrace(
                    libc::PTRACE_PEEKTEXT,
                    pid_t,
                    (addr + offset as u64) as *mut libc::c_void,
                    0,
                )
            };
            let errno = unsafe { *libc::__errno_location() };
            if errno != 0 {
                return Err(anyhow!(
                    "ptrace PEEKTEXT failed for pid={} addr=0x{:x}: {}",
                    pid,
                    addr + offset as u64,
                    std::io::Error::from_raw_os_error(errno)
                ));
            }
            let word_bytes = word.to_ne_bytes();
            let remaining = buf.len() - offset;
            let to_copy = remaining.min(word_size);
            buf[offset..offset + to_copy].copy_from_slice(&word_bytes[..to_copy]);
            offset += word_size;
        }
        Ok(())
    })
}

/// Write memory to a target process using ptrace POKETEXT.
/// This can write to read-only/executable pages (like .text segment).
/// Writes one word (8 bytes on 64-bit) at a time.
fn write_process_memory(pid: u32, addr: u64, data: &[u8]) -> Result<()> {
    let word_size = std::mem::size_of::<libc::c_long>();
    let pid_t = pid as libc::pid_t;

    with_ptrace_attach(pid, || {
        let mut offset = 0usize;
        while offset < data.len() {
            let remaining = data.len() - offset;
            let word: libc::c_long = if remaining >= word_size {
                // Full word write
                libc::c_long::from_ne_bytes(
                    data[offset..offset + word_size].try_into().unwrap(),
                )
            } else {
                // Partial word: read existing word first, then overlay our bytes
                unsafe { *libc::__errno_location() = 0 };
                let existing = unsafe {
                    libc::ptrace(
                        libc::PTRACE_PEEKTEXT,
                        pid_t,
                        (addr + offset as u64) as *mut libc::c_void,
                        0,
                    )
                };
                let errno = unsafe { *libc::__errno_location() };
                if errno != 0 {
                    return Err(anyhow!(
                        "ptrace PEEKTEXT (for partial write) failed: {}",
                        std::io::Error::from_raw_os_error(errno)
                    ));
                }
                let mut word_bytes = existing.to_ne_bytes();
                word_bytes[..remaining].copy_from_slice(&data[offset..offset + remaining]);
                libc::c_long::from_ne_bytes(word_bytes)
            };

            let ret = unsafe {
                libc::ptrace(
                    libc::PTRACE_POKETEXT,
                    pid_t,
                    (addr + offset as u64) as *mut libc::c_void,
                    word as *mut libc::c_void,
                )
            };
            if ret < 0 {
                return Err(anyhow!(
                    "ptrace POKETEXT failed for pid={} addr=0x{:x}: {}",
                    pid,
                    addr + offset as u64,
                    std::io::Error::last_os_error()
                ));
            }
            offset += word_size;
        }
        Ok(())
    })
}

// ── Event collector (async ring buffer reader) ──

fn spawn_event_collector(events: RingBuf<MapData>, shared: Shared) {
    tokio::spawn(async move {
        let mut async_fd = match AsyncFd::with_interest(events, Interest::READABLE) {
            Ok(fd) => fd,
            Err(err) => {
                warn!("create AsyncFd for ring buffer failed: {err:#}");
                return;
            }
        };

        loop {
            let mut guard = match async_fd.readable_mut().await {
                Ok(guard) => guard,
                Err(err) => {
                    warn!("ring buffer read wait failed: {err:#}");
                    return;
                }
            };

            let ring = guard.get_inner_mut();
            while let Some(item) = ring.next() {
                if item.len() != size_of::<KernelEvent>() {
                    continue;
                }
                let event =
                    unsafe { core::ptr::read_unaligned(item.as_ptr().cast::<KernelEvent>()) };
                process_event(event, &shared).await;
            }
            guard.clear_ready();
        }
    });
}

// ── Systemd service tracker ──

fn spawn_service_tracker(shared: Shared) {
    tokio::spawn(async move {
        loop {
            let services = shared.policy.read().await.monitored_services.clone();
            let enabled: Vec<&ToggleItem> = services.iter().filter(|s| s.enabled).collect();
            if enabled.is_empty() {
                // Clear stale data when nothing is enabled.
                let mut state = shared.runtime.write().await;
                if !state.service_map.is_empty() {
                    state.service_map.clear();
                    state.pid_to_service.clear();
                    state.comm_to_service.clear();
                }
                drop(state);
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
            let mut mapping = HashMap::<String, ServiceStatus>::new();
            let mut reverse = HashMap::<u32, String>::new();
            let mut comm_map = HashMap::<String, String>::new();
            for service in &enabled {
                let pids = query_service_pids(&service.value).await;
                let (active_state, sub_state) = query_service_state(&service.value).await;
                for &pid in &pids {
                    reverse.insert(pid, service.value.clone());
                    if let Ok(comm) = fs::read_to_string(format!("/proc/{pid}/comm")) {
                        let comm = comm.trim().to_string();
                        if !comm.is_empty() {
                            comm_map.entry(comm).or_insert_with(|| service.value.clone());
                        }
                    }
                }
                let state = if sub_state.is_empty() {
                    active_state.clone()
                } else {
                    format!("{active_state} ({sub_state})")
                };
                mapping.insert(service.value.clone(), ServiceStatus {
                    active_state,
                    sub_state,
                    state,
                    pids,
                });
            }
            let mut state = shared.runtime.write().await;
            state.service_map = mapping;
            state.pid_to_service = reverse;
            state.comm_to_service = comm_map;
            drop(state);
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}

/// Query all PIDs belonging to a systemd service via its cgroup.
///
/// This reads the service's ControlGroup path from systemctl, then reads
/// `/sys/fs/cgroup/<path>/cgroup.procs` which contains exactly the PIDs
/// that systemd considers part of the service — matching `systemctl status`.
///
/// Falls back to MainPID if the cgroup approach fails (e.g. cgroup v1).
async fn query_service_pids(service: &str) -> Vec<u32> {
    // Try the cgroup approach first (cgroup v2, unified hierarchy).
    if let Some(pids) = query_cgroup_pids(service).await {
        if !pids.is_empty() {
            return pids;
        }
    }
    // Fallback: just return the MainPID.
    match query_main_pid(service).await {
        Some(pid) => vec![pid],
        None => Vec::new(),
    }
}

/// Read PIDs from the service's cgroup.procs file.
async fn query_cgroup_pids(service: &str) -> Option<Vec<u32>> {
    // Get the cgroup path, e.g. "/system.slice/ssh.service"
    let output = Command::new("systemctl")
        .arg("show")
        .arg(service)
        .arg("--property")
        .arg("ControlGroup")
        .arg("--value")
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let cgroup_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if cgroup_path.is_empty() {
        return None;
    }
    // Read /sys/fs/cgroup/<path>/cgroup.procs
    let procs_file = format!("/sys/fs/cgroup{cgroup_path}/cgroup.procs");
    let content = fs::read_to_string(&procs_file).ok()?;
    let pids: Vec<u32> = content
        .split_whitespace()
        .filter_map(|s| s.parse::<u32>().ok())
        .filter(|&p| p > 0)
        .collect();
    Some(pids)
}

/// Get the MainPID of a systemd service (fallback).
async fn query_main_pid(service: &str) -> Option<u32> {
    let output = Command::new("systemctl")
        .arg("show")
        .arg(service)
        .arg("--property")
        .arg("MainPID")
        .arg("--value")
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let pid = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u32>()
        .ok()?;
    (pid > 0).then_some(pid)
}

/// Query the ActiveState and SubState of a systemd service.
/// Returns e.g. ("active", "running") or ("inactive", "dead").
async fn query_service_state(service: &str) -> (String, String) {
    let output = Command::new("systemctl")
        .arg("show")
        .arg(service)
        .arg("--property")
        .arg("ActiveState")
        .arg("--property")
        .arg("SubState")
        .arg("--value")
        .output()
        .await;
    match output {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            let mut lines = text.lines();
            let active = lines.next().unwrap_or("unknown").trim().to_string();
            let sub = lines.next().unwrap_or("").trim().to_string();
            (active, sub)
        }
        _ => ("unknown".to_string(), String::new()),
    }
}

// ── Network traffic sampler ──

/// Raw counters read from /proc/net/dev for a single interface.
#[derive(Debug, Clone)]
struct IfCounters {
    name: String,
    rx_bytes: u64,
    tx_bytes: u64,
    rx_packets: u64,
    tx_packets: u64,
}

fn read_proc_net_dev() -> Vec<IfCounters> {
    let content = match fs::read_to_string("/proc/net/dev") {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut result = Vec::new();
    for line in content.lines().skip(2) {
        // Format: "  iface: rx_bytes rx_packets ... tx_bytes tx_packets ..."
        let line = line.trim();
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_string();
        let fields: Vec<u64> = rest
            .split_whitespace()
            .filter_map(|s| s.parse::<u64>().ok())
            .collect();
        if fields.len() >= 10 {
            result.push(IfCounters {
                name,
                rx_bytes: fields[0],
                rx_packets: fields[1],
                tx_bytes: fields[8],
                tx_packets: fields[9],
            });
        }
    }
    result
}

fn spawn_traffic_sampler(shared: Shared) {
    tokio::spawn(async move {
        let mut prev: Option<(Instant, Vec<IfCounters>)> = None;

        loop {
            let now = Instant::now();
            let current = read_proc_net_dev();

            if let Some((prev_time, ref prev_counters)) = prev {
                let elapsed = now.duration_since(prev_time).as_secs_f64();
                if elapsed > 0.1 {
                    let mut interfaces = Vec::new();
                    let mut total_rx: u64 = 0;
                    let mut total_tx: u64 = 0;

                    for cur in &current {
                        if let Some(old) = prev_counters.iter().find(|p| p.name == cur.name) {
                            let rx_bps =
                                (cur.rx_bytes.saturating_sub(old.rx_bytes) as f64 / elapsed) as u64;
                            let tx_bps =
                                (cur.tx_bytes.saturating_sub(old.tx_bytes) as f64 / elapsed) as u64;
                            let rx_pps = (cur.rx_packets.saturating_sub(old.rx_packets) as f64
                                / elapsed) as u64;
                            let tx_pps = (cur.tx_packets.saturating_sub(old.tx_packets) as f64
                                / elapsed) as u64;

                            // Exclude loopback from totals
                            if cur.name != "lo" {
                                total_rx += rx_bps;
                                total_tx += tx_bps;
                            }

                            interfaces.push(NetIfTraffic {
                                name: cur.name.clone(),
                                rx_bytes_per_sec: rx_bps,
                                tx_bytes_per_sec: tx_bps,
                                rx_packets_per_sec: rx_pps,
                                tx_packets_per_sec: tx_pps,
                                rx_bytes_total: cur.rx_bytes,
                                tx_bytes_total: cur.tx_bytes,
                            });
                        }
                    }

                    let snapshot = TrafficSnapshot {
                        interfaces,
                        total_rx_bytes_per_sec: total_rx,
                        total_tx_bytes_per_sec: total_tx,
                    };

                    let mut state = shared.runtime.write().await;
                    state.traffic = snapshot;
                }
            }

            prev = Some((now, current));
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
}

// ── Anomaly detection engine ──

async fn process_event(event: KernelEvent, shared: &Shared) {
    let mut record = to_event_record(event, shared.ktime_anchor);
    let mut state = shared.runtime.write().await;
    let policy = shared.policy.read().await;
    // Collect AI notifications to send after releasing the write lock.
    let mut ai_notifications: Vec<AiAlertNotification> = Vec::new();

    // ── Service attribution: match PID, TGID, or comm name to a tracked service ──
    let service = state
        .pid_to_service
        .get(&record.pid)
        .or_else(|| state.pid_to_service.get(&record.tgid))
        .or_else(|| state.comm_to_service.get(&record.comm))
        .cloned();
    record.service = service.clone();

    // Persist to SQLite history (fire-and-forget; never blocks the event loop).
    {
        let db = shared.db.clone();
        let r = record.clone();
        tokio::spawn(async move {
            if let Err(e) = db.insert_event(r).await {
                log::warn!("db insert_event failed: {e:#}");
            }
        });
    }

    let key = record.kind.clone();
    *state.counters.entry(key.clone()).or_insert(0) += 1;

    // ── Per-second rate baseline detection ──
    // Each kind has a 1-second sliding window. If the rate in the current second
    // exceeds the configured threshold (events/sec), fire one alert and then
    // suppress further alerts for that kind for BASELINE_ALERT_COOLDOWN_SECS.
    if let Some(&threshold) = policy.baseline_thresholds.get(&key) {
        let now = Instant::now();
        let (window_start, count) = state.baseline.windows
            .entry(key.clone())
            .or_insert((now, 0));

        if window_start.elapsed() >= Duration::from_secs(1) {
            // New second: reset window
            *window_start = now;
            *count = 1;
        } else {
            *count += 1;
        }
        let rate = *count;

        if rate > threshold {
            // Only alert if outside the cooldown period for this kind
            let in_cooldown = state.baseline.last_alert.get(&key)
                .map(|t| t.elapsed() < Duration::from_secs(BASELINE_ALERT_COOLDOWN_SECS))
                .unwrap_or(false);

            if !in_cooldown {
                state.baseline.last_alert.insert(key.clone(), now);
                if let Some(n) = push_alert(
                    &mut state,
                    "medium",
                    format!("syscall rate exceeded for {key}: {rate}/s > {threshold}/s"),
                    &record,
                ) { ai_notifications.push(n); }
            }
        }
    }

    // Whitelist rule: sensitive file access
    if event.kind == EVENT_KIND_FILE_IO
        && event.action == EVENT_ACTION_ALERT
        && file_path_sensitive(&record.detail, &policy)
    {
        if let Some(n) = push_alert(
            &mut state,
            "high",
            "sensitive file accessed".into(),
            &record,
        ) { ai_notifications.push(n); }
    }

    // Whitelist rule: execve path check
    if event.kind == EVENT_KIND_PROCESS && !exec_path_whitelisted(&record.detail, &policy) {
        if let Some(n) = push_alert(
            &mut state,
            "high",
            "execve target not in whitelist prefixes".into(),
            &record,
        ) { ai_notifications.push(n); }
    }

    // Privilege escalation alert
    if event.kind == EVENT_KIND_PRIVILEGE {
        if let Some(n) = push_alert(
            &mut state,
            "high",
            "privilege change detected".into(),
            &record,
        ) { ai_notifications.push(n); }
    }

    // Active defense: blocked port
    if event.kind == EVENT_KIND_NETWORK && event.action == EVENT_ACTION_BLOCKED {
        info!(
            "BLOCKED port {} ({}) by {} (pid={}, detail={})",
            record.network.as_ref().map(|n| n.port).unwrap_or(0),
            record.network.as_ref().map(|n| n.address.as_str()).unwrap_or("?"),
            record.comm,
            record.pid,
            record.detail,
        );
        if let Some(n) = push_alert(
            &mut state,
            "critical",
            format!(
                "blocked port {} hit — active defense policy triggered ({})",
                record.network.as_ref().map(|n| n.port).unwrap_or(0),
                record.detail,
            ),
            &record,
        ) { ai_notifications.push(n); }
    }

    // Rate limit exceeded
    if event.kind == EVENT_KIND_NETWORK && event.action == EVENT_ACTION_RATE_LIMITED {
        if let Some(n) = push_alert(
            &mut state,
            "high",
            format!("IP rate limit exceeded — {}", record.detail),
            &record,
        ) { ai_notifications.push(n); }
    }

    // ── Service-specific alerts: flag notable activity from monitored services ──
    if let Some(ref svc) = service {
        // Alert on sensitive file access by a monitored service
        if event.kind == EVENT_KIND_FILE_IO && event.action == EVENT_ACTION_ALERT {
            if let Some(n) = push_alert(
                &mut state,
                "high",
                format!("monitored service [{svc}] accessed sensitive file: {}", record.detail),
                &record,
            ) { ai_notifications.push(n); }
        }
        // Alert on network activity from a monitored service
        if event.kind == EVENT_KIND_NETWORK {
            let addr_info = record
                .network
                .as_ref()
                .map(|n| format!("{}:{}", n.address, n.port))
                .unwrap_or_default();
            if let Some(n) = push_alert(
                &mut state,
                "medium",
                format!("monitored service [{svc}] network activity: {addr_info}"),
                &record,
            ) { ai_notifications.push(n); }
        }
        // Alert on privilege changes within a monitored service
        if event.kind == EVENT_KIND_PRIVILEGE {
            if let Some(n) = push_alert(
                &mut state,
                "critical",
                format!("monitored service [{svc}] privilege change: {}", record.detail),
                &record,
            ) { ai_notifications.push(n); }
        }
        // Alert on process execution from a monitored service
        if event.kind == EVENT_KIND_PROCESS {
            if let Some(n) = push_alert(
                &mut state,
                "medium",
                format!("monitored service [{svc}] exec: {}", record.detail),
                &record,
            ) { ai_notifications.push(n); }
        }
    }

    state.events.push_front(record);
    while state.events.len() > MAX_EVENT_HISTORY {
        state.events.pop_back();
    }
    // Release locks before broadcasting to avoid holding them during channel send.
    drop(state);
    drop(policy);

    // Broadcast high/critical alert notifications to AI event stream subscribers.
    for notification in ai_notifications {
        // Ignore send errors (no active subscribers is fine).
        let _ = shared.ai_alert_tx.send(notification);
    }
}

fn exec_path_whitelisted(path: &str, policy: &MonitorPolicy) -> bool {
    let enabled: Vec<&str> = policy.exec_whitelist_prefixes.iter()
        .filter(|p| p.enabled)
        .map(|p| p.value.as_str())
        .collect();
    if enabled.is_empty() {
        return true;
    }
    enabled.iter().any(|prefix| path.starts_with(prefix))
}

fn file_path_sensitive(path: &str, policy: &MonitorPolicy) -> bool {
    let enabled: Vec<&str> = policy.sensitive_prefixes.iter()
        .filter(|p| p.enabled)
        .map(|p| p.value.as_str())
        .collect();
    if enabled.is_empty() {
        return true;
    }
    enabled.iter().any(|prefix| path.starts_with(prefix))
}

/// Push an alert into the runtime state and return an `AiAlertNotification`
/// for high/critical severity events so the caller can forward it to the AI
/// broadcast channel.
fn push_alert(
    state: &mut RuntimeState,
    level: &str,
    reason: String,
    event: &EventRecord,
) -> Option<AiAlertNotification> {
    state.alerts.push_front(AlertRecord {
        level: level.to_string(),
        reason: reason.clone(),
        event: event.clone(),
    });
    // When trimming, prefer dropping lower-severity alerts to keep critical ones visible.
    while state.alerts.len() > MAX_ALERT_HISTORY {
        // Try to drop the oldest non-critical alert first.
        if let Some(pos) = state.alerts.iter().rposition(|a| a.level != "critical") {
            state.alerts.remove(pos);
        } else {
            // All alerts are critical; drop the oldest.
            state.alerts.pop_back();
        }
    }

    // Only propagate high/critical alerts to the AI event stream.
    if level == "critical" || level == "high" {
        let ts_ms = event.timestamp_ns / 1_000_000;
        let secs = ts_ms / 1000;
        let millis = ts_ms % 1000;
        let timestamp = format!("{}.{:03}Z", secs, millis);
        Some(AiAlertNotification {
            timestamp,
            level: level.to_string(),
            reason,
            event_kind: event.kind.clone(),
            event_action: event.action.clone(),
            pid: event.pid,
            comm: event.comm.clone(),
            detail: event.detail.clone(),
        })
    } else {
        None
    }
}

// ── Event conversion ──

fn to_event_record(event: KernelEvent, anchor: KtimeAnchor) -> EventRecord {
    let comm = fixed_to_string(&event.comm);
    let detail = fixed_to_string(&event.detail);
    let network = if event.kind == EVENT_KIND_NETWORK {
        Some(NetworkView {
            port: event.port,
            address: format_address(&event.addr),
        })
    } else {
        None
    };

    // Convert bpf_ktime_get_ns() (CLOCK_BOOTTIME nanoseconds) to Unix epoch
    // milliseconds so the frontend can display wall-clock times correctly.
    let timestamp_ms = anchor.to_epoch_ms(event.timestamp_ns);

    EventRecord {
        timestamp_ns: timestamp_ms,
        kind: kind_name(event.kind).to_string(),
        action: action_name(event.action).to_string(),
        pid: event.pid,
        tgid: event.tgid,
        uid: event.uid,
        gid: event.gid,
        comm,
        detail,
        network,
        service: None, // filled in by process_event after service lookup
    }
}

fn kind_name(kind: u8) -> &'static str {
    match kind {
        EVENT_KIND_FILE_IO => "file_io",
        EVENT_KIND_PROCESS => "process",
        EVENT_KIND_PRIVILEGE => "privilege",
        EVENT_KIND_NETWORK => "network",
        EVENT_KIND_HOTPATCH => "hotpatch",
        _ => "unknown",
    }
}

fn action_name(action: u8) -> &'static str {
    match action {
        1 => "enter",
        2 => "exit",
        3 => "alert",
        4 => "blocked",
        5 => "rate_limited",
        _ => "unknown",
    }
}

fn fixed_to_string(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).to_string()
}

fn format_address(raw: &[u8; 16]) -> String {
    if raw[4..].iter().all(|x| *x == 0) {
        return format!("{}.{}.{}.{}", raw[0], raw[1], raw[2], raw[3]);
    }
    std::net::Ipv6Addr::from(*raw).to_string()
}

// ── Policy loader (TOML / YAML) ──

fn load_policy(path: &Path) -> Result<MonitorPolicy> {
    if !path.exists() {
        info!("config file not found, using defaults");
        return Ok(MonitorPolicy::default());
    }
    let raw =
        fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
    match path.extension().and_then(|x| x.to_str()) {
        Some("yaml") | Some("yml") => {
            serde_yaml::from_str::<MonitorPolicy>(&raw).context("parse yaml policy")
        }
        Some("toml") | None => toml::from_str::<MonitorPolicy>(&raw).context("parse toml policy"),
        Some(other) => Err(anyhow!("unsupported config extension: {other}")),
    }
}

// ── Symbol resolver (ELF + /proc/[pid]/maps) ──

fn resolve_runtime_symbol(pid: u32, binary: &Path, symbol: &str) -> Result<u64> {
    let offset = resolve_symbol_offset(binary, symbol)?;
    let base = resolve_process_mapping_base(pid, binary)?;
    Ok(base + offset)
}

fn resolve_symbol_offset(binary: &Path, symbol: &str) -> Result<u64> {
    use object::ObjectSegment;
    let data = fs::read(binary).with_context(|| format!("read binary {}", binary.display()))?;
    let obj = object::File::parse(data.as_slice()).context("parse ELF")?;

    let mut sym_addr: Option<u64> = None;
    for s in obj.symbols() {
        if let Ok(name) = s.name() {
            if name == symbol && s.address() > 0 {
                sym_addr = Some(s.address());
                break;
            }
        }
    }
    if sym_addr.is_none() {
        // Also check dynamic symbols (.dynsym)
        for s in obj.dynamic_symbols() {
            if let Ok(name) = s.name() {
                if name == symbol && s.address() > 0 {
                    sym_addr = Some(s.address());
                    break;
                }
            }
        }
    }
    let addr = sym_addr.ok_or_else(|| anyhow!("symbol {symbol} not found in {}", binary.display()))?;

    // For non-PIE executables (ET_EXEC), symbol addresses are absolute virtual addresses.
    // We need to subtract the load base (lowest LOAD segment vaddr) so that
    // resolve_runtime_symbol can correctly compute: maps_base + offset.
    // For shared libraries / PIE (ET_DYN), the lowest vaddr is typically 0 so this is a no-op.
    let load_vaddr = obj
        .segments()
        .map(|seg| seg.address())
        .min()
        .unwrap_or(0);

    Ok(addr - load_vaddr)
}

fn resolve_process_mapping_base(pid: u32, binary: &Path) -> Result<u64> {
    let maps = fs::read_to_string(format!("/proc/{pid}/maps")).context("read /proc maps")?;
    let target = binary
        .canonicalize()
        .unwrap_or_else(|_| binary.to_path_buf());
    let target_str = target.to_string_lossy();

    for line in maps.lines() {
        if !line.contains(target_str.as_ref()) {
            continue;
        }
        let mut fields = line.split_whitespace();
        let range = fields.next().ok_or_else(|| anyhow!("bad maps range"))?;
        let _perms = fields.next();
        let offset = fields.next().ok_or_else(|| anyhow!("bad maps offset"))?;

        let (start, _) = range
            .split_once('-')
            .ok_or_else(|| anyhow!("bad maps address range"))?;
        let start = u64::from_str_radix(start, 16).context("parse maps start")?;
        let offset = u64::from_str_radix(offset, 16).context("parse maps offset")?;
        return Ok(start.saturating_sub(offset));
    }

    Err(anyhow!("binary mapping not found for pid {pid}"))
}

// ── Process detail API ──

// HTTP, AI, and process-inspection code has been split into `web`, `ai`, and
// `process` modules to keep the controller entrypoint focused on orchestration.
