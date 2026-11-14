# terrarium-tiles

Build [Terrarium](https://github.com/tilezen/joerd/blob/master/docs/formats.md#terrarium)-encoded
elevation archives (512 px lossless WebP in [PMTiles](https://github.com/protomaps/PMTiles)),
the same tile format [Mapterhorn](https://mapterhorn.com) uses, so one reader and
decoder (for example [ex_terrarium](https://github.com/cortfritz/ex_terrarium) or MapLibre
`raster-dem`) serves land and sea floor alike.

Currently one recipe: **GEBCO** global bathymetry and topography.

This is an offline build tool. It links libwebp and SQLite (C); nothing here runs
inside an application.

## GEBCO

[GEBCO](https://www.gebco.net) publishes a global 15 arc-second grid (~460 m at the
equator) of ocean depth and land height, plus a type identifier (TID) grid recording
the kind of source behind each cell. Two archives are built:

| Archive | Content |
|---|---|
| `gebco-<year>.pmtiles` | Height in metres (negative below sea level), ice surface |
| `gebco-<year>-tid.pmtiles` | GEBCO TID code per pixel, stored as a Terrarium height equal to the code |

Choices:

- **Zoom 0-8.** z8 at 512 px is 306 * cos(lat) m per pixel, finer than GEBCO's
  463 * cos(lat) m column spacing at every latitude, so z8 is the full-detail level.
- **Unbiased.** Bilinear at z8, 2x2 mean above it, then rounded to the nearest
  `step_m` (default 1 m: GEBCO is stored in whole metres, so finer steps only keep
  interpolation fractions and triple the archive). Not rounded toward shallower.
- **No land sentinel.** Land keeps its GEBCO height. Decide land vs water with the TID
  archive (sample with nearest; 0 = land) together with a land elevation source:
  GEBCO marks some below-sea-level land (Death Valley, the Dead Sea) with non-zero TIDs.
- Input GeoTIFFs are validated (uncompressed, contiguous strips, tie point) and memory
  mapped, so the build needs no GDAL and little RAM.

### Build

```bash
scripts/download_gebco.sh 2026 /data/gebco/2026        # ~11 GB, resumable
scripts/build_gebco.sh 2026 /data/gebco/2026 /data       # ~45 min on 10 cores
```

GEBCO_2026 results: `gebco-2026.pmtiles` 5.9 GB, `gebco-2026-tid.pmtiles` 159 MB.
Intermediate MBTiles need about the same space again and are removed at the end.

Single subtree, for testing: `target/release/gebco-tiles <gebco_dir> <out_dir> <year> 8 5/5/12`.

### Attribution and terms

GEBCO data is public domain with conditions: acknowledge the source, do not imply
endorsement, do not misrepresent the data, and it is not for navigation. Cite:

> GEBCO Compilation Group (2026) GEBCO 2026 Grid (doi:10.5285/4f68d5c7-45eb-f999-e063-7086abc036fa)

The terms of use are downloaded alongside the grids. Each archive's metadata carries the
attribution.

## License

MIT (the code). Data keeps its own terms.
