#!/usr/bin/env python3
"""CLI integration checks against a local qBittorrent stub; never sends email."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from email.parser import BytesParser
from email.policy import default
import argparse
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
def main(binary):
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
                body = self.rfile.read(int(self.headers.get('Content-Length', '0')))
                if self.path.endswith('/login'):
                    if added:
                        self.send_response(204); self.end_headers()
                    else:
                        self.answer('Ok.')
                elif self.path.endswith('/add'):
                    headers = ('Content-Type: '+self.headers['Content-Type']+'\r\n\r\n').encode()
                    form = BytesParser(policy=default).parsebytes(headers+body)
                    added.append({part.get_param('name', header='content-disposition'): part.get_payload(decode=True)
                                  for part in form.iter_parts()})
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
            'api':{'client_token':'test-client-'+'b'*64,'requests_per_minute':12,'jobs_per_day':1},
        }
        cfg = root/'config.json'
        cfg.write_text(json.dumps(config)); cfg.chmod(0o600)
        def cli(*args, code=0):
            p = subprocess.run([str(binary),'--config',str(cfg),*args],capture_output=True,text=True,timeout=45)
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
        assert added[0]['seedingTimeLimit']==b'-2', 'New torrents must inherit the global seeding duration'
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
        # Tracker size cannot bypass the cap on the actual torrent contents.
        oversized = str(uuid.uuid4())
        conn.execute("INSERT INTO jobs(id,release_id,language,state,message_id) VALUES(?,?,'en','queued',?)",(oversized,'mam:6',f'<{oversized}@test>')); conn.commit()
        (root/f'state/{oversized}.torrent').write_bytes(metainfo.replace(b'lengthi5e',b'lengthi104857601e'))
        cli('worker','--once')
        rejected = cli('status',oversized)['data']['job']
        assert rejected['state']=='failed' and rejected['error']['code']=='download_too_large'
        assert len(added)==1
        # Run the real HTTP server and verify auth/schema/contract boundaries.
        with socket.socket() as s:
            s.bind(('127.0.0.1',0)); port=s.getsockname()[1]
        api=subprocess.Popen([str(binary),'--config',str(cfg),'serve','--listen',f'127.0.0.1:{port}'],stdout=subprocess.PIPE,stderr=subprocess.PIPE)
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
                except urllib.error.HTTPError as e:
                    actual=e.code; result=json.load(e)
                    if result['error']['code']=='rate_limited': assert 1<=int(e.headers['Retry-After'])<=60
                assert actual==status,(actual,status,result)
                return result
            request({'action':'jobs'},'wrong',401)
            assert len(request({'action':'jobs'},config['api_token'],200)['data']['jobs'])==3
            request({'action':'shell','command':'id'},config['api_token'],400)
            request({'action':'jobs','extra':'x'},config['api_token'],400)
            request({'action':'search','query':'x'*9000},config['api_token'],400)
            # Auth runs before JSON extraction, and restricted tokens cannot resend.
            req=urllib.request.Request(base+'/v1/operations',data=b'not-json',headers={'Content-Type':'application/json'})
            try:
                urllib.request.urlopen(req,timeout=10)
                raise AssertionError('Missing auth accepted')
            except urllib.error.HTTPError as e:
                assert e.code==401 and json.load(e)['error']['code']=='unauthorized'
            client=config['api']['client_token']
            for token,expected in [('wrong',401),(client,200)]:
                req=urllib.request.Request(base+'/v1/help',headers={'Authorization':'Bearer '+token})
                try:
                    with urllib.request.urlopen(req,timeout=10) as r: help_body=json.load(r); actual=r.status
                except urllib.error.HTTPError as e: actual=e.code; help_body=json.load(e)
                assert actual==expected
            help_data=help_body['data']
            assert help_data['delivery']['configured'] is True
            assert help_data['delivery']['destination_address_required_from_caller'] is False
            assert help_data['authentication']['allowed_actions']==['search','request','select','jobs','status']
            assert help_data['limits']['requests_per_minute']==12
            assert help_data['limits']['new_jobs_per_rolling_24_hours']==1
            assert conn.execute('SELECT count(*) FROM jobs').fetchone()[0]==3
            assert conn.execute('SELECT count(*) FROM api_submissions').fetchone()[0]==0
            for secret in [config['api_token'],client,config['smtp']['kindle'],config['mam']['session']]:
                assert secret not in json.dumps(help_body)
            request({'action':'jobs'},client,200)
            request({'action':'retry','job_id':job_id,'acknowledge_possible_duplicate':True},client,403)
            request({'action':'confirm_delivery','job_id':job_id},client,403)
            request({'action':'select','release_id':'mam:1','idempotency_key':'api-existing'},client,200)
            for n in (3,4,5):
                release['id']=f'mam:{n}'
                release['size_bytes']=200*1024*1024 if n==5 else len(source)
                conn.execute('INSERT INTO releases VALUES (?,?)',(release['id'],json.dumps(release)))
            conn.commit()
            body={'action':'select','release_id':'mam:3','idempotency_key':'api-new'}
            selected=request(body,client,200)['data']['job']['id']
            assert request(body,client,200)['data']['job']['id']==selected
            assert request({'action':'select','release_id':'mam:4','idempotency_key':'api-over-quota'},client,429)['error']['code']=='daily_quota_exceeded'
            assert request({'action':'select','release_id':'mam:5','idempotency_key':'api-large'},client,422)['error']['code']=='download_too_large'
            request({'action':'jobs','extra':'x'},client,400)
            for _ in range(2): request({'action':'jobs'},client,200)
            assert request({'action':'jobs'},client,429)['error']['code']=='rate_limited'
            request({'action':'jobs'},config['api_token'],200) # Separate owner rate budget.
            assert conn.execute('SELECT count(*) FROM api_submissions').fetchone()[0]==1
            assert conn.execute('SELECT count(*) FROM deliveries').fetchone()[0]==0
        finally:
            api.terminate(); api.wait(timeout=10)
            server.shutdown(); server.server_close(); conn.close()
        cli('serve','--listen','0.0.0.0:8791',code=2)
        schema=root/'openapi.json'
        subprocess.run(['python3',str(ROOT/'scripts/export-openapi.py'),'--server','https://ebook.example.com','--output',str(schema)],check=True,timeout=10)
        spec=json.loads(schema.read_text())
        assert spec['servers']==[{'url':'https://ebook.example.com'}]
        assert set(spec['paths'])=={'/v1/operations','/v1/help'}
        assert spec['paths']['/v1/help']['get']['operationId']=='ebookHelp'
        assert len(spec['components']['schemas']['Operation']['oneOf'])==5
        assert 'retry' not in spec['components']['schemas']
        assert not (root/'escape').exists()
        print(json.dumps({'ok':True,'checks':['lost_add_response_reconciled','qbittorrent_seeding_time_inherits_global','duplicate_submission_reused','source_unchanged','restart_send_uncertain','uncertain_retry_guard','existing_workflow_preserved','api_auth','api_schema','loopback_only','auth_before_json','client_scope','api_quota','api_rate_limit','download_size_limit','metainfo_size_before_add','public_openapi','authenticated_help','help_excludes_secrets','help_creates_no_jobs']}))

if __name__=='__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=ROOT/'target/debug/ebook',
                        help='Executable to test (default: target/debug/ebook)')
    args = parser.parse_args()
    main(args.binary.resolve(strict=True))
