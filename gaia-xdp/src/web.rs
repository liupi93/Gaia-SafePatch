use super::*;

#[derive(Debug, Deserialize)]
struct ConfigUpdate {
    #[serde(default)]
    sensitive_prefixes: Option<Vec<ToggleItem>>,
    #[serde(default)]
    monitored_services: Option<Vec<ToggleItem>>,
    #[serde(default)]
    exec_whitelist_prefixes: Option<Vec<ToggleItem>>,
    #[serde(default)]
    blocked_ports: Option<Vec<TogglePort>>,
    #[serde(default)]
    baseline_thresholds: Option<HashMap<String, u32>>,
}

#[derive(Debug, Deserialize)]
struct IndexedUpdate<T> {
    index: usize,
    #[serde(flatten)]
    data: T,
}

#[derive(Debug, Deserialize)]
struct ToggleRequest {
    section: String,
    index: usize,
    enabled: bool,
}

#[derive(Serialize)]
struct ReloadResult {
    success: bool,
    message: String,
    targets_count: usize,
}

pub(crate) async fn run_http_server(shared: Shared, addr: String) -> Result<()> {
    let cors = CorsLayer::new()
        .allow_origin("*".parse::<HeaderValue>().unwrap())
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        .allow_headers(vec![header::CONTENT_TYPE, header::ACCEPT]);

    let app = Router::new()
        .route("/api/v1/state", get(api_state))
        .route("/api/v1/config", get(api_get_config).post(api_update_config))
        .route("/api/v1/config/rate-limit", get(api_get_rate_limits).post(api_add_rate_limit).put(api_update_rate_limit))
        .route("/api/v1/config/rate-limit/{index}", axum::routing::delete(api_delete_rate_limit))
        .route("/api/v1/config/hotpatch", get(api_get_hotpatch).post(api_add_hotpatch).put(api_update_hotpatch))
        .route("/api/v1/config/hotpatch/{index}", axum::routing::delete(api_delete_hotpatch))
        .route("/api/v1/config/toggle", post(api_toggle_item))
        .route("/api/v1/process/{pid}", get(crate::process::api_process_detail))
        .route("/api/v1/upload-lib", post(api_upload_lib))
        .route("/api/v1/reload-hotpatch", post(api_reload_hotpatch))
        .route("/api/v1/hotpatch-observation", get(api_hotpatch_observation))
        .route("/api/v1/config/kernel-livepatch", get(api_get_kernel_livepatch).post(api_add_kernel_livepatch))
        .route("/api/v1/config/kernel-livepatch/{index}", axum::routing::delete(api_delete_kernel_livepatch).put(api_update_kernel_livepatch))
        .route("/api/v1/kernel-livepatch/status", get(api_kernel_livepatch_status))
        .route("/api/v1/kernel-livepatch/reload", post(api_reload_kernel_livepatch))
        .route("/api/v1/file-event/detail", get(api_file_event_detail))
        .route("/api/v1/history/events", get(api_history_events))
        .route("/api/v1/ai/config", get(crate::ai::api_ai_get_config).post(crate::ai::api_ai_update_config))
        .route("/api/v1/ai/chat", post(crate::ai::api_ai_chat))
        .route("/api/v1/ai/events", get(crate::ai::api_ai_events))
        .with_state(shared)
        .layer(cors)
        .fallback(embedded_webui_handler);

    info!("serving embedded WebUI ({} files)", WebAssets::iter().count());

    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind http listener {addr}"))?;
    info!("web api listening on http://{addr}");
    axum::serve(listener, app).await.context("serve axum")?;
    Ok(())
}

async fn embedded_webui_handler(uri: Uri) -> Response {
    if uri.path().starts_with("/api/") {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    let path = uri.path().trim_start_matches('/');
    let file_path = if path.is_empty() { "index.html" } else { path };

    match WebAssets::get(file_path) {
        Some(content) => {
            let mime = guess_mime(file_path);
            ([(header::CONTENT_TYPE, mime)], content.data.to_vec()).into_response()
        }
        None => match WebAssets::get("index.html") {
            Some(index) => ([(header::CONTENT_TYPE, "text/html; charset=utf-8".to_string())], index.data.to_vec()).into_response(),
            None => (StatusCode::NOT_FOUND, "WebUI not embedded").into_response(),
        },
    }
}

fn guess_mime(path: &str) -> String {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("ico") => "image/x-icon",
        Some("json") => "application/json",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        _ => "application/octet-stream",
    }
    .to_string()
}

async fn api_state(State(shared): State<Shared>) -> Json<Snapshot> {
    let state = shared.runtime.read().await;
    let hotpatch_active = shared.hotpatch_active.lock().map(|v| *v).unwrap_or(false);
    let symbol_resolver = shared.symbol_resolver_ok.lock().map(|v| *v).unwrap_or(false);

    Json(Snapshot {
        features: FeatureStatus {
            file_io_agent: true,
            process_agent: true,
            network_agent: true,
            hotpatch_agent: hotpatch_active,
            anomaly_engine: true,
            symbol_resolver,
        },
        counters: state.counters.clone(),
        services: state.service_map.clone(),
        events: state.events.iter().cloned().collect(),
        alerts: state.alerts.iter().cloned().collect(),
        traffic: state.traffic.clone(),
    })
}

async fn api_get_config(State(shared): State<Shared>) -> Json<MonitorPolicy> {
    let policy = shared.policy.read().await;
    Json(policy.clone())
}

async fn api_update_config(State(shared): State<Shared>, Json(update): Json<ConfigUpdate>) -> Json<MonitorPolicy> {
    let mut policy = shared.policy.write().await;
    if let Some(v) = update.sensitive_prefixes { policy.sensitive_prefixes = v; }
    if let Some(v) = update.monitored_services { policy.monitored_services = v; }
    if let Some(v) = update.exec_whitelist_prefixes { policy.exec_whitelist_prefixes = v; }
    if let Some(ref v) = update.blocked_ports { policy.blocked_ports = v.clone(); }
    if let Some(v) = update.baseline_thresholds { policy.baseline_thresholds = v; }
    let snapshot = policy.clone();
    drop(policy);
    if update.blocked_ports.is_some() {
        sync_bpf_blocked_ports(&shared, &snapshot.blocked_ports);
    }
    persist_policy(&shared).await;
    Json(snapshot)
}

async fn api_get_rate_limits(State(shared): State<Shared>) -> Json<Vec<RateLimitRule>> {
    let policy = shared.policy.read().await;
    Json(policy.rate_limit_rules.clone())
}

async fn api_add_rate_limit(State(shared): State<Shared>, Json(rule): Json<RateLimitRule>) -> Json<Vec<RateLimitRule>> {
    let mut policy = shared.policy.write().await;
    policy.rate_limit_rules.push(rule);
    let rules = policy.rate_limit_rules.clone();
    drop(policy);
    sync_bpf_rate_limits(&shared, &rules);
    persist_policy(&shared).await;
    Json(rules)
}

async fn api_update_rate_limit(
    State(shared): State<Shared>,
    Json(update): Json<IndexedUpdate<RateLimitRule>>,
) -> Json<Vec<RateLimitRule>> {
    let mut policy = shared.policy.write().await;
    if update.index < policy.rate_limit_rules.len() {
        policy.rate_limit_rules[update.index] = update.data;
    }
    let rules = policy.rate_limit_rules.clone();
    drop(policy);
    sync_bpf_rate_limits(&shared, &rules);
    persist_policy(&shared).await;
    Json(rules)
}

async fn api_delete_rate_limit(
    State(shared): State<Shared>,
    axum::extract::Path(index): axum::extract::Path<usize>,
) -> Json<Vec<RateLimitRule>> {
    let mut policy = shared.policy.write().await;
    if index < policy.rate_limit_rules.len() {
        policy.rate_limit_rules.remove(index);
    }
    let rules = policy.rate_limit_rules.clone();
    drop(policy);
    sync_bpf_rate_limits(&shared, &rules);
    persist_policy(&shared).await;
    Json(rules)
}

async fn api_get_hotpatch(State(shared): State<Shared>) -> Json<Vec<HotpatchTarget>> {
    let policy = shared.policy.read().await;
    Json(policy.hotpatch.targets.clone())
}

async fn api_add_hotpatch(State(shared): State<Shared>, Json(target): Json<HotpatchTarget>) -> Json<Vec<HotpatchTarget>> {
    let mut policy = shared.policy.write().await;
    policy.hotpatch.targets.push(target);
    let targets = policy.hotpatch.targets.clone();
    drop(policy);
    sync_bpf_hotpatch_pids(&shared, &targets);
    sync_bpf_hotpatch_rules(&shared, &targets);
    {
        let mut bpf_guard = shared.bpf.lock().unwrap();
        if let Err(err) = attach_hotpatch_targets(&mut bpf_guard, &targets, &shared) {
            warn!("hotpatch uprobe auto-attach after add: {err:#}");
        }
    }
    {
        let mut patches = shared.code_patches.lock().unwrap();
        restore_hotpatch_code_patches(&mut patches);
        apply_hotpatch_code_patches(&targets, &mut patches);
    }
    persist_policy(&shared).await;
    Json(targets)
}

async fn api_update_hotpatch(
    State(shared): State<Shared>,
    Json(update): Json<IndexedUpdate<HotpatchTarget>>,
) -> Json<Vec<HotpatchTarget>> {
    let mut policy = shared.policy.write().await;
    if update.index < policy.hotpatch.targets.len() {
        policy.hotpatch.targets[update.index] = update.data;
    }
    let targets = policy.hotpatch.targets.clone();
    drop(policy);
    sync_bpf_hotpatch_pids(&shared, &targets);
    sync_bpf_hotpatch_rules(&shared, &targets);
    {
        let mut bpf_guard = shared.bpf.lock().unwrap();
        if let Err(err) = attach_hotpatch_targets(&mut bpf_guard, &targets, &shared) {
            warn!("hotpatch uprobe auto-attach after update: {err:#}");
        }
    }
    {
        let mut patches = shared.code_patches.lock().unwrap();
        restore_hotpatch_code_patches(&mut patches);
        apply_hotpatch_code_patches(&targets, &mut patches);
    }
    persist_policy(&shared).await;
    Json(targets)
}

async fn api_delete_hotpatch(
    State(shared): State<Shared>,
    axum::extract::Path(index): axum::extract::Path<usize>,
) -> Json<Vec<HotpatchTarget>> {
    let mut policy = shared.policy.write().await;
    if index < policy.hotpatch.targets.len() {
        policy.hotpatch.targets.remove(index);
    }
    let targets = policy.hotpatch.targets.clone();
    drop(policy);
    sync_bpf_hotpatch_pids(&shared, &targets);
    sync_bpf_hotpatch_rules(&shared, &targets);
    {
        let mut patches = shared.code_patches.lock().unwrap();
        restore_hotpatch_code_patches(&mut patches);
        apply_hotpatch_code_patches(&targets, &mut patches);
    }
    persist_policy(&shared).await;
    Json(targets)
}

async fn api_upload_lib(mut multipart: Multipart) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let mut file_data: Option<Vec<u8>> = None;
    let mut file_name: Option<String> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        if field.name().unwrap_or("") == "file" {
            file_name = field.file_name().map(|s| s.to_string());
            match field.bytes().await {
                Ok(bytes) => file_data = Some(bytes.to_vec()),
                Err(e) => return Err((StatusCode::BAD_REQUEST, format!("read field: {e}"))),
            }
        }
    }

    let data = file_data.ok_or_else(|| (StatusCode::BAD_REQUEST, "missing 'file' field".to_string()))?;
    let original_name = file_name.unwrap_or_else(|| "uploaded.so".to_string());

    let obj = match object::File::parse(data.as_slice()) {
        Ok(o) => o,
        Err(e) => return Err((StatusCode::BAD_REQUEST, format!("invalid ELF: {e}"))),
    };

    let host_arch = std::env::consts::ARCH;
    let elf_arch = match obj.architecture() {
        object::Architecture::Aarch64 => "aarch64",
        object::Architecture::X86_64 => "x86_64",
        other => return Err((StatusCode::BAD_REQUEST, format!("unsupported ELF architecture: {other:?}"))),
    };
    if elf_arch != host_arch {
        return Err((StatusCode::BAD_REQUEST, format!("architecture mismatch: uploaded .so is {elf_arch} but host is {host_arch}")));
    }
    if obj.kind() != object::ObjectKind::Dynamic {
        return Err((StatusCode::BAD_REQUEST, format!("uploaded file is not a shared library (got {:?}, expected ET_DYN)", obj.kind())));
    }

    let lib_dir = PathBuf::from("/tmp/gaia-libs");
    if let Err(e) = fs::create_dir_all(&lib_dir) {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("create lib dir: {e}")));
    }
    let safe_name = original_name
        .rsplit('/')
        .next()
        .unwrap_or("uploaded.so")
        .replace(|c: char| !c.is_alphanumeric() && c != '.' && c != '_' && c != '-', "_");
    let dest = lib_dir.join(&safe_name);
    if let Err(e) = fs::write(&dest, &data) {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("write file: {e}")));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dest, fs::Permissions::from_mode(0o755));
    }

    let path_str = dest.to_string_lossy().to_string();
    info!("uploaded library saved: {} ({} bytes, arch={})", path_str, data.len(), elf_arch);

    Ok(Json(serde_json::json!({
        "path": path_str,
        "size": data.len(),
        "arch": elf_arch,
        "name": safe_name,
    })))
}

async fn api_toggle_item(
    State(shared): State<Shared>,
    Json(req): Json<ToggleRequest>,
) -> Result<Json<MonitorPolicy>, StatusCode> {
    let mut policy = shared.policy.write().await;
    match req.section.as_str() {
        "sensitive_prefixes" => {
            if req.index < policy.sensitive_prefixes.len() {
                policy.sensitive_prefixes[req.index].enabled = req.enabled;
            }
        }
        "monitored_services" => {
            if req.index < policy.monitored_services.len() {
                policy.monitored_services[req.index].enabled = req.enabled;
            }
        }
        "exec_whitelist_prefixes" => {
            if req.index < policy.exec_whitelist_prefixes.len() {
                policy.exec_whitelist_prefixes[req.index].enabled = req.enabled;
            }
        }
        "blocked_ports" => {
            if req.index < policy.blocked_ports.len() {
                policy.blocked_ports[req.index].enabled = req.enabled;
            }
            let ports = policy.blocked_ports.clone();
            let snapshot = policy.clone();
            drop(policy);
            sync_bpf_blocked_ports(&shared, &ports);
            persist_policy(&shared).await;
            return Ok(Json(snapshot));
        }
        "rate_limit_rules" => {
            if req.index < policy.rate_limit_rules.len() {
                policy.rate_limit_rules[req.index].enabled = req.enabled;
            }
            let rules = policy.rate_limit_rules.clone();
            let snapshot = policy.clone();
            drop(policy);
            sync_bpf_rate_limits(&shared, &rules);
            persist_policy(&shared).await;
            return Ok(Json(snapshot));
        }
        "hotpatch_targets" => {
            if req.index < policy.hotpatch.targets.len() {
                policy.hotpatch.targets[req.index].enabled = req.enabled;
            }
            let targets = policy.hotpatch.targets.clone();
            let snapshot = policy.clone();
            drop(policy);
            sync_bpf_hotpatch_pids(&shared, &targets);
            sync_bpf_hotpatch_rules(&shared, &targets);
            persist_policy(&shared).await;
            return Ok(Json(snapshot));
        }
        "kernel_livepatch" => {
            if req.index < policy.hotpatch.kernel_livepatch.len() {
                policy.hotpatch.kernel_livepatch[req.index].enabled = req.enabled;
            }
            let snapshot = policy.clone();
            drop(policy);
            persist_policy(&shared).await;
            // Immediately sync kernel state: unload all then re-apply enabled targets.
            tokio::spawn(async move { sync_kernel_livepatches(&shared).await });
            return Ok(Json(snapshot));
        }
        _ => return Err(StatusCode::BAD_REQUEST),
    }
    let snapshot = policy.clone();
    drop(policy);
    persist_policy(&shared).await;
    Ok(Json(snapshot))
}

async fn api_reload_hotpatch(State(shared): State<Shared>) -> Json<ReloadResult> {
    let policy = shared.policy.read().await;
    let targets = policy.hotpatch.targets.clone();
    drop(policy);

    info!("reload-hotpatch requested: {} target(s)", targets.len());

    let result = {
        let mut bpf_guard = shared.bpf.lock().unwrap();
        attach_hotpatch_targets(&mut bpf_guard, &targets, &shared)
    };

    sync_bpf_hotpatch_pids(&shared, &targets);
    sync_bpf_hotpatch_rules(&shared, &targets);

    let result = result.and_then(|()| {
        let mut patches = shared.code_patches.lock().unwrap();
        restore_hotpatch_code_patches(&mut patches);
        if !patches.is_empty() { return Err(anyhow!("code restoration failed; retained for retry")); }
        apply_hotpatch_code_patches(&targets, &mut patches);
        let expected = targets.iter().filter(|t| t.enabled && t.patch_action != PatchAction::Monitor).count();
        let verified = patches.iter().filter(|p| !p.incomplete).count();
        if verified != expected || patches.iter().any(|p| p.incomplete) {
            return Err(anyhow!("machine-code patch failure: {} verified of {} requested", verified, expected));
        }
        Ok(())
    });

    match result {
        Ok(()) => Json(ReloadResult {
            success: true,
            message: format!("Hotpatch reloaded: {} target(s) processed", targets.len()),
            targets_count: targets.len(),
        }),
        Err(err) => {
            let msg = format!("Hotpatch reload error: {err:#}");
            warn!("{msg}");
            Json(ReloadResult {
                success: false,
                message: msg,
                targets_count: targets.len(),
            })
        }
    }
}

// ── Kernel Livepatch API ──

async fn api_get_kernel_livepatch(
    State(shared): State<Shared>,
) -> Json<Vec<livepatch::KernelLivepatchTarget>> {
    let policy = shared.policy.read().await;
    Json(policy.hotpatch.kernel_livepatch.clone())
}

async fn api_add_kernel_livepatch(
    State(shared): State<Shared>,
    Json(target): Json<livepatch::KernelLivepatchTarget>,
) -> Json<Vec<livepatch::KernelLivepatchTarget>> {
    let mut policy = shared.policy.write().await;
    policy.hotpatch.kernel_livepatch.push(target);
    let targets = policy.hotpatch.kernel_livepatch.clone();
    drop(policy);
    persist_policy(&shared).await;
    Json(targets)
}

async fn api_delete_kernel_livepatch(
    State(shared): State<Shared>,
    AxumPath(index): AxumPath<usize>,
) -> Json<Vec<livepatch::KernelLivepatchTarget>> {
    let mut policy = shared.policy.write().await;
    if index < policy.hotpatch.kernel_livepatch.len() {
        policy.hotpatch.kernel_livepatch.remove(index);
    }
    let targets = policy.hotpatch.kernel_livepatch.clone();
    drop(policy);
    persist_policy(&shared).await;
    Json(targets)
}

async fn api_update_kernel_livepatch(
    State(shared): State<Shared>,
    AxumPath(index): AxumPath<usize>,
    Json(target): Json<livepatch::KernelLivepatchTarget>,
) -> Result<Json<Vec<livepatch::KernelLivepatchTarget>>, StatusCode> {
    let mut policy = shared.policy.write().await;
    if index >= policy.hotpatch.kernel_livepatch.len() {
        return Err(StatusCode::NOT_FOUND);
    }
    policy.hotpatch.kernel_livepatch[index] = target;
    let targets = policy.hotpatch.kernel_livepatch.clone();
    drop(policy);
    persist_policy(&shared).await;
    // Immediately sync kernel state: unload all then re-apply enabled targets.
    tokio::spawn(async move { sync_kernel_livepatches(&shared).await });
    Ok(Json(targets))
}

async fn api_kernel_livepatch_status(
    State(shared): State<Shared>,
) -> Json<Vec<livepatch::LivepatchStatus>> {
    let mgr = shared.livepatch_mgr.lock().await;
    Json(mgr.status())
}

#[derive(Serialize)]
struct KernelLivepatchReloadResult {
    success: bool,
    message: String,
    module_name: Option<String>,
}

/// Core kernel livepatch sync: remove all loaded modules then apply enabled targets.
/// Called by toggle, PUT update, and explicit reload.
async fn sync_kernel_livepatches(shared: &Shared) -> KernelLivepatchReloadResult {
    let targets: Vec<_> = {
        let policy = shared.policy.read().await;
        policy
            .hotpatch
            .kernel_livepatch
            .iter()
            .filter(|t| t.enabled)
            .cloned()
            .collect()
    };

    // Always remove existing modules first, even if new targets list is empty.
    {
        let mut mgr = shared.livepatch_mgr.lock().await;
        mgr.remove_all().await;
    }

    if targets.is_empty() {
        return KernelLivepatchReloadResult {
            success: true,
            message: "no enabled kernel livepatch targets".to_string(),
            module_name: None,
        };
    }

    if let Err(e) = livepatch::check_livepatch_support().await {
        return KernelLivepatchReloadResult {
            success: false,
            message: format!("livepatch not supported: {e:#}"),
            module_name: None,
        };
    }

    let mut mgr = shared.livepatch_mgr.lock().await;
    match mgr.apply(&targets).await {
        Ok(name) => KernelLivepatchReloadResult {
            success: true,
            message: format!("kernel livepatch applied: {} func(s)", targets.len()),
            module_name: Some(name),
        },
        Err(e) => {
            warn!("kernel livepatch sync failed: {e:#}");
            KernelLivepatchReloadResult {
                success: false,
                message: format!("kernel livepatch failed: {e:#}"),
                module_name: None,
            }
        }
    }
}

async fn api_reload_kernel_livepatch(
    State(shared): State<Shared>,
) -> Json<KernelLivepatchReloadResult> {
    Json(sync_kernel_livepatches(&shared).await)
}

// ── File Event Detail API ──

#[derive(Debug, Deserialize)]
struct FileEventDetailQuery {
    path: Option<String>,
    pid: Option<u32>,
}

#[derive(Debug, Serialize)]
struct FileMetadata {
    path: String,
    file_type: String,
    size_bytes: u64,
    permissions: String,
    owner_uid: u32,
    owner_gid: u32,
    inode: u64,
    hard_links: u64,
    modified_secs: u64,
    accessed_secs: u64,
    created_secs: u64,
}

#[derive(Debug, Serialize)]
struct FileEventDetail {
    /// Human-readable explanation of what this event is doing.
    summary: String,
    /// Interpretation of the action field.
    action_meaning: String,
    /// Whether the file is in the sensitive prefixes policy list.
    is_sensitive: bool,
    /// File metadata from stat(2), if the file exists and is accessible.
    file_meta: Option<FileMetadata>,
    /// Whether this PID currently has the file open (checked via /proc/pid/fd).
    currently_open_by_pid: bool,
}

async fn api_file_event_detail(
    State(shared): State<Shared>,
    axum::extract::Query(q): axum::extract::Query<FileEventDetailQuery>,
) -> Response {
    let path_str = match q.path {
        Some(ref p) if !p.is_empty() => p.clone(),
        _ => return (StatusCode::BAD_REQUEST, "missing 'path' query parameter").into_response(),
    };

    let policy = shared.policy.read().await;
    let is_sensitive = policy.sensitive_prefixes.iter()
        .filter(|p| p.enabled)
        .any(|p| path_str.starts_with(&p.value));
    drop(policy);

    // Stat the file
    let file_meta = read_file_metadata(&path_str);

    // Check if the PID currently has this file open
    let currently_open_by_pid = if let Some(pid) = q.pid {
        check_file_open_by_pid(pid, &path_str)
    } else {
        false
    };

    // Build human-readable summary and action meaning based on available info
    let (summary, action_meaning) = build_file_event_explanation(
        &path_str,
        q.pid,
        is_sensitive,
        &file_meta,
        currently_open_by_pid,
    );

    let detail = FileEventDetail {
        summary,
        action_meaning,
        is_sensitive,
        file_meta,
        currently_open_by_pid,
    };

    Json(detail).into_response()
}

fn read_file_metadata(path: &str) -> Option<FileMetadata> {
    use std::os::unix::fs::MetadataExt;

    let meta = fs::metadata(path).ok()?;

    let file_type = if meta.is_dir() {
        "directory"
    } else if meta.is_symlink() {
        "symlink"
    } else if meta.is_file() {
        "regular file"
    } else {
        "special"
    }
    .to_string();

    let mode = meta.mode();
    let permissions = format_unix_permissions(mode);

    Some(FileMetadata {
        path: path.to_string(),
        file_type,
        size_bytes: meta.len(),
        permissions,
        owner_uid: meta.uid(),
        owner_gid: meta.gid(),
        inode: meta.ino(),
        hard_links: meta.nlink(),
        modified_secs: meta.mtime() as u64,
        accessed_secs: meta.atime() as u64,
        created_secs: meta.ctime() as u64,
    })
}

fn format_unix_permissions(mode: u32) -> String {
    let chars: Vec<char> = [
        (0o400, 'r'), (0o200, 'w'), (0o100, 'x'),
        (0o040, 'r'), (0o020, 'w'), (0o010, 'x'),
        (0o004, 'r'), (0o002, 'w'), (0o001, 'x'),
    ]
    .iter()
    .map(|(bit, ch)| if mode & bit != 0 { *ch } else { '-' })
    .collect();
    format!("{}{}{}{}{}{}{}{}{}{}",
        if mode & 0o170000 == 0o040000 { 'd' } else if mode & 0o170000 == 0o120000 { 'l' } else { '-' },
        chars[0], chars[1], chars[2],
        chars[3], chars[4], chars[5],
        chars[6], chars[7], chars[8],
    )
}

fn check_file_open_by_pid(pid: u32, target_path: &str) -> bool {
    let fd_dir = format!("/proc/{pid}/fd");
    let Ok(entries) = fs::read_dir(&fd_dir) else { return false };
    for entry in entries.flatten() {
        if let Ok(link_target) = fs::read_link(entry.path()) {
            if link_target.to_string_lossy() == target_path {
                return true;
            }
        }
    }
    false
}

fn build_file_event_explanation(
    path: &str,
    pid: Option<u32>,
    is_sensitive: bool,
    file_meta: &Option<FileMetadata>,
    currently_open: bool,
) -> (String, String) {
    let file_name = path.rsplit('/').next().unwrap_or(path);

    let sensitivity_note = if is_sensitive {
        " This file is on the sensitive paths watch list."
    } else {
        ""
    };

    let open_note = if currently_open {
        " The process currently has this file open."
    } else {
        ""
    };

    let type_note = match file_meta.as_ref().map(|m| m.file_type.as_str()) {
        Some("directory") => " Target is a directory.",
        Some("symlink") => " Target is a symbolic link.",
        Some("special") => " Target is a special device file.",
        _ => "",
    };

    let pid_str = pid.map(|p| format!(" by PID {p}")).unwrap_or_default();

    let summary = format!(
        "openat() syscall on \"{file_name}\"{pid_str}.{sensitivity_note}{type_note}{open_note}"
    );

    let action_meaning = format!(
        "The process is requesting to open \"{path}\" via the openat() system call. \
        The kernel intercepted this at the sys_enter_openat tracepoint and recorded the \
        file path.{}",
        if is_sensitive {
            " Because the path matches a sensitive prefix in the monitoring policy, \
            this event was escalated to an alert."
        } else {
            " The path does not match any sensitive prefix in the current policy."
        }
    );

    (summary, action_meaning)
}

// ── History API ──

#[derive(Debug, Deserialize)]
struct HistoryQuery {
    kind: Option<String>,
    page: Option<u32>,
    page_size: Option<u32>,
    since_ms: Option<i64>,
    until_ms: Option<i64>,
}

async fn api_history_events(
    State(shared): State<Shared>,
    axum::extract::Query(q): axum::extract::Query<HistoryQuery>,
) -> Response {
    let filter = crate::db::EventFilter {
        kind: q.kind,
        since_ms: q.since_ms,
        until_ms: q.until_ms,
        page: q.page.unwrap_or(0),
        page_size: q.page_size.unwrap_or(50),
    };
    match shared.db.query_events(filter).await {
        Ok(page) => Json(page).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e:#}")).into_response(),
    }
}

async fn persist_policy(shared: &Shared) {
    let policy = shared.policy.read().await;
    let path = &shared.config_path;
    match toml::to_string_pretty(&*policy) {
        Ok(content) => {
            if let Err(err) = fs::write(path, content) {
                warn!("failed to persist config to {}: {err:#}", path.display());
            } else {
                info!("config persisted to {}", path.display());
            }
        }
        Err(err) => warn!("failed to serialize config: {err:#}"),
    }
}

// Advisory snapshots may race with calls; they never authorize code modification.
async fn api_hotpatch_observation(State(shared): State<Shared>) -> Json<serde_json::Value> {
    let bpf = shared.bpf.lock().unwrap();
    let mut calls = Vec::new();
    if let Some(map) = bpf.map("SAFEPATCH_CALLS") {
        if let Ok(map) = BpfHashMap::<_, u64, [u64; 2]>::try_from(map) {
            for entry in map.iter() {
                if let Ok((key, value)) = entry {
                    calls.push(serde_json::json!({"tgid": key >> 32, "tid": key as u32,
                        "depth": value[0], "last_entry_ns": value[1]}));
                }
            }
        }
    }
    let errors = bpf.map("SAFEPATCH_OBSERVATION_ERRORS")
        .and_then(|map| aya::maps::PerCpuArray::<_, u64>::try_from(map).ok())
        .and_then(|map| map.get(&0, 0).ok())
        .map(|values| values.iter().sum::<u64>());
    Json(serde_json::json!({"advisory_only": true, "calls": calls, "errors": errors}))
}
