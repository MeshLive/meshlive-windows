"""Inspect normal and delay DLL imports without loading the executable."""
import argparse, json, struct
from pathlib import Path

def inspect(path):
    data=Path(path).read_bytes()
    def u16(p): return struct.unpack_from('<H',data,p)[0]
    def u32(p): return struct.unpack_from('<I',data,p)[0]
    pe=u32(0x3c)
    if data[:2]!=b'MZ' or data[pe:pe+4]!=b'PE\0\0': raise ValueError('Not a PE image')
    machine=u16(pe+4); sections=u16(pe+6); optional=pe+24
    if u16(optional)!=0x20b: raise ValueError('Expected PE32+ executable')
    directories=optional+112; table=optional+u16(pe+20)
    ranges=[]
    for i in range(sections):
        p=table+i*40; ranges.append((u32(p+12),max(u32(p+8),u32(p+16)),u32(p+20)))
    def offset(rva):
        for start,size,raw in ranges:
            if start<=rva<start+size:return raw+rva-start
        raise ValueError('Unmapped RVA')
    def name(rva):
        p=offset(rva);end=data.index(0,p);return data[p:end].decode('ascii').lower()
    imports=set()
    for directory,stride,name_offset in [(1,20,12),(13,32,4)]:
        rva=u32(directories+directory*8)
        if not rva:continue
        p=offset(rva)
        while any(data[p:p+stride]):
            if directory==13 and u32(p)&1==0:raise ValueError('Unexpected VA-based delay descriptor')
            imports.add(name(u32(p+name_offset)));p+=stride
    forbidden=[n for n in imports if n.startswith(('vcruntime','msvcp','msvcr','webview','microsoft.windowsappsdk','winrt.runtime')) or n in {'ucrtbase.dll','d3d12.dll','d3d12core.dll','dxcompiler.dll','hostfxr.dll','coreclr.dll'}]
    return {'machine':hex(machine),'bytes':len(data),'imports':sorted(imports),'forbidden_imports':forbidden,'scope':'PE static/delay imports only; OS COM activation and future runtime paths are not proved'}

if __name__=='__main__':
    parser=argparse.ArgumentParser();parser.add_argument('executable');parser.add_argument('--output');args=parser.parse_args()
    result=inspect(args.executable);serialized=json.dumps(result,indent=2)+'\n'
    if args.output:Path(args.output).write_text(serialized,encoding='utf-8')
    print(serialized)
    if result['forbidden_imports']:raise SystemExit(1)
