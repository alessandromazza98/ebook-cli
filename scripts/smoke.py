#!/usr/bin/env python3
"""CLI integration checks against a local qBittorrent stub; never sends email."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import hashlib
import json
from pathlib import Path
import socket
import sqlite3
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[1]
BIN = ROOT / 'target/debug/ebook'

def main():
    with tempfile.TemporaryDirectory(prefix='ebook-smoke-') as tmp:
        root = Path(tmp)
        added = []
        owned = [True]
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass
            def answer(self, body):
                data = body.encode() if isinstance(body, str) else json.dumps(body).encode()
                self.send_response(200)
                self.send_header('Content-Length', str(len(data)))
                self.end_headers()
                self.wfile.write(data)
            def do_POST(self):
                self.rfile.read(int(self.headers.get('Content-Length', '0')))
                if self.path.endswith('/login'):
                    if added:
                        self.send_response(204); self.end_headers()
                    else:
                        self.answer('Ok.')
                elif self.path.endswith('/add'):
                    added.append(1)
                    # Accepted request, lost response: next tick must reconcile.
                    self.connection.shutdown(socket.SHUT_RDWR)
                    self.connection.close()
                else:
                    self.send_error(404)
            def do_GET(self):
                endpoint = self.path.split('?')[0]
                if endpoint.endswith('/categories'):
                    self.answer({'ebook-cli': {'savePath':'/downloads/books/ebook-cli'}})
                elif endpoint.endswith('/info'):
                    self.answer([] if not added else [{'category':'ebook-cli' if owned[0] else 'chaptarr','save_path':'/downloads/books/ebook-cli','state':'stalledUP','progress':1.0,'amount_left':0}])
                elif endpoint.endswith('/files'):
                    self.answer([{'name':'book.epub','size':(root/'ebook-cli/book.epub').stat().st_size,'progress':1.0}])
                else:
                    self.send_error(404)
        server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        config = {
            'state_dir':str(root/'state'), 'processing_dir':str(root/'processing'),
            'mam':{'base_url':'https://www.myanonamouse.net','session':'test-session','vip':False},
            'qbittorrent':{'url':f'http://127.0.0.1:{server.server_port}','username':'test','password':'test','category':'ebook-cli','remote_dir':'/downloads/books/ebook-cli','local_dir':str(root/'ebook-cli')},
            'smtp':{'host':'127.0.0.1','port':1,'tls':'starttls','username':'','password':'','from':'test@example.com','kindle':'kindle@example.com'},
            'api_token':'test-only-'+'a'*64,
        }
        cfg = root/'config.json'
        cfg.write_text(json.dumps(config)); cfg.chmod(0o600)
        def cli(*args, code=0):
            p = subprocess.run([str(BIN),'--config',str(cfg),*args],capture_output=True,text=True,timeout=45)
            assert p.returncode == code, (args,p.returncode,p.stdout,p.stderr)
            return json.loads(p.stdout)
        cli('jobs')
        (root/'ebook-cli').mkdir()
        source = (ROOT/'tests/fixtures/cwa/test_minimal_valid.epub').read_bytes()
        (root/'ebook-cli/book.epub').write_bytes(source)
        metainfo = b'd4:infod6:lengthi5e4:name9:book.epub12:piece lengthi16384e6:pieces20:aaaaaaaaaaaaaaaaaaaaee'
        job_id = str(uuid.uuid4())
        conn = sqlite3.connect(root/'state/jobs.sqlite')
        release = {'id':'mam:1','title':'Test','author':'Test Author','language':'en','format':'EPUB','size_bytes':len(source),'seeders':1,'restrictions':[],'eligible':True}
        conn.execute('INSERT INTO releases VALUES (?,?)', ('mam:1',json.dumps(release)))
        conn.commit()
        job = cli('select','mam:1','--idempotency-key','smoke')['data']['job']
        job_id = job['id']
        assert cli('select','mam:1','--idempotency-key','another')['data']['job']['id'] == job_id
        assert cli('select','mam:1','--language','fr','--idempotency-key','smoke',code=5)['error']['code']=='conflict'
        (root/f'state/{job_id}.torrent').write_bytes(metainfo)
        cli('worker','--once')
        assert cli('status',job_id)['data']['job']['state'] == 'queued'
        cli('worker','--once')
        assert len(added)==1
        assert cli('status',job_id)['data']['job']['state'] == 'downloading'
        cli('worker','--once'); cli('worker','--once')
        assert cli('status',job_id)['data']['job']['state'] == 'ready'
        assert (root/'ebook-cli/book.epub').read_bytes()==source
        conn.execute("UPDATE jobs SET state='sending' WHERE id=?",(job_id,)); conn.commit()
        cli('worker','--once')
        assert cli('status',job_id)['data']['job']['state']=='send_uncertain'
        cli('retry',job_id,code=5)
        cli('confirm-delivery',job_id)
        cli('worker','--once')
        assert len(added)==1
        # Existing Chaptarr ownership is reported without any add/reclassification.
        owned[0]=False
        second = str(uuid.uuid4())
        conn.execute("INSERT INTO jobs(id,release_id,language,state,message_id) VALUES(?,?,'en','queued',?)",(second,'mam:2',f'<{second}@test>')); conn.commit()
        (root/f'state/{second}.torrent').write_bytes(metainfo.replace(b'aaaaa',b'bbbbb'))
        cli('worker','--once')
        assert cli('status',second)['data']['job']['state']=='existing_workflow'
        assert len(added)==1
        # Run the real HTTP server and verify auth/schema/contract boundaries.
        with socket.socket() as s:
            s.bind(('127.0.0.1',0)); port=s.getsockname()[1]
        api=subprocess.Popen([str(BIN),'--config',str(cfg),'serve','--listen',f'127.0.0.1:{port}'],stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        try:
            base=f'http://127.0.0.1:{port}'
            for _ in range(50):
                try:
                    with urllib.request.urlopen(base+'/health/live',timeout=1) as r: assert json.load(r)['live']
                    break
                except OSError: time.sleep(.1)
            else: raise AssertionError('API startup failed')
            def request(body, token, status):
                req=urllib.request.Request(base+'/v1/operations',data=json.dumps(body).encode(),headers={'Authorization':'Bearer '+token,'Content-Type':'application/json'})
                try:
                    with urllib.request.urlopen(req,timeout=10) as r: result=json.load(r); actual=r.status
                except urllib.error.HTTPError as e: actual=e.code; result=json.load(e)
                assert actual==status,(actual,status,result)
                return result
            request({'action':'jobs'},'wrong',401)
            assert len(request({'action':'jobs'},config['api_token'],200)['data']['jobs'])==2
            request({'action':'shell','command':'id'},config['api_token'],400)
            request({'action':'jobs','extra':'x'},config['api_token'],400)
        finally:
            api.terminate(); api.wait(timeout=10)
            server.shutdown(); server.server_close(); conn.close()
        cli('serve','--listen','0.0.0.0:8791',code=2)
        assert not (root/'escape').exists()
        print(json.dumps({'ok':True,'checks':['lost_add_response_reconciled','duplicate_submission_reused','source_unchanged','restart_send_uncertain','uncertain_retry_guard','existing_workflow_preserved','api_auth','api_schema','loopback_only']}))

if __name__=='__main__': main()
