# SysCare native-stack integration

Source: openEuler **syscare-1.2.2-8.oe2403sp4.src.rpm**, downloaded from
https://mirrors.huaweicloud.com/openeuler/openEuler-24.03-LTS-SP4/source/Packages/syscare-1.2.2-8.oe2403sp4.src.rpm

SHA256: `d61e88057c25ff1bdd1b7d765c07b5c1298f44d33824550c919e3c392cf2743d`.
Upstream project: https://atomgit.com/openeuler/syscare . No master checkout used.
The tarball and **all Patch entries in syscare.spec, in spec order**, were applied
with `git apply` (equivalent to `%autosetup -p1` for these patches). This includes
0126 (upatch-manage synchronization) and 0152 (half-open function bounds).
`upstream/` preserves the resulting relevant files verbatim for audit; these
reference files are not compiled. `bridge.c` is the adapted C implementation.
Original copyright/license notices and GPL COPYING are retained. This directory
is GPL-2.0-or-later; it is not covered by Gaia's general MIT notice.

Ported functions/flows:

- `process_list_threads`, `upatch_process_attach`: repeated task enumeration,
  ptrace every thread, confirm a stable held set before inspection/writing.
- `upatch_ptrace_attach_thread`, `upatch_ptrace_detach`: stop/wait/detach lifetime.
- `upatch_arch_reg_init`: x86_64 GETREGS, RSP and RIP.
- `stack_check`, `read_stack`, `stack_check_each_pid`, `upatch_stack_check`:
  conservative machine-word scan over every native thread stack and RIP.
- `upatch_apply_prepare`, `upatch_unapply_prepare`: release/retry on busy;
  same safety checks on restore. Gaia keeps its existing writer and readback.

Hardening relative to this release:

- SEIZE/INTERRUPT replaces signal-generating ATTACH; bounded wait, EINTR handling,
  unexpected signals rejected and preserved on release. No EXITKILL/SIGKILL.
- Compare thread identities and confirmed stops, not counts; bounded convergence.
- Pin original process lifetime with pidfd and check after each stop.
- Resolve full ELF function sizes while stopped; reject missing boundaries and
  writes beyond the function. ET_EXEC and PIE main executables are supported.
- Scan RSP through its actual private anonymous rw VMA, not controller RLIMIT_STACK.
  Reject unknown/shared/file mappings, unaligned pointers and ranges above 64 MiB.
- Signed ssize_t and exact reads; error, zero/short read, missing maps or GETREGS
  failure cannot authorize writing. Distinguish busy from inspection errors.
- Conservatively include function-end addresses (return from a terminal call).
- Reject installed application signal handlers; glibc's internal signals 32/33
  are allowed. No custom signal/alternate-stack support is claimed.
- Verified rollback on write errors. Persistent rollback failure retains threads
  stopped. Detach failure after any release never attempts an unsafe rollback;
  it returns an incomplete-operation error carrying original bytes. Gaia records
  recovery material and reload cannot report this record as a completed patch.

## Actual guarantee and limits

Success certifies **a stop-the-world native-stack address scan and verified code
write**, followed by successful detach. In ordinary native call/return execution,
return addresses reveal affected callers even while RIP is inside libc/syscalls.
The algorithm deliberately treats data pointers as potential return addresses:
false positives and indefinite busy timeouts are possible. It is not an unwinder
and does not establish a closed, independently validated call chain.

It does not certify business consistency. Work already handed to another thread,
a queue, I/O completion or an event loop may survive after the affected function
returns. Suspended fibers/coroutines, custom stacks/context switching, JIT, tail
calls with post-return business dependencies, CLONE_VM processes outside the
thread group, executable rewriting, concurrent patch coordinators and malicious
stack manipulation are outside the supported model. These cannot all be detected
from /proc snapshots. Do not interpret native_stack_check_complete as proof of
arbitrary function or asynchronous business drain. Shared-library patch targets
and stripped/unknown function bounds are rejected by this integration.

Persistent detach failure may leave traced threads requiring operator recovery.
Persistent rollback failure requires repairing code while the controlling tracer
is alive; controller exit is not crash-safe recovery. There is no promise that
all threads can be resumed normally after persistent kernel/permission failures.

## Reproduce

From repository root:

```sh
bash tests/safepatch/run-compat.sh /tmp/gaia-syscare-tests
bash tests/safepatch/demo/run.sh /tmp/gaia-syscare-demo
python3 tests/safepatch/demo/repeat.py /tmp/gaia-syscare-demo 3
cargo build --offline -p gaia-xdp
```

Business target sources are unchanged. Nine-thread document demo verifies an
active old caller inside nanosleep rejects a 30ms attempt with bytes unchanged,
then waits for that call before installation; subsequent jobs use the patch and
jobs after restoration execute the original code. Both install/restore use real
ptrace, ELF bounds, C stack scanning and Gaia readback. Directory stability is
asserted only for Cooperative, whose region includes file writing.

Fault tests cover SEIZE, INTERRUPT, GETREGS, DETACH, actual bad-fd pread, short
pread, unknown stack mapping, install/restore POKETEXT/readback failure, partial
multiword write and persistent rollback failure with explicit test-owned recovery.
The separate pointer-retaining/conflict/churning targets verify safe refusal and
cleanup; pointer false positives are not counted as successful installations.
