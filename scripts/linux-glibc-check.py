"""Check every ELF in the staged portable tarball, including non-executable libraries."""

from pathlib import Path
import os
import re
import subprocess
import sys

root = Path(sys.argv[1])
helper = root / 'isocast-browser'
checked_helper = False
count = 0
failed = False
for path in sorted(root.rglob('*')):
    if not path.is_file():
        continue
    with path.open('rb') as f:
        if f.read(4) != b'\x7fELF':
            continue
    count += 1
    output = subprocess.check_output(['readelf', '--wide', '--version-info', path],
                                     env={**os.environ, 'LC_ALL': 'C'}, text=True)
    # Include weak version nodes: a weak symbol need not have a weak VERNEED entry.
    versions = [tuple(map(int, v.split('.'))) for v in
                re.findall(r'Name: GLIBC_(\d+(?:\.\d+)+)\b', output)]
    highest = max(versions, default=())
    print(f'{path}: GLIBC_{".".join(map(str, highest))}' if highest else f'{path}: no numeric GLIBC requirements')
    limit = (2, 36)
    if path == helper:
        if path.is_symlink():
            sys.exit(f'{helper} must be the actual optional helper, not a symlink')
        needed = subprocess.check_output(['readelf', '--wide', '--dynamic', path],
                                         env={**os.environ, 'LC_ALL': 'C'}, text=True)
        if not re.search(r'\(NEEDED\).*\[libwebkitgtk-6\.0\.so\.4\]', needed):
            sys.exit(f'{helper} does not link WebKitGTK 6')
        checked_helper = True
        limit = (2, 39)
    if highest > limit:
        print(f'{path} exceeds glibc {limit[0]}.{limit[1]}', file=sys.stderr)
        failed = True
if not checked_helper or failed:
    sys.exit(f'Portable tarball ABI check failed ({count} ELFs, browser helper checked: {checked_helper})')
