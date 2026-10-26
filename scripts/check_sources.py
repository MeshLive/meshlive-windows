"""Verify frozen app assets and unchanged vendored upstream files."""
import argparse,hashlib,json,subprocess
from pathlib import Path
root=Path(__file__).resolve().parent.parent
parser=argparse.ArgumentParser();parser.add_argument('--index',action='store_true');args=parser.parse_args()
manifest=json.loads((root/'sources.lock.json').read_text())
for name,expected in manifest['files'].items():
    path=(root/name).resolve()
    if not path.is_relative_to(root):raise ValueError('Manifest path escaped checkout')
    content=subprocess.check_output(['git','show',':'+name],cwd=root) if args.index else path.read_bytes()
    if hashlib.sha256(content).hexdigest()!=expected:raise ValueError('Source hash mismatch: '+name)
print('Verified',len(manifest['files']),'frozen files; modified renderer glue is documented separately.')
