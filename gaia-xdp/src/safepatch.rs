//! Linux x86_64 cooperative and restricted verified non-cooperative runtimes.
//! See tests/safepatch/README.md for the supported consistency guarantees.
use std::{
    fs,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};

fn trace(request: u32, tid: i32, address: u64, data: u64) -> Result<i64> {
    #[cfg(test)]
    TRACE_FAILURE.with(|fault| {
        if fault.get() == Some(request) {
            fault.set(None);
            bail!("injected ptrace request {request} failure");
        }
        Ok(())
    })?;
    #[cfg(test)]
    if request == libc::PTRACE_POKETEXT {
        FAULT.with(|f| {
            let (count, fail_from, persistent) = f.get();
            f.set((count + 1, fail_from, persistent));
            if count == fail_from || (persistent && count >= fail_from) {
                bail!("injected POKETEXT failure");
            }
            Ok(())
        })?;
    }
    unsafe {
        *libc::__errno_location() = 0;
        let value = libc::ptrace(
            request,
            tid,
            address as *mut libc::c_void,
            data as *mut libc::c_void,
        );
        if *libc::__errno_location() != 0 {
            return Err(std::io::Error::last_os_error())
                .context(format!("ptrace {request} tid={tid}"));
        }
        Ok(value)
    }
}
fn tasks(pid: u32) -> Result<Vec<i32>> {
    let mut tids = fs::read_dir(format!("/proc/{pid}/task"))?
        .map(|e| Ok(e?.file_name().to_string_lossy().parse()?))
        .collect::<Result<Vec<i32>>>()?;
    tids.sort_unstable();
    Ok(tids)
}
struct Stopped {
    tids: Vec<i32>,
    signals: std::collections::HashMap<i32, i32>,
    confirmed: std::collections::HashSet<i32>,
    poisoned: bool,
}
impl Stopped {
    fn all(pid: u32) -> Result<Self> {
        anyhow::ensure!(pid > 0 && pid <= i32::MAX as u32, "invalid pid");
        let expected = tasks(pid)?;
        anyhow::ensure!(!expected.is_empty(), "empty thread set");
        let mut stopped = Self {
            tids: Vec::new(),
            signals: Default::default(),
            confirmed: Default::default(),
            poisoned: false,
        };
        for &tid in &expected {
            trace(libc::PTRACE_SEIZE, tid, 0, 0)?;
            stopped.tids.push(tid);
            trace(libc::PTRACE_INTERRUPT, tid, 0, 0)?;
            let mut status = 0;
            let waited = loop {
                let rc = unsafe { libc::waitpid(tid, &mut status, libc::__WALL) };
                if rc < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                break rc;
            };
            if waited != tid || !libc::WIFSTOPPED(status) {
                bail!("thread {tid} did not enter ptrace stop");
            }
            stopped.confirmed.insert(tid);
            // Do not suppress pending real signals or operate on group-stopped processes.
            if status >> 16 != libc::PTRACE_EVENT_STOP || libc::WSTOPSIG(status) != libc::SIGTRAP {
                stopped.signals.insert(tid, libc::WSTOPSIG(status));
                bail!("unexpected stop for thread {tid}: {status:x}");
            }
        }
        if tasks(pid)? != expected {
            bail!("thread set changed: fixed-thread model required");
        }
        Ok(stopped)
    }
    fn read(&self, addr: u64, length: usize) -> Result<Vec<u8>> {
        addr.checked_add(length.checked_add(7).context("length overflow")? as u64 & !7)
            .context("address overflow")?;
        let mut out = Vec::new();
        for offset in (0..length).step_by(8) {
            out.extend_from_slice(
                &trace(libc::PTRACE_PEEKTEXT, self.tids[0], addr + offset as u64, 0)?.to_ne_bytes(),
            );
        }
        out.truncate(length);
        Ok(out)
    }
    fn write(&self, addr: u64, bytes: &[u8]) -> Result<()> {
        addr.checked_add(bytes.len() as u64)
            .context("address overflow")?;
        if bytes.len() % 8 != 0 {
            bail!("write must contain complete words");
        }
        for (i, word) in bytes.chunks_exact(8).enumerate() {
            trace(
                libc::PTRACE_POKETEXT,
                self.tids[0],
                addr + (i * 8) as u64,
                u64::from_ne_bytes(word.try_into()?),
            )?;
        }
        let readback = self.read(addr, bytes.len())?;
        #[cfg(test)]
        if READBACK_FAIL.with(|flag| flag.replace(false)) {
            bail!("injected machine-code readback mismatch");
        }
        if readback != bytes {
            bail!("machine-code readback mismatch");
        }
        Ok(())
    }
}
impl Stopped {
    fn release(&mut self) -> Result<()> {
        let mut errors = Vec::new();
        if self.poisoned || self.tids.iter().any(|tid| !self.confirmed.contains(tid)) {
            for &tid in &self.tids {
                unsafe {
                    libc::kill(tid, libc::SIGKILL);
                }
            }
            for &tid in self.tids.iter().rev() {
                let mut status = 0;
                loop {
                    let rc = unsafe { libc::waitpid(tid, &mut status, libc::__WALL) };
                    if rc < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
                    {
                        continue;
                    }
                    break;
                }
            }
            self.tids.clear();
            return Ok(());
        }
        for &tid in self.tids.iter().rev() {
            if let Err(error) = trace(
                libc::PTRACE_DETACH,
                tid,
                0,
                self.signals.get(&tid).copied().unwrap_or(0) as u64,
            ) {
                unsafe {
                    libc::kill(tid, libc::SIGKILL);
                }
                errors.push(format!("tid={tid}: {error:#}"));
                break;
            }
        }
        if !errors.is_empty() {
            self.poisoned = true;
            self.release()?;
        }
        self.tids.clear();
        anyhow::ensure!(
            errors.is_empty(),
            "detach failed (target killed): {}",
            errors.join("; ")
        );
        Ok(())
    }
}
impl Drop for Stopped {
    fn drop(&mut self) {
        if let Err(error) = self.release() {
            eprintln!("SafePatch cleanup: {error:#}");
        }
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[path = "safepatch/compatible.rs"]
mod compatible;

/// Restricted, verified native ELF compatibility runtime. See compatible.rs.
pub fn exchange_compat(pid: u32, address: u64, bytes: &[u8], timeout: Duration) -> Result<Vec<u8>> {
    exchange_compat_related(pid, address, bytes, timeout, &[])
}

/// Include complete additional affected functions by their entry addresses.
pub fn exchange_compat_related(
    pid: u32,
    address: u64,
    bytes: &[u8],
    timeout: Duration,
    related: &[u64],
) -> Result<Vec<u8>> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        compatible::exchange(pid, address, bytes, timeout, related)
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        let _ = (pid, address, bytes, timeout, related);
        bail!("Compatible requires Linux x86_64")
    }
}
/// ABI: [magic, gate, active], all aligned sequentially-consistent atomic u64.
/// The caller must serialize coordinators for a process. Unknown targets fail closed.
pub fn exchange(
    pid: u32,
    state: u64,
    address: u64,
    bytes: &[u8],
    timeout: Duration,
) -> Result<Vec<u8>> {
    if !cfg!(target_arch = "x86_64") {
        bail!("SafePatch currently supports x86_64 only");
    }
    if bytes.is_empty() || bytes.len() % 8 != 0 || state % 8 != 0 {
        bail!("SafePatch requires aligned ABI and nonempty complete machine words");
    }
    address
        .checked_add(bytes.len() as u64)
        .context("patch address overflow")?;
    state.checked_add(24).context("ABI address overflow")?;
    let start = Instant::now();
    let mut holder = Some(Stopped::all(pid)?);
    let stopped = holder.as_ref().unwrap();
    let abi = stopped.read(state, 24)?;
    if u64::from_ne_bytes(abi[0..8].try_into()?) != 0x4741494153503031 || abi[8..16] != [0; 8] {
        bail!("missing cooperative ABI or gate already owned");
    }
    stopped.write(state + 8, &1u64.to_ne_bytes())?;
    let result = (|| {
        loop {
            let stopped = holder.as_ref().unwrap();
            let active = u64::from_ne_bytes(stopped.read(state + 16, 8)?.try_into().unwrap());
            if active == 0 {
                let original = stopped.read(address, bytes.len())?;
                if let Err(error) = stopped.write(address, bytes) {
                    if let Err(rollback) = stopped.write(address, &original) {
                        // Never resume a process with partially written instructions.
                        unsafe {
                            libc::kill(pid as i32, libc::SIGKILL);
                        }
                        bail!(
                            "write failed: {error:#}; rollback failed, target killed: {rollback:#}"
                        );
                    }
                    return Err(error);
                }
                let mut timestamp = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                };
                unsafe {
                    libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut timestamp);
                }
                let switch_ns = timestamp.tv_sec as u64 * 1_000_000_000 + timestamp.tv_nsec as u64;
                eprintln!(
                    "SafePatch write verified pid={pid} threads={} active=0 switch_ns={switch_ns} drain_us={}",
                    stopped.tids.len(),
                    start.elapsed().as_micros()
                );
                return Ok(original);
            }
            if start.elapsed() >= timeout {
                bail!("SafePatch drain timeout active={active}");
            }
            holder.take().unwrap().release()?;
            std::thread::sleep(Duration::from_millis(2));
            holder = Some(Stopped::all(pid)?);
        }
    })();
    // If reattachment failed, reacquire before releasing the admission gate.
    if holder.is_none() {
        holder = Some(Stopped::all(pid)?);
    }
    let stopped = holder.as_ref().unwrap();
    if let Err(release) = stopped.write(state + 8, &0u64.to_ne_bytes()) {
        if let Ok(original) = &result {
            if stopped.write(address, original).is_err() {
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
                bail!("gate release and rollback failed; target killed: {release:#}");
            }
        }
        return Err(release).context("release cooperative gate; code rolled back");
    }
    holder.as_mut().unwrap().release()?;
    result
}

#[cfg(test)]
thread_local! {
    static TRACE_FAILURE: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
    static READBACK_FAIL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FAULT: std::cell::Cell<(usize, usize, bool)> = const { std::cell::Cell::new((0, usize::MAX, false)) };
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader},
        process::{Command, Stdio},
    };

    use super::*;
    #[test]
    fn writer_partial_failure_and_readback_rollback() -> Result<()> {
        let executable = std::env::var("GAIA_COMPAT_TARGET")
            .context("set GAIA_COMPAT_TARGET to compiled compatible.c")?;
        for readback_failure in [false, true] {
            let mut child = Command::new(&executable).stdout(Stdio::piped()).spawn()?;
            let result = (|| -> Result<()> {
                let mut line = String::new();
                BufReader::new(child.stdout.take().unwrap()).read_line(&mut line)?;
                let fields: Vec<_> = line.split_whitespace().collect();
                let addr = u64::from_str_radix(fields[2].trim_start_matches("0x"), 16)?;
                let mut stopped = Stopped::all(child.id())?;
                let original = stopped.read(addr, 16)?;
                FAULT.with(|f| f.set((0, if readback_failure { usize::MAX } else { 1 }, false)));
                READBACK_FAIL.with(|flag| flag.set(readback_failure));
                // Exercise the shared writer, not a successful Compatible transition.
                let mut patch = vec![0x90; 16];
                patch[..8].copy_from_slice(&[0xb8, 99, 0, 0, 0, 0xc3, 0x90, 0x90]);
                let failure = stopped.write(addr, &patch);
                // Restore before assertions so a failing test cannot resume partial code.
                FAULT.with(|f| f.set((0, usize::MAX, false)));
                READBACK_FAIL.with(|flag| flag.set(false));
                stopped.write(addr, &original)?;
                anyhow::ensure!(failure.is_err(), "fault injection did not fail");
                anyhow::ensure!(stopped.read(addr, 16)? == original);
                stopped.release()?;
                anyhow::ensure!(child.try_wait()?.is_none(), "writer test killed target");
                Ok(())
            })();
            FAULT.with(|f| f.set((0, usize::MAX, false)));
            READBACK_FAIL.with(|flag| flag.set(false));
            let _ = child.kill();
            let _ = child.wait();
            result?;
        }
        Ok(())
    }
}
