"""Add the same CLOCK_MONOTONIC markers to two disposable Gaia source trees."""
import re
import sys
from pathlib import Path

root = Path(sys.argv[1])
main = root / "gaia-xdp/src/main.rs"
web = root / "gaia-xdp/src/web.rs"
source = main.read_text()
needle = r"(?m)^    let result = ptrace_read_and_write\(pid, func_addr, patch_bytes(?:, target.patch_mode)?\);$"
source, count = re.subn(needle, lambda m: (
    "    let perf_begin = gaia_perf_now();\n" + m[0] +
    "\n    let perf_end = gaia_perf_now();"
    "\n    gaia_perf_marker(\"apply_begin\", perf_begin, \"\");"
    "\n    GAIA_PERF_GATE.with(|gate| {"
    "\n        if let Some((closed, opened)) = gate.take() {"
    "\n            gaia_perf_marker(\"admission_closed\", closed, \"\");"
    "\n            gaia_perf_marker(\"admission_resumed\", opened, \"\");"
    "\n        }"
    "\n    });"
    "\n    gaia_perf_marker(\"apply_end\", perf_end, if result.is_ok() { \"ok\" } else { \"error\" });"
), source)
if count != 1 or "fn gaia_perf_marker" in source:
    raise SystemExit("unexpected Gaia writer source")
source += r'''

thread_local! {
    static GAIA_PERF_GATE: std::cell::Cell<Option<(u64, u64)>> = const { std::cell::Cell::new(None) };
}

fn gaia_perf_now() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts); }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

fn gaia_perf_marker(event: &str, stamp: u64, outcome: &str) {
    use std::io::Write as _;
    let (Ok(path), Ok(run)) = (std::env::var("GAIA_PERF_PATCH_EVENTS_FILE"),
        std::env::var("GAIA_PERF_RUN_ID")) else { return; };
    let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path)
        else { return; };
    if file.metadata().map(|m| m.len() == 0).unwrap_or(false) {
        let _ = writeln!(file, "run_id,attempt_id,event,mono_ns,outcome");
    }
    let _ = writeln!(file, "{run},patch-1,{event},{stamp},{outcome}");
}
'''
web_source = web.read_text()
start = web_source.index("async fn api_update_hotpatch(")
body = web_source.index("    let mut policy = shared.policy.write().await;", start)
web_source = web_source[:body] + (
    '    if matches!(update.data.patch_action, PatchAction::OverrideReturn) {\n'
    '        super::gaia_perf_marker("request_received", super::gaia_perf_now(), "");\n'
    '    }\n'
) + web_source[body:]
main.write_text(source)
web.write_text(web_source)
coordinator = root / "gaia-xdp/src/safepatch.rs"
if coordinator.is_file():
    source = coordinator.read_text()
    closed = '    stopped.write(state + 8, &1u64.to_ne_bytes())?;'
    opened = '    holder.as_mut().unwrap().release()?;\n    result'
    if source.count(closed) != 1 or source.count(opened) != 1:
        raise SystemExit('unexpected cooperative gate source')
    source = source.replace(closed, closed + '\n    let perf_gate_closed = crate::gaia_perf_now();')
    source = source.replace(opened, '    let perf_gate_opened = crate::gaia_perf_now();\n'
                            '    holder.as_mut().unwrap().release()?;\n'
                            '    crate::GAIA_PERF_GATE.with(|gate| gate.set(Some((perf_gate_closed, perf_gate_opened))));\n'
                            '    result')
    coordinator.write_text(source)
