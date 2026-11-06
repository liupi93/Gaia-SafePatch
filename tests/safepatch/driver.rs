#[path = "../../gaia-xdp/src/safepatch.rs"]
mod safepatch;
mod base {
    include!(concat!(env!("OUT_DIR"), "/base.rs"));
}
use anyhow::{Context, Result, ensure};
use std::{
    io::{BufRead, BufReader},
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
    let baseline = args.get(3).is_some_and(|s| s == "base");
    let compat_risk = args.get(3).is_some_and(|s| s == "compat-risk");
    let risk = compat_risk || args.get(3).is_some_and(|s| s == "risk");
    let mut command = Command::new(&args[1]);
    if risk {
        command.env("GAIA_RISK", "1");
    }
    let mut child = Child(command.stdout(Stdio::piped()).spawn()?);
    let mut output = BufReader::new(child.0.stdout.take().unwrap());
    let mut ready = String::new();
    output.read_line(&mut ready)?;
    let fields: Vec<_> = ready.split_whitespace().collect();
    ensure!(fields.first() == Some(&"READY"), "target readiness failed");
    let pid = fields[1].parse()?;
    let address = u64::from_str_radix(fields[2].trim_start_matches("0x"), 16)?;
    let state = u64::from_str_radix(fields[3].trim_start_matches("0x"), 16)?;
    let logfile = std::fs::File::create(&args[2])?;
    let collector = std::thread::spawn(move || {
        let mut log = logfile;
        std::io::copy(&mut output, &mut log).unwrap();
    });
    // Wait for all eight first calls, not a probabilistic delay: read the live active count.
    fn active(pid: u32, state: u64) -> Result<u64> {
        use std::os::unix::fs::FileExt;
        let file = std::fs::File::open(format!("/proc/{pid}/mem"))?;
        let mut bytes = [0; 8];
        file.read_exact_at(&mut bytes, state + 16)?;
        Ok(u64::from_ne_bytes(bytes))
    }
    if risk {
        use std::os::unix::fs::FileExt;
        let deadline = Instant::now() + Duration::from_secs(3);
        while active(pid, state)? == 0 {
            ensure!(Instant::now() < deadline, "no old call");
        }
        let observed = active(pid, state)?;
        let exchange = |bytes: &[u8]| if compat_risk { safepatch::exchange_compat(pid, address, bytes, Duration::from_secs(3)) } else { base::exchange(pid, address, bytes) };
        let original = exchange(&[0xb8, 99, 0, 0, 0, 0xc3, 0x90, 0x90])?;
        let release = u64::from_str_radix(fields[4].trim_start_matches("0x"), 16)?;
        std::fs::OpenOptions::new()
            .write(true)
            .open(format!("/proc/{pid}/mem"))?
            .write_all_at(&1u64.to_ne_bytes(), release)?;
        std::thread::sleep(Duration::from_millis(300));
        exchange(&original)?;
        child.0.kill()?;
        child.0.wait()?;
        collector.join().unwrap();
        println!("BASE_RISK actual_write_with_active={observed}");
        return Ok(());
    }
    if args[1].contains("long_running") {
        let deadline = Instant::now() + Duration::from_secs(3);
        while active(pid, state)? == 0 {
            ensure!(Instant::now() < deadline, "no active call");
        }
        ensure!(
            safepatch::exchange(pid, state, address, &[0x90; 8], Duration::ZERO).is_err(),
            "timeout unexpectedly succeeded"
        );
        println!("TIMEOUT verified");
    }
    let patch = [0xb8, 99, 0, 0, 0, 0xc3, 0x90, 0x90];
    let mut durations = Vec::new();
    let mut successes = 0;
    let count_address = u64::from_str_radix(fields[5].trim_start_matches("0x"), 16)?;
    let before = active(pid, count_address - 16)?;
    let wall = Instant::now();
    let exchange = |data: &[u8]| {
        if baseline {
            base::exchange(pid, address, data)
        } else {
            safepatch::exchange(pid, state, address, data, Duration::from_secs(3))
        }
    };
    for _ in 0..20 {
        let start = Instant::now();
        let original = exchange(&patch).context("install")?;
        durations.push(start.elapsed().as_micros());
        successes += 1;
        std::thread::sleep(Duration::from_millis(25));
        exchange(&original).context("restore")?;
        successes += 1;
        std::thread::sleep(Duration::from_millis(25));
    }
    let wall_us = wall.elapsed().as_micros();
    let completed_calls = active(pid, count_address - 16)? - before;
    ensure!(
        safepatch::exchange(pid, state, 1, &patch, Duration::from_secs(3)).is_err(),
        "invalid address accepted"
    );
    ensure!(
        safepatch::exchange(pid, state + 8, address, &patch, Duration::from_secs(3)).is_err(),
        "invalid ABI accepted"
    );
    child.0.kill()?;
    child.0.wait()?;
    collector.join().unwrap();
    println!(
        "successes={successes} wall_us={wall_us} completed_calls={completed_calls} drain_mean_us={} drain_max_us={} restore_failures=0",
        durations.iter().sum::<u128>() / durations.len() as u128,
        durations.iter().max().unwrap()
    );
    Ok(())
}
