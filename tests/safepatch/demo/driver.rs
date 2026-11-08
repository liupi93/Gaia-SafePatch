#[path = "../../../gaia-xdp/src/safepatch.rs"]
mod safepatch;
mod base {
    include!(concat!(env!("OUT_DIR"), "/base.rs"));
}
use std::{
    io::{BufRead, BufReader},
    os::unix::{fs::FileExt, process::ExitStatusExt},
    process::{Command, Stdio},
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
fn read(pid: u32, addr: u64) -> Result<u64> {
    let mut b = [0; 8];
    std::fs::File::open(format!("/proc/{pid}/mem"))?.read_exact_at(&mut b, addr)?;
    Ok(u64::from_ne_bytes(b))
}
fn wait_value(pid: u32, addr: u64, want: u64) -> Result<()> {
    let end = Instant::now() + Duration::from_secs(6);
    while read(pid, addr)? != want {
        ensure!(Instant::now() < end, "等待订单进度超时：演示未达到预期状态");
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        args.len() == 4,
        "用法：驱动程序 目标程序 entry（入口冲突）或 orders（订单结算） base（原版）或 compatible（兼容）或 cooperative（协作）"
    );
    let scene = &args[2];
    let mode = &args[3];
    ensure!(["base", "compatible", "cooperative"].contains(&mode.as_str()));
    let mut cmd = Command::new(&args[1]);
    if scene == "entry" {
        cmd.arg("entry");
    }
    let mut target = Target(cmd.stdout(Stdio::piped()).spawn()?);
    let mut out = BufReader::new(target.0.stdout.take().unwrap());
    let mut line = String::new();
    out.read_line(&mut line)?;
    let f: Vec<_> = line.split_whitespace().collect();
    ensure!(f.first() == Some(&"READY"), "{line}");
    let pid: u32 = f[1].parse()?;
    let addr: Vec<u64> = f[2..]
        .iter()
        .map(|s| {
            if *s == "(nil)" {
                Ok(0)
            } else {
                u64::from_str_radix(s.trim_start_matches("0x"), 16)
            }
        })
        .collect::<std::result::Result<_, _>>()?;
    let scene_name = if scene == "entry" {
        "入口指令冲突"
    } else {
        "订单结算切换"
    };
    let mode_name = match mode.as_str() {
        "base" => "原版 Gaia",
        "compatible" => "兼容模式",
        _ => "协作模式",
    };
    println!("【演示开始】场景：{scene_name}｜补丁方式：{mode_name}");
    println!("目标进程编号：{pid}｜工作线程：8 个（另有 1 个主线程）");
    let collector = std::thread::spawn(move || {
        for line in out.lines() {
            println!("  {}", line.unwrap());
        }
    });
    wait_value(pid, addr[3], 1)?;
    let patch = [0xb8, 99, 0, 0, 0, 0xc3, 0x90, 0x90];
    let exchange = |data: &[u8]| match mode.as_str() {
        "base" => base::exchange(pid, addr[0], data),
        "compatible" => safepatch::exchange_compat(pid, addr[0], data, Duration::from_millis(400)),
        _ => safepatch::exchange(pid, addr[1], addr[0], data, Duration::from_secs(4)),
    };
    if scene == "entry" {
        // Observe the actual worker RIP, without changing registers or code.
        std::thread::sleep(Duration::from_millis(30));
        let tids = std::fs::read_dir(format!("/proc/{pid}/task"))?;
        let mut found = false;
        for tid in tids {
            let tid: i32 = tid?.file_name().to_string_lossy().parse()?;
            if tid == pid as i32 {
                continue;
            }
            unsafe {
                ensure!(libc::ptrace(libc::PTRACE_SEIZE, tid, 0, 0) == 0);
                ensure!(libc::ptrace(libc::PTRACE_INTERRUPT, tid, 0, 0) == 0);
                let mut status = 0;
                ensure!(
                    libc::waitpid(tid, &mut status, libc::__WALL) == tid
                        && libc::WIFSTOPPED(status)
                );
                let mut regs: libc::user_regs_struct = std::mem::zeroed();
                ensure!(libc::ptrace(libc::PTRACE_GETREGS, tid, 0, &mut regs) == 0);
                found |= regs.rip == addr[0] + 2;
                ensure!(libc::ptrace(libc::PTRACE_DETACH, tid, 0, 0) == 0);
            }
        }
        ensure!(found, "工作线程尚未进入预定位置，不能继续本次演示");
        println!(
            "【已确认】工作线程正在入口第 3 个字节处循环；这就是稍后要覆盖的位置。\n原版写入器保持原样，演示驱动没有修改线程的执行位置。"
        );
        if mode == "base" {
            exchange(&patch)?;
            let end = Instant::now() + Duration::from_secs(3);
            let status = loop {
                if let Some(s) = target.0.try_wait()? {
                    break s;
                }
                ensure!(Instant::now() < end, "目标没有按预期崩溃：本次异常复现失败");
                std::thread::sleep(Duration::from_millis(5));
            };
            ensure!(
                status.signal() == Some(libc::SIGSEGV),
                "目标退出方式不符合预期：{status}"
            );
            println!(
                "【异常已复现】工作线程从新指令的中间继续执行，访问了无效地址。\n操作系统已终止目标进程（SIGSEGV：非法内存访问）。\n【检查通过】收到真实崩溃信号，不是演示程序伪造的报错。"
            );
        } else {
            ensure!(mode == "compatible", "入口冲突场景仅比较原版与兼容模式");
            let error = exchange(&patch).unwrap_err();
            ensure!(
                error.to_string().contains("RIP conflict timeout"),
                "{error:#}"
            );
            ensure!(target.0.try_wait()?.is_none());
            ensure!(read(pid, addr[0])? == 0x90909090feebc031);
            println!(
                "【保护生效】线程仍在准备覆盖的位置，兼容模式等待 400 毫秒后拒绝修改。\n【检查通过】原代码没有改变，目标进程仍然存活。\n本次没有安装补丁：拒绝危险操作正是预期结果。"
            );
        }
    } else {
        ensure!(scene == "orders");
        println!(
            "【旧订单已进入】订单 0 正在执行旧策略：约需 1.2 秒，返回折扣值 10。\n【开始安装】补丁让后续调用直接返回折扣值 99。"
        );
        let start = Instant::now();
        let original = exchange(&patch).context("安装补丁失败")?;
        ensure!(
            read(pid, addr[0])? == u64::from_ne_bytes(patch),
            "读回检查失败：实际机器码与补丁不一致"
        );
        println!(
            "【安装完成】耗时 {} 毫秒，已重新读取机器码并确认正确。\n【放行新订单】现在让另外 7 个线程处理订单。",
            start.elapsed().as_millis()
        );
        std::fs::OpenOptions::new()
            .write(true)
            .open(format!("/proc/{pid}/mem"))?
            .write_all_at(&1u64.to_ne_bytes(), addr[2])?;
        wait_value(pid, addr[4], 8)?;
        let late = read(pid, addr[5])?;
        ensure!(
            late == if mode == "cooperative" { 0 } else { 1 },
            "迟到旧结果数量不符合预期：{late}"
        );
        ensure!(
            exchange(&original)? == patch,
            "恢复检查失败：修改前的内容不是已安装的补丁"
        );
        ensure!(
            read(pid, addr[0])? == u64::from_ne_bytes(original.try_into().unwrap()),
            "恢复检查失败：原始机器码未正确恢复"
        );
        println!("【结果汇总】8 个订单全部完成；迟到的旧结果：{late} 个。");
        if late == 0 {
            println!("【协作效果】旧订单先提交完毕，随后才切换策略；没有出现新旧结果交错。");
        } else {
            println!("【业务顺序异常】新策略结果已经提交，之后却又提交了旧策略结果！");
            println!("本演示要求：新策略结果开始提交后，不再提交旧策略结果。");
        }
        println!("【恢复完成】已恢复原始代码；安装、读回检查和恢复检查均通过。");
    }
    let _ = target.0.kill();
    let _ = target.0.wait();
    collector.join().unwrap();
    Ok(())
}
