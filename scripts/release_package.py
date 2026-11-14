"""Build a complete, checksummed native distribution from qualified build outputs."""
from __future__ import annotations

import argparse
import hashlib
import io
import json
from pathlib import Path
import re
import tarfile


def package(binary: Path, ui: Path, output: Path, version: str, source_sha: str, target: str) -> Path:
    if not re.fullmatch(r'[0-9a-f]{40}', source_sha):
        raise ValueError('A full source commit SHA is required')
    if not all(re.fullmatch(r'[a-zA-Z0-9_.-]+', value) for value in (version, target)):
        raise ValueError('Invalid version or target')
    if not binary.is_file() or binary.is_symlink() or not (ui / 'index.html').is_file():
        raise ValueError('Executable and built UI index.html are required')
    paths = sorted(ui.rglob('*'))
    if any(path.is_symlink() for path in paths):
        raise ValueError('UI symlinks are not allowed in a distribution')
    files = {'sqrzl-emulator': binary.read_bytes()}
    files.update({f'static/{path.relative_to(ui).as_posix()}': path.read_bytes() for path in paths if path.is_file()})
    files['README.txt'] = (
        'Sqrzl Emulator\n\nRun ./sqrzl-emulator. API: http://localhost:9000; admin UI: http://localhost:9001; SMTP: localhost:2525.\n'
        'Keep static/ beside the executable. Set SQRZL_BLOBS_PATH to a writable persistent data directory.\n'
        'Local authentication is disabled unless configured. See https://github.com/sqrzl/sqrzl-emulator for configuration and compatibility limits.\n'
        'This is an emulator; cloud IAM, real message delivery and real KMS encryption are not provided.\n'
    ).encode()
    manifest = {'schema_version': 1, 'version': version, 'source_sha': source_sha, 'target': target,
                'files': {name: hashlib.sha256(content).hexdigest() for name, content in sorted(files.items())}}
    files['manifest.json'] = (json.dumps(manifest, indent=2) + '\n').encode()
    name = f'sqrzl-emulator-{version}-{target}'
    output.mkdir(parents=True, exist_ok=True)
    archive = output / f'{name}.tar.gz'
    with tarfile.open(archive, 'w:gz') as bundle:
        for relative, content in sorted(files.items()):
            member = tarfile.TarInfo(f'{name}/{relative}')
            member.size = len(content); member.mode = 0o755 if relative == 'sqrzl-emulator' else 0o644
            bundle.addfile(member, io.BytesIO(content))
    archive.with_suffix(archive.suffix + '.sha256').write_text(f'{hashlib.sha256(archive.read_bytes()).hexdigest()}  {archive.name}\n')
    return archive


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    for argument in ('binary', 'ui', 'output'):
        parser.add_argument('--' + argument, required=True, type=Path)
    for argument in ('version', 'source-sha', 'target'):
        parser.add_argument('--' + argument, required=True)
    arguments = parser.parse_args()
    print(package(arguments.binary, arguments.ui, arguments.output, arguments.version, arguments.source_sha, arguments.target))
