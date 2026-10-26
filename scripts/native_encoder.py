"""Verify pinned libaom/libvpx/NASM/make bytes; build the static software
AV1 (libaom) and VP9 (libvpx) encoders.

Same rules as native_audio.py (whose helpers this reuses): pinned archives with
SHA256 checked before extraction and every build, extracted bytes rechecked,
no installer, static CRT, one compiler job (NMake is serial), all caches and
outputs inside the checkout's .cache/.artifacts. Perl comes from the installed
Git for Windows (libaom's RTCD generator); nothing is installed.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import zipfile

import native_audio as common

ROOT = common.ROOT
HIDDEN = common.HIDDEN
AOM = 'libaom-3.14.1'
VPX = 'libvpx-1.17.0'
MAKE = 'make-4.4.1'
BASH = Path(os.environ.get('ProgramFiles', 'C:/Program Files')) / 'Git/usr/bin/bash.exe'


def prepare(lock):
    source = ROOT / '.cache/native' / AOM
    with tarfile.open(common.archive(lock['native_video']['libaom'])) as stream:
        for member in stream.getmembers():
            if member.isdir():
                continue
            if not member.isfile() or not member.name.startswith(AOM + '/'):
                raise ValueError('Unexpected libaom archive entry')
            with stream.extractfile(member) as content:
                common.materialize(source, member.name[len(AOM) + 1:], content.read())
    nasm = ROOT / '.cache/native-tools/nasm-2.16.03'
    with zipfile.ZipFile(common.archive(lock['build_tools']['nasm'])) as stream:
        for member in stream.infolist():
            if member.is_dir():
                continue
            if not member.filename.startswith('nasm-2.16.03/'):
                raise ValueError('Unexpected NASM archive entry')
            common.materialize(nasm, member.filename[len('nasm-2.16.03/'):], stream.read(member))
    vpx = ROOT / '.cache/native' / VPX
    with tarfile.open(common.archive(lock['native_video']['libvpx'])) as stream:
        for member in stream.getmembers():
            if member.isdir():
                continue
            if not member.isfile() or not member.name.startswith(VPX + '/'):
                raise ValueError('Unexpected libvpx archive entry')
            with stream.extractfile(member) as content:
                common.materialize(vpx, member.name[len(VPX) + 1:], content.read())
    make = ROOT / '.cache/native-tools' / MAKE
    with zipfile.ZipFile(common.archive(lock['build_tools']['make'])) as stream:
        for member in stream.infolist():
            if member.is_dir():
                continue
            common.materialize(make, member.filename, stream.read(member))
    cmake = ROOT / '.cache/native-tools/cmake-3.30.5/cmake/data/bin/cmake.exe'
    if not cmake.is_file():
        common.prepare(lock)  # Verifies and materializes the pinned CMake wheel.
    return source, nasm / 'nasm.exe', cmake, vpx, make / 'bin/make.exe'


def perl():
    for candidate in [shutil.which('perl'),
                      Path(os.environ.get('ProgramFiles', 'C:/Program Files')) / 'Git/usr/bin/perl.exe']:
        if candidate and Path(candidate).is_file():
            return Path(candidate)
    raise RuntimeError('Perl (installed Git for Windows) not found; no installer will run')


def shim(environment, name, includes, output):
    """Compiles src/video/encode_<name>_shim.c into meshlive_<name>.lib (static CRT)."""
    tools = {tool: shutil.which(tool, path=environment['PATH']) for tool in ('cl.exe', 'lib.exe')}
    if not all(tools.values()):
        raise RuntimeError('Installed MSVC cl/lib not found; no installer will run')
    objects = output / 'shim'
    objects.mkdir(parents=True, exist_ok=True)
    obj = objects / f'encode_{name}_shim.obj'
    subprocess.run([tools['cl.exe'], '/nologo', '/c', '/O2', '/MT', '/GS', '/W4', '/WX', '/guard:cf',
                    *[f'/I{i}' for i in includes], f'/Fo{obj}', str(ROOT / f'src/video/encode_{name}_shim.c')],
                   env=environment, check=True, creationflags=HIDDEN)
    subprocess.run([tools['lib.exe'], '/nologo', f'/OUT:{output / f"meshlive_{name}.lib"}', str(obj)],
                   env=environment, check=True, creationflags=HIDDEN)


def run(command, cwd, environment):
    """Runs a build step with piped output (MSYS tools under make misbehave on
    inherited redirected handles); shows the tail when it fails."""
    result = subprocess.run(command, cwd=cwd, env=environment, capture_output=True, text=True,
                            errors='replace', creationflags=HIDDEN)
    if result.returncode:
        print((result.stdout + result.stderr)[-4000:])
        raise RuntimeError('Native build step failed: ' + Path(command[0]).name)
    return result.stdout


def build_vpx(environment, source, make, output):
    """libvpx's supported Windows flow: configure (installed Git bash) for the
    VS2022 target, `make target=libs vpx.vcxproj` to generate the project and RTCD headers,
    then MSBuild of the Release library only, one job."""
    # Next to its source: the project generator needs a short relative source
    # path (absolute paths overflow the command line; deep relative ones gave an
    # empty project). Outputs are copied into `output`.
    build = source.parent / f'build-vpx-{output.parent.name}'
    build.mkdir(parents=True, exist_ok=True)
    output.mkdir(parents=True, exist_ok=True)
    if not BASH.is_file():
        raise RuntimeError('Installed Git bash not found; no installer will run')
    environment = dict(environment)
    environment['PATH'] = os.pathsep.join([str(make.parent), str(BASH.parent), environment['PATH']])
    if not (build / 'vpx.vcxproj').is_file():
        # Native make cannot read MSYS /d/... paths: relative source path.
        run([str(BASH), '-c', f'../{source.name}/configure --target=x86_64-win64-vs17 --enable-static-msvcrt '
             '--disable-vp8 --disable-vp9-decoder --enable-vp9-encoder --enable-realtime-only '
             '--disable-examples --disable-tools --disable-docs --disable-unit-tests '
             '--disable-webm-io --disable-libyuv --as=nasm'], build, environment)
        # make generates the prerequisites (RTCD headers, vpx.def, config.asm), but
        # the project generator's ~13 KB argument list is truncated between native
        # make and sh (files silently dropped). Run that exact command, as printed
        # by `make -n`, from a script file instead.
        run([str(make), 'target=libs', 'vpx.vcxproj'], build, environment)
        (build / 'vpx.vcxproj').unlink()
        printed = run([str(make), '-n', 'target=libs', 'vpx.vcxproj'], build, environment)
        (build / 'generate-vcxproj.sh').write_text(printed, encoding='utf-8', newline='\n')
        run([str(BASH), 'generate-vcxproj.sh'], build, environment)
        project = (build / 'vpx.vcxproj').read_text(encoding='utf-8', errors='replace')
        for needed in ('vpx_mem', 'vpx_scale', 'vpx_util', 'vpx_ports'):
            if needed not in project:
                raise RuntimeError('Generated libvpx project is missing ' + needed)
    msbuild = shutil.which('msbuild.exe', path=environment['PATH'])
    if not msbuild:
        raise RuntimeError('Installed MSBuild not found; no installer will run')
    run([msbuild, 'vpx.vcxproj', '/nologo', '/m:1', '/v:minimal', '/p:Configuration=Release', '/p:Platform=x64'],
        build, environment)
    library = build / 'x64/Release/vpxmt.lib'
    if not library.is_file():
        raise RuntimeError('Native build did not produce the static libvpx library')
    shutil.copyfile(library, output / 'vpx.lib')
    shim(environment, 'vpx', [source, build], output)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--target', choices=['x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc'], required=True)
    args = parser.parse_args()
    if args.target != 'x86_64-pc-windows-msvc':
        raise RuntimeError('ARM64 compiler/runtime are not verified on this host; no installer will run')
    lock = json.loads((ROOT / 'sources.lock.json').read_text())
    source, nasm, cmake, vpx_source, make = prepare(lock)
    environment = common.compiler_environment()
    # libaom's configure searches PATH for nasm (it unsets an explicit compiler path).
    # vcvars reports "Path" while os.environ.copy() holds "PATH": keep exactly
    # one key (the vcvars value) so the child sees NASM regardless of the shell.
    keys = [key for key in environment if key.upper() == 'PATH']
    value = environment.get('Path', environment.get('PATH', ''))
    for key in keys:
        del environment[key]
    environment['PATH'] = str(nasm.parent) + os.pathsep + value
    output = ROOT / '.artifacts/native' / args.target / 'aom'
    build = output / 'build'
    configure = [str(cmake), '-S', str(source), '-B', str(build), '-G', 'NMake Makefiles',
                 '-DCMAKE_BUILD_TYPE=Release', '-DCMAKE_POLICY_DEFAULT_CMP0091=NEW',
                 '-DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded',
                 '-DENABLE_NASM=ON',
                 f'-DPERL_EXECUTABLE={perl().as_posix()}',
                 '-DBUILD_SHARED_LIBS=OFF', '-DCONFIG_AV1_ENCODER=1', '-DCONFIG_AV1_DECODER=0',
                 '-DCONFIG_REALTIME_ONLY=0', '-DENABLE_APPS=0', '-DENABLE_DOCS=0',
                 '-DENABLE_EXAMPLES=0', '-DENABLE_TESTDATA=0', '-DENABLE_TESTS=0',
                 '-DENABLE_TOOLS=0', '-DCONFIG_WEBM_IO=0']
    subprocess.run(configure, env=environment, check=True, creationflags=HIDDEN)
    subprocess.run([str(cmake), '--build', str(build), '--target', 'aom'], env=environment,
                   check=True, creationflags=HIDDEN)
    library = build / 'aom.lib'
    if not library.is_file():
        raise RuntimeError('Native build did not produce the static libaom library')
    # The narrow C boundary (src/video/encode_aom_shim.c), same static CRT.
    shim(environment, 'aom', [source, build], output)
    shutil.copyfile(library, output / 'aom.lib')
    build_vpx(environment, vpx_source, make, ROOT / '.artifacts/native' / args.target / 'vpx')
    print('Verified libaom 3.14.1, libvpx 1.17.0, NASM 2.16.03 and make 4.4.1 bytes; built static-CRT AV1/VP9 encoders and shims, one compiler job.')


if __name__ == '__main__':
    main()
