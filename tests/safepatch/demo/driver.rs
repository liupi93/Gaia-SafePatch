#[path = "../../../gaia-xdp/src/safepatch.rs"]
mod safepatch;
mod base {
    include!(concat!(env!("OUT_DIR"), "/base.rs"));
}
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::fs::FileExt,
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};

struct Target(std::process::Child);
impl Drop for Target {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn now_ns() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time);
    }
    time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
}
fn read_code(pid: u32, address: u64) -> Result<[u8; 8]> {
    let mut bytes = [0; 8];
    fs::File::open(format!("/proc/{pid}/mem"))?.read_exact_at(&mut bytes, address)?;
    Ok(bytes)
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        args.len() == 4,
        "用法：驱动 目标程序 control|base|compatible|cooperative 输出目录"
    );
    let mode = args[2].as_str();
    ensure!(
        ["control", "base", "compatible", "cooperative"].contains(&mode),
        "未知模式"
    );
    let directory = std::path::Path::new(&args[3]);
    ensure!(
        !directory.exists(),
        "输出目录已存在，请换一个目录以免混入旧文件"
    );
    fs::create_dir_all(directory)?;
    let directory = fs::canonicalize(directory)?;
    let mut target = Target(
        Command::new(&args[1])
            .arg(&directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?,
    );
    let mut input = target.0.stdin.take().unwrap();
    let mut output = BufReader::new(target.0.stdout.take().unwrap());
    let mut ready = String::new();
    output.read_line(&mut ready)?;
    let fields: Vec<_> = ready.split_whitespace().collect();
    ensure!(
        fields.len() == 4 && fields[0] == "READY",
        "文档服务未正确启动：{ready}"
    );
    let pid: u32 = fields[1].parse()?;
    ensure!(pid == target.0.id());
    let address = u64::from_str_radix(fields[2].trim_start_matches("0x"), 16)?;
    let state = if fields[3] == "(nil)" {
        0
    } else {
        u64::from_str_radix(fields[3].trim_start_matches("0x"), 16)?
    };
    let name = match mode {
        "control" => "不打补丁（对照）",
        "base" => "原版 Gaia",
        "compatible" => "兼容模式",
        _ => "协作模式",
    };
    println!("【文档导出服务】{name}｜8 个工作线程，1 个主线程");
    println!("【业务要求】进入维护状态之后，输出目录不应再新增文档。");
    println!("【输出目录】{}", directory.display());
    let (sender, receiver) = mpsc::channel();
    let collector = std::thread::spawn(move || {
        for line in output.lines() {
            match line {
                Ok(line) => {
                    if let Some((_, text)) = line.split_once('\t') {
                        println!("  {text}");
                    }
                    if sender.send(line).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(format!("读取日志失败：{error}"));
                    break;
                }
            }
        }
    });
    // 通过正常作业输入提交文档，不读取或改写业务控制变量。
    writeln!(input, "0")?;
    input.flush()?;
    loop {
        let event = receiver.recv_timeout(Duration::from_secs(5))?;
        if event.starts_with("LAYOUT") {
            break;
        }
    }
    let original = read_code(pid, address)?;
    // 与现有 override_return 相同的 8 字节编码，这次返回 0（暂停导出）。
    let patch = [0xb8, 0, 0, 0, 0, 0xc3, 0x90, 0x90];
    let exchange = |bytes: &[u8]| match mode {
        "base" => base::exchange(pid, address, bytes),
        "compatible" => safepatch::exchange_compat(pid, address, bytes, Duration::from_secs(3)),
        "cooperative" => safepatch::exchange(pid, state, address, bytes, Duration::from_secs(3)),
        _ => unreachable!(),
    };
    if mode == "compatible" {
        let rejected = safepatch::exchange_compat(pid, address, &patch, Duration::from_millis(30));
        ensure!(rejected.is_err(), "活动旧调用被提前接受");
        ensure!(read_code(pid, address)? == original, "等待超时改变原代码");
        ensure!(target.0.try_wait()?.is_none(), "等待超时导致目标退出");
    }
    let started = Instant::now();
    let cutoff = if mode != "control" {
        println!("【切换维护状态】新版本的导出函数将直接返回“暂不导出”。");
        let result = exchange(&patch);
        // Sample immediately on return, before validation, logging or file reads.
        let returned_at = now_ns();
        ensure!(result? == original, "安装时保存的原始代码不一致");
        returned_at
    } else {
        now_ns()
    };
    if mode == "compatible" {
        ensure!(
            started.elapsed() >= Duration::from_millis(500),
            "活动旧调用未等待"
        );
        println!("【原生栈检查通过】已等待旧函数调用；不声明业务排空或目录稳定。");
    }
    if mode != "control" {
        ensure!(read_code(pid, address)? == patch, "补丁读回检查失败");
        if mode == "compatible" {
            println!(
                "【代码安装返回】耗时 {} 毫秒；业务排空未确认。现在提交第二个文档。",
                started.elapsed().as_millis()
            );
        } else {
            println!(
                "【切换已返回成功】耗时 {} 毫秒；现在提交第二个文档。",
                started.elapsed().as_millis()
            );
        }
    } else {
        println!("【保持原程序】未安装补丁，现在提交第二个文档。");
    }
    writeln!(input, "1")?;
    input.flush()?;
    let mut done = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while done.len() < 2 {
        let event = receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
        if event.starts_with("DONE ") {
            let data: Vec<_> = event
                .split_once('\t')
                .context("作业日志格式不正确")?
                .0
                .split_whitespace()
                .collect();
            done.push((
                data[1].parse::<u32>()?,
                data[2] == "1",
                data[3].parse::<u64>()?,
                data[4].parse::<u64>()?,
            ));
        }
    }
    let old = done
        .iter()
        .find(|row| row.0 == 0)
        .context("缺少旧作业记录")?;
    let new = done
        .iter()
        .find(|row| row.0 == 1)
        .context("缺少新作业记录")?;
    ensure!(
        old.1 && directory.join("document-0.txt").is_file(),
        "旧作业没有实际生成文件"
    );
    ensure!(new.1 == (matches!(mode, "control")), "新作业行为不符合预期");
    ensure!(
        directory.join("document-1.txt").exists() == (matches!(mode, "control")),
        "新作业文件不符合预期"
    );
    if matches!(mode, "control") {
        println!("【未安装验证通过】两个文档均正常生成；原始代码未变，进程仍存活。");
    } else {
        if mode == "cooperative" {
            ensure!(old.3 < cutoff, "旧文件提交跨过维护完成时间");
            println!("【顺序正确】旧文档在维护完成之前已经写完；维护完成后没有新增文件。");
        } else if mode == "base" {
            ensure!(old.2 > cutoff, "本次未复现维护完成后新增文件，不能记为通过");
            println!("【异常已复现】维护切换已经返回成功，旧作业却在之后新增了 document-0.txt！");
        }
        if old.2 > cutoff {
            println!(
                "【时间对照】切换返回成功之后，又过了 {} 毫秒，旧文件才开始写入。",
                (old.2 - cutoff) / 1_000_000
            );
        } else if old.3 < cutoff {
            println!("【时间对照】本次旧文件在安装返回前已经关闭；此观测不代表通用业务排空保证。");
        } else {
            println!("【时间对照】旧文件活动区间跨越安装返回时刻，不能确认返回时业务已排空。");
        }
        ensure!(
            exchange(&original)? == patch,
            "恢复前的机器码不是已安装补丁"
        );
        ensure!(
            read_code(pid, address)? == original,
            "恢复原始代码后的检查失败"
        );
        println!("【恢复通过】原始代码已恢复，读回检查一致。现在提交第三个文档。");
        writeln!(input, "2")?;
        input.flush()?;
        loop {
            let event = receiver.recv_timeout(Duration::from_secs(5))?;
            if event.starts_with("DONE 2 ") {
                ensure!(
                    event.starts_with("DONE 2 1 "),
                    "恢复后第三个文档没有成功导出"
                );
                ensure!(
                    directory.join("document-2.txt").is_file(),
                    "恢复后缺少真实输出文件"
                );
                println!("【恢复后试运行】第三个文档已正常生成，服务恢复导出。");
                break;
            }
        }
    }
    ensure!(
        fs::read_to_string(directory.join("document-0.txt"))? == "文档 0：20 页排版完成。\n",
        "旧文档内容不符合预期"
    );
    ensure!(target.0.try_wait()?.is_none(), "目标进程异常退出");
    let installed = mode != "control";
    let consistent = mode == "cooperative";
    let observation = if mode == "control" {
        "no_patch"
    } else if old.2 > cutoff {
        "file_creation_started_after_return"
    } else if old.3 < cutoff {
        "file_closed_before_return"
    } else {
        "file_activity_spans_return"
    };
    let report = serde_json::to_string_pretty(&serde_json::json!({
        "code_installed": installed,
        // Legacy field expresses a claim, not whether a violation was observed.
        "consistency_complete": consistent,
        "business_consistency_claimed": consistent,
        "business_observation": observation,
        "return_timestamp_sampling": "immediately_after_exchange_before_validation",
        "mode": mode,
        "switch_return_ns": cutoff,
        "old_file_begin_ns": old.2,
        "old_file_end_ns": old.3,
        "old_file_begin_minus_return_ns": old.2 as i128 - cutoff as i128,
        "old_file_end_minus_return_ns": old.3 as i128 - cutoff as i128,
        "new_file_exists": directory.join("document-1.txt").exists(),
        "restored_file_exists": directory.join("document-2.txt").exists(),
    }))?;
    for id in 0..=2 {
        let expected = id == 0
            || (id == 1 && matches!(mode, "control"))
            || (id == 2 && !matches!(mode, "control"));
        let file = directory.join(format!("document-{id}.txt"));
        if expected {
            ensure!(
                fs::read_to_string(file)? == format!("文档 {id}：20 页排版完成。\n"),
                "文档 {id} 内容不符合预期"
            );
        } else {
            ensure!(!file.exists(), "不应生成文档 {id}");
        }
    }
    let mut report_path = directory.as_os_str().to_os_string();
    report_path.push(".result.json");
    fs::write(std::path::PathBuf::from(report_path), report)?;
    println!("【实物证据】{}", directory.join("document-0.txt").display());
    let _ = target.0.kill();
    let _ = target.0.wait();
    collector.join().unwrap();
    Ok(())
}
