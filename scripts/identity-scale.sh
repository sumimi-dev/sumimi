#!/bin/zsh
# The scale runs for the identity layout (mneme #168).
#
# Every layout runs in its own process, so the peak RSS is attributable:
# /usr/bin/time -l reports "maximum resident set size" for the process.
set -e

BIN=./target/release/examples/identity_scale
VISITORS=${VISITORS:-1000000}
OUT=${OUT:-/tmp/identity-scale}
mkdir -p "$OUT"

LAYOUTS=(today marker8 marker10 marker12 selfdesc8 selfdesc10 selfdesc12 blob8 blob12)
NOLIMIT=(today marker12 selfdesc8 selfdesc12 blob8)

# one layout, one process
one() {
  local layout="$1" file="$2" rows="$3" limit="$4" interleave="$5"
  echo "--- $layout ($rows rows, memory_limit=$limit, interleave=$interleave)" | tee -a "$OUT/$file.txt"
  /usr/bin/time -l $BIN --rows "$rows" --visitors $VISITORS --days 90 \
    --memory-limit "$limit" --repeats 2 --interleave "$interleave" --layouts "$layout" 2>&1 \
    | grep -E "the layout|^  [a-z]|maximum resident|real|Error" | tee -a "$OUT/$file.txt"
}

ROWS=${ROWS:-20000000}

echo "=== the whole matrix, the product's own 256MB ceiling ===" | tee "$OUT/matrix-256mb.txt"
for layout in $LAYOUTS; do one "$layout" matrix-256mb $ROWS 256MB 4; done

echo "=== the same, no memory limit (what #165 measured) ===" | tee "$OUT/matrix-nolimit.txt"
for layout in $NOLIMIT; do one "$layout" matrix-nolimit $ROWS 8GB 4; done

echo "=== where it breaks: the same two layouts, smaller ===" | tee "$OUT/scale.txt"
for rows in 2000000 5000000 10000000; do
  for layout in today selfdesc8; do one "$layout" scale $rows 256MB 4; done
done

echo "=== the engine's handholds ===" | tee "$OUT/knobs.txt"
$BIN --rows 10000000 --visitors $VISITORS --days 90 --interleave 4 \
  --memory-limit 256MB --knobs 2>&1 | tee -a "$OUT/knobs.txt"

echo "=== a quiet site (a visit's events land next to each other) ===" | tee "$OUT/quiet.txt"
for layout in today selfdesc8; do one "$layout" quiet $ROWS 256MB 1; done

echo "done: $OUT"
