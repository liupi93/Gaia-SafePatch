#!/usr/bin/env python3
"""Produce reviewable evidence from archived run.py outputs; never change raw logs."""
import argparse
import collections
import hashlib
import json
from pathlib import Path

def main():
    p=argparse.ArgumentParser(); p.add_argument('--input',type=Path,required=True); p.add_argument('--output',type=Path,required=True)
    a=p.parse_args(); a.output.mkdir(parents=True,exist_ok=True)
    def read(path): return json.loads((a.input/path).read_text(encoding='utf-8'))
    def redact(s):
        return s.replace('/opt/gaia/tests/be3dd66.dv6tEw/src','<SOURCE>').replace('/opt/gaia/tests/functional-20261107','<TEST_ROOT>')
    made=[]
    def write(name,obj):
        text=obj if isinstance(obj,str) else json.dumps(obj,ensure_ascii=False,indent=2)+'\n'
        (a.output/name).write_text(redact(text),encoding='utf-8',newline='\n'); made.append(name)
    manifest=read('raw-manifest.json')
    for item in manifest:
        if item.get('archived',True):
            assert hashlib.sha256((a.input/item['path']).read_bytes()).hexdigest()==item['sha256'],item['path']
    batches=['formal','reload-returns','timestamp-validation']; rounds=[]; groups=[]; http=[]; env={}
    for batch in batches:
        rows=read(batch+'/results.json'); env[batch]=read(batch+'/environment.json')
        counts=collections.defaultdict(lambda:[0,0])
        for r in rows:
            rounds.append({'batch':batch,**r}); counts[r['case']][0]+=1; counts[r['case']][1]+=bool(r['matched'])
            if r['case']=='monitoring': continue
            folder='%s/%s-%02d'%(batch,r['case'],r['repetition'])
            for req in read(folder+'/http.json'):
                if req['method']!='GET': http.append(json.dumps({'batch':batch,'case':r['case'],'round':r['repetition'],**req},ensure_ascii=False))
        groups.extend({'batch':batch,'case':name,'runs':c[0],'matched':c[1]} for name,c in counts.items())
    write('supplemental-results.json',{'note':'matched means scenario expectation, not all features passed; reload-original matches a crash',
                                      'groups':groups,'rounds':rounds})
    write('supplemental-http.jsonl','\n'.join(http)+'\n')
    monitoring=read('formal/monitoring/result.json'); state=read('formal/monitoring/state.json')
    pids=[monitoring['ordinary_pid'],monitoring['custom_pid'],*monitoring['service_pids']]
    write('supplemental-monitor.json',{'result':monitoring,
         'alerts':[x for x in state['alerts'] if x.get('event',{}).get('pid') in pids],
         'events':[x for x in state['events'] if x.get('pid') in pids],
         'config':(a.input/'formal/monitoring/gaia.toml').read_text(encoding='utf-8'),
         'process_outputs':{s:read('formal/monitoring/'+s+'.json') for s in ['baseline','ordinary','custom-prefix','after-cleanup']}})
    root='formal/reload-original-01/'
    write('supplemental-core.log',(a.input/root/'gdb.txt').read_text(encoding='utf-8'))
    before=read(root+'before-reload-probes.json'); after=read(root+'after-reload-probes.json')
    ids={r['id'] for r in json.loads(before['link']['output'])}
    write('supplemental-probes.json',{'before':before,'after':after,
          'new_links':[r for r in json.loads(after['link']['output']) if r['id'] not in ids],
          'limitation':'bpftool 7.2 perf show did not expose Gaia uprobes here; link show adds two perf_event links. Do not infer absence from perf output.'})
    excerpts=[]
    for path in ['formal/reload-original-01/daemon.log','formal/replace-cooperative-01/daemon.log',
                 'formal/partial-01/daemon.log','reload-returns/cooperative-reload-03/daemon.log']:
        excerpts.extend(['# '+path,(a.input/path).read_text(encoding='utf-8')])
    write('supplemental-daemon.log','\n'.join(excerpts))
    write('supplemental-environment.json',{'batches':env,'cleanup':{b:read(b+'/cleanup.json') for b in batches},
          'script_snapshots':[x for x in manifest if '/test-scripts/' in x['path']],
          'cores_retained_on_server':[x for x in manifest if x.get('archived') is False],
          'raw_archive_sha256':hashlib.sha256((a.input/'evidence-only.tar.gz').read_bytes()).hexdigest()})
    write('supplemental-SHA256SUMS',''.join(hashlib.sha256((a.output/n).read_bytes()).hexdigest()+'  '+n+'\n' for n in made))
    print(json.dumps(groups,ensure_ascii=False,indent=2))

if __name__=='__main__': main()
