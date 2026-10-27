"""Prepare verified sources/tools and build static Opus, inside Build.ps1's lock."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tarfile
import urllib.request
import zipfile

ROOT = Path(__file__).resolve().parent.parent
HIDDEN = subprocess.CREATE_NO_WINDOW


def archive(item):
    path = ROOT / '.cache/native-downloads' / item['filename']
    path.parent.mkdir(parents=True, exist_ok=True)
    if not path.exists():
        temporary = path.with_suffix(path.suffix + '.part')
        with urllib.request.urlopen(item['url'], timeout=30) as response, temporary.open('wb') as output:
            while block := response.read(1024 * 1024):
                output.write(block)
        verify(temporary, item['sha256'])
        temporary.rename(path)
    verify(path, item['sha256'])
    return path


def verify(path, expected):
    with path.open('rb') as stream:
        actual = hashlib.file_digest(stream, 'sha256').hexdigest()
    if actual != expected:
        raise ValueError('Pinned archive/source differs: ' + path.name)


def materialize(base, name, data):
    target = (base / name).resolve()
    if not target.is_relative_to(base.resolve()):
        raise ValueError('Archive path escaped its owned cache')
    if target.exists():
        verify(target, hashlib.sha256(data).hexdigest())
    else:
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(data)


def prepare(lock):
    source = ROOT / '.cache/native/opus-1.5.2'
    with tarfile.open(archive(lock['native_audio'])) as stream:
        for member in stream.getmembers():
            if member.isdir():
                continue
            if not member.isfile() or not member.name.startswith('opus-1.5.2/'):
                raise ValueError('Unexpected native source archive entry')
            with stream.extractfile(member) as content:
                materialize(source, member.name[len('opus-1.5.2/'):], content.read())
    tool = ROOT / '.cache/native-tools/cmake-3.30.5'
    with zipfile.ZipFile(archive(lock['build_tools']['cmake'])) as stream:
        for member in stream.infolist():
            if member.is_dir():
                continue
            if (member.external_attr >> 16) & 0o170000 == 0o120000:
                raise ValueError('Unexpected build-tool archive symlink')
            materialize(tool, member.filename, stream.read(member))
    return source, tool / 'cmake/data/bin/cmake.exe'


def compiler_environment():
    # No installation or account operation. Capture vcvars output only in memory;
    # never persist/log an environment containing personal paths or credentials.
    vswhere = Path(os.environ['ProgramFiles(x86)']) / 'Microsoft Visual Studio/Installer/vswhere.exe'
    location = subprocess.check_output([str(vswhere), '-latest', '-products', '*', '-requires',
                                       'Microsoft.VisualStudio.Component.VC.Tools.x86.x64',
                                       '-property', 'installationPath'], text=True, creationflags=HIDDEN).strip()
    if not location:
        raise RuntimeError('Installed MSVC x64 tools not found; no installer will run')
    vcvars = Path(location) / 'VC/Auxiliary/Build/vcvarsall.bat'
    # cmd's /c grammar does not understand Python list2cmdline's backslash quotes.
    # Use its outer /s quotes with the installed batch path quoted inside them.
    result = subprocess.run(f'cmd.exe /d /s /c "call "{vcvars}" x64 >nul && set"',
                            capture_output=True, text=True, errors='replace', creationflags=HIDDEN)
    if result.returncode:
        raise RuntimeError('Installed MSVC environment could not initialize')
    environment = os.environ.copy()
    for line in result.stdout.splitlines():
        if '=' in line and not line.startswith('='):
            key, value = line.split('=', 1)
            environment[key] = value
    for key in ('TEMP', 'TMP'):
        environment[key] = str(ROOT / '.cache/temp')
    for key in ('CMAKE_BUILD_PARALLEL_LEVEL', 'CL', '_CL_', 'CFLAGS', 'MAKEFLAGS'):
        environment.pop(key, None)
    return environment


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--target', choices=['x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc'], required=True)
    args = parser.parse_args()
    if args.target != 'x86_64-pc-windows-msvc':
        raise RuntimeError('ARM64 compiler/runtime are not verified on this host; no installer will run')
    lock = json.loads((ROOT / 'sources.lock.json').read_text())
    source, cmake = prepare(lock)
    environment = compiler_environment()
    # Opus asserts with __FILE__; trim it to the source-relative path so no
    # build-machine folder ends up inside the release exe.
    environment['_CL_'] = f'/d1trimfile:{source}\\'
    output = ROOT / '.artifacts/native' / args.target / 'opus'
    configure = [str(cmake), '-S', str(source), '-B', str(output), '-G', 'NMake Makefiles',
                 '-DCMAKE_BUILD_TYPE=Release', '-DOPUS_STATIC_RUNTIME=ON',
                 '-DOPUS_BUILD_SHARED_LIBRARY=OFF', '-DBUILD_SHARED_LIBS=OFF',
                 '-DOPUS_BUILD_TESTING=OFF', '-DOPUS_BUILD_PROGRAMS=OFF',
                 '-DOPUS_INSTALL_PKG_CONFIG_MODULE=OFF', '-DOPUS_INSTALL_CMAKE_CONFIG_MODULE=OFF',
                 '-DOPUS_DRED=OFF', '-DOPUS_OSCE=OFF']
    subprocess.run(configure, env=environment, check=True, creationflags=HIDDEN)
    # NMake is serial; /MP is absent and inherited parallel flags are cleared.
    # A library built before path trimming is rebuilt once from clean objects.
    stamp = output / 'trimmed-file-paths'
    build = [str(cmake), '--build', str(output), '--target', 'opus']
    if not stamp.is_file():
        build.append('--clean-first')
    subprocess.run(build, env=environment, check=True, creationflags=HIDDEN)
    if not (output / 'opus.lib').is_file():
        raise RuntimeError('Native build did not produce the pinned static library')
    # Archive member names keep the object paths (they never reach the exe);
    # the source folder must not appear.
    library = (output / 'opus.lib').read_bytes().lower()
    if any(str(source).replace('\\', slash).lower().encode() in library for slash in ('\\', '/')):
        raise RuntimeError('The static library still embeds the build folder')
    stamp.write_text('opus built with /d1trimfile\n')
    print('Verified Opus 1.5.2 source/tool bytes; built static CRT with one compiler job.')


if __name__ == '__main__':
    main()
