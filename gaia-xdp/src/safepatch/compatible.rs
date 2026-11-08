//! Deliberately small verifiable ABI: static ET_EXEC, one native stack, direct
//! calls, fixed rbp frames, no tail calls, context switching, handlers or JIT.
//! objdump is used as an instruction decoder, never as a stack unwinder.
use super::{Stopped, tasks, trace};
use anyhow::{Context, Result, bail, ensure};
use object::{Object, ObjectSegment, ObjectSymbol};
use std::{
    fs,
    io::Write,
    mem::ManuallyDrop,
    os::fd::{AsRawFd, FromRawFd},
    process::Command,
    time::{Duration, Instant},
};

struct Insn {
    ip: u64,
    next: u64,
    op: String,
    args: String,
}
struct Function {
    start: u64,
    end: u64,
    name: String,
    insns: Vec<Insn>,
    allocation: u64,
    body: u64,
    leave: u64,
}
struct Image {
    data: Vec<u8>,
    functions: Vec<Function>,
    root_return: u64,
    main: u64,
}
fn hex(s: &str) -> Result<u64> {
    Ok(u64::from_str_radix(s.trim_start_matches("0x"), 16)?)
}

impl Image {
    fn load(pid: u32) -> Result<Self> {
        let path = format!("/proc/{pid}/exe");
        let data = fs::read(&path)?;
        let elf = object::File::parse(data.as_slice())?;
        ensure!(
            elf.architecture() == object::Architecture::X86_64
                && elf.kind() == object::ObjectKind::Executable,
            "unsupported ELF: static x86_64 ET_EXEC required"
        );
        ensure!(
            !elf.sections().any(|s| {
                use object::ObjectSection;
                matches!(s.name(), Ok(".interp" | ".dynamic"))
            }),
            "unsupported ELF: dynamic linking is not verifiable in this subset"
        );
        // Decode the exact immutable byte snapshot that is also compared with
        // stopped memory, rather than reopening /proc/pid/exe across an exec.
        let fd = unsafe {
            libc::memfd_create(
                c"gaia-compat-elf".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        ensure!(
            fd >= 0,
            "cannot create sealed ELF snapshot: {}",
            std::io::Error::last_os_error()
        );
        let mut sealed = unsafe { fs::File::from_raw_fd(fd) };
        sealed.write_all(&data)?;
        ensure!(
            unsafe {
                libc::fcntl(
                    sealed.as_raw_fd(),
                    libc::F_ADD_SEALS,
                    libc::F_SEAL_SEAL
                        | libc::F_SEAL_SHRINK
                        | libc::F_SEAL_GROW
                        | libc::F_SEAL_WRITE,
                )
            } == 0,
            "cannot seal ELF snapshot"
        );
        let decoded_path = format!("/proc/{}/fd/{}", std::process::id(), sealed.as_raw_fd());
        let decoded = Command::new("objdump")
            .args(["-d", "-z", "-Mintel", "--no-show-raw-insn", &decoded_path])
            .output()
            .context("objdump required")?;
        ensure!(decoded.status.success(), "objdump failed");
        let text = String::from_utf8(decoded.stdout)?;
        let mut all = Vec::new();
        for line in text.lines() {
            let Some((address, instruction)) = line.trim().split_once(':') else {
                continue;
            };
            let Ok(ip) = hex(address) else { continue };
            let mut parts = instruction.split_whitespace();
            let Some(op) = parts.next() else { continue };
            // Strip objdump's symbol annotations/comments, retain numeric operands.
            let args = parts
                .collect::<Vec<_>>()
                .join(" ")
                .split('#')
                .next()
                .unwrap()
                .trim()
                .to_string();
            all.push(Insn {
                ip,
                next: 0,
                op: op.into(),
                args,
            });
        }
        for i in 0..all.len().saturating_sub(1) {
            all[i].next = all[i + 1].ip;
        }
        let mut functions = Vec::new();
        for symbol in elf
            .symbols()
            .filter(|s| s.kind() == object::SymbolKind::Text && s.size() > 0 && !s.is_undefined())
        {
            let start = symbol.address();
            let end = start
                .checked_add(symbol.size())
                .context("function bounds overflow")?;
            let insns = all
                .iter()
                .filter(|i| (start..end).contains(&i.ip))
                .map(|i| Insn {
                    ip: i.ip,
                    next: if i.next == 0 || i.next > end {
                        end
                    } else {
                        i.next
                    },
                    op: i.op.clone(),
                    args: i.args.clone(),
                })
                .collect();
            functions.push(Function {
                start,
                end,
                name: symbol.name()?.into(),
                insns,
                allocation: 0,
                body: 0,
                leave: 0,
            });
        }
        functions.sort_by_key(|f| f.start);
        ensure!(!functions.is_empty(), "function boundaries unavailable");
        for pair in functions.windows(2) {
            ensure!(
                pair[0].end <= pair[1].start,
                "overlapping function boundaries"
            );
        }
        // Every executable byte must belong to a declared function. This rejects
        // hidden assembly, stripped sizes and executable gaps instead of guessing.
        for segment in elf.segments() {
            if !matches!(segment.flags(), object::SegmentFlags::Elf { p_flags } if p_flags & 1 != 0)
            {
                continue;
            }
            ensure!(
                segment.size() == segment.data()?.len() as u64,
                "unverified executable zero-fill tail"
            );
            let mut at = segment.address();
            let end = at
                .checked_add(segment.data()?.len() as u64)
                .context("text bounds overflow")?;
            for f in functions
                .iter()
                .filter(|f| f.start >= at && f.start < end)
                .collect::<Vec<_>>()
            {
                ensure!(f.start == at, "executable gap at {at:x}");
                at = f.end;
            }
            ensure!(at == end, "unknown executable bytes at {at:x}");
        }
        let root = functions
            .iter()
            .find(|f| f.name == "_start")
            .context("known startup root required")?;
        ensure!(root.start == elf.entry(), "unexpected ELF entry");
        let main = functions
            .iter()
            .find(|f| f.name == "main")
            .context("main required")?
            .start;
        ensure!(
            root.insns.len() == 6
                && root.insns[0].op == "xor"
                && root.insns[0].args == "ebp,ebp"
                && root.insns[1].op == "call"
                && hex(root.insns[1].args.split_whitespace().next().unwrap_or(""))? == main
                && root.insns[2].op == "mov"
                && root.insns[2].args == "edi,eax"
                && root.insns[3].op == "mov"
                && root.insns[3].args == "eax,0x3c"
                && root.insns[4].op == "syscall"
                && root.insns[5].op == "ud2",
            "unverified startup root"
        );
        let root_return = root.insns[1].next;
        let root_start = root.start;
        let starts: Vec<_> = functions.iter().map(|f| f.start).collect();
        for f in &mut functions {
            if f.name == "_start" {
                continue;
            }
            ensure!(
                f.insns.len() >= 4
                    && f.insns[0].op == "push"
                    && f.insns[0].args == "rbp"
                    && f.insns[1].op == "mov"
                    && f.insns[1].args == "rbp,rsp",
                "{}: canonical frame absent",
                f.name
            );
            f.body = f.insns[1].next;
            let mut index = 2;
            if f.insns[index].op == "sub" && f.insns[index].args.starts_with("rsp,") {
                f.allocation = hex(&f.insns[index].args[4..])?;
                ensure!(
                    f.allocation <= 65536 && f.allocation % 8 == 0,
                    "invalid frame allocation"
                );
                f.body = f.insns[index].next;
                index += 1;
            }
            let n = f.insns.len();
            ensure!(
                f.insns[n - 1].op == "ret"
                    && f.insns[n - 1].args.is_empty()
                    && matches!(f.insns[n - 2].op.as_str(), "leave" | "pop"),
                "{}: unsupported epilogue",
                f.name
            );
            if f.insns[n - 2].op == "pop" {
                ensure!(
                    f.insns[n - 2].args == "rbp" && f.allocation == 0,
                    "unsupported pop frame"
                );
            }
            f.leave = f.insns[n - 2].ip;
            for (j, i) in f.insns.iter().enumerate().take(n - 2).skip(index) {
                ensure!(!i.args.contains(':'), "segment overrides unsupported");
                let dest = i.args.split(',').next().unwrap_or("");
                ensure!(
                    !matches!(
                        dest,
                        "rsp" | "esp" | "sp" | "spl" | "rbp" | "ebp" | "bp" | "bpl"
                    ),
                    "{}: stack register mutation",
                    f.name
                );
                match i.op.as_str() {
                    "call" => {
                        let target = hex(i.args.split_whitespace().next().unwrap_or(""))?;
                        ensure!(
                            starts.contains(&target) && target != root_start,
                            "indirect/unknown/root call"
                        );
                    }
                    "syscall" => {
                        ensure!(
                            j > 0 && f.insns[j - 1].op == "mov",
                            "syscall number unverified"
                        );
                        match f.insns[j - 1].args.as_str() {
                            "eax,0x1" | "eax,0x3c" => (), // write and exit: no user memory writes
                            "eax,0x23" => ensure!(
                                j > 1
                                    && f.insns[j - 2].op == "xor"
                                    && f.insns[j - 2].args == "esi,esi",
                                "nanosleep remainder must be null"
                            ),
                            _ => bail!(
                                "unsupported syscall (clone/exec/mmap/signal/context operations forbidden)"
                            ),
                        }
                    }
                    op if op.starts_with('j') => {
                        let target = hex(i.args.split_whitespace().next().unwrap_or(""))?;
                        ensure!(
                            (f.body..=f.leave).contains(&target)
                                && f.insns.iter().any(|x| x.ip == target),
                            "tail jump or invalid branch"
                        );
                    }
                    "mov" | "movzx" | "movsx" | "movsxd" | "lea" | "add" | "sub" | "imul"
                    | "and" | "or" | "xor" | "cmp" | "test" | "nop" | "cdqe" | "sete" | "setne"
                    | "setg" | "seta" => (),
                    _ => bail!("{}: unsupported instruction {}", f.name, i.op),
                }
                // Writes may only touch bounded frame locals. No arbitrary stores
                // can forge or bypass a return chain. Reads/LEA/CMP don't write RAM.
                if dest.contains('[') && !matches!(i.op.as_str(), "cmp" | "test" | "nop") {
                    ensure!(
                        dest.contains("[rbp-"),
                        "unverified memory store: {}",
                        i.args
                    );
                    let offset = dest
                        .split("[rbp-")
                        .nth(1)
                        .and_then(|s| s.split(']').next())
                        .context("stack operand")?;
                    let offset = hex(offset)?;
                    let width = if dest.starts_with("BYTE") {
                        1
                    } else if dest.starts_with("WORD") {
                        2
                    } else if dest.starts_with("DWORD") {
                        4
                    } else if dest.starts_with("QWORD") {
                        8
                    } else {
                        bail!("unknown store width")
                    };
                    ensure!(
                        offset >= width && offset <= f.allocation + 128,
                        "frame store out of bounds"
                    );
                }
            }
        }
        // A branch must not enter the middle of a syscall setup sequence.
        for f in &functions {
            for (j, i) in f
                .insns
                .iter()
                .enumerate()
                .filter(|(_, i)| i.op == "syscall")
            {
                let first = if j > 1 && f.insns[j - 1].args == "eax,0x23" {
                    j - 2
                } else {
                    j - 1
                };
                for branch in f.insns.iter().filter(|i| i.op.starts_with('j')) {
                    let destination = hex(branch.args.split_whitespace().next().unwrap_or(""))?;
                    ensure!(
                        destination <= f.insns[first].ip || destination > i.ip,
                        "branch bypasses syscall setup"
                    );
                }
                if j > 0 && f.insns[j - 1].args == "eax,0x1" {
                    let setup = f.insns[..j]
                        .iter()
                        .rposition(|i| {
                            matches!(i.args.split(',').next(), Some("edi" | "rdi" | "di" | "dil"))
                        })
                        .context("write fd not verified")?;
                    ensure!(
                        f.insns[setup].op == "mov"
                            && matches!(f.insns[setup].args.as_str(), "edi,0x1" | "edi,0x2"),
                        "write fd must be stdout/stderr"
                    );
                    ensure!(
                        !f.insns[setup + 1..j]
                            .iter()
                            .any(|i| i.op.starts_with('j') || i.op == "call"),
                        "write setup crosses control flow"
                    );
                    for branch in f.insns.iter().filter(|i| i.op.starts_with('j')) {
                        let destination = hex(branch.args.split_whitespace().next().unwrap_or(""))?;
                        ensure!(
                            destination <= f.insns[setup].ip || destination > i.ip,
                            "branch bypasses write fd setup"
                        );
                    }
                }
            }
        }
        Ok(Self {
            data,
            functions,
            root_return,
            main,
        })
    }
    fn function(&self, ip: u64) -> Result<&Function> {
        self.functions
            .iter()
            .find(|f| (f.start..f.end).contains(&ip))
            .context("unknown code/frame address")
    }
    fn verify_memory(&self, stopped: &Stopped, address: u64) -> Result<()> {
        let elf = object::File::parse(self.data.as_slice())?;
        for segment in elf.segments() {
            if !matches!(segment.flags(), object::SegmentFlags::Elf { p_flags } if p_flags & 1 != 0)
            {
                continue;
            }
            let expected = segment.data()?;
            ensure!(expected.len() <= 1024 * 1024, "text too large for subset");
            let actual = stopped.read(segment.address(), expected.len())?;
            for (i, (&a, &b)) in actual.iter().zip(expected).enumerate() {
                let ip = segment.address() + i as u64;
                ensure!(
                    (address..address + 8).contains(&ip) || a == b,
                    "runtime text differs from verified ELF at {ip:x}"
                );
            }
        }
        Ok(())
    }
}

// Separate ownership/cleanup policy: Cooperative's historical failure behavior
// is untouched. ManuallyDrop prevents its kill-on-cleanup policy on this path.
struct Snapshot {
    inner: ManuallyDrop<Stopped>,
    retained: bool,
    newborn: std::collections::HashSet<i32>,
    group_stops: std::collections::HashMap<i32, i32>,
}
impl Snapshot {
    fn stop(pid: u32, deadline: Instant) -> Result<Self> {
        let state = fs::read_to_string(format!("/proc/{pid}/status"))?;
        ensure!(
            !state.lines().any(|s| s.starts_with("State:\tT")),
            "pre-existing group stop"
        );
        let expected = tasks(pid)?;
        ensure!(
            expected.len() == 1 && expected[0] == pid as i32,
            "supported subset requires one native thread"
        );
        let mut s = Self {
            inner: ManuallyDrop::new(Stopped {
                tids: vec![],
                signals: Default::default(),
                confirmed: Default::default(),
                poisoned: false,
            }),
            retained: false,
            newborn: Default::default(),
            group_stops: Default::default(),
        };
        let work = (|| -> Result<()> {
            let options = libc::PTRACE_O_TRACECLONE
                | libc::PTRACE_O_TRACEFORK
                | libc::PTRACE_O_TRACEVFORK
                | libc::PTRACE_O_TRACEEXEC
                | libc::PTRACE_O_TRACEEXIT;
            trace(libc::PTRACE_SEIZE, pid as i32, 0, options as u64)?;
            s.inner.tids.push(pid as i32);
            trace(libc::PTRACE_INTERRUPT, pid as i32, 0, 0)?;
            let status = s.wait(pid as i32, deadline)?;
            let event = status >> 16;
            if matches!(
                event,
                libc::PTRACE_EVENT_CLONE | libc::PTRACE_EVENT_FORK | libc::PTRACE_EVENT_VFORK
            ) {
                bail!("thread/process creation during snapshot");
            }
            if event == 0 {
                s.inner.signals.insert(pid as i32, libc::WSTOPSIG(status));
            }
            ensure!(
                event == libc::PTRACE_EVENT_STOP && libc::WSTOPSIG(status) == libc::SIGTRAP,
                "signal/exit/exec/group stop during snapshot"
            );
            ensure!(
                tasks(pid)? == expected,
                "thread set changed during snapshot"
            );
            Ok(())
        })();
        if let Err(error) = work {
            if let Err(cleanup) = s.release() {
                return Err(error).context(format!(
                    "snapshot failed; thread restoration incomplete: {cleanup:#}"
                ));
            }
            return Err(error);
        }
        Ok(s)
    }
    fn wait(&mut self, tid: i32, deadline: Instant) -> Result<i32> {
        loop {
            let mut status = 0;
            let rc = unsafe { libc::waitpid(tid, &mut status, libc::__WALL | libc::WNOHANG) };
            if rc == tid {
                if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
                    self.inner.tids.retain(|t| *t != tid);
                    self.inner.confirmed.remove(&tid);
                    bail!("thread exited during snapshot")
                }
                ensure!(libc::WIFSTOPPED(status), "unknown wait status");
                self.inner.confirmed.insert(tid);
                let event = status >> 16;
                let signal = libc::WSTOPSIG(status);
                if event == 0
                    || event == libc::PTRACE_EVENT_STOP
                        && signal != libc::SIGTRAP
                        && !self.newborn.contains(&tid)
                {
                    self.inner.signals.insert(tid, signal);
                    if matches!(
                        signal,
                        libc::SIGSTOP | libc::SIGTSTP | libc::SIGTTIN | libc::SIGTTOU
                    ) {
                        self.group_stops.insert(tid, signal);
                    }
                }
                if matches!(
                    event,
                    libc::PTRACE_EVENT_CLONE | libc::PTRACE_EVENT_FORK | libc::PTRACE_EVENT_VFORK
                ) {
                    let mut child = 0u64;
                    trace(
                        libc::PTRACE_GETEVENTMSG,
                        tid,
                        0,
                        &mut child as *mut _ as u64,
                    )?;
                    self.newborn.insert(child as i32);
                    if !self.inner.tids.contains(&(child as i32)) {
                        self.inner.tids.push(child as i32);
                    }
                }
                return Ok(status);
            }
            if rc < 0 {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(e.into());
            }
            ensure!(Instant::now() < deadline, "ptrace stop timeout");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn release(&mut self) -> Result<()> {
        if self.retained {
            bail!(
                "rollback incomplete: target retained stopped, tids={:?}; manual recovery required",
                self.inner.tids
            )
        }
        let mut errors = vec![];
        for tid in self.inner.tids.clone().into_iter().rev() {
            if !self.inner.confirmed.contains(&tid) {
                let _ = trace(libc::PTRACE_INTERRUPT, tid, 0, 0);
                if let Err(e) = self.wait(tid, Instant::now() + Duration::from_millis(200)) {
                    if !self.inner.tids.contains(&tid)
                        || !std::path::Path::new(&format!("/proc/{tid}")).exists()
                    {
                        self.inner.tids.retain(|t| *t != tid);
                        continue;
                    }
                    errors.push(format!("unconfirmed tid={tid}: {e:#}"));
                    continue;
                }
            }
            match trace(
                libc::PTRACE_DETACH,
                tid,
                0,
                self.inner.signals.get(&tid).copied().unwrap_or(0) as u64,
            ) {
                Ok(_) => {
                    self.inner.tids.retain(|t| *t != tid);
                    if let Some(&signal) = self.group_stops.get(&tid) {
                        // DETACH from a group-stop can ignore its signal argument.
                        // Reassert the original stop and wait for it to take effect.
                        if unsafe { libc::kill(tid, signal) } != 0 {
                            errors.push(format!(
                                "restore group stop tid={tid}: {}",
                                std::io::Error::last_os_error()
                            ));
                            continue;
                        }
                        let deadline = Instant::now() + Duration::from_millis(200);
                        loop {
                            let stopped = fs::read_to_string(format!("/proc/{tid}/status"))
                                .is_ok_and(|s| s.lines().any(|l| l.starts_with("State:\tT")));
                            if stopped {
                                break;
                            }
                            if Instant::now() >= deadline {
                                errors.push(format!(
                                    "group stop restoration not confirmed tid={tid}"
                                ));
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(1));
                        }
                    }
                }
                Err(e) => errors.push(format!("detach tid={tid}: {e:#}")),
            }
        }
        if !self.inner.tids.is_empty() && errors.is_empty() {
            errors.push(format!("unreleased tracked tids={:?}", self.inner.tids));
        }
        ensure!(
            errors.is_empty(),
            "thread restoration incomplete (no target killed): {}",
            errors.join("; ")
        );
        Ok(())
    }
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        if let Err(e) = self.release() {
            eprintln!("SafePatch compatible cleanup: {e:#}")
        }
        // Drop allocations without invoking the legacy kill policy, including
        // intentionally retained tracees after an unrecoverable write failure.
        self.inner.tids.clear();
        unsafe {
            ManuallyDrop::drop(&mut self.inner);
        }
    }
}

// A complete root-closed trace, with exact stack sizes and direct call edges.
// Err => unreliable; Some(reason) => valid active old frame; None => safe.
fn check_stack(
    image: &Image,
    s: &Snapshot,
    affected: &[(u64, u64)],
    pid: u32,
) -> Result<Option<String>> {
    let mut regs: libc::user_regs_struct = unsafe { std::mem::zeroed() };
    trace(
        libc::PTRACE_GETREGS,
        pid as i32,
        0,
        &mut regs as *mut _ as u64,
    )?;
    let maps = fs::read_to_string(format!("/proc/{pid}/maps"))?;
    let stack = maps
        .lines()
        .find(|line| line.ends_with("[stack]"))
        .context("native stack mapping absent")?;
    let mut fields = stack.split_whitespace();
    let (lo, hi) = fields
        .next()
        .unwrap()
        .split_once('-')
        .context("stack bounds")?;
    let (lo, hi) = (hex(lo)?, hex(hi)?);
    ensure!(
        fields.next() == Some("rw-p") && (lo..hi).contains(&regs.rsp),
        "unexpected/alternate stack"
    );
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let initial_sp = stat
        .rsplit_once(") ")
        .context("process stat format")?
        .1
        .split_whitespace()
        .nth(25)
        .context("kernel startstack unavailable")?
        .parse::<u64>()?;
    let mut ip = regs.rip;
    let mut fp = regs.rbp;
    let mut sp = regs.rsp;
    for depth in 0..256 {
        let f = image.function(ip)?;
        if affected.iter().any(|(a, b)| (*a..*b).contains(&ip)) {
            return Ok(Some(format!("active={} depth={depth} ip={ip:x}", f.name)));
        }
        ensure!(
            f.body <= ip && ip <= f.leave && f.insns.iter().any(|i| i.ip == ip),
            "frame in unsupported prologue/epilogue/root at {ip:x}"
        );
        ensure!(
            fp % 8 == 0
                && fp >= lo
                && fp.checked_add(16).is_some_and(|v| v <= hi)
                && fp.checked_sub(f.allocation) == Some(sp),
            "invalid stack/frame layout depth={depth}"
        );
        let frame = s.inner.read(fp, 16)?;
        let parent = u64::from_ne_bytes(frame[..8].try_into()?);
        let ret = u64::from_ne_bytes(frame[8..].try_into()?);
        eprintln!(
            "SafePatch compatible frame tid={pid} depth={depth} function={} ip={ip:x} fp={fp:x} return={ret:x}",
            f.name
        );
        if ret == image.root_return {
            ensure!(
                f.start == image.main && parent == 0 && fp.checked_add(16) == Some(initial_sp),
                "unverified root termination"
            );
            eprintln!(
                "SafePatch compatible stack tid={pid} frames={} root=verified",
                depth + 1
            );
            return Ok(None);
        }
        let caller = image.function(ret)?;
        let call = caller
            .insns
            .iter()
            .find(|i| i.next == ret && i.op == "call")
            .context("return is not a direct call continuation")?;
        ensure!(
            hex(call.args.split_whitespace().next().unwrap_or(""))? == f.start,
            "call chain edge mismatch"
        );
        ensure!(
            parent > fp && parent.checked_sub(caller.allocation + 16) == Some(fp),
            "missing/noncontiguous frame"
        );
        sp = fp + 16;
        fp = parent;
        ip = ret;
    }
    bail!("stack depth limit: root not reached")
}

// A pidfd observes the original process lifetime even if its numeric PID is
// recycled between retries. A stale PID must never authorize a new process.
fn still_alive(pidfd: &fs::File) -> Result<()> {
    let mut p = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut p, 1, 0) };
    ensure!(
        result == 0,
        "original process exited or pidfd liveness check failed"
    );
    Ok(())
}

pub(super) fn exchange(
    pid: u32,
    address: u64,
    bytes: &[u8],
    timeout: Duration,
    related: &[u64],
) -> Result<Vec<u8>> {
    let started = Instant::now();
    let mut checks = 0;
    let mut installed = false;
    let mut code_state = "unchanged";
    let result = (|| -> Result<Vec<u8>> {
        ensure!(
            cfg!(all(target_os = "linux", target_arch = "x86_64")),
            "Linux x86_64 required"
        );
        ensure!(
            pid > 0 && pid <= i32::MAX as u32 && bytes.len() == 8,
            "valid pid and 8 byte entry replacement required"
        );
        address.checked_add(8).context("address overflow")?;
        let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        ensure!(
            descriptor >= 0,
            "pidfd required: {}",
            std::io::Error::last_os_error()
        );
        let pidfd = unsafe { fs::File::from_raw_fd(descriptor as i32) };
        let image = Image::load(pid)?;
        let mut affected = vec![];
        for &entry in std::iter::once(&address).chain(related.iter()) {
            let f = image.function(entry)?;
            ensure!(
                entry == f.start && f.name != "_start" && f.end - f.start >= 8,
                "complete affected function entry required"
            );
            eprintln!(
                "SafePatch compatible scope function={} start={:x} end={:x}",
                f.name, f.start, f.end
            );
            affected.push((f.start, f.end));
        }
        let elf = object::File::parse(image.data.as_slice())?;
        let original_disk = elf
            .segments()
            .find_map(|seg| {
                let start = seg.address();
                let data = seg.data().ok()?;
                let offset = address.checked_sub(start)? as usize;
                data.get(offset..offset + 8).map(|s| s.to_vec())
            })
            .context("patch outside ELF text")?;
        ensure!(
            bytes == original_disk || bytes[0] == 0xb8 && bytes[5..] == [0xc3, 0x90, 0x90],
            "only verified return-immediate stub or original entry restoration supported"
        );
        let deadline = started.checked_add(timeout).context("timeout overflow")?;
        loop {
            ensure!(Instant::now() < deadline, "active old call timeout");
            still_alive(&pidfd)?;
            let mut snapshot = Snapshot::stop(pid, deadline)?;
            let work = (|| -> Result<Option<Vec<u8>>> {
                still_alive(&pidfd)?;
                ensure!(tasks(pid)? == [pid as i32], "thread set changed");
                let status = fs::read_to_string(format!("/proc/{pid}/status"))?;
                for field in ["SigCgt:", "SigPnd:", "ShdPnd:"] {
                    let value = status
                        .lines()
                        .find(|l| l.starts_with(field))
                        .context("signal state unavailable")?
                        .split_whitespace()
                        .nth(1)
                        .context("signal state")?;
                    ensure!(
                        hex(value)? == 0,
                        "custom handler or pending signal: {field}"
                    );
                }
                // No mappings besides the ELF, vdso/vsyscall may be executable;
                // no frame is allowed in vdso. No writable executable text.
                for line in fs::read_to_string(format!("/proc/{pid}/maps"))?.lines() {
                    let p: Vec<_> = line.split_whitespace().collect();
                    if p[1].contains('x') {
                        ensure!(
                            !p[1].contains('w')
                                && (p.get(5).is_some_and(
                                    |v| v.ends_with("[vdso]") || v.ends_with("[vsyscall]")
                                ) || p.get(4).and_then(|s| s.parse::<u64>().ok()).is_some_and(
                                    |ino| {
                                        use std::os::unix::fs::MetadataExt;
                                        fs::metadata(format!("/proc/{pid}/exe")).is_ok_and(|m| {
                                            m.ino() == ino
                                                && p[3]
                                                    == format!(
                                                        "{:02x}:{:02x}",
                                                        libc::major(m.dev()),
                                                        libc::minor(m.dev())
                                                    )
                                        })
                                    }
                                )),
                            "unverified executable mapping"
                        );
                    }
                }
                for fd in [1, 2] {
                    if let Ok(path) = fs::read_link(format!("/proc/{pid}/fd/{fd}")) {
                        use std::os::unix::fs::FileTypeExt;
                        let metadata = fs::metadata(format!("/proc/{pid}/fd/{fd}"))?;
                        let kind = metadata.file_type();
                        let path = path.to_string_lossy();
                        ensure!(
                            kind.is_fifo()
                                || kind.is_socket()
                                || metadata.is_file()
                                    && !path.starts_with("/proc/")
                                    && !path.starts_with("/sys/")
                                || path == "/dev/null"
                                || path.starts_with("/dev/pts/"),
                            "unverified output fd"
                        );
                    }
                }
                image.verify_memory(&snapshot.inner, address)?;
                let entry = snapshot.inner.read(address, 8)?;
                ensure!(
                    entry == original_disk || entry[0] == 0xb8 && entry[5..] == [0xc3, 0x90, 0x90],
                    "unverified current entry bytes"
                );
                checks += 1;
                if let Some(reason) = check_stack(&image, &snapshot, &affected, pid)? {
                    eprintln!("SafePatch compatible check={checks} result=busy {reason}");
                    return Ok(None);
                }
                ensure!(Instant::now() < deadline, "stack check timeout");
                let old = snapshot.inner.read(address, 8)?;
                code_state = "writing";
                if let Err(write) = snapshot.inner.write(address, bytes) {
                    if let Err(rollback) = snapshot.inner.write(address, &old) {
                        snapshot.retained = true;
                        code_state = "indeterminate";
                        bail!(
                            "write failed: {write:#}; rollback incomplete: {rollback:#}; target retained stopped; manual recovery required"
                        )
                    }
                    code_state = "restored";
                    bail!("write failed; original bytes restored and verified: {write:#}")
                }
                installed = true;
                code_state = "installed";
                eprintln!(
                    "SafePatch compatible install_verified pid={pid} checks={checks} threads=1 full_stack=true full_functions=true code_installed=true function_stack_clear=true consistency_complete=pending_resume business_consistency=unclaimed elapsed_us={}",
                    started.elapsed().as_micros()
                );
                Ok(Some(old))
            })();
            let cleanup = snapshot.release();
            if let Err(e) = cleanup {
                // This subset has exactly one thread; if DETACH failed and it is
                // still in our ptrace stop, rollback before any further release.
                if let Ok(Some(old)) = &work {
                    let mut regs: libc::user_regs_struct = unsafe { std::mem::zeroed() };
                    if trace(
                        libc::PTRACE_GETREGS,
                        pid as i32,
                        0,
                        &mut regs as *mut _ as u64,
                    )
                    .is_ok()
                    {
                        match snapshot.inner.write(address, old) {
                            Ok(()) => {
                                installed = false;
                                code_state = "restored";
                            }
                            Err(rollback) => {
                                snapshot.retained = true;
                                code_state = "indeterminate";
                                bail!(
                                    "detach failed: {e:#}; rollback incomplete: {rollback:#}; target retained stopped"
                                )
                            }
                        }
                    }
                }
                bail!(
                    "code_installed={installed} code_state={code_state}; {e:#}; operation not reported complete; write_result={:?}",
                    work.as_ref().err()
                )
            }
            if let Some(old) = work? {
                return Ok(old);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    })();
    match &result {
        Ok(_) => eprintln!(
            "SafePatch compatible result=success elapsed_us={} checks={checks} code_installed=true function_consistency_complete=true business_consistency=unclaimed",
            started.elapsed().as_micros()
        ),
        Err(e) => eprintln!(
            "SafePatch compatible result=error elapsed_us={} checks={checks} code_installed={installed} code_state={code_state} consistency_complete=false reason={e:#}",
            started.elapsed().as_micros()
        ),
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader},
        process::{Command, Stdio},
    };
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn target() -> Result<(Child, u64)> {
        let exe = std::env::var("GAIA_RESTRICTED_TARGET").context("set GAIA_RESTRICTED_TARGET")?;
        let data = fs::read(&exe)?;
        let elf = object::File::parse(data.as_slice())?;
        let address = elf
            .symbols()
            .find(|s| s.name() == Ok("target_value"))
            .context("target symbol")?
            .address();
        let mut child = Child(Command::new(exe).stdout(Stdio::piped()).spawn()?);
        let mut output = BufReader::new(child.0.stdout.take().unwrap());
        loop {
            let mut line = String::new();
            ensure!(output.read_line(&mut line)? > 0, "target exited");
            if line == "GAP\n" {
                break;
            }
        }
        // Keep consuming normal output so the target cannot get SIGPIPE.
        std::thread::spawn(move || {
            for line in output.lines() {
                if line.is_err() {
                    break;
                }
            }
        });
        Ok((child, address))
    }
    #[test]
    fn corrupted_chain_is_rejected_while_stopped() -> Result<()> {
        let (mut child, address) = target()?;
        let pid = child.0.id();
        let image = Image::load(pid)?;
        let mut s = Snapshot::stop(pid, Instant::now() + Duration::from_secs(1))?;
        let affected = vec![(address, image.function(address)?.end)];
        ensure!(check_stack(&image, &s, &affected, pid)?.is_none());
        let mut regs: libc::user_regs_struct = unsafe { std::mem::zeroed() };
        trace(
            libc::PTRACE_GETREGS,
            pid as i32,
            0,
            &mut regs as *mut _ as u64,
        )?;
        let saved = s.inner.read(regs.rbp, 8)?;
        // Corrupt a saved parent link, check, then repair in this same stop.
        s.inner.write(regs.rbp, &0u64.to_ne_bytes())?;
        let check = check_stack(&image, &s, &affected, pid);
        s.inner.write(regs.rbp, &saved)?;
        ensure!(check.is_err(), "truncated stack falsely accepted");
        ensure!(check_stack(&image, &s, &affected, pid)?.is_none());
        s.release()?;
        ensure!(child.0.try_wait()?.is_none());
        Ok(())
    }
    #[test]
    fn compatible_write_failure_restores_or_explicitly_retains() -> Result<()> {
        for mode in 0..3 {
            let (mut child, address) = target()?;
            let pid = child.0.id();
            let mut s = Snapshot::stop(pid, Instant::now() + Duration::from_secs(1))?;
            let old = s.inner.read(address, 8)?;
            s.release()?;
            super::super::FAULT.with(|f| {
                f.set((
                    0,
                    if mode == 0 {
                        0
                    } else if mode == 2 {
                        1
                    } else {
                        usize::MAX
                    },
                    mode == 2,
                ))
            });
            super::super::READBACK_FAIL.with(|f| f.set(mode != 0));
            let failure = exchange(
                pid,
                address,
                &[0xb8, 99, 0, 0, 0, 0xc3, 0x90, 0x90],
                Duration::from_secs(2),
                &[],
            );
            super::super::FAULT.with(|f| f.set((0, usize::MAX, false)));
            super::super::READBACK_FAIL.with(|f| f.set(false));
            let error = failure.unwrap_err();
            ensure!(child.0.try_wait()?.is_none(), "failure killed target");
            if mode == 2 {
                ensure!(
                    format!("{error:#}").contains("retained stopped"),
                    "{error:#}"
                );
                // Public API reported incomplete recovery. Explicitly recover
                // this test-owned stopped tracee; no success is fabricated.
                let mut held = Snapshot {
                    inner: ManuallyDrop::new(Stopped {
                        tids: vec![pid as i32],
                        signals: Default::default(),
                        confirmed: std::collections::HashSet::from([pid as i32]),
                        poisoned: false,
                    }),
                    retained: false,
                    newborn: Default::default(),
                    group_stops: Default::default(),
                };
                held.inner.write(address, &old)?;
                held.release()?;
            } else {
                ensure!(
                    error.to_string().contains("restored and verified"),
                    "{error:#}"
                );
                let mut stopped = Snapshot::stop(pid, Instant::now() + Duration::from_secs(1))?;
                ensure!(stopped.inner.read(address, 8)? == old);
                stopped.release()?;
            }
            ensure!(child.0.try_wait()?.is_none());
        }
        Ok(())
    }
    #[test]
    fn tracking_errors_do_not_kill_or_leave_modified_code() -> Result<()> {
        for request in [
            libc::PTRACE_SEIZE,
            libc::PTRACE_GETREGS,
            libc::PTRACE_DETACH,
        ] {
            let (mut child, address) = target()?;
            let pid = child.0.id();
            let mut s = Snapshot::stop(pid, Instant::now() + Duration::from_secs(1))?;
            let old = s.inner.read(address, 8)?;
            s.release()?;
            super::super::TRACE_FAILURE.with(|f| f.set(Some(request)));
            let result = exchange(
                pid,
                address,
                &[0xb8, 99, 0, 0, 0, 0xc3, 0x90, 0x90],
                Duration::from_secs(2),
                &[],
            );
            super::super::TRACE_FAILURE.with(|f| f.set(None));
            ensure!(result.is_err(), "tracking failure reported success");
            ensure!(
                child.0.try_wait()?.is_none(),
                "tracking failure killed target"
            );
            let mut s = Snapshot::stop(pid, Instant::now() + Duration::from_secs(1))?;
            ensure!(
                s.inner.read(address, 8)? == old,
                "tracking failure left code installed"
            );
            s.release()?;
            ensure!(
                fs::read_to_string(format!("/proc/{pid}/status"))?
                    .lines()
                    .any(|s| s == "TracerPid:\t0")
            );
        }
        Ok(())
    }
    #[test]
    fn group_stop_is_rejected_and_preserved() -> Result<()> {
        let (mut child, address) = target()?;
        let pid = child.0.id();
        ensure!(unsafe { libc::kill(pid as i32, libc::SIGSTOP) } == 0);
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let state = fs::read_to_string(format!("/proc/{pid}/status"))?;
            if state.lines().any(|s| s.starts_with("State:\tT")) {
                break;
            }
            ensure!(Instant::now() < deadline, "group stop not observed");
            std::thread::sleep(Duration::from_millis(1));
        }
        let result = exchange(
            pid,
            address,
            &[0xb8, 99, 0, 0, 0, 0xc3, 0x90, 0x90],
            Duration::from_secs(1),
            &[],
        );
        ensure!(result.is_err(), "group-stopped process accepted");
        let state = fs::read_to_string(format!("/proc/{pid}/status"))?;
        ensure!(
            state.lines().any(|s| s.starts_with("State:\tT")),
            "group stop lost on refusal: {state}"
        );
        ensure!(state.lines().any(|s| s == "TracerPid:\t0"));
        ensure!(unsafe { libc::kill(pid as i32, libc::SIGCONT) } == 0);
        ensure!(child.0.try_wait()?.is_none());
        Ok(())
    }
}
