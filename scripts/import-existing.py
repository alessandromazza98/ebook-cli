#!/usr/bin/env python3
"""Read existing stack settings without changing its databases or logging secrets."""
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import uuid

root = Path(__file__).resolve().parents[1]
destination = Path(sys.argv[1]) if len(sys.argv) > 1 else root / "config.json"

def rows(path, table):
    with sqlite3.connect(f"file:{path}?mode=ro", uri=True) as connection:
        connection.row_factory = sqlite3.Row
        return [dict(r) for r in connection.execute(f'SELECT * FROM "{table}"')]

def main():
    mam = next(json.loads(r["Settings"]) for r in rows('/opt/chaptarr/config/chaptarr.db', 'Indexers') if r['Implementation'] == 'MyAnonaMouse')
    qb = next(json.loads(r["Settings"]) for r in rows('/opt/chaptarr/config/chaptarr.db', 'DownloadClients') if r['Implementation'] == 'QBittorrent')
    mail = rows('/opt/calibre-web-automated/config/app.db', 'settings')[0]
    kindle = {r['kindle_mail'] for r in rows('/opt/calibre-web-automated/config/app.db', 'user') if r.get('kindle_mail')}
    if len(kindle) != 1 or mail['mail_server_type'] != 0:
        raise ValueError('Exactly one Kindle destination and ordinary SMTP configuration required')
    # Decrypt inside CWA using its installed cryptography library; capture, never log.
    code = """import sqlite3; from cryptography.fernet import Fernet; from pathlib import Path
c=sqlite3.connect('file:/config/app.db?mode=ro',uri=True)
p=c.execute('select mail_password_e from settings').fetchone()[0]
print(Fernet(Path('/config/.key').read_bytes()).decrypt(p).decode(),end='')
"""
    password = subprocess.run(['sudo', '-n', 'docker', 'exec', 'calibre-web-automated', 'python3', '-c', code], check=True, capture_output=True, timeout=30).stdout.decode()
    cfg = {
        'state_dir': str(root/'runtime/state'), 'processing_dir': str(root/'runtime/processing'),
        'mam': {'base_url': mam['baseUrl'], 'session': mam['mamId'], 'vip': mam.get('isVip', False)},
        'qbittorrent': {'url':os.environ.get('EBOOK_QBITTORRENT_URL','http://127.0.0.1:8080'), 'username':qb['username'], 'password':qb['password'], 'category':'ebook-cli', 'remote_dir':'/downloads/books/ebook-cli','local_dir':'/data/downloads/books/ebook-cli'},
        'smtp': {'host':mail['mail_server'], 'port':mail['mail_port'], 'tls': {1:'starttls',2:'tls'}[mail['mail_use_ssl']], 'username':mail['mail_login'], 'password':password, 'from':mail['mail_from'], 'kindle':next(iter(kindle))},
        'api_token':str(uuid.uuid4())+str(uuid.uuid4()),
    }
    fd = os.open(destination, os.O_WRONLY|os.O_CREAT|os.O_EXCL, 0o600)
    with os.fdopen(fd, 'w') as f:
        json.dump(cfg, f, indent=2)
        f.flush()
        os.fsync(f.fileno())
    print(json.dumps({'ok':True,'config_created':str(destination)}))

if __name__ == '__main__':
    try:
        main()
    except Exception as exc:
        # Exceptions from credential decryption can contain private payloads.
        print(json.dumps({'ok':False,'error':'Settings import failed', 'failure_class':type(exc).__name__}))
        sys.exit(1)
