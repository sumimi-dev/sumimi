#!/bin/zsh
# The full sweep for mneme#168: the marker layout and the blob, each at four
# entropy widths, against today's uuid — on every axis.
#
#   ./scripts/identity-sweep.sh
#
# One layout per process, so the peak RSS is attributable.
set -e

BIN=./target/release/examples/identity_scale
ROWS=${ROWS:-20000000}
OUT=${OUT:-/tmp/identity-sweep}
mkdir -p "$OUT"

ALL=(today selfdesc6 selfdesc8 selfdesc10 selfdesc12 blob6 blob8 blob10 blob12)

one() {
  local layout="$1" tag="$2" limit="$3" visitors="$4" interleave="$5"
  echo "--- $layout ($tag)" | tee -a "$OUT/$tag.txt"
  /usr/bin/time -l $BIN --rows $ROWS --visitors "$visitors" --interleave "$interleave" \
    --days 90 --memory-limit "$limit" --repeats 2 --layouts "$layout" 2>&1 \
    | grep -E "the layout|^  [a-z]|maximum resident|real|Error" | tee -a "$OUT/$tag.txt"
}

# 1. the product's own ceiling, a normal site's cardinality
for l in $ALL; do one "$l" ceiling-256mb 256MB 1000000 4; done

# 2. the same with room to spare — the "no spilling" world
for l in $ALL; do one "$l" no-limit-8gb 8GB 1000000 4; done

# 3. the cardinality axis: the same rows, ten times fewer visitors
#    (the visitor column's dictionary shrinks; the visit column's does not)
for l in today selfdesc8 blob8 blob12; do one "$l" few-visitors 256MB 100000 4; done

# 4. a quiet site: a visit's events land next to each other
for l in today selfdesc8 blob8; do one "$l" quiet 256MB 1000000 1; done

echo "done: $OUT"
