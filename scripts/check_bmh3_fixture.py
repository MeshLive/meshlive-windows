"""Compare frozen JSON vectors with the read-only Android Kotlin fixture; no build."""
import argparse
import json
import re
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument('android_kotlin_fixture', type=Path)
args = parser.parse_args()
root = Path(__file__).resolve().parent.parent
vectors = json.loads((root / 'tests/fixtures/bmh3-vectors.json').read_text())['vectors']
kotlin = args.android_kotlin_fixture.read_text(encoding='utf-8')
blocks = re.split(r'\bVector\(\s*name\s*=', kotlin)[1:]
by_name = {re.match(r'\s*"([^"]+)"', block)[1]: block for block in blocks}
if set(by_name) != {v['name'] for v in vectors}:
    raise ValueError('Scenario names differ')
messages = traces = 0
for vector in vectors:
    block = by_name[vector['name']]
    for field in ('messages', 'trace'):
        section = re.search(r'\b' + field + r'\s*=\s*mapOf\((.*?)\)', block, re.S)
        actual = dict(re.findall(r'"([^"]+)"\s+to\s+"([^"]*)"', section[1]))
        expected = vector['messages'] if field == 'messages' else {
            prefix + '_' + key: value
            for prefix, name in [('i', 'initiator_trace'), ('r', 'responder_trace')]
            for key, value in vector[name].items()
        }
        if actual != expected:
            raise ValueError(vector['name'] + ': ' + field + ' differs')
        if field == 'messages': messages += len(expected)
        else: traces += len(expected)
    for kotlin_name, json_name in [('bms1FirstRecordI2r', 'bms1_first_record_i2r'),
                                   ('bms1FirstRecordR2i', 'bms1_first_record_r2i')]:
        actual = re.search(kotlin_name + r'\s*=\s*"([^"]+)"', block)[1]
        if actual != vector[json_name]: raise ValueError('First record differs')
print(f'Exact Android fixture match: {len(vectors)} scenarios, {messages} messages, '
      f'{traces} trace values and {2 * len(vectors)} first records. No Android test executed.')
