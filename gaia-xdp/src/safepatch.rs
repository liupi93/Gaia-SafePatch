//! Linux x86_64 cooperative stop/drain/write runtime. See tests/safepatch/README.md.
use anyhow::{Context, Result, bail};
use std::{
    fs,
    time::{Duration, Instant},
};

fn trace(request: u32, tid: i32, address: u64, data: u64) -> Result<i64> {
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
}
impl Stopped {
    fn all(pid: u32) -> Result<Self> {
        let expected = tasks(pid)?;
        let mut stopped = Self {
            tids: Vec::new(),
            signals: Default::default(),
        };
        for &tid in &expected {
            trace(libc::PTRACE_SEIZE, tid, 0, 0)?;
            stopped.tids.push(tid);
            trace(libc::PTRACE_INTERRUPT, tid, 0, 0)?;
            let mut status = 0;
            if unsafe { libc::waitpid(tid, &mut status, libc::__WALL) } != tid
                || !libc::WIFSTOPPED(status)
            {
                bail!("thread {tid} did not enter ptrace stop");
            }
            // Do not suppress pending real signals or operate on group-stopped processes.
            if status >> 16 != libc::PTRACE_EVENT_STOP || libc::WSTOPSIG(status) != libc::SIGTRAP {
                stopped.signals.insert(tid, libc::WSTOPSIG(status));
                bail!("unexpected stop for thread {tid}: {status:x}");
            }
        }
        if tasks(pid)? != expected {
            bail!("thread set changed: cooperative fixed-thread model required");
        }
        Ok(stopped)
    }
    fn read(&self, addr: u64, length: usize) -> Result<Vec<u8>> {
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
        if self.read(addr, bytes.len())? != bytes {
            bail!("machine-code readback mismatch");
        }
        Ok(())
    }
}
impl Drop for Stopped {
    fn drop(&mut self) {
        for &tid in self.tids.iter().rev() {
            if let Err(error) = trace(
                libc::PTRACE_DETACH,
                tid,
                0,
                self.signals.get(&tid).copied().unwrap_or(0) as u64,
            ) {
                eprintln!("SafePatch detach failed: {error:#}");
            }
        }
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
            drop(holder.take());
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
    result
}
