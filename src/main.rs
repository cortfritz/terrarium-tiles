//! Build Terrarium WebP tile pyramids from the GEBCO GeoTIFF grids.
//!
//! Inputs: the eight 90x90 degree GEBCO tiles of the ice-surface elevation grid
//! and of the type identifier (TID) grid. Outputs two MBTiles files (convert
//! with `pmtiles convert`):
//!
//! * `gebco-<year>.mbtiles` - elevation in metres, unbiased: bilinear at the
//!   deepest zoom, 2x2 mean above it, rounded to the nearest `step_m` (default
//!   1 m, GEBCO's own precision).
//! * `gebco-<year>-tid.mbtiles` - the TID code of each pixel, stored as a
//!   Terrarium "height" equal to the code (nearest at the deepest zoom, 2x2
//!   mode above it). TID 0 is land.
//!
//! Tiles are 512 px, Web Mercator XYZ, lossless WebP.

use memmap2::Mmap;
use rayon::prelude::*;
use rusqlite::{params, Connection};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

const TILE: usize = 512;
const QUAD: usize = 21_600; // cells per side of one GEBCO GeoTIFF
const COLS: usize = QUAD * 4; // 86400 cells around the globe
const ROWS: usize = QUAD * 2; // 43200 cells pole to pole
const CELLS_PER_DEG: f64 = 240.0; // 15 arc-seconds

const NS: [&str; 2] = ["n90.0_s0.0", "n0.0_s-90.0"];
const WE: [&str; 4] = [
    "w-180.0_e-90.0",
    "w-90.0_e0.0",
    "w0.0_e90.0",
    "w90.0_e180.0",
];

/// One uncompressed, one-row-per-strip GeoTIFF, memory mapped.
struct Raster {
    map: Mmap,
    data_offset: usize,
    bytes_per_cell: usize,
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn f64_at(b: &[u8], o: usize) -> f64 {
    f64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

impl Raster {
    fn open(path: &Path, expect_bps: u16, west: f64, north: f64) -> Result<Raster, String> {
        let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let map = unsafe { Mmap::map(&file) }.map_err(|e| format!("{}: {e}", path.display()))?;
        let b = &map[..];
        if &b[0..4] != b"II*\0" {
            return Err(format!("{}: not a little-endian TIFF", path.display()));
        }
        let ifd = u32_at(b, 4) as usize;
        let n = u16_at(b, ifd) as usize;
        let mut tag = std::collections::HashMap::new();
        for i in 0..n {
            let e = ifd + 2 + 12 * i;
            tag.insert(u16_at(b, e), (u16_at(b, e + 2), u32_at(b, e + 4), e + 8));
        }
        let short = |t: u16| tag.get(&t).map(|&(_, _, o)| u16_at(b, o) as u32);
        let check = |t: u16, want: u32, what: &str| match short(t) {
            Some(v) if v == want => Ok(()),
            other => Err(format!("{}: {what} {other:?}, want {want}", path.display())),
        };
        check(256, QUAD as u32, "width")?;
        check(257, QUAD as u32, "height")?;
        check(258, expect_bps as u32, "bits per sample")?;
        check(259, 1, "compression")?;
        check(277, 1, "samples per pixel")?;
        check(278, 1, "rows per strip")?;

        let bytes_per_cell = expect_bps as usize / 8;
        let row_bytes = QUAD * bytes_per_cell;
        let &(_, count, o) = tag.get(&273).ok_or("no strip offsets")?;
        if count as usize != QUAD {
            return Err(format!("{}: {count} strips", path.display()));
        }
        let strips = u32_at(b, o) as usize;
        let first = u32_at(b, strips) as usize;
        for k in 0..QUAD {
            if u32_at(b, strips + 4 * k) as usize != first + k * row_bytes {
                return Err(format!("{}: strips not contiguous at {k}", path.display()));
            }
        }
        if first + QUAD * row_bytes > b.len() {
            return Err(format!("{}: truncated", path.display()));
        }

        // Model tie point: raster (0,0) must sit at the tile's north-west corner.
        let &(_, _, o) = tag.get(&33922).ok_or("no tie point")?;
        let tp = u32_at(b, o) as usize;
        let (tx, ty) = (f64_at(b, tp + 24), f64_at(b, tp + 32));
        if (tx - west).abs() > 1e-6 || (ty - north).abs() > 1e-6 {
            return Err(format!("{}: tie point {tx},{ty}", path.display()));
        }

        Ok(Raster {
            map,
            data_offset: first,
            bytes_per_cell,
        })
    }
}

/// The global grid stitched from eight GeoTIFFs. Cell (i, j) has its centre at
/// lon = -180 + (i + 0.5) / 240, lat = 90 - (j + 0.5) / 240.
struct Grid {
    quads: Vec<Option<Raster>>,
}

impl Grid {
    fn open(dir: &Path, prefix: &str, bps: u16) -> Result<Grid, String> {
        let mut quads = Vec::new();
        for (r, ns) in NS.iter().enumerate() {
            for (c, we) in WE.iter().enumerate() {
                let path = dir.join(format!("{prefix}_{ns}_{we}_geotiff.tif"));
                let west = -180.0 + 90.0 * c as f64;
                let north = 90.0 - 90.0 * r as f64;
                // A missing quadrant is allowed so partial builds work; reading it panics.
                quads.push(if path.exists() {
                    Some(Raster::open(&path, bps, west, north)?)
                } else {
                    eprintln!("missing {}", path.display());
                    None
                });
            }
        }
        Ok(Grid { quads })
    }

    #[inline]
    fn offset(&self, i: usize, j: usize) -> (&Raster, usize) {
        let q = self.quads[(j / QUAD) * 4 + i / QUAD]
            .as_ref()
            .expect("cell falls in a missing GEBCO quadrant");
        let o = q.data_offset + ((j % QUAD) * QUAD + i % QUAD) * q.bytes_per_cell;
        (q, o)
    }

    #[inline]
    fn i16_at(&self, i: usize, j: usize) -> f32 {
        let (q, o) = self.offset(i, j);
        i16::from_le_bytes([q.map[o], q.map[o + 1]]) as f32
    }

    #[inline]
    fn u8_at(&self, i: usize, j: usize) -> u8 {
        let (q, o) = self.offset(i, j);
        q.map[o]
    }
}

/// Continuous cell coordinates of a point: integer values are cell centres.
fn cell_coords(lat: f64, lon: f64) -> (f64, f64) {
    let u = (lon + 180.0) * CELLS_PER_DEG - 0.5;
    let v = (90.0 - lat) * CELLS_PER_DEG - 0.5;
    (u, v.clamp(0.0, (ROWS - 1) as f64))
}

fn wrap(i: i64) -> usize {
    i.rem_euclid(COLS as i64) as usize
}

fn bilinear(g: &Grid, lat: f64, lon: f64) -> f32 {
    let (u, v) = cell_coords(lat, lon);
    let (i0, j0) = (u.floor() as i64, v.floor() as usize);
    let (fu, fv) = ((u - u.floor()) as f32, (v - v.floor()) as f32);
    let (ia, ib) = (wrap(i0), wrap(i0 + 1));
    let j1 = (j0 + 1).min(ROWS - 1);
    let top = g.i16_at(ia, j0) * (1.0 - fu) + g.i16_at(ib, j0) * fu;
    let bot = g.i16_at(ia, j1) * (1.0 - fu) + g.i16_at(ib, j1) * fu;
    top * (1.0 - fv) + bot * fv
}

fn nearest(g: &Grid, lat: f64, lon: f64) -> u8 {
    let (u, v) = cell_coords(lat, lon);
    g.u8_at(wrap(u.round() as i64), v.round() as usize)
}

/// Latitude/longitude of a pixel centre in tile (z, x, y).
fn pixel_lat_lon(z: u8, x: u32, y: u32, px: usize, py: usize) -> (f64, f64) {
    let n = (1u64 << z) as f64;
    let fx = x as f64 + (px as f64 + 0.5) / TILE as f64;
    let fy = y as f64 + (py as f64 + 0.5) / TILE as f64;
    let lon = fx / n * 360.0 - 180.0;
    let lat = (std::f64::consts::PI * (1.0 - 2.0 * fy / n))
        .sinh()
        .atan()
        .to_degrees();
    (lat, lon)
}

struct Tile {
    elev: Vec<f32>,
    tid: Vec<u8>,
}

fn sample_tile(elev: &Grid, tid: &Grid, z: u8, x: u32, y: u32) -> Tile {
    let mut t = Tile {
        elev: vec![0.0; TILE * TILE],
        tid: vec![0; TILE * TILE],
    };
    for py in 0..TILE {
        for px in 0..TILE {
            let (lat, lon) = pixel_lat_lon(z, x, y, px, py);
            t.elev[py * TILE + px] = bilinear(elev, lat, lon);
            t.tid[py * TILE + px] = nearest(tid, lat, lon);
        }
    }
    t
}

fn mode4(v: [u8; 4]) -> u8 {
    let mut best = v[0];
    let mut best_n = 0;
    for a in v {
        let n = v.iter().filter(|&&b| b == a).count();
        if n > best_n {
            best = a;
            best_n = n;
        }
    }
    best
}

/// Children in order: top-left, top-right, bottom-left, bottom-right.
fn downsample(children: [Tile; 4]) -> Tile {
    let mut t = Tile {
        elev: vec![0.0; TILE * TILE],
        tid: vec![0; TILE * TILE],
    };
    let h = TILE / 2;
    for py in 0..TILE {
        for px in 0..TILE {
            let c = &children[(py / h) * 2 + px / h];
            let (cx, cy) = (2 * (px % h), 2 * (py % h));
            let idx = [
                cy * TILE + cx,
                cy * TILE + cx + 1,
                (cy + 1) * TILE + cx,
                (cy + 1) * TILE + cx + 1,
            ];
            t.elev[py * TILE + px] = idx.iter().map(|&k| c.elev[k]).sum::<f32>() / 4.0;
            t.tid[py * TILE + px] = mode4(idx.map(|k| c.tid[k]));
        }
    }
    t
}

/// Terrarium RGB: R*256 + G + B/256 - 32768. `step` is the vertical quantum in
/// metres (1.0 = whole metres, GEBCO's own precision; 1/256 = full Terrarium
/// precision). Rounds to nearest, so the quantization is unbiased.
fn terrarium_rgb(heights: impl Iterator<Item = f64>, step: f64) -> Vec<u8> {
    let mut out = Vec::with_capacity(TILE * TILE * 3);
    for h in heights {
        let h = (h / step).round() * step;
        let q = ((h + 32768.0) * 256.0).round().clamp(0.0, 16_777_215.0) as u32;
        out.extend_from_slice(&[(q >> 16) as u8, (q >> 8) as u8, q as u8]);
    }
    out
}

fn webp_lossless(rgb: &[u8]) -> Vec<u8> {
    let mut config = webp::WebPConfig::new().expect("webp config");
    config.lossless = 1;
    config.exact = 1;
    config.quality = 100.0;
    config.method = 4;
    let enc = webp::Encoder::from_rgb(rgb, TILE as u32, TILE as u32);
    enc.encode_advanced(&config).expect("webp encode").to_vec()
}

struct Out {
    db: Mutex<Connection>,
}

impl Out {
    fn create(path: &Path, meta: &[(&str, String)]) -> Out {
        let _ = std::fs::remove_file(path);
        let db = Connection::open(path).expect("open mbtiles");
        db.execute_batch(
            "PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF;
             CREATE TABLE metadata (name TEXT, value TEXT);
             CREATE TABLE tiles (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_data BLOB);
             CREATE UNIQUE INDEX tile_index ON tiles (zoom_level, tile_column, tile_row);",
        )
        .expect("schema");
        for (k, v) in meta {
            db.execute("INSERT INTO metadata VALUES (?1, ?2)", params![k, v])
                .expect("metadata");
        }
        Out { db: Mutex::new(db) }
    }

    fn put(&self, z: u8, x: u32, y: u32, data: &[u8]) {
        let tms_y = (1u32 << z) - 1 - y;
        self.db
            .lock()
            .unwrap()
            .execute(
                "INSERT OR REPLACE INTO tiles VALUES (?1, ?2, ?3, ?4)",
                params![z, x, tms_y, data],
            )
            .expect("insert tile");
    }
}

struct Ctx {
    elev: Grid,
    tid: Grid,
    max_zoom: u8,
    step: f64,
    elev_out: Out,
    tid_out: Out,
    done: AtomicUsize,
    total: usize,
    started: Instant,
}

fn emit(ctx: &Ctx, z: u8, x: u32, y: u32, t: &Tile) {
    let elev = webp_lossless(&terrarium_rgb(t.elev.iter().map(|&h| h as f64), ctx.step));
    let tid = webp_lossless(&terrarium_rgb(t.tid.iter().map(|&c| c as f64), 1.0));
    ctx.elev_out.put(z, x, y, &elev);
    ctx.tid_out.put(z, x, y, &tid);
}

fn build(ctx: &Ctx, z: u8, x: u32, y: u32) -> Tile {
    let tile = if z == ctx.max_zoom {
        let t = sample_tile(&ctx.elev, &ctx.tid, z, x, y);
        let n = ctx.done.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_multiple_of(512) || n == ctx.total {
            let s = ctx.started.elapsed().as_secs_f64();
            eprintln!(
                "{n}/{} deepest tiles, {:.0}s elapsed, ~{:.0}s left",
                ctx.total,
                s,
                s / n as f64 * (ctx.total - n) as f64
            );
        }
        t
    } else {
        let kids = [(0, 0), (1, 0), (0, 1), (1, 1)];
        let mut built: Vec<(usize, Tile)> = kids
            .par_iter()
            .enumerate()
            .map(|(k, &(dx, dy))| (k, build(ctx, z + 1, 2 * x + dx, 2 * y + dy)))
            .collect();
        built.sort_by_key(|(k, _)| *k);
        let mut it = built.into_iter().map(|(_, t)| t);
        let children = [
            it.next().unwrap(),
            it.next().unwrap(),
            it.next().unwrap(),
            it.next().unwrap(),
        ];
        downsample(children)
    };
    emit(ctx, z, x, y, &tile);
    tile
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!(
            "usage: gebco-tiles <gebco_year_dir> <out_dir> <year> [max_zoom] [root z/x/y] [step_m]"
        );
        std::process::exit(2);
    }
    let dir = PathBuf::from(&args[1]);
    let out_dir = PathBuf::from(&args[2]);
    let year = &args[3];
    let max_zoom: u8 = args.get(4).map(|s| s.parse().unwrap()).unwrap_or(8);
    let root: (u8, u32, u32) = args
        .get(5)
        .map(|s| {
            let p: Vec<u32> = s.split('/').map(|v| v.parse().unwrap()).collect();
            (p[0] as u8, p[1], p[2])
        })
        .unwrap_or((0, 0, 0));

    let step: f64 = args.get(6).map(|s| s.parse().unwrap()).unwrap_or(1.0);
    let elev = Grid::open(&dir.join("elevation"), &format!("gebco_{year}"), 16)
        .unwrap_or_else(|e| panic!("elevation grid: {e}"));
    let tid = Grid::open(&dir.join("tid"), &format!("gebco_{year}_tid"), 8)
        .unwrap_or_else(|e| panic!("tid grid: {e}"));

    let attribution =
        format!("GEBCO Compilation Group ({year}) GEBCO_{year} Grid. Not for navigation.");
    let common = |name: &str, desc: &str| {
        vec![
            ("name", name.to_string()),
            ("format", "webp".to_string()),
            ("type", "baselayer".to_string()),
            ("minzoom", root.0.to_string()),
            ("maxzoom", max_zoom.to_string()),
            ("bounds", "-180,-85.051129,180,85.051129".to_string()),
            ("attribution", attribution.clone()),
            ("description", desc.to_string()),
            ("encoding", "terrarium".to_string()),
            ("tileSize", TILE.to_string()),
        ]
    };
    std::fs::create_dir_all(&out_dir).expect("out dir");
    let elev_out = Out::create(
        &out_dir.join(format!("gebco-{year}.mbtiles")),
        &common(
            &format!("gebco-{year}"),
            "GEBCO ice-surface elevation, metres (negative below sea level). Unbiased: bilinear at max zoom, 2x2 mean above.",
        ),
    );
    let tid_out = Out::create(
        &out_dir.join(format!("gebco-{year}-tid.mbtiles")),
        &common(
            &format!("gebco-{year}-tid"),
            "GEBCO type identifier (TID) code per pixel, stored as a Terrarium height equal to the code. 0 = land.",
        ),
    );

    let levels = (max_zoom - root.0) as u32;
    let ctx = Ctx {
        elev,
        tid,
        max_zoom,
        step,
        elev_out,
        tid_out,
        done: AtomicUsize::new(0),
        total: 1usize << (2 * levels),
        started: Instant::now(),
    };
    build(&ctx, root.0, root.1, root.2);
    eprintln!("done in {:.0}s", ctx.started.elapsed().as_secs_f64());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(rgb: &[u8]) -> f64 {
        rgb[0] as f64 * 256.0 + rgb[1] as f64 + rgb[2] as f64 / 256.0 - 32768.0
    }

    #[test]
    fn terrarium_round_trips_and_rounds_to_nearest() {
        let rgb = terrarium_rgb([-10921.4, -0.5, 0.0, 8521.6, 12.25].into_iter(), 1.0);
        let got: Vec<f64> = rgb.chunks(3).map(decode).collect();
        // -0.5 rounds away from zero (f64::round); still nearest, not toward shallower.
        assert_eq!(got, vec![-10921.0, -1.0, 0.0, 8522.0, 12.0]);

        let fine = terrarium_rgb([12.25].into_iter(), 1.0 / 256.0);
        assert_eq!(decode(&fine), 12.25);
    }

    #[test]
    fn pixel_centres_follow_web_mercator() {
        let (lat, lon) = pixel_lat_lon(0, 0, 0, 255, 255);
        assert!((lon - (-180.0 + 255.5 / 512.0 * 360.0)).abs() < 1e-9);
        assert!(lat > 0.0 && lat < 1.0);
        let (lat, _) = pixel_lat_lon(0, 0, 0, 0, 0);
        assert!(lat < 85.0512 && lat > 84.0);
    }

    #[test]
    fn cell_coords_put_centres_on_integers() {
        // Centre of cell (0, 0) is lon -180 + 1/480, lat 90 - 1/480.
        let (u, v) = cell_coords(90.0 - 0.5 / 240.0, -180.0 + 0.5 / 240.0);
        assert!(u.abs() < 1e-9 && v.abs() < 1e-9);
        assert_eq!(wrap(COLS as i64), 0);
        assert_eq!(wrap(-1), COLS - 1);
    }

    #[test]
    fn mode4_picks_the_majority() {
        assert_eq!(mode4([11, 0, 11, 40]), 11);
        assert_eq!(mode4([0, 0, 40, 40]), 0);
        assert_eq!(mode4([1, 2, 3, 4]), 1);
    }
}
