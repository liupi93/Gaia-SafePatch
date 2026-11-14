"""对四种运行方式重复实测；任何时间顺序或文件检查不符都会失败。"""
import json
import pathlib
import subprocess
import sys
import tempfile

out = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else '/tmp/gaia-document-demo').resolve()
count = int(sys.argv[2]) if len(sys.argv) > 2 else 10
if count < 1:
    raise ValueError('重复次数必须大于零')
driver = pathlib.Path(__file__).resolve().parents[1] / 'target/debug/safepatch-video-demo'
results = []
for mode in ['control', 'base', 'compatible', 'cooperative']:
    cases = []
    for i in range(count):
        directory = pathlib.Path(tempfile.mkdtemp(prefix=f'repeat-{mode}-', dir=out))
        exe = out / ('cooperative' if mode == 'cooperative' else 'plain')
        run = subprocess.run([str(driver), str(exe), mode, str(directory / 'files')],
                             capture_output=True, text=True, timeout=12)
        (directory / 'runtime.log').write_text(run.stdout + run.stderr)
        if run.returncode:
            raise RuntimeError(('重复验证失败', mode, i, run.stdout, run.stderr))
        report = json.loads((directory / 'files.result.json').read_text())
        assert (directory / 'files/document-0.txt').read_text() == '文档 0：20 页排版完成。\n'
        if mode == 'compatible':
            assert report['code_installed'] and not report['consistency_complete']
        if mode in ('control',):
            assert report['new_file_exists']
        else:
            assert not report['new_file_exists'] and report['restored_file_exists']
            if mode == 'cooperative':
                assert report['old_file_end_ns'] < report['switch_return_ns']
            elif mode == 'base':
                assert report['old_file_begin_ns'] > report['switch_return_ns']
        for job in range(3):
            expected = job == 0 or (job == 1 and mode in ('control',)) or (job == 2 and mode not in ('control',))
            document = directory / f'files/document-{job}.txt'
            if expected:
                assert document.read_text() == f'文档 {job}：20 页排版完成。\n'
            else:
                assert not document.exists()
        begin, end, returned = (report[k] for k in
                                ('old_file_begin_ns', 'old_file_end_ns', 'switch_return_ns'))
        assert begin <= end
        observed = ('no_patch' if mode == 'control' else
                    'file_creation_started_after_return' if begin > returned else
                    'file_closed_before_return' if end < returned else 'file_activity_spans_return')
        assert report['business_observation'] == observed
        assert report['old_file_end_minus_return_ns'] == end - returned
        assert report['return_timestamp_sampling'] == 'immediately_after_exchange_before_validation'
        cases.append(report)
    observations = {kind: sum(c['business_observation'] == kind for c in cases)
                    for kind in sorted({c['business_observation'] for c in cases})}
    results.append(dict(mode=mode, passed=count, observations=observations, cases=cases))
    name = {'control': '正常对照', 'base': '原版', 'compatible': '兼容模式', 'cooperative': '协作模式'}[mode]
    print(f'【重复验证通过】{name}｜{count} 次全部符合预期', flush=True)
(out / 'repeat-results.json').write_text(json.dumps(results, ensure_ascii=False, indent=2) + '\n')
print('真实文件、每轮日志和时间戳记录均保存在：', out)
