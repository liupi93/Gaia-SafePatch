"""Observe real output files from the currently selected demo session."""
import argparse
import json
from pathlib import Path
import time

parser = argparse.ArgumentParser()
parser.add_argument('mode', choices=['base', 'compatible', 'cooperative'])
parser.add_argument('root', nargs='?', default='/tmp/gaia-document-interactive')
parser.add_argument('--once', action='store_true')
args = parser.parse_args()
link = Path(args.root).expanduser().resolve() / ('current-' + args.mode)
previous, selected, origin, waiting = {}, None, time.monotonic_ns(), False

def emit(message):
    print(f'+{max(0,time.monotonic_ns()-origin)/1e9:.3f}s  {message}', flush=True)

try:
    while True:
        try:
            files = link.resolve(strict=True)
            metadata = json.loads((files.parent/'session.json').read_text())
            if selected != files:
                selected, previous = files, {}
                origin = metadata['started_monotonic_ns']
                emit('监视 ' + args.mode)
                waiting = False
            current = {}
            for file in sorted(files.glob('document-*.txt')):
                try:
                    stat = file.stat()
                    if file.is_file():
                        current[file.name] = (stat.st_size, stat.st_mtime_ns, stat.st_ino)
                except FileNotFoundError:
                    continue
            if not current and not previous and not waiting:
                emit('暂无文档')
                waiting = True
            for name, info in current.items():
                if name not in previous:
                    emit(f'新增 {name}  {info[0]} B')
                elif previous[name] != info:
                    emit(f'变化 {name}  {info[0]} B')
            for name in previous.keys() - current.keys():
                emit('移除 ' + name)
            previous = current
        except (FileNotFoundError, json.JSONDecodeError):
            if not waiting:
                emit('等待服务')
                waiting = True
        except OSError:
            emit('监视失败')
            raise SystemExit(1)
        if args.once:
            break
        time.sleep(.25)
except KeyboardInterrupt:
    pass
