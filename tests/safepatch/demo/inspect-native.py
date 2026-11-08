"""Read-only acceptance probe using the unchanged SysCare C bridge.

Stops/checks/releases an unmodified document target without installing a patch.
Records the exact register or stack word that explains each busy result. This is
observational evidence, not a replacement safety checker or an unwind algorithm.
Usage: python3 inspect-native.py /absolute/path/plain /output/audit.json
"""
import ctypes as C
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time

exe = Path(sys.argv[1]).resolve()
output = Path(sys.argv[2]).resolve()
bridge = Path(__file__).resolve().parents[3] / 'gaia-xdp/src/safepatch/syscare/bridge.c'
GETREGS = 12
fields = 'r15 r14 r13 r12 rbp rbx r11 r10 r9 r8 rax rcx rdx rsi rdi orig_rax rip cs eflags rsp ss fs_base gs_base ds es fs gs'.split()

class Regs(C.Structure):
    _fields_ = [(name, C.c_uint64) for name in fields]

class Range(C.Structure):
    _fields_ = [('start', C.c_uint64), ('end', C.c_uint64)]

libc = C.CDLL(None, use_errno=True)
libc.ptrace.argtypes = [C.c_uint, C.c_int, C.c_void_p, C.c_void_p]
libc.ptrace.restype = C.c_long
trace_type = C.CFUNCTYPE(C.c_long, C.c_uint, C.c_int, C.c_uint64, C.c_uint64)
checked = []

@trace_type
def trace(req, tid, addr, data):
    C.set_errno(0)
    rc = libc.ptrace(req, tid, addr, data)
    error = C.get_errno()
    if error:
        return -error
    if req == GETREGS:
        regs = C.cast(data, C.POINTER(Regs)).contents
        checked.append(dict(tid=tid, rip=regs.rip, rsp=regs.rsp))
    return rc

with tempfile.TemporaryDirectory(prefix='gaia-stack-audit-') as temporary:
    tmp = Path(temporary)
    subprocess.run(['cc', '-O2', '-fPIC', '-shared', str(bridge), '-o', str(tmp/'bridge.so')], check=True)
    lib = C.CDLL(str(tmp/'bridge.so'))
    lib.gaia_upatch_new.argtypes = [C.c_int, trace_type]
    lib.gaia_upatch_new.restype = C.c_void_p
    for name in ['stop', 'check', 'release', 'tids']:
        getattr(lib, 'gaia_upatch_'+name).restype = C.c_int
    lib.gaia_upatch_stop.argtypes = [C.c_void_p, C.c_uint]
    lib.gaia_upatch_check.argtypes = [C.c_void_p, C.POINTER(Range), C.c_size_t, C.c_int]
    lib.gaia_upatch_release.argtypes = [C.c_void_p]
    lib.gaia_upatch_tids.argtypes = [C.c_void_p, C.POINTER(C.c_int)]
    disassembly = subprocess.check_output(['objdump', '-d', '--disassemble=render_document', str(exe)], text=True)
    instructions = []
    for line in disassembly.splitlines():
        match = re.match(r'\s*([0-9a-f]+):\s+(?:[0-9a-f]{2}\s+)+\s*(.*)', line)
        if match:
            instructions.append((int(match[1], 16), match[2]))
    layout_returns = [instructions[i+1][0] for i, (_, text) in enumerate(instructions[:-1])
                      if 'call' in text and '<layout_document' in text]
    assert layout_returns, disassembly
    symbols = subprocess.check_output(['nm', '-S', str(exe)], text=True)
    symbol = next(line.split() for line in symbols.splitlines() if line.endswith(' render_document'))
    symbol_start, size = int(symbol[0], 16), int(symbol[1], 16)
    # This inspector deliberately restricts itself to the existing non-PIE demo.
    child = subprocess.Popen([str(exe), str(tmp)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1)
    ctx = None
    try:
        ready = child.stdout.readline().split()
        assert ready[0] == 'READY' and int(ready[2], 16) == symbol_start
        affected = Range(symbol_start, symbol_start + size)
        child.stdin.write('0\n'); child.stdin.flush()
        assert child.stdout.readline().startswith('LAYOUT')
        started = time.monotonic_ns()
        samples = []
        while time.monotonic_ns() - started < 3_000_000_000:
            ctx = lib.gaia_upatch_new(child.pid, trace)
            assert ctx
            try:
                assert lib.gaia_upatch_stop(ctx, 1000) == 0
                tids = (C.c_int * 4096)()
                count = lib.gaia_upatch_tids(ctx, tids)
                assert count == 9
                checked.clear()
                result = lib.gaia_upatch_check(ctx, C.byref(affected), 1, 0)
                assert result >= 0
                record = dict(elapsed_ns=time.monotonic_ns()-started, result='busy' if result else 'clean',
                              held_threads=count, file_exists=(tmp/'document-0.txt').exists())
                if result:
                    # The C checker stops at the first match; its last GETREGS
                    # identifies the thread containing that match.
                    regs = checked[-1]
                    mappings = [line.split() for line in Path(f'/proc/{child.pid}/maps').read_text().splitlines()]
                    pc_mapping = next(m for m in mappings if int(m[0].split('-')[0], 16) <= regs['rip'] < int(m[0].split('-')[1], 16))
                    record.update(regs)
                    record['rip_mapping'] = pc_mapping[-1]
                    record['rip_file_offset'] = regs['rip'] - int(pc_mapping[0].split('-')[0], 16) + int(pc_mapping[2], 16)
                    if affected.start <= regs['rip'] <= affected.end:
                        record['match_kind'] = 'rip'
                    else:
                        mapping = next(m for m in mappings if int(m[0].split('-')[0], 16) <= regs['rsp'] < int(m[0].split('-')[1], 16))
                        end = int(mapping[0].split('-')[1], 16)
                        fd = os.open(f'/proc/{child.pid}/mem', os.O_RDONLY)
                        try:
                            stack = os.pread(fd, end-regs['rsp'], regs['rsp'])
                        finally:
                            os.close(fd)
                        assert len(stack) == end-regs['rsp']
                        matches = [(i, int.from_bytes(stack[i:i+8], 'little')) for i in range(0, len(stack), 8)
                                   if affected.start <= int.from_bytes(stack[i:i+8], 'little') <= affected.end]
                        assert matches
                        offset, value = matches[0]
                        record.update(match_kind='stack_word', stack_slot=regs['rsp']+offset,
                                      matched_address=value, render_offset=value-symbol_start,
                                      matches_layout_return=value in layout_returns)
                samples.append(record)
            finally:
                rc = lib.gaia_upatch_release(ctx)
                if rc:
                    rc = lib.gaia_upatch_release(ctx)
                assert rc == 0
                ctx = None
            if not result:
                break
            time.sleep(.002)
        assert samples[-1]['result'] == 'clean'
        busy = [s for s in samples if s['result'] == 'busy']
        assert busy and any(s.get('matches_layout_return') for s in busy)
        done = child.stdout.readline()
        assert done.startswith('DONE 0 1 '), done
        locations = {(s['rip_mapping'], s['rip_file_offset']) for s in busy}
        resolved = [{'module': module, 'file_offset': offset, 'addr2line': subprocess.check_output(
                    ['addr2line', '-f', '-e', module, hex(offset)], text=True).strip()}
                    for module, offset in sorted(locations)]
        report = dict(bridge_sha256=hashlib.sha256(bridge.read_bytes()).hexdigest(),
                      executable_sha256=hashlib.sha256(exe.read_bytes()).hexdigest(),
                      rip_locations=resolved, purpose='read-only busy-context inspection; no patch installed',
                      executable=str(exe), affected_start=symbol_start, affected_size=size,
                      render_disassembly=disassembly, layout_call_return_addresses=layout_returns,
                      samples=samples, done_event=done.strip(),
                      note='Clean snapshot/file ordering is an observation, not proof that the checker waits for file I/O.')
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(json.dumps(report, ensure_ascii=False, indent=2)+'\n')
        print(f'{len(busy)} busy snapshots; {sum(s.get("matches_layout_return", False) for s in busy)} match layout return; evidence: {output}')
    finally:
        if ctx:
            lib.gaia_upatch_release(ctx)
        child.kill(); child.wait()
