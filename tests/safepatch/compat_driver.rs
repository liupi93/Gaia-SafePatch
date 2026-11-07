#[path = "../../gaia-xdp/src/safepatch.rs"]
mod safepatch;
use anyhow::{Result, ensure};
use std::{
    io::{BufRead, BufReader},
    process::{Command, Stdio},
    time::Duration,
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
    for conflict in [false, true] {
        let mut cmd = Command::new(&args[1]);
        if conflict {
            cmd.arg("conflict");
        }
        let mut child = Child(cmd.stdout(Stdio::piped()).spawn()?);
        let mut line = String::new();
        BufReader::new(child.0.stdout.take().unwrap()).read_line(&mut line)?;
        let f: Vec<_> = line.split_whitespace().collect();
        let pid = f[1].parse()?;
        let addr = u64::from_str_radix(f[2].trim_start_matches("0x"), 16)?;
        let patch = [0xb8, 99, 0, 0, 0, 0xc3, 0x90, 0x90];
        if conflict {
            let error = safepatch::exchange_compat(pid, addr, &patch, Duration::from_millis(30))
                .unwrap_err();
            ensure!(
                error.to_string().contains("RIP conflict timeout"),
                "{error:#}"
            );
            ensure!(child.0.try_wait()?.is_none(), "conflict killed target");
            println!("RIP conflict retries/timeout verified");
        } else {
            ensure!(
                safepatch::exchange(pid, addr, addr, &patch, Duration::ZERO).is_err(),
                "strict mode accepted missing ABI"
            );
            for _ in 0..20 {
                let old = safepatch::exchange_compat(pid, addr, &patch, Duration::from_secs(2))?;
                ensure!(old == [0xb8, 10, 0, 0, 0, 0xc3, 0x90, 0x90]);
                let installed =
                    safepatch::exchange_compat(pid, addr, &old, Duration::from_secs(2))?;
                ensure!(installed == patch, "readback failed");
            }
            for (address, bytes) in [
                (1, patch.as_slice()),
                (u64::MAX - 3, patch.as_slice()),
                (addr, &patch[..7]),
                (addr, &patch[..0]),
            ] {
                ensure!(
                    safepatch::exchange_compat(pid, address, bytes, Duration::ZERO).is_err(),
                    "invalid input succeeded"
                );
            }
            ensure!(child.0.try_wait()?.is_none(), "target died");
            println!("20 installs/restores, original bytes, readback, invalid input verified");
        }
    }
    Ok(())
}
