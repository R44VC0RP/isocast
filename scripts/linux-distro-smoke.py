#!/usr/bin/env python3
"""Check the distro codec with no Cisco download, capture devices or hardware encoder."""

import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time


def cli(*args, check=True):
    result = subprocess.run(["target/debug/isocast", *args], text=True, capture_output=True, timeout=60)
    print(result.stdout, result.stderr, flush=True)
    if check:
        result.check_returncode()
    return result


def smoke(distro, root):
    os.environ.update(HOME=str(root), ISOCAST_SETUP=str(root / 'live.toml'),
                      ISOCAST_SOCKET=str(root / 'engine.sock'), ISOCAST_CALIBRATION='off')
    os.environ.pop('ISOCAST_OPENH264', None)
    setup = root / 'pattern.toml'
    setup.write_text(f'''version = 1
name = "Distro smoke"
canvas = {{ width = 320, height = 180, fps = 30 }}
[recording]
folder = "{root}/recordings"
program = {{ encoder = {{ codec = "h264" }}, audio = ["tone"] }}
[[sources]]
id = "bars"
kind = "test_pattern"
width = 320
height = 180
fps = 30
[[sources]]
id = "tone"
kind = "tone"
[[scenes]]
id = "main"
items = [{{ source = "bars", rect = {{ x = 0, y = 0, width = 320, height = 180 }} }}]
''')
    doctor = cli('doctor', '--json', check=False)
    report = json.loads(doctor.stdout)
    checks = report['checks']
    assert all(c['status'] in ('ok', 'warning', 'problem') and c['detail'] for c in checks), checks
    assert doctor.returncode == int(any(c['status'] == 'problem' for c in checks)), doctor.returncode
    assert report['machine']['hardware_encoders'] == [], report['machine']
    codec, = (c for c in checks if c['name'] == 'codec: OpenH264')
    if distro == 'archlinux':
        assert codec['status'] == 'ok', codec
        provider = re.fullmatch(r"your system's OpenH264 [\d.]+ \((/[^)]+)\)", codec['detail'])
        assert provider, codec
        owner = subprocess.check_output(['pacman', '-Qoq', Path(provider[1]).resolve()], text=True).strip()
        assert owner == 'openh264', owner
    else:
        assert distro == 'fedora' and codec['status'] == 'problem' and 'noopenh264' in codec['detail'], codec

    start = cli('start', setup, '--json', check=False)
    if distro == 'fedora' and start.returncode == 1:
        assert re.search(r'openh264', start.stderr, re.I), 'recording refusal did not identify the codec'
        return
    start.check_returncode()
    time.sleep(3)
    cli('stop', '--wait', '--json')
    path, = json.loads(start.stdout)['files']
    streams = json.loads(subprocess.check_output([
        'ffprobe', '-v', 'error', '-count_frames', '-show_entries', 'stream=codec_name,nb_read_frames',
        '-of', 'json', path], text=True))['streams']
    print('Decoded streams:', streams)
    counts = {s['codec_name']: int(s['nb_read_frames']) for s in streams}
    assert counts['h264'] >= 60 and counts['aac'] > 0, counts


if __name__ == '__main__':
    with tempfile.TemporaryDirectory() as tmp:
        try:
            smoke(sys.argv[1], Path(tmp))
        finally:
            cli('engine', 'shutdown', check=False)
