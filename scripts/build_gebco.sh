#!/bin/bash
# Build gebco-<year>.pmtiles and gebco-<year>-tid.pmtiles from a downloaded GEBCO
# directory. Needs the `pmtiles` CLI (https://github.com/protomaps/go-pmtiles).
#
#   scripts/build_gebco.sh <year> <gebco_dir> <out_dir> [max_zoom] [step_m]
set -euo pipefail
YEAR=${1:?year}
SRC=${2:?gebco directory from download_gebco.sh}
OUT=${3:?output directory}
MAXZ=${4:-8}
STEP=${5:-1}
WORK="$OUT/.build-gebco-$YEAR"
cd "$(dirname "$0")/.."
cargo build --release
mkdir -p "$WORK"
target/release/gebco-tiles "$SRC" "$WORK" "$YEAR" "$MAXZ" 0/0/0 "$STEP"
pmtiles convert "$WORK/gebco-$YEAR.mbtiles" "$OUT/gebco-$YEAR.pmtiles"
pmtiles convert "$WORK/gebco-$YEAR-tid.mbtiles" "$OUT/gebco-$YEAR-tid.pmtiles"
pmtiles verify "$OUT/gebco-$YEAR.pmtiles"
pmtiles verify "$OUT/gebco-$YEAR-tid.pmtiles"
rm -rf "$WORK"
ls -la "$OUT"/gebco-"$YEAR"*.pmtiles
