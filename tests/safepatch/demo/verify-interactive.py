"""Exercise actual Enter-driven sessions through a PTY, including refusal/EOF.
No target memory, registers, instruction positions or progress events are forged.
Usage: python3 verify-interactive.py [/tmp/gaia-interactive-validation]
"""
import codecs
import errno
import json
import os
from pathlib import Path
import pty
import selectors
import subprocess
import sys
import time
import re
import struct
import fcntl
import termios
import signal
from concurrent.futures import ThreadPoolExecutor

root = Path(sys.argv[1] if len(sys.argv) > 1 else '/tmp/gaia-interactive-validation').resolve()
recording = '--recording' in sys.argv[2:]
root.mkdir(parents=True, exist_ok=True)
(root/'transcripts').mkdir(exist_ok=True)
script = Path(__file__).with_name('interactive.sh').resolve()
markers = ['[Enter] 启动服务', '[Enter] 提交旧任务 #0', '[Enter] 安装维护补丁',
           '[Enter] 提交新任务 #1', '[Enter] 恢复补丁', '[Enter] 关闭服务']

def run(mode, label, duration=30000 if recording else 5000, timeout=None, late=False, eof=False):
    before = set(root.glob(f'{mode}-*'))
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 110, 0, 0))
    command = ['bash', str(script), mode, str(duration), str(root)]
    if timeout is not None:
        command.append(str(timeout))
    monitor = None
    monitor_file = None
    if label.endswith(('-5s','-30s')):
        monitor_file = (root/'transcripts'/f'{label}.monitor.log').open('w')
        monitor = subprocess.Popen(['bash',str(script.with_name('monitor.sh')),mode,str(root)],
                                   stdout=monitor_file)
    process = subprocess.Popen(command, stdin=slave, stdout=slave, stderr=slave, start_new_session=True,
                               env={**os.environ, "TERM":"xterm-256color"})
    os.close(slave)
    selector = selectors.DefaultSelector()
    selector.register(master, selectors.EVENT_READ)
    decoder = codecs.getincrementaldecoder('utf-8')()
    transcript, consumed, step = '', 0, 0
    deadline = time.monotonic() + max(60, 2*duration/1000+20)
    try:
        while True:
            if time.monotonic() > deadline:
                raise TimeoutError(label)
            ready = selector.select(.1)
            if ready:
                try:
                    data = os.read(master, 65536)
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
                    data = b''
                if not data:
                    break
                transcript += decoder.decode(data)
            if step < len(markers):
                position = transcript.find(markers[step], consumed)
                end = position + len(markers[step]) if position >= 0 else -1
                if end >= 0:
                    # Verify the process really waits for input before each step.
                    time.sleep(.08)
                    if late and step == 2:
                        time.sleep(.5)
                    elif recording and step == 2 and label.endswith('-30s'):
                        # Simulate a lecturer taking six seconds before pressing Enter.
                        time.sleep(6)
                    if eof and step == 0:
                        os.write(master, b'\x04')
                        step = 99
                    else:
                        os.write(master, b'\n')
                        consumed = end
                        step += 1
            if process.poll() is not None and not ready:
                break
        process.wait(timeout=5)
    finally:
        if process.poll() is None:
            process.kill(); process.wait()
        selector.close(); os.close(master)
        (root/'transcripts'/f'{label}.log').write_text(transcript)
        if monitor:
            time.sleep(.3)  # Allow the independent file observer its next poll.
            monitor.send_signal(signal.SIGINT)
            monitor.wait(timeout=3)
            monitor_file.close()

    sessions = set(root.glob(f'{mode}-*')) - before
    sessions = {p for p in sessions if p.is_dir()}
    assert len(sessions) == 1, sessions
    session = sessions.pop()
    summary = json.loads((session/'summary.json').read_text())
    if monitor:
        monitor_text = (root/'transcripts'/f'{label}.monitor.log').read_text()
        assert '新增 document-0.txt' in monitor_text and '新增 document-2.txt' in monitor_text
        assert 'document-1.txt' not in monitor_text

    events = [json.loads(line) for line in (session/'events.jsonl').read_text().splitlines()]
    assert '[单调' not in transcript and '时序总结' not in transcript
    assert str(root) not in transcript and '业务排空' not in transcript
    assert '\x1b[1A' in transcript, 'No in-place footer updates'
    assert '单调' not in transcript
    assert all('relative_ns' in e for e in events)
    if eof:
        assert process.returncode != 0 and not summary['completed']
        assert not any(e['kind'] in ('ready', 'submit', 'patch_request') for e in events)
    elif timeout is not None:
        assert process.returncode != 0 and not summary['completed']
        assert step == 3, step
        assert any(e['kind'] == 'patch_refused' for e in events)
        assert summary['patch_return_ns'] is None
        assert not any(e['kind'] == 'submit' and e['data']['job'] in (1, 2) for e in events)
    else:
        assert process.returncode == 0, transcript
        assert step == 6 and summary['completed'], (step, summary)
        tasks = {t['id']: t for t in summary['tasks']}
        assert tasks[0]['exported'] and not tasks[1]['exported'] and tasks[2]['exported']
        assert (session/'files/document-0.txt').read_text() == '文档 0：20 页排版完成。\n'
        assert not (session/'files/document-1.txt').exists()
        assert (session/'files/document-2.txt').read_text() == '文档 2：20 页排版完成。\n'
        if late:
            assert not summary['old_task_unfinished_at_patch_request']
            assert any(e['kind'] == 'old_already_done' for e in events)
        elif mode == 'base':
            assert summary['old_file_begin_ns'] > summary['patch_return_ns']
        elif mode == 'compatible':
            checks = [e for e in events if e['kind'] == 'stack_check']
            assert any(e['data']['rc'] > 0 for e in checks)
            assert all(e['data']['function_entry'] == next(e['data']['address'] for e in events if e['kind']=='ready')
                       for e in checks if e['data']['rc']>0)
            assert 'render_document@0x' in transcript
            assert any(e['data']['rc'] == 0 for e in checks)
            for check in checks:
                assert check['data']['checks'] >= 1
        elif mode == 'cooperative':
            assert any(e['kind'] == 'cooperative_sample' and e['data']['gate'] == 1
                       and e['data']['active'] > 0 for e in events)
            assert summary['old_file_close_ns'] < summary['patch_return_ns']
    # Render the CSI subset used by the real UI to verify that footer redraws
    # do not append duplicate events to the scrollback.
    rows, row, col = [''], 0, 0
    token = re.compile(r'\x1b\[([0-9;]*)([A-Za-z])|.', re.S)
    for match in token.finditer(transcript):
        if match.group(1) is not None:
            count = int(match.group(1) or '1') if ';' not in match.group(1) else 1
            code = match.group(2)
            if code == 'A': row = max(0, row-count)
            elif code == 'K': rows[row] = ''
            continue
        char = match.group(0)
        if char == '\r': col = 0
        elif char == '\n':
            row += 1
            while len(rows) <= row: rows.append('')
        else:
            rows[row] = rows[row][:col].ljust(col) + char + rows[row][col+1:]
            col += 1
    rendered = '\n'.join(line for line in rows if line.strip()) + '\n'
    (root/'transcripts'/f'{label}.screen.txt').write_text(rendered)
    if not eof:
        assert rendered.count('服务启动') == 1
    if summary['completed']:
        assert rendered.count('补丁安装成功') == 1 and rendered.count('补丁恢复成功') == 1
        assert rendered.count('任务 #0 文件关闭') == 1 and rendered.count('任务 #2 文件关闭') == 1
        assert rendered.count('任务 #1 拒绝导出') == 1
        assert rendered.count('busy') <= 1 and rendered.count('gate=') <= 1, rendered
    print(f'PASS {label}: {session}', flush=True)
    return dict(label=label, session=str(session), exit_code=process.returncode, summary=summary)

if recording:
    with ThreadPoolExecutor(max_workers=3) as pool:
        results = list(pool.map(lambda mode: run(mode, mode+'-30s'), ('base','compatible','cooperative')))
else:
    results = [run(mode, mode+'-5s') for mode in ('base', 'compatible', 'cooperative')]
results.append(run('compatible', 'compatible-refused', duration=5000, timeout=100))
results.append(run('compatible', 'old-finished-before-enter', duration=200, late=True))
results.append(run('base', 'input-eof', duration=200, eof=True))
(root/'validation.json').write_text(json.dumps(results, ensure_ascii=False, indent=2)+'\n')
