"""Run after demo/run.sh; exits on any missing crash or incorrect outcome."""
import json
import pathlib
import subprocess
import sys

out = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else '/tmp/gaia-video-demo').resolve()
driver = pathlib.Path(__file__).resolve().parents[1] / 'target/debug/safepatch-video-demo'
results = []
for scene, count, modes in [('entry', 20, ['base', 'compatible']),
                            ('orders', 5, ['base', 'compatible', 'cooperative'])]:
    for mode in modes:
        for i in range(count):
            exe = out / ('cooperative' if mode == 'cooperative' else 'plain')
            p = subprocess.run([str(driver), str(exe), scene, mode], capture_output=True,
                               text=True, timeout=10)
            (out / f'repeat-{scene}-{mode}-{i}.log').write_text(p.stdout + p.stderr)
            if p.returncode:
                raise RuntimeError(("重复测试失败", scene, mode, i, p.stdout, p.stderr))
        results.append(dict(scene=scene, mode=mode, passed=count))
(out / 'repeat-results.json').write_text(json.dumps(results, indent=2) + '\n')
for row in results:
    scene = {'entry': '入口指令冲突', 'orders': '订单结算'}[row['scene']]
    mode = {'base': '原版', 'compatible': '兼容模式', 'cooperative': '协作模式'}[row['mode']]
    print(f"【重复验证通过】{scene}｜{mode}｜{row['passed']} 次全部符合预期")
print("完整记录已保存到：", out / 'repeat-results.json')
