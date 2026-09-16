from test_home import isolate
isolate()

import sys
sys.dont_write_bytecode=True
import importlib.util,pathlib,os,pty,fcntl,termios,struct,subprocess,time,select,sqlite3,json,signal
repo=pathlib.Path(__file__).resolve().parents[2]
spec=importlib.util.spec_from_file_location('helper',repo/'tests/support/clipboard_pty.py')
helper=importlib.util.module_from_spec(spec);spec.loader.exec_module(helper)
import tempfile
fixture = tempfile.TemporaryDirectory(prefix='rustrace-clipboard-recovery-')
root=pathlib.Path(fixture.name)
package=helper.package_at(str(root),include_src=True); work=root/'assignment.work'; source=work/'src'
master,slave=pty.openpty();fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',36,140,0,0))
def child():
 os.setsid();fcntl.ioctl(0,termios.TIOCSCTTY,0)
proc=subprocess.Popen([sys.argv[1],'work',package],stdin=slave,stdout=slave,stderr=slave,preexec_fn=child,cwd=root,env={**os.environ,'TERM':'xterm-256color'})
transcript=bytearray()
def pump(seconds):
 end=time.monotonic()+seconds
 while time.monotonic()<end:
  if select.select([master],[],[],0.025)[0]:
   try:transcript.extend(os.read(master,65536))
   except OSError:break
  assert len(transcript)<2*1024*1024
def send(keys):os.write(master,keys);pump(0.3)
try:
 for _ in range(50):
  pump(0.1)
  if b' files' in transcript:break
 assert b' files' in transcript
 send(b'\x1b[<0;4;3M\x1b[<0;4;3m')
 send(b'\x01')
 source.chmod(0o500)
 send(b'\x1b[<2;4;1M');send(b'\x1b[<0;5;5M');send(b'new.rs\r')
 assert not (source/'new.rs').exists(),'denied src/ create unexpectedly succeeded'
 source.chmod(0o700)
 screen=helper.rendered_screen(transcript)
 assert 'recovery' in screen.lower(), 'failed creation did not latch recovery'
 meta=json.loads((work/'.rustrace/session.json').read_text())
 db=work/'.rustrace'/ (meta['session_id']+'.sqlite')
 def copies():
  with sqlite3.connect(db) as c:events=[json.loads(row[0]) for row in c.execute('SELECT payload FROM events ORDER BY sequence')]
  return sum(e['event']['type']=='clipboard_copied' for e in events)
 before=copies()
 send(b'\x03')
 after=copies();send(b'\x11');proc.wait(timeout=10)
 print(f'actual CLI: recovery-required after denied create; ClipboardCopied before={before}, after={after}; exit={proc.returncode}',flush=True)
 assert after==before,'actual CLI Ctrl-C appended clipboard source despite recovery-required controller'
finally:
 source.chmod(0o700)
 if proc.poll() is None:os.killpg(proc.pid,signal.SIGKILL);proc.wait()
 os.close(master);os.close(slave)
 fixture.cleanup()
