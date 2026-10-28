"""Verify pinned dav1d/meson/ninja/NASM bytes; build the static software AV1
decoder (dav1d 1.5.3, Design AD) and MeshLive's C boundary.

Same rules as native_audio.py and native_encoder.py (whose helpers this
reuses): every archive is checked against its sources.lock.json SHA256 before
extraction and on every build, extracted bytes are rechecked, nothing is
installed (meson and ninja run from their pinned wheels inside .cache), static
CRT, one compiler job, all caches and outputs inside the checkout's
.cache/.artifacts. dav1d's source is not modified.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import zipfile

import native_audio as common

ROOT = common.ROOT
HIDDEN = common.HIDDEN
DAV1D = 'dav1d-1.5.3'
MESON = 'meson-1.9.1'
NINJA = 'ninja-1.13.0'
NASM = 'nasm-2.16.03'


def wheel(item, name):
    """Materializes a pinned wheel (zip) into .cache/native-tools/<name>."""
    base = ROOT / '.cache/native-tools' / name
    with zipfile.ZipFile(common.archive(item)) as stream:
        for member in stream.infolist():
            if member.is_dir():
                continue
            if (member.external_attr >> 16) & 0o170000 == 0o120000:
                raise ValueError('Unexpected build-tool archive symlink')
            common.materialize(base, member.filename, stream.read(member))
    return base


def prepare(lock):
    entry = lock['native_video']['dav1d']
    if entry['version'] != '1.5.3' or entry['filename'] != DAV1D + '.tar.xz':
        raise ValueError('dav1d pin changed without updating this script')
    source = ROOT / '.cache/native' / DAV1D
    with tarfile.open(common.archive(entry), 'r:xz') as stream:
        for member in stream.getmembers():
            if member.isdir():
                continue
            if not member.isfile() or not member.name.startswith(DAV1D + '/'):
                raise ValueError('Unexpected dav1d archive entry: ' + member.name)
            with stream.extractfile(member) as content:
                common.materialize(source, member.name[len(DAV1D) + 1:], content.read())
    nasm = ROOT / '.cache/native-tools' / NASM
    with zipfile.ZipFile(common.archive(lock['build_tools']['nasm'])) as stream:
        for member in stream.infolist():
            if member.is_dir():
                continue
            if not member.filename.startswith(NASM + '/'):
                raise ValueError('Unexpected NASM archive entry')
            common.materialize(nasm, member.filename[len(NASM) + 1:], stream.read(member))
    meson = wheel(lock['build_tools']['meson'], MESON)
    ninja = wheel(lock['build_tools']['ninja'], NINJA) / 'ninja-1.13.0.data/scripts/ninja.exe'
    if not (meson / 'mesonbuild/mesonmain.py').is_file() or not ninja.is_file():
        raise RuntimeError('Pinned meson/ninja wheels are missing their entry points')
    return source, nasm / 'nasm.exe', meson, ninja


def run(command, cwd, environment):
    result = subprocess.run(command, cwd=cwd, env=environment, capture_output=True, text=True,
                            errors='replace', creationflags=HIDDEN)
    if result.returncode:
        print((result.stdout + result.stderr)[-6000:])
        raise RuntimeError('Native build step failed: ' + ' '.join(str(c) for c in command[:4]))
    return result.stdout


def shim(environment, includes, output):
    """Compiles src/video/decode_dav1d_shim.c into meshlive_dav1d.lib (static CRT)."""
    tools = {tool: shutil.which(tool, path=environment['PATH']) for tool in ('cl.exe', 'lib.exe')}
    if not all(tools.values()):
        raise RuntimeError('Installed MSVC cl/lib not found; no installer will run')
    objects = output / 'shim'
    objects.mkdir(parents=True, exist_ok=True)
    obj = objects / 'decode_dav1d_shim.obj'
    subprocess.run([tools['cl.exe'], '/nologo', '/c', '/O2', '/MT', '/GS', '/W4', '/WX', '/guard:cf',
                    *[f'/I{i}' for i in includes], f'/Fo{obj}', str(ROOT / 'src/video/decode_dav1d_shim.c')],
                   env=environment, check=True, creationflags=HIDDEN)
    subprocess.run([tools['lib.exe'], '/nologo', f'/OUT:{output / "meshlive_dav1d.lib"}', str(obj)],
                   env=environment, check=True, creationflags=HIDDEN)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--target', choices=['x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc'], required=True)
    args = parser.parse_args()
    if args.target != 'x86_64-pc-windows-msvc':
        raise RuntimeError('ARM64 compiler/runtime are not verified on this host; no installer will run')
    lock = json.loads((ROOT / 'sources.lock.json').read_text())
    source, nasm, meson, ninja = prepare(lock)
    (ROOT / '.cache/temp').mkdir(parents=True, exist_ok=True)
    environment = common.compiler_environment()
    keys = [key for key in environment if key.upper() == 'PATH']
    value = environment.get('Path', environment.get('PATH', ''))
    for key in keys:
        del environment[key]
    environment['PATH'] = os.pathsep.join([str(nasm.parent), str(ninja.parent), value])
    environment['NINJA'] = str(ninja)
    environment['PYTHONPATH'] = str(meson)
    environment['PYTHONDONTWRITEBYTECODE'] = '1'
    output = ROOT / '.artifacts/native' / args.target / 'dav1d'
    build = output / 'build'
    meson_command = [sys.executable, '-B', '-m', 'mesonbuild.mesonmain']
    # No build-machine folder in the release exe (as native_audio.py does for
    # Opus): __FILE__ is trimmed for the source, build and checkout folders.
    environment['_CL_'] = ' '.join(f'/d1trimfile:{folder}\\' for folder in (source, build, ROOT))
    stamp = output / 'trimmed-file-paths'
    if not stamp.is_file() and build.exists():
        shutil.rmtree(build)  # built before trimming: once from clean objects
    if not (build / 'build.ninja').is_file():
        if build.exists():
            shutil.rmtree(build)
        run([*meson_command, 'setup', str(build), str(source), '--backend=ninja',
             '--buildtype=release', '--default-library=static', '-Db_vscrt=mt', '-Db_ndebug=true',
             '-Dbitdepths=8', '-Denable_asm=true', '-Denable_tools=false', '-Denable_tests=false',
             '-Denable_examples=false', '-Denable_docs=false', '-Dlogging=false',
             '-Dtestdata_tests=false', '-Dxxhash_muxer=disabled'], ROOT, environment)
    run([str(ninja), '-C', str(build), '-j', '1', 'src/libdav1d.a'], ROOT, environment)
    library = build / 'src/libdav1d.a'
    if not library.is_file():
        raise RuntimeError('Native build did not produce the static dav1d library')
    shutil.copyfile(library, output / 'dav1d.lib')
    shim(environment, [source / 'include', build / 'include'], output)
    # Archive member names keep the object paths (they never reach the exe);
    # the source folder must not appear.
    for name in ('dav1d.lib', 'meshlive_dav1d.lib'):
        data = (output / name).read_bytes().lower()
        if any(str(source).replace('\\', slash).lower().encode() in data for slash in ('\\', '/')):
            raise RuntimeError('The static library still embeds the build folder: ' + name)
    stamp.write_text('dav1d built with /d1trimfile\n')
    print('Verified dav1d 1.5.3, meson 1.9.1, ninja 1.13.0 and NASM 2.16.03 bytes; built the static-CRT '
          '8-bit AV1 decoder and its shim, one compiler job.')


if __name__ == '__main__':
    main()
