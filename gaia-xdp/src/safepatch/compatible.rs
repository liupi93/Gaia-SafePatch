//! SysCare native stack-word scanning, integrated with Gaia's verified writer.
//! Success means no affected address was observed on any stopped native stack.
//! It does not certify business drain, suspended coroutines or alternate stacks.
use std::{
    ffi::c_void,
    fs,
    mem::ManuallyDrop,
    os::fd::{AsRawFd, FromRawFd},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use object::{Object, ObjectSegment, ObjectSymbol};

use super::{CompatIncomplete, Stopped, trace};
#[repr(C)]
struct Range {
    start: u64,
    end: u64,
}
unsafe extern "C" {
    fn gaia_upatch_new(pid: i32, trace: extern "C" fn(u32, i32, u64, u64) -> i64) -> *mut c_void;
    fn gaia_upatch_stop(ctx: *mut c_void, timeout_ms: u32) -> i32;
    fn gaia_upatch_tids(ctx: *mut c_void, out: *mut i32) -> usize;
    fn gaia_upatch_check(ctx: *mut c_void, ranges: *const Range, count: usize, fault: i32) -> i32;
    fn gaia_upatch_release(ctx: *mut c_void) -> i32;
}
extern "C" fn traced(req: u32, tid: i32, addr: u64, data: u64) -> i64 {
    trace(req, tid, addr, data).unwrap_or(-(libc::EIO as i64))
}
// Borrow Gaia's writer without granting it ownership of SysCare's trace session.
struct Writer(ManuallyDrop<Stopped>);
impl std::ops::Deref for Writer {
    type Target = Stopped;
    fn deref(&self) -> &Stopped {
        &self.0
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        self.0.tids.clear();
        unsafe {
            ManuallyDrop::drop(&mut self.0);
        }
    }
}
struct Snapshot {
    ctx: *mut c_void,
    retained: bool,
}
impl Snapshot {
    fn stop(pid: u32, timeout: Duration) -> Result<Self> {
        let ctx = unsafe { gaia_upatch_new(pid as i32, traced) };
        ensure!(!ctx.is_null(), "SysCare context allocation failed");
        let s = Self {
            ctx,
            retained: false,
        };
        let rc = unsafe { gaia_upatch_stop(ctx, timeout.as_millis().clamp(1, 1000) as u32) };
        ensure!(rc != -libc::ETIMEDOUT, "SysCare thread stop timeout");
        ensure!(rc == 0, "SysCare thread stop failed: {rc}");
        Ok(s)
    }
    fn writer(&self) -> Writer {
        let mut tids = vec![0; 4096];
        let n = unsafe { gaia_upatch_tids(self.ctx, tids.as_mut_ptr()) };
        tids.truncate(n);
        Writer(ManuallyDrop::new(Stopped {
            tids,
            signals: Default::default(),
            confirmed: Default::default(),
            poisoned: false,
        }))
    }
    fn release(&mut self) -> Result<()> {
        if self.ctx.is_null() {
            return Ok(());
        }
        let rc = unsafe { gaia_upatch_release(self.ctx) };
        if rc == 0 {
            self.ctx = std::ptr::null_mut();
        }
        ensure!(rc == 0, "SysCare detach failed: {rc}; completion unclaimed");
        Ok(())
    }
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        if !self.retained {
            if let Err(e) = self.release() {
                eprintln!("SafePatch cleanup: {e:#}");
                if let Err(e) = self.release() {
                    eprintln!("SafePatch cleanup retained traced threads: {e:#}");
                }
            }
        }
    }
}
// Full ELF STT_FUNC bounds, resolved against the stopped executable's PT_LOAD.
// No instruction decoder, frame-pointer walk or custom unwind algorithm.
fn ranges(pid: u32, addresses: &[u64], length: usize) -> Result<Vec<Range>> {
    let data = fs::read(format!("/proc/{pid}/exe"))?;
    let elf = object::File::parse(data.as_slice())?;
    ensure!(
        elf.architecture() == object::Architecture::X86_64,
        "unsupported ELF architecture"
    );
    let maps = fs::read_to_string(format!("/proc/{pid}/maps"))?;
    let exe = fs::read_link(format!("/proc/{pid}/exe"))?;
    let mut bias = None;
    for line in maps.lines() {
        let f: Vec<_> = line.split_whitespace().collect();
        if f.len() < 6 || f[5] != exe.to_string_lossy() || !f[1].contains('x') {
            continue;
        }
        let start = u64::from_str_radix(f[0].split('-').next().unwrap(), 16)?;
        let offset = u64::from_str_radix(f[2], 16)?;
        for seg in elf.segments() {
            if seg.file_range().0 & !4095 == offset {
                let b = start
                    .checked_sub(seg.address() & !4095)
                    .context("load bias underflow")?;
                ensure!(bias.is_none_or(|v| v == b), "ambiguous ELF load bias");
                bias = Some(b);
            }
        }
    }
    let bias =
        bias.context("executable mapping unavailable (shared-library targets unsupported)")?;
    addresses
        .iter()
        .enumerate()
        .map(|(i, &a)| {
            let symbol = elf
                .symbols()
                .chain(elf.dynamic_symbols())
                .find(|s| {
                    s.kind() == object::SymbolKind::Text
                        && !s.is_undefined()
                        && s.size() > 0
                        && s.address().checked_add(bias) == Some(a)
                })
                .context("full affected function symbol unavailable")?;
            let end = a.checked_add(symbol.size()).context("function overflow")?;
            ensure!(
                i != 0 || length as u64 <= symbol.size(),
                "write exceeds affected function"
            );
            Ok(Range { start: a, end })
        })
        .collect()
}
#[cfg(test)]
thread_local! { static STACK_FAULT: std::cell::Cell<i32> = const { std::cell::Cell::new(0) }; }
pub(super) fn exchange(
    pid: u32,
    address: u64,
    bytes: &[u8],
    timeout: Duration,
    related: &[u64],
) -> Result<Vec<u8>> {
    exchange_observed(pid, address, bytes, timeout, related, &mut |_, _| {})
}

// Read-only observation; the callback must not block or mutate the target.
pub(super) fn exchange_observed(
    pid: u32,
    address: u64,
    bytes: &[u8],
    timeout: Duration,
    related: &[u64],
    observe: &mut dyn FnMut(u64, i32),
) -> Result<Vec<u8>> {
    ensure!(pid > 0 && pid <= i32::MAX as u32, "invalid pid");
    ensure!(
        !bytes.is_empty() && bytes.len() % 8 == 0,
        "complete nonempty machine words required"
    );
    address
        .checked_add(bytes.len() as u64)
        .context("address overflow")?;
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    ensure!(
        fd >= 0,
        "pidfd_open failed: {}",
        std::io::Error::last_os_error()
    );
    let identity = unsafe { fs::File::from_raw_fd(fd) };
    let start = Instant::now();
    let mut checks = 0;
    let result = (|| {
        loop {
            ensure!(
                start.elapsed() < timeout,
                "SysCare stack check timeout (active old calls or conservative stack matches)"
            );
            let mut snapshot = Snapshot::stop(pid, timeout.saturating_sub(start.elapsed()))?;
            let mut poll = libc::pollfd {
                fd: identity.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            ensure!(
                unsafe { libc::poll(&mut poll, 1, 0) } == 0,
                "original process exited; refuse PID reuse"
            );
            let mut affected = vec![address];
            affected.extend_from_slice(related);
            let status = fs::read_to_string(format!("/proc/{pid}/status"))?;
            let caught = status
                .lines()
                .find_map(|l| l.strip_prefix("SigCgt:\t"))
                .context("signal disposition unavailable")?;
            let caught = u64::from_str_radix(caught.trim(), 16)?;
            // glibc reserves signals 32/33 for pthread cancellation/setxid.
            ensure!(
                caught & !((1u64 << 31) | (1u64 << 32)) == 0,
                "user signal handlers/alternate signal stacks unsupported; refused closed"
            );
            let bounds = ranges(pid, &affected, bytes.len())?;
            #[cfg(not(test))]
            let fault = 0;
            #[cfg(test)]
            let fault = STACK_FAULT.with(|f| f.get());
            checks += 1;
            let rc =
                unsafe { gaia_upatch_check(snapshot.ctx, bounds.as_ptr(), bounds.len(), fault) };
            observe(checks, rc);
            ensure!(rc >= 0, "SysCare stack check failed closed: {rc}");
            if rc > 0 {
                snapshot.release()?;
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
            let writer = snapshot.writer();
            let old = writer.read(address, bytes.len())?;
            if let Err(e) = writer.write(address, bytes) {
                if let Err(rollback) = writer.write(address, &old) {
                    snapshot.retained = true;
                    return Err(CompatIncomplete {
                        original_bytes: old,
                        reason: format!("write failed: {e:#}; rollback failed: {rollback:#}; target retained stopped; code state indeterminate"),
                    }.into());
                }
                bail!("write failed: {e:#}; original bytes restored and verified");
            }
            let threads = writer.tids.len();
            // Detach may partially resume threads. Never attempt a post-detach rollback.
            if let Err(e) = snapshot.release() {
                return Err(CompatIncomplete {
                    original_bytes: old,
                    reason: format!(
                        "code_installed=true; code verified but completion unclaimed: {e:#}"
                    ),
                }
                .into());
            }
            eprintln!(
                "SafePatch compatible result=success threads={threads} checks={checks} elapsed_us={} code_installed=true native_stack_check_complete=true business_consistency=unclaimed",
                start.elapsed().as_micros()
            );
            return Ok(old);
        }
    })();
    if let Err(e) = &result {
        eprintln!(
            "SafePatch compatible result=error checks={checks} consistency_complete=false reason={e:#}"
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader},
        os::unix::fs::FileExt,
        process::{Command, Stdio},
    };

    use super::*;
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn target() -> Result<(Child, u64)> {
        let exe = std::env::var("GAIA_DOCUMENT_TARGET")?;
        let dir = std::env::temp_dir().join(format!("gaia-fault-{}", std::process::id()));
        fs::create_dir_all(&dir)?;
        let mut child = Child(
            Command::new(exe)
                .arg(dir)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()?,
        );
        let mut reader = BufReader::new(child.0.stdout.take().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let addr = u64::from_str_radix(
            line.split_whitespace()
                .nth(2)
                .context("READY")?
                .trim_start_matches("0x"),
            16,
        )?;
        std::thread::spawn(move || {
            for line in reader.lines() {
                if line.is_err() {
                    break;
                }
            }
        });
        Ok((child, addr))
    }
    fn read(pid: u32, addr: u64, len: usize) -> Result<Vec<u8>> {
        let mut b = vec![0; len];
        fs::File::open(format!("/proc/{pid}/mem"))?.read_exact_at(&mut b, addr)?;
        Ok(b)
    }
    fn healthy(c: &mut Child) -> Result<()> {
        ensure!(c.0.try_wait()?.is_none());
        for tid in super::super::tasks(c.0.id())? {
            let status = fs::read_to_string(format!("/proc/{tid}/status"))?;
            ensure!(
                status.lines().any(|s| s == "TracerPid:\t0"),
                "leaked tracer {tid}"
            );
        }
        Ok(())
    }
    #[test]
    fn preexisting_group_stop_is_preserved() -> Result<()> {
        let (mut c, a) = target()?;
        let pid = c.0.id();
        let old = read(pid, a, 8)?;
        ensure!(unsafe { libc::kill(pid as i32, libc::SIGSTOP) } == 0);
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let status = fs::read_to_string(format!("/proc/{pid}/status"))?;
            if status.lines().any(|s| s.starts_with("State:\tT")) {
                break;
            }
            ensure!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        ensure!(exchange(pid, a, &PATCH, Duration::from_secs(1), &[]).is_err());
        ensure!(read(pid, a, 8)? == old);
        healthy(&mut c)?;
        ensure!(
            fs::read_to_string(format!("/proc/{pid}/status"))?
                .lines()
                .any(|s| s.starts_with("State:\tT"))
        );
        unsafe {
            libc::kill(pid as i32, libc::SIGCONT);
        }
        Ok(())
    }
    const PATCH: [u8; 8] = [0xb8, 0, 0, 0, 0, 0xc3, 0x90, 0x90];
    #[test]
    fn multithread_install_restore_and_stack_errors() -> Result<()> {
        let (mut c, a) = target()?;
        let pid = c.0.id();
        let original = read(pid, a, 8)?;
        for restoring in [false, true] {
            if restoring {
                exchange(pid, a, &PATCH, Duration::from_secs(2), &[])?;
            }
            let before = read(pid, a, 8)?;
            for fault in 1..=3 {
                STACK_FAULT.with(|f| f.set(fault));
                let result = exchange(
                    pid,
                    a,
                    if restoring { &original } else { &PATCH },
                    Duration::from_secs(1),
                    &[],
                );
                STACK_FAULT.with(|f| f.set(0));
                ensure!(format!("{:#}", result.unwrap_err()).contains("failed closed"));
                ensure!(read(pid, a, 8)? == before);
                healthy(&mut c)?;
            }
            if restoring {
                exchange(pid, a, &original, Duration::from_secs(2), &[])?;
            }
        }
        for _ in 0..10 {
            ensure!(exchange(pid, a, &PATCH, Duration::from_secs(2), &[])? == original);
            ensure!(read(pid, a, 8)? == PATCH);
            ensure!(exchange(pid, a, &original, Duration::from_secs(2), &[])? == PATCH);
            ensure!(read(pid, a, 8)? == original);
            healthy(&mut c)?;
        }
        Ok(())
    }
    #[test]
    fn thread_control_errors() -> Result<()> {
        for req in [
            libc::PTRACE_SEIZE,
            libc::PTRACE_INTERRUPT,
            libc::PTRACE_GETREGS,
            libc::PTRACE_DETACH,
        ] {
            let (mut c, a) = target()?;
            let pid = c.0.id();
            let old = read(pid, a, 8)?;
            super::super::TRACE_FAILURE.with(|f| f.set(Some(req)));
            let result = exchange(pid, a, &PATCH, Duration::from_secs(2), &[]);
            super::super::TRACE_FAILURE.with(|f| f.set(None));
            ensure!(result.is_err());
            healthy(&mut c)?;
            if req == libc::PTRACE_DETACH {
                ensure!(read(pid, a, 8)? == PATCH); // error explicitly says installed, no completion
                exchange(pid, a, &old, Duration::from_secs(2), &[])?;
            } else {
                ensure!(read(pid, a, 8)? == old);
            }
        }
        Ok(())
    }
    #[test]
    fn install_and_restore_write_errors() -> Result<()> {
        for restoring in [false, true] {
            for readback in [false, true] {
                let (mut c, a) = target()?;
                let pid = c.0.id();
                let old = read(pid, a, 8)?;
                if restoring {
                    exchange(pid, a, &PATCH, Duration::from_secs(2), &[])?;
                }
                let before = read(pid, a, 8)?;
                if readback {
                    super::super::READBACK_FAIL.with(|f| f.set(true));
                } else {
                    super::super::FAULT.with(|f| f.set((0, 0, false)));
                }
                let result = exchange(
                    pid,
                    a,
                    if restoring { &old } else { &PATCH },
                    Duration::from_secs(2),
                    &[],
                );
                super::super::FAULT.with(|f| f.set((0, usize::MAX, false)));
                super::super::READBACK_FAIL.with(|f| f.set(false));
                ensure!(format!("{:#}", result.unwrap_err()).contains("restored and verified"));
                ensure!(read(pid, a, 8)? == before);
                healthy(&mut c)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    // Exercise a partial multiword write and a persistent rollback failure in a
    // test-owned target, then explicitly recover before any thread resumes.
    #[test]
    fn partial_write_and_failed_rollback_retain_all_threads() -> Result<()> {
        use std::{
            io::{BufRead, BufReader},
            process::{Command, Stdio},
        };
        let exe = std::env::var("GAIA_DOCUMENT_TARGET")?;
        let dir = std::env::temp_dir().join("gaia-rollback-test");
        fs::create_dir_all(&dir)?;
        let mut child = Command::new(exe)
            .arg(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let result = (|| {
            let mut reader = BufReader::new(child.stdout.take().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line)?;
            let a = u64::from_str_radix(
                line.split_whitespace()
                    .nth(2)
                    .unwrap()
                    .trim_start_matches("0x"),
                16,
            )?;
            let pid = child.id();
            let mut snap = Snapshot::stop(pid, Duration::from_secs(1))?;
            let old = snap.writer().read(a, 16)?;
            snap.release()?;
            for persistent in [false, true] {
                let patch = [0x90; 16];
                super::super::FAULT.with(|f| f.set((0, 1, persistent)));
                let result = exchange(pid, a, &patch, Duration::from_secs(2), &[]);
                super::super::FAULT.with(|f| f.set((0, usize::MAX, false)));
                let error = format!("{:#}", result.unwrap_err());
                if persistent {
                    ensure!(error.contains("retained stopped"), "{error}");
                    let tids = super::super::tasks(pid)?;
                    for tid in &tids {
                        let status = fs::read_to_string(format!("/proc/{tid}/status"))?;
                        ensure!(
                            status.lines().any(|s| s.starts_with("State:\tt")),
                            "thread resumed with partial code"
                        );
                    }
                    let writer = ManuallyDrop::new(Stopped {
                        tids: tids.clone(),
                        signals: Default::default(),
                        confirmed: Default::default(),
                        poisoned: false,
                    });
                    writer.write(a, &old)?;
                    for tid in tids {
                        trace(libc::PTRACE_DETACH, tid, 0, 0)?;
                    }
                } else {
                    ensure!(error.contains("restored and verified"));
                }
                let mut snap = Snapshot::stop(pid, Duration::from_secs(1))?;
                ensure!(snap.writer().read(a, 16)? == old);
                snap.release()?;
                ensure!(child.try_wait()?.is_none());
            }
            Ok(())
        })();
        super::super::FAULT.with(|f| f.set((0, usize::MAX, false)));
        let _ = child.kill();
        let _ = child.wait();
        result
    }
}
