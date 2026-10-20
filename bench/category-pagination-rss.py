#!/usr/bin/env python3
"""Linux fresh-process RSS probe for a prepared category-pagination fixture.
Run after category-pagination.py --prepare; only reads its saved snapshots.
"""
import argparse, json, os, selectors, subprocess, time
from pathlib import Path
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--output', required=True, type=Path)
args = parser.parse_args()
p = args.output.resolve()
manifest = json.loads((p / 'manifest.json').read_text())
rows = []
for root in ['small', 'large']:
    for binary, mode in [('before', 'legacy'), ('after', 'legacy'), ('after', 'first'), ('after', 'late')]:
        for trial in range(3):
            c = subprocess.Popen([manifest[binary], '--mcp', '--cache', '--config', str(p / 'config.toml')], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=dict(os.environ, DUTYPE_CACHE_DIR=str(p / 'cache'), RAYON_NUM_THREADS='4'))
            sel = selectors.DefaultSelector()
            sel.register(c.stdout, selectors.EVENT_READ)

            def call(**opts):
                c.stdin.write(json.dumps({'jsonrpc': '2.0', 'id': 1, 'method': 'tools/call', 'params': {'name': 'find_by_category', 'arguments': {'path': str(p / root), 'category': 'PDF', **opts}}}) + '\n')
                c.stdin.flush()
                assert sel.select(30), 'timeout'
                r = json.loads(c.stdout.readline())
                assert r['result']['isError'] == False, r
                return r['result']['structuredContent']

            def rss():
                status = dict((line.split(':', 1) for line in Path(f'/proc/{c.pid}/status').read_text().splitlines() if ':' in line))
                return {field: int(status[field].split()[0]) for field in ['VmRSS', 'VmHWM']}
            call(top=50)
            options = {'top': 50}
            if mode == 'first':
                options['paginate'] = True
            elif mode == 'late':
                page = call(top=500, paginate=True)
                while page['pagination']['has_more']:
                    token = page['pagination']['next_cursor']
                    page = call(top=500, cursor=token)
                options['cursor'] = token
            before = rss()
            for _ in range(200):
                call(**options)
            after = rss()
            c.stdin.close()
            deadline = time.monotonic() + 30
            while True:
                pid, status, usage = os.wait4(c.pid, os.WNOHANG)
                if pid:
                    break
                assert time.monotonic() < deadline, 'exit timeout'
                time.sleep(0.001)
            c.returncode = os.waitstatus_to_exitcode(status)
            assert c.returncode == 0
            assert not c.stderr.read()
            sel.close()
            rows.append({'root': root, 'binary': binary, 'mode': mode, 'trial': trial, 'before_kib': before, 'after_kib': after, 'wait4_peak_rss_kib': usage.ru_maxrss})
(p / 'rss-fresh-processes.json').write_text(json.dumps(rows, indent=2) + '\n')
print(json.dumps(rows, indent=2))
