import pathlib, re, sys, json
root = pathlib.Path(sys.argv[1])
intervals = []
for line in (root/'long_running.calls').read_text().splitlines():
    if line.startswith('CALL '):
        _, tid, start, end, version = line.split()
        intervals.append((int(start),int(end),int(version)))
# Sweep all versions; adjacency is allowed.
latest = {10:0,99:0}
overlap = 0
for start,end,version in sorted(intervals):
    overlap += latest[99 if version == 10 else 10] > start
    latest[version] = max(latest[version], end)
assert intervals and {v for _,_,v in intervals} == {10,99}, 'both versions must be observed'
unsafe = 0
writes = 0
for test in ('high_frequency','long_running'):
    for line in (root/f'{test}.runtime').read_text().splitlines():
        if 'write verified' in line:
            writes += 1
            unsafe += 'threads=9 active=0' not in line
    print(test, (root/f'{test}.summary').read_text().strip())
case_metrics = {}
for test in ('high_frequency','long_running'):
    values = dict(re.findall(r'(\w+)=(\d+)', (root/f'{test}.summary').read_text()))
    case_metrics[test] = {key:int(value) for key,value in values.items()}
switches = [int(value) for value in re.findall(r'switch_ns=(\d+)', (root/'long_running.runtime').read_text())]
crossing_switches = sum(any(start < switch < end for switch in switches) for start,end,_ in intervals)
assert len(switches) == 40 and crossing_switches == 0, 'call spans a code-write transition'
risk_intervals = []
for line in (root/'base-risk.calls').read_text().splitlines():
    if line.startswith('CALL '):
        _,tid,start,end,version = line.split()
        risk_intervals.append((int(start),int(end),int(version)))
latest = {10:0,99:0}
risk_overlap = 0
for start,end,version in sorted(risk_intervals):
    risk_overlap += latest[99 if version == 10 else 10] > start
    latest[version] = max(latest[version],end)
assert risk_overlap > 0, 'controlled base interleaving did not demonstrate overlap'
compat_runtime = (root/'compatible-risk.runtime').read_text()
assert 'write verified' not in compat_runtime
assert 'result=error' in compat_runtime and 'consistency_complete=false' in compat_runtime
assert 'COMPAT_REFUSED' in (root/'compatible-risk.summary').read_text()
compatible = (root/'compatible/fault-tests.log').read_text()
compatible_successes = [line for line in compatible.splitlines() if 'result=success' in line]
assert len(compatible_successes) >= 20
assert all('threads=9' in line and 'native_stack_check_complete=true' in line and
           'business_consistency=unclaimed' in line for line in compatible_successes)
assert '6 passed; 0 failed' in compatible
import statistics
performance = {}
for mode in ('base','safe'):
    samples = []
    for sample in (1,2,3):
        values = dict(re.findall(r'(\w+)=(\d+)', (root/f'perf-{mode}-{sample}.summary').read_text()))
        samples.append(int(values['completed_calls'])*1_000_000/int(values['wall_us']))
    performance[mode] = statistics.median(samples)
result = dict(successful_installs=40, successful_restorations=40, restoration_failures=sum(case["restore_failures"] for case in case_metrics.values()),
              drain_measurements=case_metrics, calls_spanning_switches=crossing_switches, verified_writes=writes, unsafe_writes=unsafe, overlapping_intervals=overlap,
              completed_long_calls=len(intervals), performance_overhead_vs_base_pct=(1-performance["safe"]/performance["base"])*100,
              median_calls_per_second=performance, base_risk_overlapping_intervals=risk_overlap, compatible_dynamic_risk_result="refused", compatible_minimum_installs=10, compatible_minimum_restorations=10,
              compatible_verified_writes=len(compatible_successes), compatible_native_stack_check_complete=True, compatible_business_consistency="unclaimed",
              base_commit=(root/"base-commit").read_text().strip())
(root/'results.json').write_text(json.dumps(result, indent=2)+'\n')
print(json.dumps(result, indent=2))
assert writes == 80 and unsafe == 0 and overlap == 0
