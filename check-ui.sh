#!/usr/bin/env bash
# Parses every UI module. A syntax error in one of these takes the whole page
# down before a single line runs — no handler can catch it and nothing appears
# on screen, which is indistinguishable from a backend failure. Cheap to check,
# expensive to debug.
set -e
cd "$(dirname "$0")/ui"
for f in *.js; do
  node --experimental-vm-modules -e "
    const fs=require('fs'), vm=require('vm');
    try { new vm.SourceTextModule(fs.readFileSync('$f','utf8')); }
    catch(e){ console.error('$f: ' + e.message); process.exit(1); }
  " 2>/dev/null || { echo "FAIL $f"; exit 1; }
done
echo "ui: all modules parse"
