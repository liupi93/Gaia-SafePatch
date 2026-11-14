#[path = "../../../gaia-xdp/src/safepatch.rs"]
mod safepatch;
#[path = "../demo/terminal.rs"]
mod terminal;
mod base {
    include!(concat!(env!("OUT_DIR"), "/base.rs"));
}
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::fs::FileExt,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

use anyhow::{Context, Result, ensure};
fn now() -> u64 {
    let mut t: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t);
    }
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}
struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
type Ui = Option<Arc<Mutex<terminal::Terminal>>>;
fn prompt(ui: &Ui, text: &str) -> Result<()> {
    if let Some(ui) = ui {
        ui.lock().unwrap().prompt(Some(text))?;
        let mut line = String::new();
        ensure!(std::io::stdin().read_line(&mut line)? > 0, "input closed");
        ui.lock().unwrap().prompt(None)?;
    }
    Ok(())
}
fn event(ui: &Ui, at: u64, text: &str, kind: &str) {
    if let Some(ui) = ui {
        let _ = ui.lock().unwrap().event(at, text, kind);
    }
}
#[allow(clippy::too_many_arguments)]
fn exchange(
    mode: &str,
    pid: u32,
    state: u64,
    addr: u64,
    bytes: &[u8],
    timeout: Duration,
    ui: &Ui,
    checks: &mut u64,
    busy: &mut u64,
) -> (u64, Result<Vec<u8>>) {
    let done = Arc::new(AtomicBool::new(false));
    let monitor = if mode == "cooperative" && ui.is_some() {
        let (done, ui) = (done.clone(), ui.clone());
        Some(std::thread::spawn(move || {
            let mut last = None;
            while !done.load(Ordering::Relaxed) {
                let mut abi = [0; 24];
                if let Ok(f) = fs::File::open(format!("/proc/{pid}/mem")) {
                    if f.read_exact_at(&mut abi, state).is_ok() {
                        let gate = u64::from_ne_bytes(abi[8..16].try_into().unwrap());
                        let active = u64::from_ne_bytes(abi[16..24].try_into().unwrap());
                        eprintln!("ABI {} {gate} {active}", now());
                        if last != Some((gate, active)) {
                            last = Some((gate, active));
                            let _ = ui.as_ref().unwrap().lock().unwrap().status(
                                now(),
                                &format!("{gate}:{active}"),
                                &format!("gate={gate} active={active}"),
                            );
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }))
    } else {
        None
    };
    let mut last = 0;
    let result = match mode {
        "base" => base::exchange(pid, addr, bytes),
        "compatible" => {
            safepatch::exchange_compat_observed(pid, addr, bytes, timeout, &mut |n, rc| {
                *checks = n;
                if rc > 0 {
                    *busy += 1;
                }
                let at = now();
                eprintln!("CHECK {at} {n} {rc} {addr:x}");
                if let Some(ui) = ui {
                    if n == 1 || rc <= 0 || at - last >= 250_000_000 {
                        last = at;
                        let status = if rc > 0 {
                            "busy"
                        } else if rc == 0 {
                            "clean"
                        } else {
                            "error"
                        };
                        let _ = ui.lock().unwrap().status(
                            at,
                            status,
                            &format!("检查次数={n} {status} 函数=0x{addr:x}"),
                        );
                    }
                }
            })
        }
        _ => safepatch::exchange(pid, state, addr, bytes, timeout),
    };
    let returned = now();
    done.store(true, Ordering::Relaxed);
    if let Some(t) = monitor {
        let _ = t.join();
    }
    if let Some(ui) = ui {
        let _ = ui.lock().unwrap().clear_status();
    }
    (returned, result)
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        (4..=6).contains(&args.len()),
        "TARGET MODE OUTPUT [async|aligned]"
    );
    let interactive = args.get(5).is_some_and(|s| s == "interactive");
    let origin = now();
    let ui = interactive.then(|| Arc::new(Mutex::new(terminal::Terminal::new(origin))));
    let timeout = Duration::from_millis(
        std::env::var("GAIA_CASE_TIMEOUT_MS")
            .unwrap_or_else(|_| "10000".into())
            .parse()?,
    );
    let scenario = args.get(4).map(String::as_str).unwrap_or("async");
    ensure!(["async", "aligned"].contains(&scenario));
    let mode = &args[2];
    ensure!(["base", "compatible", "cooperative"].contains(&mode.as_str()));
    let out = std::path::Path::new(&args[3]);
    ensure!(!out.exists(), "output already exists");
    fs::create_dir_all(out)?;
    if interactive {
        fs::write(
            out.parent()
                .context("session directory missing")?
                .join("session.json"),
            serde_json::to_string_pretty(
                &serde_json::json!({"started_monotonic_ns":origin,"mode":mode,"scenario":scenario,"files":out}),
            )?,
        )?;
    }
    prompt(&ui, "启动服务")?;
    let mut c = Child(
        Command::new(&args[1])
            .arg(out)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?,
    );
    let mut input = c.0.stdin.take().unwrap();
    let mut reader = BufReader::new(c.0.stdout.take().unwrap());
    let mut ready = String::new();
    reader.read_line(&mut ready)?;
    let f: Vec<_> = ready.split_whitespace().collect();
    ensure!(f[0] == "READY");
    let pid = c.0.id();
    let threads = fs::read_dir(format!("/proc/{pid}/task"))?.count();
    ensure!(threads == 9, "expected nine live threads");
    let addr = u64::from_str_radix(f[2].trim_start_matches("0x"), 16)?;
    let state = if f[3] == "(nil)" {
        0
    } else {
        u64::from_str_radix(f[3].trim_start_matches("0x"), 16)?
    };
    event(
        &ui,
        now(),
        &format!("服务启动 PID={pid} 线程={threads} {scenario}/{mode}"),
        "ready",
    );
    let (tx, rx) = mpsc::channel();
    let relay = ui.clone();
    let raw_path = out.join("../events-".to_owned() + mode + ".log");
    let collector = std::thread::spawn(move || {
        let mut raw = fs::File::create(raw_path).unwrap();
        for line in reader.lines() {
            match line {
                Ok(s) => {
                    let _ = writeln!(raw, "{s}");
                    let _ = raw.flush();
                    let p: Vec<_> = s.split_whitespace().collect();
                    if p.len() >= 3 {
                        let at = p[2].parse().unwrap_or_else(|_| now());
                        let id = p[1];
                        let text = match p[0] {
                            "BEGIN" => format!("任务 #{id} 开始"),
                            "RENDER_END" => format!("任务 #{id} 目标函数返回"),
                            "PRINT_BEGIN" => format!("任务 #{id} 开始写出"),
                            "COMMIT" => format!("任务 #{id} 文件关闭并提交"),
                            _ => format!("任务 #{id} 拒绝导出"),
                        };
                        event(
                            &relay,
                            if p[0] == "COMMIT" {
                                p[3].parse().unwrap()
                            } else {
                                at
                            },
                            &text,
                            "business",
                        );
                    }
                    if tx.send(s).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    let mut events = Vec::<String>::new();
    let mut begins = BTreeMap::<usize, u64>::new();
    let mut ends = BTreeMap::<usize, u64>::new();
    let mut prints = BTreeMap::<usize, u64>::new();
    let mut commits = BTreeMap::<usize, u64>::new();
    let mut rejects = BTreeMap::<usize, u64>::new();
    let collect = |line: String,
                   events: &mut Vec<String>,
                   begins: &mut BTreeMap<usize, u64>,
                   ends: &mut BTreeMap<usize, u64>,
                   prints: &mut BTreeMap<usize, u64>,
                   commits: &mut BTreeMap<usize, u64>,
                   rejects: &mut BTreeMap<usize, u64>|
     -> Result<()> {
        let p: Vec<_> = line.split_whitespace().collect();
        let id: usize = p[1].parse()?;
        match p[0] {
            "BEGIN" => {
                begins.insert(id, p[2].parse()?);
            }
            "RENDER_END" => {
                ends.insert(id, p[2].parse()?);
            }
            "PRINT_BEGIN" => {
                prints.insert(id, p[2].parse()?);
            }
            "COMMIT" => {
                commits.insert(id, p[3].parse()?);
            }
            "REJECT" => {
                rejects.insert(id, p[2].parse()?);
            }
            _ => anyhow::bail!("unknown event {line}"),
        };
        events.push(line);
        Ok(())
    };
    macro_rules! receive {
        () => {
            collect(
                rx.recv_timeout(timeout + Duration::from_secs(5))?,
                &mut events,
                &mut begins,
                &mut ends,
                &mut prints,
                &mut commits,
                &mut rejects,
            )?
        };
    }
    prompt(&ui, "提交四个旧任务")?;
    for id in 0..4 {
        writeln!(input, "{id}")?;
    }
    input.flush()?;
    while begins.len() < 4 {
        receive!();
    }
    ensure!(ends.is_empty(), "old render finished before all started");
    let mut original = [0; 8];
    let mem = fs::File::open(format!("/proc/{pid}/mem"))?;
    mem.read_exact_at(&mut original, addr)?;
    let patch = [0xb8, 0, 0, 0, 0, 0xc3, 0x90, 0x90];
    let mut busy = 0;
    let mut checks = 0;
    prompt(&ui, "安装维护补丁")?;
    let active = if mode == "cooperative" {
        let mut abi = [0; 24];
        mem.read_exact_at(&mut abi, state)?;
        let n = u64::from_ne_bytes(abi[16..24].try_into()?);
        ensure!(n == 4);
        Some(n)
    } else {
        None
    };
    let requested = now();
    event(&ui, requested, "补丁安装请求", "patch_request");
    let (returned, result) = exchange(
        mode,
        pid,
        state,
        addr,
        &patch,
        timeout,
        &ui,
        &mut checks,
        &mut busy,
    );
    if result.is_err() {
        event(&ui, returned, "补丁安装失败", "patch_failed");
    }
    ensure!(result.context("install")? == original);
    event(&ui, returned, "补丁安装成功", "patch_success");
    let mut actual = [0; 8];
    mem.read_exact_at(&mut actual, addr)?;
    ensure!(actual == patch);
    prompt(&ui, "提交四个新任务")?;
    for id in 4..8 {
        writeln!(input, "{id}")?;
    }
    input.flush()?;
    while commits.len() < 4 || rejects.len() < 4 || ends.len() < 8 {
        receive!();
    }
    let overlap_start = *(0..4).map(|id| begins.get(&id).unwrap()).max().unwrap();
    let overlap_end = *(0..4).map(|id| ends.get(&id).unwrap()).min().unwrap();
    ensure!(
        overlap_start < requested && requested < overlap_end,
        "four old calls not simultaneously active at request"
    );
    let print_overlap_start = *(0..4).map(|id| prints.get(&id).unwrap()).max().unwrap();
    let closes: BTreeMap<usize, u64> = events
        .iter()
        .filter_map(|line| {
            let p: Vec<_> = line.split_whitespace().collect();
            (p[0] == "COMMIT").then(|| (p[1].parse().unwrap(), p[2].parse().unwrap()))
        })
        .collect();
    let print_overlap_end = *(0..4).map(|id| closes.get(&id).unwrap()).min().unwrap();
    ensure!(
        print_overlap_start < print_overlap_end,
        "printers not concurrent"
    );
    let commits_after = (0..4).filter(|id| commits[id] > returned).count();
    match mode.as_str() {
        "base" => {
            ensure!((0..4).all(|id| ends[&id] > returned));
            ensure!(commits_after == 4);
        }
        "compatible" => {
            ensure!((0..4).all(|id| ends[&id] < returned));
            ensure!(busy > 0);
            ensure!(
                commits_after == if scenario == "async" { 4 } else { 0 },
                "business boundary result differs from expected matrix"
            );
        }
        _ => ensure!(commits_after == 0),
    };
    for id in 0..4 {
        ensure!(out.join(format!("document-{id}.txt")).is_file());
    }
    for id in 4..8 {
        ensure!(!out.join(format!("document-{id}.txt")).exists());
    }
    prompt(&ui, "恢复补丁")?;
    event(&ui, now(), "补丁恢复请求", "patch_request");
    let mut restore_checks = 0;
    let mut restore_busy = 0;
    let (restore_at, restored) = exchange(
        mode,
        pid,
        state,
        addr,
        &original,
        timeout,
        &ui,
        &mut restore_checks,
        &mut restore_busy,
    );
    if restored.is_err() {
        event(&ui, restore_at, "补丁恢复失败", "patch_failed");
    }
    let restore = restored?;
    event(&ui, restore_at, "补丁恢复成功", "patch_success");
    ensure!(restore == patch);
    mem.read_exact_at(&mut actual, addr)?;
    ensure!(actual == original);
    for id in 8..12 {
        writeln!(input, "{id}")?;
    }
    input.flush()?;
    while commits.len() < 8 {
        receive!();
    }
    for id in (0..4).chain(8..12) {
        let text = fs::read_to_string(out.join(format!("document-{id}.txt")))?;
        ensure!(
            text == (0..10)
                .map(|page| format!("Document {id} page {page}\n"))
                .collect::<String>()
        );
    }
    ensure!(c.0.try_wait()?.is_none());
    let report = serde_json::json!({"scenario":scenario,"patch_target":if scenario=="async" {"render_document"} else {"export_document"},"mode":mode,"threads_at_start":threads,"cooperative_active_before_install":active,"concurrent_old_render_calls":4,"concurrent_printers":4,"request_ns":requested,"return_ns":returned,"old_render_begin_ns":begins.iter().filter(|(id,_)|**id<4).collect::<BTreeMap<_,_>>(),"old_render_end_ns":ends.iter().filter(|(id,_)|**id<4).collect::<BTreeMap<_,_>>(),"old_close_ns":closes.iter().filter(|(id,_)|**id<4).collect::<BTreeMap<_,_>>(),"old_commit_ns":commits.iter().filter(|(id,_)|**id<4).collect::<BTreeMap<_,_>>(),"four_render_overlap_ns":overlap_end-overlap_start,"four_print_overlap_ns":print_overlap_end-print_overlap_start,"old_commits_after_return":commits_after,"checks":checks,"busy":busy,"new_jobs_rejected":4,"restored_jobs_exported":4,"original_bytes_restored":true});
    fs::write(
        out.join("../events-".to_owned() + mode + ".log"),
        events.join("\n"),
    )?;
    fs::write(
        out.join("../result-".to_owned() + mode + ".json"),
        serde_json::to_string_pretty(&report)?,
    )?;
    if !interactive {
        println!(
            "{scenario}/{mode}: 旧目标调用同时活动=4，写出同时活动=4，返回后旧文档提交={commits_after}/4，新任务拒绝=4，恢复导出=4"
        );
    }
    prompt(&ui, "关闭服务")?;
    c.0.kill()?;
    c.0.wait()?;
    drop(rx);
    collector.join().unwrap();
    Ok(())
}
