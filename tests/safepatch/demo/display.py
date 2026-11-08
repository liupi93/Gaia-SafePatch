"""将已知的底层运行日志解释为中文；未知日志和错误原样保留。"""
import re
import sys

for line in sys.stdin:
    if line.startswith('SafePatch compatible write verified '):
        n = re.search(r'threads=(\d+)', line)
        print(f'【底层检查】已暂停全部 {n[1] if n else "未知数量的"} 个线程；没有线程正在执行覆盖区间；写入后检查一致。', flush=True)
    elif line.startswith('SafePatch write verified '):
        n = re.search(r'threads=(\d+)', line)
        print(f'【底层检查】旧调用已全部结束；全部 {n[1] if n else "未知数量的"} 个线程已暂停；写入后检查一致。', flush=True)
    else:
        print(line, end='', flush=True)
