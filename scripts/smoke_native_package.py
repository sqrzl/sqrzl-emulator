"""Smoke an extracted native bundle, including UI assets, from an unrelated cwd."""
from __future__ import annotations
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request


def smoke(package: Path):
    package = package.resolve()
    manifest = json.loads((package / 'manifest.json').read_text())
    for name, digest in manifest['files'].items():
        path = (package / name).resolve()
        if not path.is_relative_to(package) or hashlib.sha256(path.read_bytes()).hexdigest() != digest:
            raise ValueError(f'Invalid packaged payload: {name}')
    binary = package / 'sqrzl-emulator'
    version = subprocess.check_output([str(binary), '--version'], text=True).strip()
    if version != f"sqrzl-emulator {manifest['version']}":
        raise ValueError('Executable version differs from manifest')
    with tempfile.TemporaryDirectory(prefix='sqrzl-native-smoke-') as working:
        # Reserve distinct ports together before transferring them to the child.
        sockets = [socket.socket() for _ in range(3)]
        for listener in sockets: listener.bind(('127.0.0.1', 0))
        ports = [listener.getsockname()[1] for listener in sockets]
        environment = {key: value for key, value in os.environ.items() if not re.match(r'^(SQRZL_|AZURE_|GCS_|OCI_|SES_|SENDGRID_|TWILIO_|ACS_)', key)}
        environment.update(SQRZL_API_PORT=str(ports[0]), SQRZL_UI_PORT=str(ports[1]), SQRZL_SMTP_PORT=str(ports[2]), SQRZL_BLOBS_PATH=str(Path(working) / 'data'))
        for listener in sockets: listener.close()
        with tempfile.TemporaryFile() as log:
            child = subprocess.Popen([str(binary)], cwd=working, env=environment, stdout=log, stderr=log)
            try:
                api, ui = (f'http://127.0.0.1:{port}' for port in ports[:2])
                deadline = time.monotonic() + 30
                while True:
                    if child.poll() is not None: raise RuntimeError('Packaged server exited during startup')
                    try:
                        with urllib.request.urlopen(api + '/healthz', timeout=1) as response: health = json.load(response)
                        with urllib.request.urlopen(ui + '/admin/buckets', timeout=1) as response: index = response.read()
                        if index != (package / 'static/index.html').read_bytes(): raise ValueError('Packaged SPA was not served')
                        if health['status'] == 'ok' and health['version'] == manifest['version']: break
                    except (OSError, urllib.error.URLError): pass
                    if time.monotonic() >= deadline: raise TimeoutError('Packaged server never became ready')
                    time.sleep(.1)
                with urllib.request.urlopen(ui + '/admin/buckets') as response: index = response.read()
                if index != (package / 'static/index.html').read_bytes(): raise ValueError('Packaged SPA was not served')
                assets = re.findall(rb'(?:src|href)="(/assets/[^"?]+)', index)
                if not assets: raise ValueError('Packaged SPA has no assets')
                for asset in assets:
                    with urllib.request.urlopen(ui + asset.decode()) as response: body = response.read()
                    if body != (package / 'static' / asset.decode().lstrip('/')).read_bytes(): raise ValueError('Packaged asset mismatch')
                print(json.dumps({'version': manifest['version'], 'source_sha': manifest['source_sha'], 'target': manifest['target'], 'smoke': 'passed', 'ui_assets': len(assets)}))
            except BaseException:
                log.seek(0); print(log.read().decode(errors='replace')); raise
            finally:
                child.terminate()
                try: child.wait(timeout=10)
                except subprocess.TimeoutExpired: child.kill(); child.wait()

if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__); parser.add_argument('package', type=Path)
    smoke(parser.parse_args().package)
