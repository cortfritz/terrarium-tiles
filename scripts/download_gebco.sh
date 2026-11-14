#!/bin/bash
# Download the GEBCO GeoTIFF grids (ice-surface elevation + type identifier) from
# the CEDA archive. Resumable; re-run after a failure.
#
#   scripts/download_gebco.sh <year> <dest_dir>
#
# Produces <dest_dir>/{elevation,tid}/*.tif plus the terms of use and docs.
set -euo pipefail
YEAR=${1:?year, e.g. 2026}
DEST=${2:?destination directory}
B=https://dap.ceda.ac.uk/bodc/gebco/global/gebco_${YEAR}
mkdir -p "$DEST/elevation" "$DEST/tid"
cd "$DEST"
for f in 00README_catalogue_and_licence.txt 00readme.txt GEBCO_Grid_docmentation.pdf GEBCO_Grid_terms_of_use.pdf "metadata_GEBCO_${YEAR}.csv"; do
  curl -sSfL --retry 5 -C - -o "$f" "$B/$f" || echo "warning: could not fetch $f"
done
for ns in "n90.0_s0.0" "n0.0_s-90.0"; do
  for we in "w-180.0_e-90.0" "w-90.0_e0.0" "w0.0_e90.0" "w90.0_e180.0"; do
    e=gebco_${YEAR}_${ns}_${we}_geotiff.tif
    t=gebco_${YEAR}_tid_${ns}_${we}_geotiff.tif
    curl -sSfL --retry 5 -C - -o "elevation/$e" "$B/ice_surface_elevation/geotiff/$e"
    curl -sSfL --retry 5 -C - -o "tid/$t" "$B/type_identifier_grid/geotiff/$t"
    echo "$(date +%T) $e $t"
  done
done
