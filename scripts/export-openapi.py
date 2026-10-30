#!/usr/bin/env python3
"""Export the restricted client's OpenAPI contract for a deployment."""
import argparse
import json
from pathlib import Path
from urllib.parse import urlsplit

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--server', required=True, help='Public HTTPS origin')
parser.add_argument('--output', required=True, type=Path)
args = parser.parse_args()
url = urlsplit(args.server)
if (url.scheme != 'https' or not url.hostname or url.username or url.password
        or url.query or url.fragment or url.path not in ('', '/')):
    parser.error('--server must be an HTTPS origin without credentials, path or query')
spec = json.loads((Path(__file__).resolve().parents[1] / 'openapi.json').read_text())
spec['servers'] = [{'url': args.server.rstrip('/')}]
spec['paths'].pop('/health/live')
schemas = spec['components']['schemas']
for name in ('retry', 'confirm_delivery'):
    del schemas[name]
schemas['Operation']['oneOf'] = [op for op in schemas['Operation']['oneOf']
                               if op['$ref'].split('/')[-1] in schemas]
spec['info']['title'] = 'Ebook Client API'
spec['info']['description'] += ' This client contract permits search, request, select, jobs and status only.'
args.output.write_text(json.dumps(spec, indent=2) + '\n')
