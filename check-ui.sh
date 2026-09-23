#!/usr/bin/env bash
# Gate for the interface: every module must parse, and the logic tests must
# pass.
#
# A syntax error in one of these files takes the whole page down before a line
# runs — no handler can catch it and nothing appears on screen, which looks
# exactly like a backend failure. That cost two debugging sessions before this
# script existed.
set -e
cd "$(dirname "$0")/ui"

for f in *.js; do
  case "$f" in *.test.js) continue;; esac
  node --experimental-vm-modules -e "
    const fs=require('fs'), vm=require('vm');
    try { new vm.SourceTextModule(fs.readFileSync('$f','utf8')); }
    catch(e){ console.error('$f: ' + e.message); process.exit(1); }
  " 2>/dev/null || { echo "FAIL parse: $f"; exit 1; }
done

# A helper deleted during a refactor leaves its callers behind, and the
# resulting ReferenceError only appears when that branch runs -- which in a UI
# can be much later. This catches it here instead.
python3 check-references.py || exit 1

node --test 2>&1 | grep -E "^ℹ (tests|pass|fail)" | sed 's/^ℹ /ui: /'
node --test >/dev/null 2>&1 || { echo "FAIL: ui tests"; exit 1; }
