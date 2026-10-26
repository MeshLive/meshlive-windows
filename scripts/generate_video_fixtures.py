"""Generate owned synthetic lossless media; FFmpeg is fixture tooling, never runtime.

Run explicitly under the MeshLive build mutex. Never uses capture, radio or phones.
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / '.artifacts/video-fixture-generation'
FIXTURES = ROOT / 'tests/fixtures'
OUT.mkdir(parents=True, exist_ok=True)
for key in ('TEMP', 'TMP'):
    os.environ[key] = str(ROOT / '.cache/temp')
FFMPEG = shutil.which('ffmpeg')
if not FFMPEG:
    raise SystemExit('Installed FFmpeg is required only for explicit fixture regeneration')

def run(arguments):
    return subprocess.run([FFMPEG, '-hide_banner', '-nostdin', *arguments],
                          check=True, capture_output=True, timeout=60,
                          creationflags=subprocess.CREATE_NO_WINDOW).stdout

def sha(data):
    return hashlib.sha256(data).hexdigest()

tool_hash = sha(Path(FFMPEG).read_bytes())
previous = FIXTURES / 'video-lossless.json'
if previous.exists() and json.loads(previous.read_text(encoding='utf-8'))['tool']['exe_sha256'] != tool_hash:
    raise SystemExit('Fixture tool changed; review its version/license/hash before regeneration')

raw_frames = []
for frame in range(3):
    y = bytes(16 + (x * 3 + y * 5 + frame * 17) % 220 for y in range(64) for x in range(64))
    u = bytes(16 + (x * 7 + y * 11 + frame * 13) % 225 for y in range(32) for x in range(32))
    v = bytes(16 + (x * 13 + y * 3 + frame * 29) % 225 for y in range(32) for x in range(32))
    raw_frames.append(y + u + v)
raw = b''.join(raw_frames)
raw_path = OUT / 'source.i420'
raw_path.write_bytes(raw)
version = run(['-version']).decode('utf-8').splitlines()[0]
manifest = {
    'format': 'MeshLive synthetic lossless video v1',
    'license': 'GPL-3.0-or-later; original generated pixels, no captured or third-party footage',
    'provenance': 'Generated here, not published Android codec vectors or peer interoperability evidence',
    'generator': 'scripts/generate_video_fixtures.py',
    'tool': {'version': version, 'exe_sha256': tool_hash,
             'license': 'GPL-3.0-or-later build (--enable-gpl --enable-version3); fixture-only, not bundled',
             'source': 'https://ffmpeg.org/'},
    'width': 64, 'height': 64, 'fps': 5, 'pixel_format': 'yuv420p',
    'matrix': 'bt709', 'range': 'limited',
    'frames': [{'pts_us': index * 200000, 'i420_sha256': sha(data)} for index, data in enumerate(raw_frames)],
    'codecs': {},
}
for codec, encoder, options, decoder in [
    ('vp9', 'libvpx-vp9', ['-lossless', '1', '-deadline', 'good', '-cpu-used', '0'], 'vp9'),
    ('av1', 'libaom-av1', ['-crf', '0', '-b:v', '0', '-cpu-used', '6', '-aom-params', 'lossless=1'], 'libdav1d'),
]:
    ivf = OUT / f'{codec}.ivf'
    run(['-y', '-f', 'rawvideo', '-pixel_format', 'yuv420p', '-video_size', '64x64',
         '-framerate', '5', '-color_range', 'tv', '-colorspace', 'bt709',
         '-color_primaries', 'bt709', '-color_trc', 'bt709', '-i', str(raw_path),
         '-frames:v', '3', '-an', '-c:v', encoder, '-pix_fmt', 'yuv420p',
         '-threads', '1', '-row-mt', '0', '-g', '30', *options,
         '-color_primaries', 'bt709', '-color_trc', 'bt709', '-colorspace', 'bt709',
         '-color_range', 'tv', '-f', 'ivf', str(ivf)])
    decoded = run(['-threads', '1', '-c:v', decoder, '-i', str(ivf), '-an',
                   '-pix_fmt', 'yuv420p', '-f', 'rawvideo', 'pipe:1'])
    if decoded != raw:
        raise ValueError(f'{codec} did not reconstruct every source byte losslessly')
    data = ivf.read_bytes()
    assert data[:4] == b'DKIF' and struct.unpack_from('<I', data, 24)[0] == 3
    position = 32
    packets = []
    while position < len(data):
        size, ticks = struct.unpack_from('<IQ', data, position)
        position += 12
        packet = data[position:position+size]
        assert len(packet) == size
        packets.append({'bytes': size, 'ticks': ticks, 'sha256': sha(packet)})
        position += size
    assert len(packets) == 3 and position == len(data)
    filename = f'video-{codec}.ivf'
    (FIXTURES / filename).write_bytes(data)
    manifest['codecs'][codec] = {'file': filename, 'sha256': sha(data), 'packets': packets,
                                'encoder': encoder, 'verification_decoder': decoder,
                                'encoder_options': options}
(FIXTURES / 'video-lossless.json').write_text(json.dumps(manifest, indent=2)+'\n', encoding='utf-8', newline='\n')
print('Generated VP9/AV1 fixtures; all 36864 independently decoded I420 bytes equal their synthetic sources.')
