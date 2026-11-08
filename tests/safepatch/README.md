# SafePatch on openEuler x86_64

Run from the repository root:

```sh
bash tests/safepatch/run.sh /tmp/gaia-safepatch-results
```

Requires C compiler, Rust, and permission to ptrace child processes. The driver
uses the exact Rust runtime compiled into Gaia, independently of the daemon's
WebUI/eBPF build. Logs, summaries and results.json are retained. Child processes
are killed and reaped even on driver errors. Each case performs 20 installs and
20 restorations. The long case waits for an observed active call before testing
zero-timeout rejection. Invalid addresses and invalid ABI are also rejected.
The interval sweep checks completed calls of both versions across all workers.
A zero result is evidence for this run, not proof for arbitrary target programs.

## Cooperative contract

Only x86_64 fixed-thread targets are supported. Export `gaia_safepatch_state` as
three aligned sequentially consistent atomic uint64_t values: magic
0x4741494153503031, admission gate, active call count. See target.h for entry code.
Every path into the patched function must pass admission, increment active,
recheck the gate, and remain counted until return and completion of any use of
version-specific state. A single process-wide counter conservatively covers all
patched functions and replacement versions. No recursion outside this wrapper,
thread creation/destruction, fork/exec, nonlocal exits, signal-handler calls,
JIT/self-modification or competing ptrace coordinators are permitted. The ABI
symbol is an explicit opt-in contract, not automatic validation of arbitrary code.
The supplied tests are non-PIE and disable inlining.

The coordinator stops every /proc/PID/task thread with SEIZE/INTERRUPT and verifies
the set. With all stopped, it closes admission. If active is nonzero, it resumes
threads to drain while admission remains closed, then stops and checks again.
Only active=0 plus all threads stopped permits writing. Every machine-code word
is read back. A failed write rolls back before resumption; rollback failure kills
the target to prevent execution of partial instructions. Restoration uses the same
protocol. A drain timeout reopens admission without changing code. Failures to
reacquire may leave admission closed and require operator recovery; no write is
authorized in that state. Fixed-thread and cooperative assumptions are mandatory.
Remote dlopen replacement is rejected until a cooperative loader is available.

Gaia serializes transitions using its code_patches mutex. Reload success requires
all requested machine-code patches to be recorded after verified writes, and a
failed restoration retains its saved bytes for retry. The endpoint does not claim
multi-target transactional rollback. Other configuration endpoints retain their
existing configuration response schema; reload reports execution status.

## Aya observation

Entry/return probes maintain SAFEPATCH_CALLS keyed by full TGID/TID with nesting
and monotonic entry timestamp; update failures and unmatched returns increment
SAFEPATCH_OBSERVATION_ERRORS. `/api/v1/hotpatch-observation` exposes snapshots.
Existing ring events contain entry/return events. Probes must be attached before
the workload is released for complete observation. Late attach, longjmp, missing
returns, map exhaustion, process exit and ring loss make observation incomplete;
it never substitutes for the cooperative drain proof. Multiple probed functions
share conservative nesting. The map retains completed entries for inspection.

## Measurement limitations

The harness extracts and compiles the exact original ptrace_read_and_write
function from `base:gaia-xdp/src/main.rs` at build time; the commit is recorded.
Create `base` at the original revision before running the suite. A controlled risk
case parks seven workers, waits for worker zero to enter the old delayed function,
writes using the base writer, then releases the seven workers. The analyzer must
observe overlapping old/new completed calls to accept the risk demonstration.
These intentionally unsafe writes are isolated in base-risk logs and never count
as safe-mode successes. A base crash is possible and is a real test failure, not
replaced by a fabricated result.

Three alternating base/safe high-frequency samples measure median calls/second
through twenty installation/restoration cycles with identical 25 ms hold periods.
The base performance binary omits admission/counter operations; the safe binary
includes them. Both retain identical call timing and total-call instrumentation.
The reported overhead is 100*(1-safe_throughput/base_throughput). This is an
end-to-end microbenchmark of the writer plus cooperative protocol, not a whole
Gaia daemon/eBPF overhead measurement. Scheduling, tracing and instrumented O0
code affect the result; no CPU affinity or isolated CPU is imposed. Signed
negative values reflect noise, not a guaranteed speedup. Full daemon/API and
loaded eBPF verification remain separate from this standalone runtime suite.

## Recorded run (2026-11-06)

`measured-results.json` records the actual openEuler 24.03 SP4 x86_64 run
(kernel 6.6.0-159.4.19.173.oe2403sp4). Full raw logs are in
`/tmp/gaia-safepatch-results` on the test host. Safe mode verified 40 installations
and 40 restorations, zero unsafe writes, zero mixed-version overlap and zero calls
spanning the recorded monotonic write transitions. Timeout, invalid-address and
invalid-ABI rejection passed. Base commit eec1660a4d6679fb3ce687b757a25578a6abd326
produced mixed-version overlap under the controlled interleaving. The three-pair
microbenchmark measured a 13.28% median throughput reduction; reruns can vary.

`cargo check --offline -p gaia-xdp` passed, including the real BPF target build.
The runtime suite does not load the BPF object or issue HTTP requests to a running
Gaia daemon; verifier acceptance, live observation and HTTP integration have not
been measured by this run. Existing remote dlopen replacement is deliberately
rejected, so a replacement target cannot report a successful code installation.

兼容模式新增入口：`bash tests/safepatch/run-compat.sh`。配置与三模式能力差异见 [原理说明](../../docs/safepatch-explained.md) 和 [测试文档](../../docs/safepatch-testing.md)；`run.sh` 现在也验证 compatible 的受控新旧重叠局限。
