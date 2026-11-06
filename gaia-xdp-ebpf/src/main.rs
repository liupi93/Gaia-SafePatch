#![no_std]
#![no_main]

use aya_ebpf::{
    helpers::{
        bpf_get_current_comm, bpf_get_current_pid_tgid, bpf_get_current_uid_gid, bpf_ktime_get_ns,
        bpf_probe_read_user, bpf_probe_read_user_str_bytes,
    },
    macros::{cgroup_sock_addr, kprobe, map, tracepoint, uprobe, uretprobe},
    maps::{Array, HashMap, PerCpuArray, RingBuf},
    programs::{ProbeContext, RetProbeContext, SockAddrContext, TracePointContext},
};
// Note: bpf_override_return only works with kprobes on error-injectable kernel
// functions. It does NOT work with uprobes. For userspace function patching,
// we use process_vm_writev in the userspace daemon instead.
use gaia_xdp_common::{
    HotpatchPidEntry, HotpatchRuleEntry, KernelEvent, RateLimitCounter, RateLimitEntry,
    EVENT_ACTION_ALERT, EVENT_ACTION_BLOCKED, EVENT_ACTION_ENTER, EVENT_ACTION_EXIT,
    EVENT_ACTION_RATE_LIMITED, EVENT_KIND_FILE_IO, EVENT_KIND_HOTPATCH, EVENT_KIND_NETWORK,
    EVENT_KIND_PRIVILEGE, EVENT_KIND_PROCESS, HOTPATCH_ACTION_MONITOR,
    HOTPATCH_ACTION_OVERRIDE_RETURN, HOTPATCH_ACTION_REPLACE_FUNCTION,
    HOTPATCH_ACTION_SKIP_CALL, MAX_HOTPATCH_PIDS,
    MAX_HOTPATCH_RULES, MAX_RATE_LIMIT_COUNTERS, MAX_RATE_LIMIT_RULES, PROTOCOL_TCP,
    RATE_ACTION_BLOCK,
};

// ── Tracepoint field offsets ──
const OPENAT_FILENAME_OFFSET: usize = 24;
const EXIT_RET_OFFSET: usize = 16;
const EXECVE_FILENAME_OFFSET: usize = 16;
const SETID_VAL_OFFSET: usize = 16;
const SOCKADDR_PTR_OFFSET: usize = 24;

const AF_INET: u16 = 2;
const AF_INET6: u16 = 10;
const ONE_SEC_NS: u64 = 1_000_000_000;

const SENSITIVE_PATHS: [&[u8]; 4] = [b"/etc/shadow", b"/etc/ssl", b"/root/.ssh", b"/var/lib/gaia"];

#[repr(C)]
#[derive(Clone, Copy)]
struct SockAddrIn {
    sin_family: u16,
    sin_port: u16,
    sin_addr: u32,
    sin_zero: [u8; 8],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct In6Addr {
    s6_addr: [u8; 16],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SockAddrIn6 {
    sin6_family: u16,
    sin6_port: u16,
    sin6_flowinfo: u32,
    sin6_addr: In6Addr,
    sin6_scope_id: u32,
}

// ── BPF Maps ──

#[map]
static EVENTS: RingBuf = RingBuf::with_byte_size(256 * 1024, 0);

#[map]
static BLOCKED_PORTS: HashMap<u16, u8> = HashMap::with_max_entries(128, 0);

#[map]
static SCRATCH: Array<KernelEvent> = Array::with_max_entries(1, 0);

/// Rate limit rules: key = rule index (u32), value = RateLimitEntry
#[map]
static RATE_LIMIT_RULES: HashMap<u32, RateLimitEntry> =
    HashMap::with_max_entries(MAX_RATE_LIMIT_RULES, 0);

/// Number of active rate limit rules (stored at index 0)
#[map]
static RATE_LIMIT_RULE_COUNT: Array<u32> = Array::with_max_entries(1, 0);

/// Per-IP connection counters: key = IPv4 addr as u32
#[map]
static RATE_LIMIT_COUNTERS: HashMap<u32, RateLimitCounter> =
    HashMap::with_max_entries(MAX_RATE_LIMIT_COUNTERS, 0);

/// Hotpatch PID filter: key = PID (u32), value = HotpatchPidEntry
#[map]
static HOTPATCH_PIDS: HashMap<u32, HotpatchPidEntry> =
    HashMap::with_max_entries(MAX_HOTPATCH_PIDS, 0);

/// Hotpatch rules: key = rule index (u32), value = HotpatchRuleEntry.
/// Each rule corresponds to one configured HotpatchTarget and defines
/// the action (monitor / override_return / skip_call) and return value.
#[map]
static HOTPATCH_RULES: HashMap<u32, HotpatchRuleEntry> =
    HashMap::with_max_entries(MAX_HOTPATCH_RULES, 0);

/// Number of active hotpatch rules (stored at index 0).
#[map]
static HOTPATCH_RULE_COUNT: Array<u32> = Array::with_max_entries(1, 0);

// ── helpers ──

#[inline(always)]
fn get_scratch() -> Option<*mut KernelEvent> {
    SCRATCH.get_ptr_mut(0)
}

#[inline(always)]
fn fill_base(event: &mut KernelEvent, kind: u8, action: u8) {
    let pid_tgid = bpf_get_current_pid_tgid();
    let uid_gid = bpf_get_current_uid_gid();
    event.timestamp_ns = unsafe { bpf_ktime_get_ns() };
    event.pid = pid_tgid as u32;
    event.tgid = (pid_tgid >> 32) as u32;
    event.uid = uid_gid as u32;
    event.gid = (uid_gid >> 32) as u32;
    event.kind = kind;
    event.action = action;
    event.protocol = 0;
    event.reserved = 0;
    event.port = 0;
    event.reserved2 = 0;
    event.addr = [0u8; 16];
    event.detail = [0u8; 128];
    if let Ok(comm) = bpf_get_current_comm() {
        event.comm = comm;
    } else {
        event.comm = [0u8; 16];
    }
}

#[inline(always)]
fn emit(event: &KernelEvent) {
    let _ = EVENTS.output::<KernelEvent>(event, 0);
}

fn copy_bytes(dst: &mut [u8], src: &[u8]) {
    let mut i = 0;
    while i < dst.len() && i < src.len() {
        dst[i] = src[i];
        i += 1;
    }
}

fn is_sensitive_path(path: &[u8]) -> bool {
    let mut i = 0;
    while i < SENSITIVE_PATHS.len() {
        if starts_with(path, SENSITIVE_PATHS[i]) {
            return true;
        }
        i += 1;
    }
    false
}

fn starts_with(input: &[u8], prefix: &[u8]) -> bool {
    if prefix.len() > input.len() {
        return false;
    }
    let mut i = 0;
    while i < prefix.len() {
        if input[i] != prefix[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Check if an IPv4 address (as u32 in network byte order) matches a CIDR rule.
#[inline(always)]
fn ipv4_matches_cidr(addr: u32, network: &[u8; 16], prefix_len: u8) -> bool {
    if prefix_len == 0 {
        return true;
    }
    if prefix_len > 32 {
        return false;
    }
    // Build network u32 from first 4 bytes (network byte order)
    let net = u32::from_be_bytes([network[0], network[1], network[2], network[3]]);
    let addr_host = u32::from_be(addr);
    let net_host = u32::from_be(net);
    let mask = if prefix_len == 32 {
        0xFFFF_FFFFu32
    } else {
        !((1u32 << (32 - prefix_len)) - 1)
    };
    (addr_host & mask) == (net_host & mask)
}

/// Check rate limit rules against an IPv4 address. Returns the action if rate exceeded.
#[inline(always)]
fn check_rate_limit_ipv4(addr: u32, now_ns: u64) -> Option<u8> {
    let count_ptr = RATE_LIMIT_RULE_COUNT.get_ptr(0)?;
    let rule_count = unsafe { *count_ptr };
    if rule_count == 0 {
        return None;
    }

    // Iterate rules (bounded to avoid verifier issues)
    let max = if rule_count > 16 { 16 } else { rule_count };
    let mut idx: u32 = 0;
    while idx < max {
        if let Some(rule) = unsafe { RATE_LIMIT_RULES.get(&idx) } {
            if rule.enabled != 0 && ipv4_matches_cidr(addr, &rule.network, rule.prefix_len) {
                // Check / update per-IP counter
                if let Some(counter) = unsafe { RATE_LIMIT_COUNTERS.get(&addr) } {
                    let elapsed = now_ns.saturating_sub(counter.last_reset_ns);
                    if elapsed >= ONE_SEC_NS {
                        // Window expired, reset
                        let new_counter = RateLimitCounter {
                            count: 1,
                            last_reset_ns: now_ns,
                            rule_idx: idx,
                        };
                        let _ = RATE_LIMIT_COUNTERS.insert(&addr, &new_counter, 0);
                    } else {
                        let new_count = counter.count + 1;
                        if new_count > rule.max_conn_per_sec {
                            return Some(rule.action);
                        }
                        let new_counter = RateLimitCounter {
                            count: new_count,
                            last_reset_ns: counter.last_reset_ns,
                            rule_idx: idx,
                        };
                        let _ = RATE_LIMIT_COUNTERS.insert(&addr, &new_counter, 0);
                    }
                } else {
                    // First connection from this IP
                    let new_counter = RateLimitCounter {
                        count: 1,
                        last_reset_ns: now_ns,
                        rule_idx: idx,
                    };
                    let _ = RATE_LIMIT_COUNTERS.insert(&addr, &new_counter, 0);
                }
                // Rule matched, stop checking further rules
                return None;
            }
        }
        idx += 1;
    }
    None
}

// ── File I/O Agent ──

#[tracepoint]
pub fn tp_sys_enter_openat(ctx: TracePointContext) -> u32 {
    match handle_openat_enter(ctx) {
        Ok(_) => 0,
        Err(_) => 0,
    }
}

fn handle_openat_enter(ctx: TracePointContext) -> Result<(), i32> {
    let ptr = get_scratch().ok_or(1)?;
    let event = unsafe { &mut *ptr };
    fill_base(event, EVENT_KIND_FILE_IO, EVENT_ACTION_ENTER);

    let filename_ptr: u64 = unsafe { ctx.read_at(OPENAT_FILENAME_OFFSET) }.map_err(|_| 1)?;
    let bytes =
        unsafe { bpf_probe_read_user_str_bytes(filename_ptr as *const u8, &mut event.detail) }?;

    if is_sensitive_path(bytes) {
        event.action = EVENT_ACTION_ALERT;
    }

    emit(event);
    Ok(())
}

#[tracepoint]
pub fn tp_sys_exit_openat(ctx: TracePointContext) -> u32 {
    let Some(ptr) = get_scratch() else { return 0 };
    let event = unsafe { &mut *ptr };
    fill_base(event, EVENT_KIND_FILE_IO, EVENT_ACTION_EXIT);
    let ret: i64 = unsafe { ctx.read_at(EXIT_RET_OFFSET) }.unwrap_or_default();
    write_i64_detail(&mut event.detail, ret);
    emit(event);
    0
}

// ── Process & Privilege Agent ──

#[tracepoint]
pub fn tp_sys_enter_execve(ctx: TracePointContext) -> u32 {
    match handle_execve_enter(ctx) {
        Ok(_) => 0,
        Err(_) => 0,
    }
}

fn handle_execve_enter(ctx: TracePointContext) -> Result<(), i32> {
    let ptr = get_scratch().ok_or(1)?;
    let event = unsafe { &mut *ptr };
    fill_base(event, EVENT_KIND_PROCESS, EVENT_ACTION_ENTER);

    let filename_ptr: u64 = unsafe { ctx.read_at(EXECVE_FILENAME_OFFSET) }.map_err(|_| 1)?;
    let _ = unsafe { bpf_probe_read_user_str_bytes(filename_ptr as *const u8, &mut event.detail) }?;

    emit(event);
    Ok(())
}

#[tracepoint]
pub fn tp_sys_enter_setuid(ctx: TracePointContext) -> u32 {
    handle_privilege_change(ctx, true)
}

#[tracepoint]
pub fn tp_sys_enter_setgid(ctx: TracePointContext) -> u32 {
    handle_privilege_change(ctx, false)
}

fn handle_privilege_change(ctx: TracePointContext, is_uid: bool) -> u32 {
    let Some(ptr) = get_scratch() else { return 0 };
    let event = unsafe { &mut *ptr };
    fill_base(event, EVENT_KIND_PRIVILEGE, EVENT_ACTION_ALERT);

    let val: u32 = unsafe { ctx.read_at(SETID_VAL_OFFSET) }.unwrap_or_default();

    if is_uid {
        copy_bytes(&mut event.detail, b"setuid:");
    } else {
        copy_bytes(&mut event.detail, b"setgid:");
    }
    let prefix_len = 7;
    let mut buf = [0u8; 10];
    let n = u32_to_ascii(val, &mut buf);
    let mut j = 0;
    while j < n && (prefix_len + j) < event.detail.len() {
        event.detail[prefix_len + j] = buf[j];
        j += 1;
    }

    emit(event);
    0
}

// ── Network Telemetry Agent (with rate limiting) ──

#[tracepoint]
pub fn tp_sys_enter_connect(ctx: TracePointContext) -> u32 {
    match handle_network_event(ctx, EVENT_ACTION_ENTER) {
        Ok(_) => 0,
        Err(_) => 0,
    }
}

#[tracepoint]
pub fn tp_sys_enter_bind(ctx: TracePointContext) -> u32 {
    match handle_network_event(ctx, EVENT_ACTION_EXIT) {
        Ok(_) => 0,
        Err(_) => 0,
    }
}

fn handle_network_event(ctx: TracePointContext, default_action: u8) -> Result<(), i32> {
    let sockaddr_ptr: u64 = unsafe { ctx.read_at(SOCKADDR_PTR_OFFSET) }.map_err(|_| 1)?;
    let family: u16 = unsafe { bpf_probe_read_user(sockaddr_ptr as *const u16) }.map_err(|_| 1)?;

    let ptr = get_scratch().ok_or(1)?;
    let event = unsafe { &mut *ptr };
    fill_base(event, EVENT_KIND_NETWORK, default_action);
    event.protocol = PROTOCOL_TCP;

    if family == AF_INET {
        let sockaddr: SockAddrIn =
            unsafe { bpf_probe_read_user(sockaddr_ptr as *const SockAddrIn) }.map_err(|_| 1)?;
        event.port = u16::from_be(sockaddr.sin_port);
        let addr_bytes = sockaddr.sin_addr.to_ne_bytes();
        event.addr[0] = addr_bytes[0];
        event.addr[1] = addr_bytes[1];
        event.addr[2] = addr_bytes[2];
        event.addr[3] = addr_bytes[3];

        // Check rate limit for this IPv4 address
        let now_ns = unsafe { bpf_ktime_get_ns() };
        if let Some(action) = check_rate_limit_ipv4(sockaddr.sin_addr, now_ns) {
            if action == RATE_ACTION_BLOCK {
                event.action = EVENT_ACTION_BLOCKED;
                copy_bytes(&mut event.detail, b"rate-limit:blocked");
            } else {
                event.action = EVENT_ACTION_RATE_LIMITED;
                copy_bytes(&mut event.detail, b"rate-limit:exceeded");
            }
            emit(event);
            return Ok(());
        }
    } else if family == AF_INET6 {
        let sockaddr: SockAddrIn6 =
            unsafe { bpf_probe_read_user(sockaddr_ptr as *const SockAddrIn6) }.map_err(|_| 1)?;
        event.port = u16::from_be(sockaddr.sin6_port);
        event.addr = sockaddr.sin6_addr.s6_addr;
    } else {
        return Ok(());
    }

    // Check blocked ports
    if unsafe { BLOCKED_PORTS.get(&event.port).is_some() } {
        event.action = EVENT_ACTION_BLOCKED;
    }

    emit(event);
    Ok(())
}

// ── Hot-patching Agent (with PID filtering) ──

#[kprobe]
pub fn kprobe_hotpatch_guard(ctx: ProbeContext) -> u32 {
    let Some(ptr) = get_scratch() else { return 0 };
    let event = unsafe { &mut *ptr };
    fill_base(event, EVENT_KIND_HOTPATCH, EVENT_ACTION_ALERT);
    copy_bytes(&mut event.detail, b"kprobe:tcp_connect");
    emit(event);
    let _ = ctx;
    0
}

/// Look up the first matching hotpatch rule for the current PID.
/// Returns the rule if found and enabled.
#[inline(always)]
fn find_hotpatch_rule(pid: u32) -> Option<HotpatchRuleEntry> {
    let count_ptr = HOTPATCH_RULE_COUNT.get_ptr(0)?;
    let rule_count = unsafe { *count_ptr };
    if rule_count == 0 {
        return None;
    }
    let max = if rule_count > 32 { 32 } else { rule_count };
    let mut idx: u32 = 0;
    while idx < max {
        if let Some(rule) = unsafe { HOTPATCH_RULES.get(&idx) } {
            if rule.enabled != 0 {
                // target_pid == 0 means match all PIDs
                if rule.target_pid == 0 || rule.target_pid == pid {
                    return Some(*rule);
                }
            }
        }
        idx += 1;
    }
    None
}

// Observational only: key is full TGID/TID, value [nesting depth, last entry ns].
// Map update failure and missed returns must never authorize a machine-code write.
#[map]
static SAFEPATCH_CALLS: HashMap<u64, [u64; 2]> = HashMap::with_max_entries(16384, 0);
#[map]
static SAFEPATCH_OBSERVATION_ERRORS: PerCpuArray<u64> = PerCpuArray::with_max_entries(1, 0);
fn observe_call(enter: bool) {
    let key = bpf_get_current_pid_tgid();
    let previous = unsafe { SAFEPATCH_CALLS.get(&key).copied() }.unwrap_or([0, 0]);
    let next = if enter {
        [previous[0].saturating_add(1), unsafe { bpf_ktime_get_ns() }]
    } else {
        [previous[0].saturating_sub(1), previous[1]]
    };
    if SAFEPATCH_CALLS.insert(&key, &next, 0).is_err() || (!enter && previous[0] == 0) {
        if let Some(ptr) = SAFEPATCH_OBSERVATION_ERRORS.get_ptr_mut(0) {
            unsafe { *ptr = (*ptr).wrapping_add(1); }
        }
    }
}

#[uprobe]
pub fn uprobe_hotpatch_entry(_ctx: ProbeContext) -> u32 {
    observe_call(true);
    let Some(ptr) = get_scratch() else { return 0 };
    let event = unsafe { &mut *ptr };
    fill_base(event, EVENT_KIND_HOTPATCH, EVENT_ACTION_ENTER);

    let pid = (bpf_get_current_pid_tgid() >> 32) as u32;

    // Legacy PID filter check (backward compat)
    if let Some(entry) = unsafe { HOTPATCH_PIDS.get(&pid) } {
        if entry.active != 0 {
            copy_bytes(&mut event.detail, b"uprobe-entry:pid-filtered");
            event.action = EVENT_ACTION_BLOCKED;
            emit(event);
            return 0;
        }
    }

    // Check hotpatch rules and log the action.
    // NOTE: Actual function replacement (override_return / skip_call) is performed
    // by the userspace daemon via process_vm_writev, NOT by bpf_override_return
    // (which only works with kprobes, not uprobes). The eBPF uprobe here serves
    // as a monitoring/auditing layer.
    if let Some(rule) = find_hotpatch_rule(pid) {
        match rule.action {
            HOTPATCH_ACTION_OVERRIDE_RETURN => {
                // Log that this function entry was observed under an override_return rule.
                copy_bytes(&mut event.detail, b"uprobe:override_return=");
                let prefix_len = 22;
                let mut buf = [0u8; 20];
                let val = rule.override_return_value;
                let n = if val < 0 {
                    event.detail[prefix_len] = b'-';
                    let abs_n = u64_to_ascii(val.unsigned_abs(), &mut buf);
                    let mut j = 0;
                    while j < abs_n && (prefix_len + 1 + j) < event.detail.len() {
                        event.detail[prefix_len + 1 + j] = buf[j];
                        j += 1;
                    }
                    abs_n + 1
                } else {
                    let abs_n = u64_to_ascii(val as u64, &mut buf);
                    let mut j = 0;
                    while j < abs_n && (prefix_len + j) < event.detail.len() {
                        event.detail[prefix_len + j] = buf[j];
                        j += 1;
                    }
                    abs_n
                };
                let _ = n;
                event.action = EVENT_ACTION_BLOCKED;
                emit(event);
                return 0;
            }
            HOTPATCH_ACTION_SKIP_CALL => {
                copy_bytes(&mut event.detail, b"uprobe:skip_call");
                event.action = EVENT_ACTION_BLOCKED;
                emit(event);
                return 0;
            }
            HOTPATCH_ACTION_REPLACE_FUNCTION => {
                // The actual function replacement is done by the userspace daemon
                // via dlopen injection + trampoline. The uprobe here just monitors
                // that the trampoline is being hit (the original entry point).
                copy_bytes(&mut event.detail, b"uprobe:replace_function");
                event.action = EVENT_ACTION_BLOCKED;
                emit(event);
                return 0;
            }
            HOTPATCH_ACTION_MONITOR | _ => {
                copy_bytes(&mut event.detail, b"uprobe-entry:monitored");
                emit(event);
                return 0;
            }
        }
    }

    copy_bytes(&mut event.detail, b"uprobe-entry");
    emit(event);
    0
}

#[uretprobe]
pub fn uretprobe_hotpatch_exit(ctx: RetProbeContext) -> u32 {
    observe_call(false);
    let Some(ptr) = get_scratch() else { return 0 };
    let event = unsafe { &mut *ptr };
    fill_base(event, EVENT_KIND_HOTPATCH, EVENT_ACTION_EXIT);

    let pid = (bpf_get_current_pid_tgid() >> 32) as u32;

    // Legacy PID filter
    if let Some(entry) = unsafe { HOTPATCH_PIDS.get(&pid) } {
        if entry.active != 0 {
            copy_bytes(&mut event.detail, b"uretprobe-exit:pid-filtered");
            emit(event);
            return 0;
        }
    }

    // Log the actual return value for monitoring/auditing
    let retval: u64 = ctx.ret();
    copy_bytes(&mut event.detail, b"uretprobe-exit:ret=");
    let prefix_len = 19;
    let mut buf = [0u8; 20];
    let n = u64_to_ascii(retval, &mut buf);
    let mut j = 0;
    while j < n && (prefix_len + j) < event.detail.len() {
        event.detail[prefix_len + j] = buf[j];
        j += 1;
    }

    emit(event);
    0
}

// ── Numeric formatting helpers ──

fn u32_to_ascii(mut val: u32, out: &mut [u8]) -> usize {
    if out.is_empty() {
        return 0;
    }
    if val == 0 {
        out[0] = b'0';
        return 1;
    }
    let mut rev = [0u8; 10];
    let mut len = 0;
    while val > 0 && len < rev.len() {
        rev[len] = b'0' + (val % 10) as u8;
        val /= 10;
        len += 1;
    }
    let mut i = 0;
    while i < len && i < out.len() {
        out[i] = rev[len - 1 - i];
        i += 1;
    }
    i
}

fn write_i64_detail(dst: &mut [u8], val: i64) {
    if dst.is_empty() {
        return;
    }
    let mut buf = [0u8; 21];
    let start;
    let abs_val;
    if val < 0 {
        buf[0] = b'-';
        abs_val = val.unsigned_abs();
        start = 1;
    } else {
        abs_val = val as u64;
        start = 0;
    }
    let n = u64_to_ascii(abs_val, &mut buf[start..]);
    copy_bytes(dst, &buf[..start + n]);
}

fn u64_to_ascii(mut val: u64, out: &mut [u8]) -> usize {
    if out.is_empty() {
        return 0;
    }
    if val == 0 {
        out[0] = b'0';
        return 1;
    }
    let mut rev = [0u8; 20];
    let mut len = 0;
    while val > 0 && len < rev.len() {
        rev[len] = b'0' + (val % 10) as u8;
        val /= 10;
        len += 1;
    }
    let mut i = 0;
    while i < len && i < out.len() {
        out[i] = rev[len - 1 - i];
        i += 1;
    }
    i
}

// ── Port Blocking Agent (cgroup/sock_addr) ──
// These programs are attached to the root cgroup and can actually BLOCK
// connect/bind syscalls by returning 0 (deny) or 1 (allow).

#[cgroup_sock_addr(connect4)]
pub fn cgroup_connect4(ctx: SockAddrContext) -> i32 {
    match try_block_port(&ctx) {
        Ok(blocked) => if blocked { 0 } else { 1 },
        Err(_) => 1, // allow on error
    }
}

#[cgroup_sock_addr(bind4)]
pub fn cgroup_bind4(ctx: SockAddrContext) -> i32 {
    match try_block_port(&ctx) {
        Ok(blocked) => if blocked { 0 } else { 1 },
        Err(_) => 1, // allow on error
    }
}

#[cgroup_sock_addr(connect6)]
pub fn cgroup_connect6(ctx: SockAddrContext) -> i32 {
    match try_block_port(&ctx) {
        Ok(blocked) => if blocked { 0 } else { 1 },
        Err(_) => 1, // allow on error
    }
}

#[cgroup_sock_addr(bind6)]
pub fn cgroup_bind6(ctx: SockAddrContext) -> i32 {
    match try_block_port(&ctx) {
        Ok(blocked) => if blocked { 0 } else { 1 },
        Err(_) => 1, // allow on error
    }
}

/// Read the destination/bind port from bpf_sock_addr and check BLOCKED_PORTS map.
/// Returns Ok(true) if the port should be blocked.
fn try_block_port(ctx: &SockAddrContext) -> Result<bool, i32> {
    let sa = unsafe { &*ctx.sock_addr };
    // user_port contains the port in network byte order (big-endian) in the low 16 bits
    // of a __u32. The kernel sets it as: ctx->user_port = (__force __u32)sin->sin_port.
    // We extract the low 16 bits and convert from network to host byte order.
    let port = u16::from_be(sa.user_port as u16);

    if port == 0 {
        return Ok(false);
    }

    if unsafe { BLOCKED_PORTS.get(&port).is_some() } {
        // Emit an event so the user-space daemon knows we blocked something
        if let Some(ptr) = get_scratch() {
            let event = unsafe { &mut *ptr };
            fill_base(event, EVENT_KIND_NETWORK, EVENT_ACTION_BLOCKED);
            event.protocol = PROTOCOL_TCP;
            event.port = port;
            copy_bytes(&mut event.detail, b"cgroup:port-blocked");
            emit(event);
        }
        return Ok(true);
    }

    Ok(false)
}

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
