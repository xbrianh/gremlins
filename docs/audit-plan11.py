#!/usr/bin/env python3
"""Read-only physical Rust line audit. Python 3; no third-party packages.
Usage: python3 docs/audit-plan11.py [--base REV] [--target worktree|REV]
Stdout is JSON. Git/index and Rust files are never written.
"""
import argparse
import collections
import datetime
import difflib
import hashlib
import json
import pathlib
import re
import subprocess


def git(*args):
    return subprocess.check_output(['git', *args])


def digest(data):
    return hashlib.sha256(data).hexdigest()


def tree(rev):
    paths = git('ls-tree', '-r', '--name-only', '-z', rev).decode().split('\0')
    return {p: git('show', f'{rev}:{p}') for p in paths if p.endswith('.rs')}


def worktree():
    paths = git('ls-files', '-z').decode().split('\0')
    return {p: pathlib.Path(p).read_bytes() for p in paths
            if p.endswith('.rs') and pathlib.Path(p).is_file()}


# Ordered lexer: nested comments and literals are opaque, so their braces and
# cfg-like text cannot affect item boundaries. Lifetimes are not char literals.
TOKEN = re.compile(r'''(?P<space>\s+)|(?P<line>//[^\n]*)|(?P<block>/\*)|(?P<raw>(?:br|cr|r)(?P<hash>\#*)")|(?P<string>(?:b|c)?")|(?P<char>b?'(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|.)|[^'\\\n])')|(?P<word>[A-Za-z_][A-Za-z_0-9]*)|(?P<other>.)''', re.DOTALL | re.VERBOSE)


def tokens(text):
    out, i = [], 0
    while i < len(text):
        m = TOKEN.match(text, i)
        kind, end = m.lastgroup, m.end()
        if kind == 'block':
            depth = 1
            while depth and end < len(text):
                if text.startswith('/*', end):
                    depth += 1
                    end += 2
                elif text.startswith('*/', end):
                    depth -= 1
                    end += 2
                else:
                    end += 1
        elif kind == 'raw':
            close = '"' + m.group('hash')
            pos = text.find(close, end)
            end = len(text) if pos < 0 else pos + len(close)
        elif kind == 'string':
            while end < len(text):
                if text[end] == '\\':
                    end += 2
                elif text[end] == '"':
                    end += 1
                    break
                else:
                    end += 1
        if kind not in ('space', 'line', 'block'):
            out.append((text[i:end], i, end))
        i = end
    return out


def analyze(data):
    text = data.decode('utf-8')
    ts = tokens(text)
    spans, modules = [], []
    pairs, stack = {}, []
    for i, (s, _, _) in enumerate(ts):
        if s in ('{', '[', '('):
            stack.append(i)
        elif s in ('}', ']', ')') and stack:
            j = stack.pop()
            pairs[j] = i
    for i in range(len(ts) - 5):
        if [t[0] for t in ts[i:i+6]] != ['#', '[', 'cfg', '(', 'test', ')']:
            continue
        if i+6 >= len(ts) or ts[i+6][0] != ']':
            continue
        j = i + 7
        # Skip further attributes before the gated item.
        while j+1 < len(ts) and ts[j][0] == '#' and ts[j+1][0] == '[':
            j = pairs[j+1] + 1
        start = i
        if j < len(ts) and ts[j][0] == '!' and j == i+7:
            continue
        k = j
        while k < len(ts):
            s = ts[k][0]
            if s in (';', ','):
                spans.append((ts[start][1], ts[k][2]))
                break
            if s == '{':
                spans.append((ts[start][1], ts[pairs[k]][2]))
                break
            if s in ('[', '('):
                k = pairs[k]
            k += 1
    # Inner crate cfg(test), matched as tokens, never inside comments/strings.
    if any([t[0] for t in ts[i:i+8]] == ['#', '!', '[', 'cfg', '(', 'test', ')', ']'] for i in range(len(ts))):
        spans = [(0, len(text))]
    for i in range(len(ts)-2):
        if ts[i][0] == 'mod' and ts[i+2][0] == ';':
            inside = any(a <= ts[i][1] < b for a, b in spans)
            # Explicit #[path] resolves relative to the containing source file.
            # Walk adjacent outer attributes; they may precede #[cfg(test)].
            j, explicit = i-1, None
            while j >= 0 and ts[j][0] == ']':
                starts = [a for a, b in pairs.items() if b == j]
                if not starts:
                    break
                a = starts[0]
                attr = [t[0] for t in ts[a+1:j]]
                if len(attr) == 3 and attr[:2] == ['path', '=']:
                    explicit = json.loads(attr[2])
                j = a-2
            modules.append((ts[i+1][0], explicit, inside))
    labels = [False] * len(text.splitlines())
    for a, b in spans:
        first = text.count('\n', 0, a)
        last = text.count('\n', 0, max(a, b-1))
        for n in range(first, min(last+1, len(labels))):
            labels[n] = True
    return labels, modules


def classify(files):
    parsed = {p: analyze(b) for p, b in files.items()}
    whole = {p for p in files if 'tests' in pathlib.PurePosixPath(p).parts}
    # Follow external modules to include test-support source files without a
    # test filename. Both standard module layouts are supported.
    changed = True
    while changed:
        changed = False
        for p, (_, mods) in parsed.items():
            path = pathlib.PurePosixPath(p)
            folder = path.parent if path.name in ('lib.rs', 'main.rs', 'mod.rs') else path.with_suffix('')
            for name, explicit, gated in mods:
                if not (gated or p in whole):
                    continue
                candidates = (str(path.parent / explicit),) if explicit else (str(folder / (name + '.rs')), str(folder / name / 'mod.rs'))
                for candidate in candidates:
                    if candidate in files and candidate not in whole:
                        whole.add(candidate)
                        changed = True
    return {p: [True]*len(labels) if p in whole else labels for p, (labels, _) in parsed.items()}


def manifest(files):
    return {p: digest(b) for p, b in sorted(files.items())}


def measure(old, new):
    oc, nc = classify(old), classify(new)
    rows = {}
    for p in sorted(old.keys() | new.keys()):
        a, b = old.get(p, b'').decode().splitlines(), new.get(p, b'').decode().splitlines()
        al, bl = oc.get(p, []), nc.get(p, [])
        row = dict(old_prod=al.count(False), old_test=al.count(True), new_prod=bl.count(False), new_test=bl.count(True), add_prod=0, del_prod=0, add_test=0, del_test=0)
        # Deterministic Python difflib, NOT Git's hunk heuristic.
        for tag, i, j, k, l in difflib.SequenceMatcher(None, a, b, autojunk=False).get_opcodes():
            if tag in ('delete', 'replace'):
                row['del_test'] += al[i:j].count(True)
                row['del_prod'] += al[i:j].count(False)
            if tag in ('insert', 'replace'):
                row['add_test'] += bl[k:l].count(True)
                row['add_prod'] += bl[k:l].count(False)
        rows[p] = row
    def total(items):
        items = list(items)
        return {k: sum(r[k] for r in items) for k in next(iter(rows.values()))}
    crates = sorted({ '/'.join(p.split('/')[:2]) if p.startswith('crates/') else '(other)' for p in rows })
    return {'workspace': total(rows.values()), 'crates': {c: total(r for p, r in rows.items() if (p.startswith(c+'/') if c != '(other)' else not p.startswith('crates/'))) for c in crates}, 'changed_files': {p: r for p, r in rows.items() if old.get(p) != new.get(p)}, 'deleted_files': sorted(old.keys()-new.keys()), 'file_count_old': len(old), 'file_count_new': len(new)}


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--base', default='45a8bd545')
    ap.add_argument('--target', default='worktree')
    args = ap.parse_args()
    head = git('rev-parse', 'HEAD').decode().strip()
    base = git('rev-parse', args.base).decode().strip()
    old = tree(base)
    new = worktree() if args.target == 'worktree' else tree(args.target)
    hashes = manifest(new)
    if args.target == 'worktree' and hashes != manifest(worktree()):
        raise SystemExit('Rust files changed while capturing: retry')
    if head != git('rev-parse', 'HEAD').decode().strip():
        raise SystemExit('HEAD changed while capturing: retry')
    result = {'captured_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(), 'base': base, 'head': head, 'target': args.target, 'rust_snapshot_sha256': digest(json.dumps(hashes, sort_keys=True, separators=(',', ':')).encode()), 'rust_manifest': hashes, **measure(old, new)}
    print(json.dumps(result, indent=2, sort_keys=True))


if __name__ == '__main__':
    main()
