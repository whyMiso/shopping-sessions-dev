#!/bin/bash
# Usage: tests/sweep.sh REF_CMD SEED_FROM SEED_TO EVENTS [CLICKS]
# Compares engine output against a reference command on generated seeds.
set -u
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
REF="$1"; A=$2; B=$3; E=$4; C=${5:-}
PY=$ROOT/.venv/bin/python
ENG=${ENG:-$ROOT/repo/engine/target/release/engine}
W=$ROOT/data/sweep; mkdir -p $W
fail=0
for s in $(seq $A $B); do
  cl=${C:-$(( (E * (s % 7 + 1)) / 8 ))}
  d=$W/s${s}_$E; [ -d $d ] || $ROOT/bin/generate --seed $s --events $E --clicks $cl --output $d >/dev/null
  rm -rf $W/eo $W/ro
  $ENG $d $W/eo || { echo "seed $s ENGINE FAIL"; fail=1; continue; }
  $REF $d $W/ro || { echo "seed $s REF FAIL"; fail=1; continue; }
  r=$(cd $ROOT/repo/scripts && $PY compare.py $W/eo $W/ro | tail -1)
  echo "seed $s events $E clicks $cl: $r"
  [ "$r" = MATCH ] || fail=1
  [ -n "${KEEP:-}" ] || rm -rf $d
done
exit $fail
