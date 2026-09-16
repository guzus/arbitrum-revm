#!/usr/bin/env python3
"""Fail if the pinned handler methods mirrored by phase timing have drifted.

Pass the resolved revm-handler crate directory, e.g. a Cargo registry checkout.
This source check supplements execution differential tests; it does not prove parity.
"""
import argparse
import hashlib
import json
from pathlib import Path
import tomllib

def method(text, name):
    start = text.index('    fn ' + name + '(')
    brace = text.index('{', start)
    depth, end = 1, brace + 1
    while depth:
        depth += (text[end] == '{') - (text[end] == '}')
        end += 1
    return text[start:end]

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('crate', type=Path)
    args = parser.parse_args()
    expected = json.loads((Path(__file__).resolve().parents[1] / 'docs/phase-timing-upstream.json').read_text())
    package = tomllib.loads((args.crate / 'Cargo.toml').read_text())['package']
    if package['name'] != expected['package'] or package['version'] != expected['version']:
        raise SystemExit('upstream package/version mismatch; review orchestration before updating fingerprints')
    for key, digest in expected['method_sha256'].items():
        source, name = key.split(':')
        body = method((args.crate / 'src' / source).read_text(), name)
        if hashlib.sha256(body.encode()).hexdigest() != digest:
            raise SystemExit(f'{key} changed; review ordering/error behavior and differential tests')
    print('All three pinned upstream orchestration methods match.')

if __name__ == '__main__':
    main()
