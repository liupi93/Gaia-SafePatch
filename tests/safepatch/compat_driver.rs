#[path = "../../gaia-xdp/src/safepatch.rs"]
mod safepatch;
use anyhow::{Result, ensure};
use std::{
    io::{BufRead, BufReader},
    os::unix::fs::FileExt,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.get(2).is_some_and(|v| v == "restricted") {
        return restricted(&args[1]);
    }
    if args.get(2).is_some_and(|v| v == "reject-static") {
        return reject_static(&args[1]);
    }
    for scenario in ["normal", "conflict", "churn"] {
        let mut cmd = Command::new(&args[1]);
        if scenario != "normal" {
            cmd.arg(scenario);
        }
        let mut child = Child(cmd.stdout(Stdio::piped()).spawn()?);
        let mut line = String::new();
        BufReader::new(child.0.stdout.take().unwrap()).read_line(&mut line)?;
        let f: Vec<_> = line.split_whitespace().collect();
        let pid = f[1].parse()?;
        let addr = u64::from_str_radix(f[2].trim_start_matches("0x"), 16)?;
        let patch = [0xb8, 99, 0, 0, 0, 0xc3, 0x90, 0x90];
        let memory = std::fs::File::open(format!("/proc/{pid}/mem"))?;
        let mut original = [0; 8];
        memory.read_exact_at(&mut original, addr)?;
        let start = Instant::now();
        for _ in 0..20 {
            let error = safepatch::exchange_compat(pid, addr, &patch, Duration::from_millis(30))
                .unwrap_err();
            ensure!(error.to_string().contains("unsupported ELF"), "{error:#}");
            let mut current = [0; 8];
            memory.read_exact_at(&mut current, addr)?;
            ensure!(current == original, "refusal changed code");
            ensure!(child.0.try_wait()?.is_none(), "refusal killed target");
            let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
            ensure!(
                status.lines().any(|s| s == "TracerPid:\t0"),
                "refusal left tracer attached"
            );
        }
        for (address, bytes) in [
            (1, patch.as_slice()),
            (u64::MAX - 3, patch.as_slice()),
            (addr, &patch[..7]),
            (addr, &patch[..0]),
        ] {
            ensure!(safepatch::exchange_compat(pid, address, bytes, Duration::ZERO).is_err());
        }
        println!(
            "result=refused scenario={scenario} attempts=20 checks=0 elapsed_us={} code_unchanged=true target_alive=true tracer=0 reason=unsupported ELF",
            start.elapsed().as_micros()
        );
    }
    Ok(())
}

fn restricted(executable: &str) -> Result<()> {
    use object::{Object, ObjectSymbol};
    let data = std::fs::read(executable)?;
    let elf = object::File::parse(data.as_slice())?;
    let addr = elf
        .symbols()
        .find(|s| s.name() == Ok("target_value"))
        .unwrap()
        .address();
    let main = elf
        .symbols()
        .find(|s| s.name() == Ok("main"))
        .unwrap()
        .address();
    let mut child = Child(Command::new(executable).stdout(Stdio::piped()).spawn()?);
    let mut output = BufReader::new(child.0.stdout.take().unwrap());
    let mut line = String::new();
    output.read_line(&mut line)?;
    ensure!(line == "READY\n");
    line.clear();
    output.read_line(&mut line)?;
    ensure!(line == "OLD_START\n");
    let (send, recv) = std::sync::mpsc::channel();
    let collector = std::thread::spawn(move || {
        for line in output.lines() {
            if send.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let pid = child.0.id();
    let memory = std::fs::File::open(format!("/proc/{pid}/mem"))?;
    let mut original = [0; 8];
    memory.read_exact_at(&mut original, addr)?;
    let patch = [0xb8, 99, 0, 0, 0, 0xc3, 0x90, 0x90];
    let timed = std::time::Instant::now();
    let error =
        safepatch::exchange_compat(pid, addr, &patch, Duration::from_millis(30)).unwrap_err();
    ensure!(error.to_string().contains("timeout"), "{error:#}");
    let mut current = [0; 8];
    memory.read_exact_at(&mut current, addr)?;
    ensure!(current == original && child.0.try_wait()?.is_none());
    println!(
        "timeout_refusal elapsed_us={} code_unchanged=true target_alive=true reason={error:#}",
        timed.elapsed().as_micros()
    );
    let begin = std::time::Instant::now();
    ensure!(safepatch::exchange_compat(pid, addr, &patch, Duration::from_secs(2))? == original);
    let elapsed = begin.elapsed();
    // The function is still sleeping after the 30ms failed attempt. A RIP-only
    // test would accept immediately in delay(); this must wait for its caller.
    ensure!(
        elapsed >= Duration::from_millis(100),
        "old call was not drained: {elapsed:?}"
    );
    ensure!(recv.recv_timeout(Duration::from_secs(1))? == "OLD_DONE");
    memory.read_exact_at(&mut current, addr)?;
    ensure!(current == patch);
    // Execute the installed entry, not just inspect its bytes.
    loop {
        if recv.recv_timeout(Duration::from_secs(2))? == "NEW" {
            break;
        }
    }
    let error = safepatch::exchange_compat_related(
        pid,
        addr,
        &original,
        Duration::from_millis(30),
        &[main],
    )
    .unwrap_err();
    ensure!(
        error.to_string().contains("timeout"),
        "related main not protected: {error:#}"
    );
    memory.read_exact_at(&mut current, addr)?;
    ensure!(current == patch);
    let mut installs = 1;
    for _ in 0..3 {
        ensure!(safepatch::exchange_compat(pid, addr, &original, Duration::from_secs(2))? == patch);
        memory.read_exact_at(&mut current, addr)?;
        ensure!(current == original);
        ensure!(safepatch::exchange_compat(pid, addr, &patch, Duration::from_secs(2))? == original);
        installs += 1;
    }
    ensure!(safepatch::exchange_compat(pid, addr, &original, Duration::from_secs(2))? == patch);
    memory.read_exact_at(&mut current, addr)?;
    ensure!(current == original);
    ensure!(child.0.try_wait()?.is_none());
    ensure!(
        std::fs::read_to_string(format!("/proc/{pid}/status"))?
            .lines()
            .any(|s| s == "TracerPid:\t0")
    );
    // Observe actual original-function execution again after the last restore.
    loop {
        if recv.recv_timeout(Duration::from_secs(2))? == "OLD_START" {
            break;
        }
    }
    ensure!(recv.recv_timeout(Duration::from_secs(2))? == "OLD_DONE");
    ensure!(child.0.try_wait()?.is_none());
    println!(
        "restricted installs={installs} restorations=4 waited_us={} target_alive=true original_restored=true function_consistency_complete=true business_consistency=unclaimed",
        elapsed.as_micros()
    );
    child.0.kill()?;
    child.0.wait()?;
    drop(recv);
    collector.join().unwrap();
    Ok(())
}

fn reject_static(executable: &str) -> Result<()> {
    use object::{Object, ObjectSymbol};
    let data = std::fs::read(executable)?;
    let elf = object::File::parse(data.as_slice())?;
    let addr = elf
        .symbols()
        .find(|s| s.name() == Ok("target_value"))
        .unwrap()
        .address();
    let mut child = Child(Command::new(executable).stdout(Stdio::piped()).spawn()?);
    let mut line = String::new();
    let mut output = BufReader::new(child.0.stdout.take().unwrap());
    output.read_line(&mut line)?;
    let memory = std::fs::File::open(format!("/proc/{}/mem", child.0.id()))?;
    let mut original = [0; 8];
    memory.read_exact_at(&mut original, addr)?;
    let error = safepatch::exchange_compat(
        child.0.id(),
        addr,
        &[0xb8, 99, 0, 0, 0, 0xc3, 0x90, 0x90],
        Duration::from_secs(1),
    )
    .unwrap_err();
    let mut current = [0; 8];
    memory.read_exact_at(&mut current, addr)?;
    ensure!(current == original && child.0.try_wait()?.is_none());
    println!("static_rejection code_unchanged=true target_alive=true reason={error:#}");
    Ok(())
}
