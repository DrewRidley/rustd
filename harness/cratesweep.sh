#!/bin/bash
# cratesweep.sh <out-dir> <name-version>...: vendors each crate and runs its own integration
# tests against rustd, two crates at a time. One log and one json per crate in <out-dir>.
here=$(cd "$(dirname "$0")" && pwd)
out=$1; shift; mkdir -p "$out"
one() {
    name=${1%-*}; name=${name%-*+*}
    "$here/vendor.sh" "$1" > "$out/$name.log" 2>&1 &&
        timeout 1800 python3 "$here/cratetest.py" "$here/vendor/$name" --json "$out/$name.json" >> "$out/$name.log" 2>&1
    # CLEAN=1: a sweep over many crates fills the disk with their target directories.
    [ -n "$CLEAN" ] && rm -rf "$here/vendor/$name/target"
    echo "$name: $(grep -E '^(MATCH|MISMATCH): ' "$out/$name.log" || echo "FAILED $(tail -1 "$out/$name.log" | cut -c1-150)")"
}
export -f one; export here out
printf '%s\n' "$@" | xargs -P "${JOBS:-2}" -I{} bash -c 'one {}'
