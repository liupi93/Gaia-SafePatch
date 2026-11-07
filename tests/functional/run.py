#!/usr/bin/env python3
"""Isolated be3dd66 integration checks. No third-party Python dependencies.

Run on an idle dedicated openEuler test host as root. Never attaches GDB live.
Only owned child processes and one uniquely named transient unit are stopped.
"""
import argparse
import collections
import errno
import hashlib
import json
import os
from pathlib import Path
import platform
import resource
import selectors
import shutil
import signal
import socket
import subprocess as sp
import threading
import time
import urllib.request
import urllib.error

COMMIT = 'be3dd66ecc1aa4479a490df8cfb8caefa9c2c836'
HERE = Path(__file__).resolve().parent

def save(path, data):
    Path(path).write_text(json.dumps(data, ensure_ascii=False, indent=2) + '\n')

def command(argv, timeout=20):
    if argv[0] == 'gdb' and not shutil.which('gdb') and Path('/usr/libexec/gdb').exists():
        argv = ['/usr/libexec/gdb', *argv[1:]]
    p = sp.run([str(x) for x in argv], stdout=sp.PIPE, stderr=sp.STDOUT, text=True, timeout=timeout)
    return {'argv': [str(x) for x in argv], 'rc': p.returncode, 'output': p.stdout}

def stop(p):
    if p.poll() is None:
        p.send_signal(signal.SIGINT)
        try: p.wait(timeout=5)
        except sp.TimeoutExpired:
            p.kill(); p.wait(timeout=5)

class Target:
    def __init__(self, binary, folder, name, core=False):
        def limits():
            resource.setrlimit(resource.RLIMIT_CORE, (resource.RLIM_INFINITY if core else 0, resource.RLIM_INFINITY))
            if core: Path('/proc/self/coredump_filter').write_text('0x3f')
        self.binary = binary
        self.p = sp.Popen([str(binary)], stdout=sp.PIPE, text=True, bufsize=1, cwd=folder, preexec_fn=limits)
        self.values = []
        self.ready = threading.Event()
        self.address = None
        def read():
            with (folder / (name + '-target.log')).open('w') as out:
                for line in self.p.stdout:
                    out.write(line)
                    parts = line.split()
                    if parts and parts[0] == 'READY':
                        self.address = int(parts[2], 16); self.ready.set()
                    elif parts and parts[0] == 'VALUE':
                        self.values.append((int(parts[4]), int(parts[2])))
        self.reader = threading.Thread(target=read, daemon=True); self.reader.start()
        if not self.ready.wait(8):
            stop(self.p); raise RuntimeError('target did not report READY')
    def snapshot(self):
        start = time.monotonic_ns(); time.sleep(.15)
        vals = [v for t,v in self.values if t >= start]
        code, error = '', None
        try:
            with open('/proc/%d/mem' % self.p.pid, 'rb', buffering=0) as f:
                code = os.pread(f.fileno(), 16, self.address).hex()
        except OSError as e: error = str(e)
        return {'pid': self.p.pid, 'address': hex(self.address), 'exit': self.p.poll(),
                'entry': code, 'read_error': error, 'returns': dict(collections.Counter(vals)), 'sample_count':len(vals),
                'sample_clock':'target CLOCK_MONOTONIC call completion'}
    def close(self):
        stop(self.p); self.reader.join(timeout=2); self.p.stdout.close()

class Gaia:
    def __init__(self, binary, folder, port, sensitive=None, unit=None, blocked=None):
        self.folder, self.port = folder, port
        self.trace = []
        text = 'rate_limit_rules = []\n'
        for key, vals in [('sensitive_prefixes',sensitive if isinstance(sensitive,list) else [sensitive] if sensitive else []),
                          ('monitored_services',[unit] if unit else []),
                          ('exec_whitelist_prefixes',['/usr/bin','/bin','/usr/sbin'])]:
            text += key + ' = ' + json.dumps([{'value':str(v),'enabled':True} for v in vals]).replace(':',' =') + '\n'
        # Inline TOML tables use '='; paths here never contain colon characters.
        text += 'blocked_ports = ' + ('[{port = %d, enabled = true}]' % blocked if blocked else '[]') + '\n'
        text += '[baseline_thresholds]\nfile_io = 1000000\nprocess = 1000000\nnetwork = 1000000\nprivilege = 1000000\nhotpatch = 1000000\n[hotpatch]\ntargets = []\nkernel_livepatch = []\n'
        # JSON quotes around table keys are valid TOML quoted keys.
        config = folder/'gaia.toml'; config.write_text(text)
        self.log = (folder/'daemon.log').open('w')
        self.p = sp.Popen([str(binary),'--config',str(config),'--db-path',str(folder/'events.db'),
                           '--web-listen','127.0.0.1:%d'%port],cwd=folder,stdout=self.log,stderr=sp.STDOUT,
                          env={**os.environ,'RUST_LOG':'info'})
        try:
            for _ in range(60):
                if self.p.poll() is not None: raise RuntimeError('Gaia exited: '+str(self.p.returncode))
                try: self.request('GET','/state'); break
                except OSError: time.sleep(.1)
            else: raise RuntimeError('Gaia startup timeout')
        except BaseException:
            self.close(); raise
    def request(self, method, path, data=None):
        req=urllib.request.Request('http://127.0.0.1:%d/api/v1%s'%(self.port,path),
            data=None if data is None else json.dumps(data).encode(), method=method,
            headers={'Content-Type':'application/json'})
        opener=urllib.request.build_opener(urllib.request.ProxyHandler({}))
        began=time.monotonic_ns()
        try:
            with opener.open(req,timeout=15) as r: status, raw=r.status,r.read().decode()
        except urllib.error.HTTPError as e: status,raw=e.code,e.read().decode()
        try: body=json.loads(raw)
        except ValueError: body=raw
        self.trace.append({'method':method,'path':path,'request':data,'status':status,'body':body,
                           'start_ns':began,'end_ns':time.monotonic_ns()})
        return body
    def probes(self, label):
        save(self.folder/(label+'-probes.json'), {kind:command(['bpftool','-j',kind,'show']) for kind in ['link','perf']})
    def close(self):
        stop(self.p); self.log.close(); save(self.folder/'http.json',self.trace)

def policy(target, mode='cooperative', action='override_return', value=99):
    return {'binary':str(target.binary),'symbol':'target_value','pid':target.p.pid,'enabled':True,
            'patch_action':action,'override_return_value':value,'patch_mode':mode}

def sample_is(snapshot, value):
    return snapshot['exit'] is None and snapshot['sample_count']>0 and set(snapshot['returns'])=={value}

def core_report(target, folder):
    core=folder/'target.core'
    for _ in range(30):
        result=command(['coredumpctl','--no-pager','dump',str(target.p.pid),'--output',core])
        if result['rc']==0: break
        time.sleep(.5)
    save(folder/'core-export.json',result)
    if result['rc']!=0: return
    args=['gdb','-q','-nx','-batch',str(target.binary),str(core)]
    for expr in ['set pagination off','set disassembly-flavor intel','info registers rip rax rsp rbp',
                 'p $_siginfo','p/x &target_value','x/16bx target_value','x/8i $rip','x/8gx $rsp',
                 'info proc mappings','thread apply all bt 4']:
        args.extend(['-ex',expr])
    r=command(args,60); (folder/'gdb.txt').write_text(r['output'])

def http_case(args, bins, name, repetition):
    folder=args.output/('%s-%02d'%(name,repetition)); folder.mkdir()
    gaia=None; targets=[]; result={'case':name,'repetition':repetition}
    try:
        gaia=Gaia(args.binary,folder,args.port)
        kind='original' if name in ['reload-original','monitor-control'] else 'pie' if name=='pie' else 'noabi' if name in ['compatible','compatible-reload','missing-abi'] else 'return'
        target=Target(bins[kind],folder,'primary',core=name=='reload-original' and repetition==1); targets.append(target)
        result['before']=target.snapshot()
        mode='compatible' if name in ['compatible','compatible-reload','replace-compatible'] else 'cooperative'
        action='monitor' if name=='monitor-control' else 'skip_call' if name=='skip' else 'replace_function' if name.startswith('replace-') else 'override_return'
        desired=0 if name=='skip' else 99
        item=policy(target,mode,action,desired)
        gaia.request('POST','/config/hotpatch',item)
        result['after_add']=target.snapshot()
        if name in ['reload-original','monitor-control'] and repetition==1: gaia.probes('before-reload')
        if name=='put':
            item['override_return_value']=77
            gaia.request('PUT','/config/hotpatch',{'index':0,**item})
            result['after_put']=target.snapshot(); desired=77
        elif name=='partial':
            second=Target(bins['noabi'],folder,'secondary'); targets.append(second)
            gaia.request('POST','/config/hotpatch',policy(second))
            result['second_after_add']=second.snapshot()
        if name in ['reload-original','monitor-control','missing-abi','partial','compatible-reload','cooperative-reload'] or name.startswith('replace-'):
            result['reload']=gaia.request('POST','/reload-hotpatch')
            result['after_reload']=target.snapshot()
            if name in ['reload-original','monitor-control'] and repetition==1: gaia.probes('after-reload')
        if name=='reload-original' and repetition==1 and target.p.poll()==-11: core_report(target,folder)
        if name=='partial':
            result['second_after_reload']=targets[1].snapshot()
            gaia.request('DELETE','/config/hotpatch/1')
        gaia.request('DELETE','/config/hotpatch/0')
        result['after_delete']=target.snapshot()
        if name=='reload-original':
            result['expectation']='reproduce_known_failure'
            result['matched']=result['after_reload']['exit']==-11 and result['reload'].get('success') is False
        elif name=='monitor-control':
            result['matched']=result['reload'].get('success') is True and result['after_reload']['exit'] is None
        elif name=='missing-abi' or name.startswith('replace-'):
            result['matched']=result['reload'].get('success') is False and sample_is(result['after_reload'],10)
        elif name=='partial':
            result['matched']=result['reload'].get('success') is False and sample_is(result['after_reload'],99) and sample_is(result['second_after_reload'],10)
        else:
            result['matched']=sample_is(result.get('after_put',result['after_add']),desired) and sample_is(result['after_delete'],10)
            if name in ['compatible-reload','cooperative-reload']:
                result['matched']=result['matched'] and result['reload'].get('success') is True and sample_is(result['after_reload'],99)
    except Exception as e:
        result.update(error=repr(e),matched=False)
    finally:
        if gaia: gaia.close()
        for t in targets: t.close()
        save(folder/'result.json',result)
    return result

def monitoring(args,bins):
    folder=args.output/'monitoring'; folder.mkdir()
    custom=folder/'custom-sensitive-marker'; custom.write_text('synthetic test data\n')
    sentinel=Path('/var/lib/gaia')/('functional-marker-%d'%os.getpid())
    sentinel.parent.mkdir(parents=True,exist_ok=True)
    with sentinel.open('x') as f: f.write('synthetic test data\n')
    trigger=folder/'trigger'; unit='gaia-functional-%d.service'%os.getpid()
    probe=bins['monitor']; blocked=45444
    with socket.socket() as s: s.bind(('127.0.0.1',blocked))
    baseline=command([probe,'gaia-ordinary','-',sentinel,blocked])
    save(folder/'baseline.json',baseline)
    gaia=None; started=False
    try:
        gaia=Gaia(args.binary,folder,args.port,[sentinel,custom],unit,blocked)
        started=True
        launch=command(['systemd-run','--unit',unit,'--collect','--property=Type=exec',str(probe),'gaia-service',trigger,sentinel,str(blocked)])
        save(folder/'unit-launch.json',launch)
        if launch['rc']: raise RuntimeError('transient service launch failed')
        for _ in range(80):
            state=gaia.request('GET','/state')
            pids=state.get('services',{}).get(unit,{}).get('pids',[])
            if pids: break
            time.sleep(.1)
        else: raise RuntimeError('service PID not mapped')
        # Run ordinary probe after mapping; unique comm names prevent misattribution.
        ordinary=command([probe,'gaia-ordinary','-',sentinel,blocked]); save(folder/'ordinary.json',ordinary)
        record=json.loads(ordinary['output'])
        custom_run=command([probe,'gaia-custom','-',custom,blocked]); save(folder/'custom-prefix.json',custom_run)
        custom_pid=json.loads(custom_run['output'])['pid']
        trigger.touch()
        alerts=[]
        for _ in range(40):
            state=gaia.request('GET','/state'); alerts=state.get('alerts',[])
            if any(a.get('level')=='critical' and a.get('event',{}).get('pid') in pids and a.get('event',{}).get('kind')=='privilege' for a in alerts): break
            time.sleep(.1)
        save(folder/'state.json',state)
        def has(pid,level,needle):
            return any(a.get('event',{}).get('pid')==pid and a.get('level')==level and needle in a.get('reason','') for a in alerts)
        pid=record['pid']
        checks={'baseline_bind_allowed':json.loads(baseline['output'])['bind_rc']==0,
            'sensitive_file_high':has(pid,'high','sensitive file'),
            'custom_sensitive_file_high':has(custom_pid,'high','sensitive file'),
            'exec_high':has(pid,'high','execve target'),
            'ordinary_setuid_high':has(pid,'high','privilege change'),
            'ordinary_no_critical':not has(pid,'critical','privilege change'),
            'service_setuid_high':any(has(p,'high','privilege change') for p in pids),
            'service_setuid_critical':any(has(p,'critical','privilege change') for p in pids),
            'bind_blocked':record['bind_rc']==-1 and record['bind_errno'] in [errno.EPERM,errno.EACCES],
            'connect_blocked':record['connect_rc']==-1 and record['connect_errno'] in [errno.EPERM,errno.EACCES],
            'port_alert':has(pid,'critical','blocked port')}
        result={'case':'monitoring','checks':checks,'matched':all(checks.values()),'ordinary_pid':pid,'service_pids':pids,
                'custom_pid':custom_pid,'custom_open_event_seen':any(e.get('pid')==custom_pid and str(custom) in e.get('detail','') for e in state.get('events',[]))}
    except Exception as e: result={'case':'monitoring','matched':False,'error':repr(e)}
    finally:
        if started: save(folder/'unit-stop.json',command(['systemctl','stop',unit]))
        if gaia: gaia.close()
    after=command([probe,'gaia-ordinary','-',sentinel,blocked]); save(folder/'after-cleanup.json',after)
    result['cleanup_bind_allowed']=after['rc']==0 and json.loads(after['output'])['bind_rc']==0
    result['matched']=result['matched'] and result['cleanup_bind_allowed']
    sentinel.unlink()
    save(folder/'result.json',result); return result

def main():
    p=argparse.ArgumentParser()
    p.add_argument('--source',type=Path,required=True); p.add_argument('--binary',type=Path,required=True)
    p.add_argument('--output',type=Path,required=True); p.add_argument('--port',type=int,default=17892)
    p.add_argument('--repeats',type=int,default=10); p.add_argument('--cases',default='all')
    args=p.parse_args(); args.source=args.source.resolve(); args.binary=args.binary.resolve(); args.output=args.output.resolve()
    if os.geteuid()!=0: raise SystemExit('root required')
    if platform.machine()!='x86_64': raise SystemExit('x86_64 required')
    if command(['git','-C',args.source,'rev-parse','HEAD'])['output'].strip()!=COMMIT: raise SystemExit('wrong source commit')
    if command(['pgrep','-x','gaia-xdp'])['rc']==0: raise SystemExit('another Gaia is running; refusing to replace global cgroup hooks')
    with socket.socket() as sock:
        sock.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
        sock.bind(('127.0.0.1',args.port))
    args.output.mkdir(parents=True,exist_ok=False)
    env={'commit':COMMIT,'kernel':platform.release(),'os_release':Path('/etc/os-release').read_text(),
         'binary_sha256':hashlib.sha256(args.binary.read_bytes()).hexdigest(),
         'core_pattern':Path('/proc/sys/kernel/core_pattern').read_text(),
         'utc':time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime()),'repeats':args.repeats,
         'source_status':command(['git','-C',args.source,'status','--short']),
         'tools':{t:command([t,'--version']) for t in ['gcc','python3','gdb','bpftool']}}
    save(args.output/'environment.json',env)
    bins={}; build=[]
    for name,src,extra in [('return',HERE/'return_target.c',['-fno-pie','-no-pie']),
                           ('noabi',HERE/'return_target.c',['-fno-pie','-no-pie','-DNO_ABI']),
                           ('pie',HERE/'return_target.c',['-fPIE','-pie']),
                           ('original',args.source/'tests/safepatch/high_frequency.c',['-fno-pie','-no-pie']),
                           ('monitor',HERE/'monitor_target.c',['-fno-pie','-no-pie'])]:
        bins[name]=args.output/name
        r=command(['gcc','-O0','-g','-pthread','-fno-inline',*extra,src,'-o',bins[name]])
        build.append(r)
        if r['rc']: raise RuntimeError(r['output'])
    save(args.output/'build.json',build)
    names=['monitor-control','reload-original','missing-abi','compatible','cooperative','skip','put','replace-cooperative','replace-compatible','partial','pie','compatible-reload','cooperative-reload']
    chosen=names if args.cases=='all' else args.cases.split(',')
    results=[]
    if args.cases=='all' or 'monitoring' in chosen:
        results.append(monitoring(args,bins)); print(json.dumps(results[-1]),flush=True)
    for name in chosen:
        if name=='monitoring': continue
        if name not in names: raise ValueError(name)
        for n in range(1,args.repeats+1):
            r=http_case(args,bins,name,n); results.append(r)
            save(args.output/'results.json',results)
            print(json.dumps({'case':name,'round':n,'matched':r['matched'],'error':r.get('error')}),flush=True)
    save(args.output/'results.json',results)
    save(args.output/'cleanup.json',{'gaia':command(['pgrep','-x','gaia-xdp']),
          'core_pattern':Path('/proc/sys/kernel/core_pattern').read_text()})
    return 0 if all(r['matched'] for r in results) else 1

if __name__=='__main__': raise SystemExit(main())
