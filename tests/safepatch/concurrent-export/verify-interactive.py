"""Drive all six real PTY sessions; do not modify target state or synthesize progress."""
import codecs,errno,json,os,pty,selectors,subprocess,sys,time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
root=Path(sys.argv[1] if len(sys.argv)>1 else '/tmp/gaia-3x2-interactive').resolve()
root.mkdir(parents=True,exist_ok=True)
script=Path(__file__).with_name('show.sh')
markers=['[Enter] '+s for s in ['启动服务','提交四个旧任务','安装维护补丁','提交四个新任务','恢复补丁','关闭服务']]
def run(pair):
 scenario,mode=pair;out=root/f'{scenario}-{mode}';out.mkdir()
 master,slave=pty.openpty();decoder=codecs.getincrementaldecoder('utf-8')()
 p=subprocess.Popen(['bash',str(script),scenario,mode,'3000','2000'],stdin=slave,stdout=slave,stderr=slave,start_new_session=True,env={**os.environ,'TERM':'xterm-256color','GAIA_CASE_OUTPUT_DIR':str(out),'GAIA_CASE_ROOT':str(root)})
 os.close(slave);select=selectors.DefaultSelector();select.register(master,selectors.EVENT_READ)
 raw='';step=0;pos=0;deadline=time.monotonic()+45
 try:
  while True:
   assert time.monotonic()<deadline
   ready=select.select(.1)
   if ready:
    try: data=os.read(master,65536)
    except OSError as e:
     if e.errno!=errno.EIO:raise
     data=b''
    if not data:break
    raw+=decoder.decode(data)
   if step<len(markers):
    found=raw.find(markers[step],pos)
    if found>=0:
     time.sleep(.08);os.write(master,b'\n');pos=found+len(markers[step]);step+=1
   if p.poll() is not None and not ready:break
  p.wait(timeout=5);assert p.returncode==0,(scenario,mode,raw,(out/'runtime.log').read_text())
  assert step==6
  report=json.loads((out/f'result-{mode}.json').read_text())
  assert report['old_commits_after_return']==(4 if mode=='base' or scenario=='async' and mode=='compatible' else 0)
  assert report['original_bytes_restored'] and report['new_jobs_rejected']==4 and report['restored_jobs_exported']==4
  elapsed=time.monotonic()-(deadline-45)
  report['interactive_unit_seconds']=elapsed
  assert elapsed<30,(scenario,mode,elapsed)
  print('PASS',scenario,mode,f'{elapsed:.2f}s',flush=True)
  return report
 finally:
  if p.poll() is None:p.kill();p.wait()
  (out/'terminal.log').write_text(raw);os.close(master);select.close()
pairs=[(s,m) for s in ['aligned','async'] for m in ['base','compatible','cooperative']]
started=time.monotonic()
if '--sequential' in sys.argv[2:]:
 results=[run(pair) for pair in pairs]
else:
 with ThreadPoolExecutor(max_workers=6) as pool:
  results=list(pool.map(run,pairs))
(root/'timing.json').write_text(json.dumps({'sequential':'--sequential' in sys.argv[2:],'wall_seconds':time.monotonic()-started},indent=2)+'\n')
(root/'validation.json').write_text(json.dumps(results,ensure_ascii=False,indent=2)+'\n')
