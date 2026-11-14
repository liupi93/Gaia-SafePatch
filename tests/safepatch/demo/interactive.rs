mod terminal;
// Interactive UI over the existing Base/SysCare/Cooperative writers.
#[path = "../../../gaia-xdp/src/safepatch.rs"]
mod safepatch;
mod base {
    include!(concat!(env!("OUT_DIR"), "/base.rs"));
}
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, BufRead, BufReader, Write},
    os::unix::fs::FileExt,
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
const PATCH: [u8; 8] = [0xb8, 0, 0, 0, 0, 0xc3, 0x90, 0x90];
fn now() -> u64 {
    let mut t: libc::timespec = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) },
        0
    );
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}
#[derive(Clone)]
struct Done {
    exported: bool,
    begin: u64,
    end: u64,
}
struct Outcome {
    at: u64,
    result: Result<Vec<u8>>,
}
enum Event {
    Enter,
    Eof,
    OtherInput,
    Target(u64, String),
    TargetEof,
    Check { at: u64, count: u64, rc: i32 },
    Finished(Outcome),
}
struct Demo {
    mode: String,
    files: PathBuf,
    session: PathBuf,
    journal: File,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    child: Option<Child>,
    input: Option<ChildStdin>,
    pid: u32,
    address: u64,
    state: u64,
    layouts: usize,
    done: BTreeMap<u32, Done>,
    pending: Option<Outcome>,
    last_check_print: u64,
    terminal: terminal::Terminal,
    origin: u64,
    rendering_coop: Option<(u64, u64)>,
    processing: std::collections::VecDeque<u32>,
    latest_check: Option<(u64, i32)>,
    install_requested: Option<u64>,
    installed_at: Option<u64>,
    restored_at: Option<u64>,
}
impl Drop for Demo {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            if child.wait().is_ok() {
                let _ = self.log("service_stopped", "服务停止", json!({"pid":self.pid}));
            }
        }
    }
}
impl Demo {
    fn mode_name(&self) -> &str {
        match self.mode.as_str() {
            "base" => "Base",
            "compatible" => "Compatible",
            _ => "Cooperative",
        }
    }
    fn log_at(
        &mut self,
        at: u64,
        kind: &str,
        message: &str,
        data: Value,
        display: bool,
    ) -> Result<()> {
        writeln!(
            self.journal,
            "{}",
            json!({"monotonic_ns":at,"relative_ns":at.saturating_sub(self.origin),"kind":kind,"message":message,"data":data})
        )?;
        self.journal.flush()?;
        if display
            && matches!(
                kind,
                "ready"
                    | "layout"
                    | "task_done"
                    | "submit"
                    | "patch_request"
                    | "patch_success"
                    | "patch_refused"
                    | "patch_failed"
                    | "sample_unavailable"
                    | "demo_stopped"
                    | "service_stopped"
            )
        {
            self.terminal.event(at, message, kind)?;
        }
        Ok(())
    }
    fn log(&mut self, kind: &str, message: &str, data: Value) -> Result<()> {
        self.log_at(now(), kind, message, data, true)
    }
    fn read(&self, addr: u64, length: usize) -> Result<Vec<u8>> {
        let mut b = vec![0; length];
        File::open(format!("/proc/{}/mem", self.pid))?.read_exact_at(&mut b, addr)?;
        Ok(b)
    }
    fn handle(&mut self, event: Event) -> Result<bool> {
        match event {
            Event::Enter => return Ok(true),
            Event::Eof => bail!("终端输入已关闭；停止演示，不自动执行后续步骤"),
            Event::OtherInput => self.log("input", "无效输入", json!({}))?,
            Event::Target(at, line) => {
                let f: Vec<_> = line.split_whitespace().collect();
                if f.first() == Some(&"READY") {
                    ensure!(f.len() == 4, "invalid READY");
                    self.pid = f[1].parse()?;
                    ensure!(
                        Some(self.pid) == self.child.as_ref().map(Child::id),
                        "PID mismatch"
                    );
                    self.address = u64::from_str_radix(f[2].trim_start_matches("0x"), 16)?;
                    self.state = if f[3] == "(nil)" {
                        0
                    } else {
                        u64::from_str_radix(f[3].trim_start_matches("0x"), 16)?
                    };
                    self.log_at(
                        at,
                        "ready",
                        &format!(
                            "服务启动  PID={}  线程={}  {}",
                            self.pid,
                            fs::read_dir(format!("/proc/{}/task", self.pid))?.count(),
                            self.mode_name()
                        ),
                        json!({"pid":self.pid,"address":self.address,"state":self.state,"raw":line}),
                        true,
                    )?;
                } else if f.first() == Some(&"LAYOUT") {
                    self.layouts += 1;
                    let job = self.processing.pop_front().context("开始事件无对应任务")?;
                    self.log_at(
                        at,
                        "layout",
                        &format!("任务 #{job} 开始处理"),
                        json!({"job":job,"received_ns":at,"raw":line}),
                        true,
                    )?;
                } else if f.first() == Some(&"DONE") {
                    ensure!(f.len() >= 5, "invalid DONE");
                    let id: u32 = f[1].parse()?;
                    let d = Done {
                        exported: f[2] == "1",
                        begin: f[3].parse()?,
                        end: f[4].parse()?,
                    };
                    ensure!(d.begin <= d.end, "invalid document interval");
                    if d.exported {
                        let text =
                            fs::read_to_string(self.files.join(format!("document-{id}.txt")))?;
                        ensure!(
                            text == format!("文档 {id}：20 页排版完成。\n"),
                            "文件内容错误"
                        );
                    } else {
                        ensure!(
                            !self.files.join(format!("document-{id}.txt")).exists(),
                            "未导出任务出现文件"
                        );
                    }
                    self.log_at(d.end,"task_done",&format!("任务 #{id} {}",if d.exported {"文件关闭"} else {"拒绝导出"}),json!({"job":id,"exported":d.exported,"file_begin_ns":d.begin,"file_end_ns":d.end,"log_received_ns":at,"raw":line}),true)?;
                    self.processing.retain(|job| *job != id);
                    self.done.insert(id, d);
                } else {
                    self.log_at(at, "target_log", &line, json!({}), true)?;
                }
            }
            Event::TargetEof => bail!("业务服务日志关闭；停止后续步骤"),
            Event::Check { at, count, rc } => {
                self.latest_check = Some((count, rc));
                let display = count == 1
                    || rc <= 0
                    || at.saturating_sub(self.last_check_print) >= 250_000_000;
                if display {
                    self.last_check_print = at;
                }
                let status = if rc > 0 {
                    "busy"
                } else if rc == 0 {
                    "clean"
                } else {
                    "error"
                };
                let function = if rc > 0 {
                    format!("  render_document@0x{:x}", self.address)
                } else {
                    String::new()
                };
                self.log_at(at,"stack_check",status,json!({"checks":count,"rc":rc,"function_entry":if rc>0 {Some(self.address)} else {None}}),false)?;
                if display {
                    self.terminal.status(
                        at,
                        status,
                        &format!("检查次数={count}  {status}{function}"),
                    )?;
                }
            }
            Event::Finished(outcome) => self.pending = Some(outcome),
        }
        Ok(false)
    }
    fn pump(&mut self, timeout: Duration) -> Result<bool> {
        match self.rx.recv_timeout(timeout) {
            Ok(e) => self.handle(e),
            // Never call Child::try_wait here: waitpid could consume a ptrace
            // stop owned by the patch worker. TargetEof reports service exit.
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
    fn prompt(&mut self, instruction: &str) -> Result<()> {
        // A queued Enter from a previous operation does not authorize a new step.
        while let Ok(event) = self.rx.try_recv() {
            if matches!(event, Event::Enter) {
                continue;
            }
            self.handle(event)?;
        }
        self.log_at(now(), "prompt", instruction, json!({}), false)?;
        self.terminal.prompt(Some(instruction))?;
        loop {
            if self.pump(Duration::from_millis(100))? {
                self.terminal.prompt(None)?;
                return Ok(());
            }
        }
    }
    fn submit(&mut self, id: u32) -> Result<()> {
        writeln!(self.input.as_mut().context("服务尚未启动")?, "{id}")?;
        self.input.as_mut().unwrap().flush()?;
        self.processing.push_back(id);
        self.log("submit", &format!("任务 #{id} 提交"), json!({"job":id}))
    }
    fn wait_job(&mut self, id: u32, seconds: u64) -> Result<()> {
        let until = Instant::now() + Duration::from_secs(seconds);
        while !self.done.contains_key(&id) {
            ensure!(Instant::now() < until, "等待任务 {id} 超时");
            self.pump(Duration::from_millis(100))?;
        }
        Ok(())
    }
    fn sample_cooperative(&mut self) -> Result<()> {
        match self.read(self.state, 24) {
            Ok(b) => {
                let gate = u64::from_ne_bytes(b[8..16].try_into()?);
                let active = u64::from_ne_bytes(b[16..24].try_into()?);
                let at = now();
                self.log_at(
                    at,
                    "cooperative_sample",
                    "采样",
                    json!({"gate":gate,"active":active}),
                    false,
                )?;
                if self.rendering_coop != Some((gate, active)) {
                    self.rendering_coop = Some((gate, active));
                    self.terminal.status(
                        at,
                        &format!("{gate}:{active}"),
                        &format!("gate={gate}  active={active}"),
                    )?;
                }
            }
            Err(e) => self.log(
                "sample_unavailable",
                "状态读取失败",
                json!({"error":format!("{e:#}")}),
            )?,
        }
        Ok(())
    }
    fn operate(&mut self, bytes: Vec<u8>, timeout: Duration, restore: bool) -> Result<Vec<u8>> {
        let requested = now();
        if !restore {
            self.install_requested = Some(requested);
        }
        self.latest_check = None;
        self.rendering_coop = None;
        self.last_check_print = 0;
        self.log_at(
            requested,
            "patch_request",
            if restore {
                "补丁恢复请求"
            } else {
                "补丁安装请求"
            },
            json!({"operation":if restore {"restore"} else {"install"}}),
            true,
        )?;
        let (tx, pid, address, state, mode) = (
            self.tx.clone(),
            self.pid,
            self.address,
            self.state,
            self.mode.clone(),
        );
        let expected_bytes = bytes.clone();
        let worker = std::thread::spawn(move || {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match mode.as_str() {
                    "base" => base::exchange(pid, address, &bytes),
                    "compatible" => safepatch::exchange_compat_observed(
                        pid,
                        address,
                        &bytes,
                        timeout,
                        &mut |count, rc| {
                            let _ = tx.send(Event::Check {
                                at: now(),
                                count,
                                rc,
                            });
                        },
                    ),
                    "cooperative" => safepatch::exchange(pid, state, address, &bytes, timeout),
                    _ => unreachable!(),
                }))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("补丁线程异常退出")));
            let at = now(); // Actual return, before UI validation or output.
            let _ = tx.send(Event::Finished(Outcome { at, result }));
        });
        let mut next_sample = Instant::now();
        while self.pending.is_none() {
            if self.mode == "cooperative" && Instant::now() >= next_sample {
                self.sample_cooperative()?;
                next_sample = Instant::now() + Duration::from_millis(250);
            }
            if self.pump(Duration::from_millis(50))? {
                self.log("input_while_waiting", "操作期间输入", json!({}))?;
            }
        }
        worker
            .join()
            .map_err(|_| anyhow::anyhow!("补丁线程 join 失败"))?;
        let outcome = self.pending.take().unwrap();
        self.terminal.clear_status()?;
        match outcome.result {
            Ok(previous) => {
                ensure!(
                    self.read(self.address, 8)? == expected_bytes,
                    "交换完成后的机器码读回不一致"
                );
                if restore {
                    self.restored_at = Some(outcome.at);
                } else {
                    self.installed_at = Some(outcome.at);
                }
                self.log_at(outcome.at,"patch_success",if restore {"补丁恢复成功"} else {"补丁安装成功"},json!({"operation":if restore {"restore"} else {"install"},"requested_ns":requested,"returned_ns":outcome.at,"latest_stack_check":self.latest_check}),true)?;
                if self.mode == "cooperative" {
                    self.sample_cooperative()?;
                    self.terminal.clear_status()?;
                }
                Ok(previous)
            }
            Err(e) => {
                let text = format!("{e:#}");
                let incomplete = e.downcast_ref::<safepatch::CompatIncomplete>().is_some();
                let refused = !incomplete
                    && (text.contains("timeout")
                        || text.contains("failed closed")
                        || text.contains("unsupported")
                        || text.contains("symbol unavailable"));
                let status = if refused { "拒绝" } else { "失败" };
                let unchanged = if incomplete {
                    None
                } else {
                    self.read(self.address, 8).ok()
                };

                self.log_at(outcome.at,if refused {"patch_refused"} else {"patch_failed"},&format!("补丁{}{status}",if restore {"恢复"} else {"安装"}),json!({"error":text,"incomplete":incomplete,"returned_ns":outcome.at,"current_code":unchanged,"latest_stack_check":self.latest_check}),true)?;
                Err(e)
            }
        }
    }
    fn summary(&mut self, success: bool) -> Result<()> {
        let old = self.done.get(&0).cloned();
        let observation = match (&old, self.installed_at) {
            (Some(d), Some(t)) if d.begin > t => "old_file_after_patch_return",
            (Some(d), Some(t)) if d.end < t => "old_file_closed_before_patch_return",
            (Some(_), Some(_)) => "old_file_interval_crosses_patch_return",
            _ => "unpaired",
        };
        let scope = match self.mode.as_str() {
            "base" => "unclaimed",
            "compatible" => "native_stack_check",
            _ => "cooperative_region",
        };
        let old_active_at_request = match (&old, self.install_requested) {
            (Some(d), Some(t)) => Some(d.end > t),
            _ => None,
        };
        let report = json!({"mode":self.mode,"completed":success,"files":self.files,
            "started_monotonic_ns":self.origin,
            "install_requested_ns":self.install_requested,"patch_return_ns":self.installed_at,"restore_return_ns":self.restored_at,
            "old_task_unfinished_at_patch_request":old_active_at_request,
            "old_file_begin_ns":old.as_ref().map(|d|d.begin),"old_file_close_ns":old.as_ref().map(|d|d.end),
            "observation":observation,"consistency_scope":scope,
            "tasks":self.done.iter().map(|(id,d)|json!({"id":id,"exported":d.exported,"begin_ns":d.begin,"end_ns":d.end})).collect::<Vec<_>>()});
        fs::write(
            self.session.join("summary.json"),
            serde_json::to_string_pretty(&report)?,
        )?;
        self.log_at(now(), "summary", "", report, false)
    }
}
fn run(d: &mut Demo, exe: &str, task_ms: u64, timeout: Duration) -> Result<()> {
    d.prompt("启动服务")?;
    let mut child = Command::new(exe)
        .arg(&d.files)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    d.input = child.stdin.take();
    let output = child.stdout.take().unwrap();
    d.child = Some(child);
    let tx = d.tx.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            match line {
                Ok(line) => {
                    if tx.send(Event::Target(now(), line)).is_err() {
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(Event::TargetEof);
    });
    while d.pid == 0 {
        d.pump(Duration::from_millis(100))?;
    }
    let original = d.read(d.address, 8)?;
    d.prompt("提交旧任务 #0")?;
    d.submit(0)?;
    while d.layouts == 0 {
        d.pump(Duration::from_millis(100))?;
    }
    d.prompt("安装维护补丁")?;
    // Drain already received events before describing the current old-task state.
    while let Ok(event) = d.rx.try_recv() {
        d.handle(event)?;
    }
    if d.done.contains_key(&0) {
        d.log("old_already_done", "任务 #0 已完成", json!({}))?;
    }
    ensure!(
        d.operate(PATCH.to_vec(), timeout, false)? == original,
        "保存的原代码不一致"
    );
    d.prompt("提交新任务 #1")?;
    d.submit(1)?;
    d.wait_job(1, task_ms / 1000 + 10)?;
    ensure!(!d.done[&1].exported, "新入口未生效");
    d.wait_job(0, task_ms / 1000 + 10)?;
    d.prompt("恢复补丁")?;
    ensure!(
        d.operate(original.clone(), timeout, true)? == PATCH,
        "恢复前字节不是已安装补丁"
    );
    ensure!(d.read(d.address, 8)? == original, "恢复后字节不一致");
    d.submit(2)?;
    d.wait_job(2, task_ms / 1000 + 10)?;
    ensure!(d.done[&2].exported, "恢复后实际导出失败");
    Ok(())
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        args.len() == 6,
        "usage: interactive TARGET MODE SESSION TASK_MS TIMEOUT_MS"
    );
    ensure!(
        ["base", "compatible", "cooperative"].contains(&args[2].as_str()),
        "未知模式"
    );
    let task_ms: u64 = args[4].parse()?;
    let timeout_ms: u64 = args[5].parse()?;
    ensure!(task_ms > 0 && timeout_ms > 0, "时长必须大于零");
    let session = fs::canonicalize(&args[3])?;
    let files = session.join("files");
    fs::create_dir(&files)?;
    let origin = now();
    fs::write(
        session.join("session.json"),
        serde_json::to_string_pretty(
            &json!({"started_monotonic_ns":origin,"mode":args[2],"task_ms":task_ms,"files":files}),
        )?,
    )?;
    let terminal = terminal::Terminal::new(origin);
    let (tx, rx) = mpsc::channel();
    let keyboard = tx.clone();
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let event = match line {
                Ok(s) if s.trim().is_empty() => Event::Enter,
                Ok(_) => Event::OtherInput,
                Err(_) => break,
            };
            if keyboard.send(event).is_err() {
                return;
            }
        }
        let _ = keyboard.send(Event::Eof);
    });
    let mut d = Demo {
        mode: args[2].clone(),
        files,
        session: session.clone(),
        journal: File::create(session.join("events.jsonl"))?,
        tx,
        rx,
        child: None,
        input: None,
        pid: 0,
        address: 0,
        state: 0,
        layouts: 0,
        done: BTreeMap::new(),
        pending: None,
        last_check_print: 0,
        terminal,
        origin,
        rendering_coop: None,
        processing: Default::default(),
        latest_check: None,
        install_requested: None,
        installed_at: None,
        restored_at: None,
    };
    d.log(
        "configuration",
        &format!(
            "交互演示：{}；任务时长 {} ms；等待上限 {} ms。",
            args[2], task_ms, timeout_ms
        ),
        json!({"task_ms":task_ms,"timeout_ms":timeout_ms}),
    )?;
    d.log(
        "directory",
        &format!("独立终端可监视真实输出目录：{}", d.files.display()),
        json!({"files":d.files}),
    )?;
    let result = run(&mut d, &args[1], task_ms, Duration::from_millis(timeout_ms));
    if let Err(e) = &result {
        d.log(
            "demo_stopped",
            "操作停止",
            json!({"error":format!("{e:#}")}),
        )?;
    }
    d.summary(result.is_ok())?;
    if result.is_ok() {
        d.prompt("关闭服务")?;
    }
    result
}
