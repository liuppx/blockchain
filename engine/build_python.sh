#!/usr/bin/env bash
# Build the Python extension module for the ZhixingGraph engine (whitepaper §7.3)
# WITHOUT maturin — plain cargo + abi3, so it works offline once crates are cached.
#
#   ./engine/build_python.sh
#
# Produces engine/zhixing_engine.<ext> importable from Python. abi3-py38 means
# one build works for any CPython >= 3.8.
set -euo pipefail
cd "$(dirname "$0")"

cargo build --release --features python

# Locate the built dynamic library (name/extension varies by OS).
OUT=""
for cand in \
    target/release/libzhixing_engine.dylib \
    target/release/libzhixing_engine.so \
    target/release/zhixing_engine.dll; do
    if [ -f "$cand" ]; then OUT="$cand"; break; fi
done
if [ -z "$OUT" ]; then
    echo "error: built library not found under target/release/" >&2
    exit 1
fi

case "$(uname -s)" in
    Darwin|Linux) DEST="zhixing_engine.abi3.so" ;;
    *)            DEST="zhixing_engine.pyd" ;;
esac

cp "$OUT" "$DEST"
echo "built $DEST from $OUT"
python3 -c "import zhixing_engine as z; g=z.PyGraph(); print('import OK, DIM =', z.DIM)"
