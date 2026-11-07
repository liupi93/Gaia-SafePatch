"""Deliver Python's real KeyboardInterrupt while the controller owns child processes."""
import _thread
import json
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import runner

config_path = Path(sys.argv[1])
config = json.loads(config_path.read_text(encoding='utf-8'))


def interrupt():
    end = time.monotonic()+8
    while time.monotonic() < end:
        for path in Path(config['output_root']).glob('*/*/manifest.json'):
            try:
                if json.loads(path.read_text(encoding='utf-8')).get('status') == 'measuring':
                    time.sleep(.12)
                    _thread.interrupt_main()
                    return
            except (ValueError, OSError):
                pass
        time.sleep(.01)


threading.Thread(target=interrupt, daemon=True).start()
sys.argv = ['runner.py', '--config', str(config_path), '--variant', 'original', '--mode', 'steady', '--synthetic']
raise SystemExit(runner.main())
