//! The measurement behind the JPEG classifier in `andris::server::updates`.
//!
//! Not a test of andris's behaviour, and so `#[ignore]`d: it reads a
//! corpus of raw screen captures that is never committed, and prints
//! numbers. Each file in the corpus directory (`target/corpus` beside
//! the workspace, or `$ANDRIS_JPEG_CORPUS`) is packed 32-bit B, G, R, X
//! pixels, named `<name>_<W>x<H>.bgrx`.
//!
//! Each image is cut into rectangles as andris sends them: 64 x 64
//! tiles, 256 x 256 and 512 x 512 squares, and the whole image. Each
//! rectangle is encoded as LZ4 and as JPEG at quality 85, and timed. The
//! report gives, per image and rectangle size, the LZ4 and JPEG ratios
//! and encode rates, and the spread of LZ4 ratios. Then, for andris's
//! own classifier and for trial ones (a minimum area, a least LZ4 ratio
//! and a largest JPEG share), the share of rectangles and of area each
//! sends as JPEG, and how many times fewer bytes the first paint takes
//! than lossless would.
//!
//! Run it in the devcontainer, in release, so that the times mean
//! something:
//!
//! ```text
//! cargo test --release --frozen --test jpeg_corpus -- --ignored --nocapture
//! ```
//!
//! It prints numbers only, never pixels: the corpus is someone's desktop.

#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use andris::server::updates::{JPEG_MAX_PERCENT, JPEG_MIN_SIDE, looks_photographic};
use shakenfist_spice_compression::{Bgrx, encode_spice_jpeg, encode_spice_lz4};

/// The quality the measurements encode JPEG at: `--jpeg-quality`'s
/// default.
const QUALITY: u8 = 85;

/// One rectangle's measurements.
struct Sample {
    width: u32,
    height: u32,
    area: u64,
    raw: u64,
    lz4: u64,
    jpeg: u64,
    lz4_time: Duration,
    jpeg_time: Duration,
}

impl Sample {
    /// What andris sends losslessly: LZ4 if smaller, else raw.
    fn lossless(&self) -> u64 {
        self.lz4.min(self.raw)
    }

    fn lz4_ratio(&self) -> f64 {
        self.lz4 as f64 / self.raw as f64
    }
}

/// A classifier.
#[derive(Clone, Copy)]
enum Candidate {
    /// Andris's own: [`looks_photographic`], then [`JPEG_MAX_PERCENT`].
    Andris,
    /// JPEG when the rectangle has at least `min_area` pixels and no side
    /// under [`JPEG_MIN_SIDE`], LZ4 leaves more than `min_ratio` of the
    /// raw bytes, and JPEG comes to at most `max_share` of what lossless
    /// would send.
    Trial {
        min_area: u64,
        min_ratio: f64,
        max_share: f64,
    },
}

impl Candidate {
    fn chooses_jpeg(&self, s: &Sample) -> bool {
        match *self {
            Self::Andris => {
                looks_photographic(s.width, s.height, s.lz4 as usize)
                    && s.jpeg * 100 <= s.lossless() * JPEG_MAX_PERCENT
            }
            Self::Trial {
                min_area,
                min_ratio,
                max_share,
            } => {
                s.area >= min_area
                    && s.width.min(s.height) >= JPEG_MIN_SIDE
                    && s.lz4_ratio() > min_ratio
                    && (s.jpeg as f64) <= max_share * s.lossless() as f64
            }
        }
    }
}

fn corpus_dir() -> PathBuf {
    std::env::var_os("ANDRIS_JPEG_CORPUS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/corpus"))
}

/// The images in the corpus, by name, with their size and pixels.
fn images() -> Vec<(String, u32, u32, Vec<u8>)> {
    let mut out = Vec::new();
    let dir = corpus_dir();
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "bgrx"))
        .collect();
    entries.sort();
    for path in entries {
        let stem = path.file_stem().unwrap().to_str().unwrap().to_string();
        let (name, dims) = stem.rsplit_once('_').unwrap();
        let (w, h) = dims.split_once('x').unwrap();
        let (w, h): (u32, u32) = (w.parse().unwrap(), h.parse().unwrap());
        let pixels = std::fs::read(&path).unwrap();
        assert_eq!(pixels.len(), w as usize * h as usize * 4, "{name}");
        out.push((name.to_string(), w, h, pixels));
    }
    out
}

/// The rectangles of a `w` x `h` image cut into `side` squares, clipped.
fn grid(w: u32, h: u32, side: u32) -> Vec<(u32, u32, u32, u32)> {
    let mut out = Vec::new();
    for y in (0..h).step_by(side as usize) {
        for x in (0..w).step_by(side as usize) {
            out.push((x, y, side.min(w - x), side.min(h - y)));
        }
    }
    out
}

fn measure(pixels: &[u8], w: u32, rect: (u32, u32, u32, u32)) -> Sample {
    let (x, y, rw, rh) = rect;
    let stride = w as usize * 4;
    let start = y as usize * stride + x as usize * 4;
    let end = (y + rh - 1) as usize * stride + (x + rw) as usize * 4;
    let image = Bgrx::new(&pixels[start..end], rw, rh, stride).unwrap();
    let t = Instant::now();
    let lz4 = encode_spice_lz4(&image).len() as u64;
    let lz4_time = t.elapsed();
    let t = Instant::now();
    let jpeg = encode_spice_jpeg(&image, QUALITY).unwrap().len() as u64;
    let jpeg_time = t.elapsed();
    let area = u64::from(rw) * u64::from(rh);
    Sample {
        width: rw,
        height: rh,
        area,
        raw: area * 4,
        lz4,
        jpeg,
        lz4_time,
        jpeg_time,
    }
}

/// Megabytes of raw pixels encoded a second.
fn rate(raw: u64, time: Duration) -> f64 {
    raw as f64 / 1e6 / time.as_secs_f64().max(1e-9)
}

fn summarise(label: &str, samples: &[Sample]) {
    let raw: u64 = samples.iter().map(|s| s.raw).sum();
    let lz4: u64 = samples.iter().map(|s| s.lz4).sum();
    let lossless: u64 = samples.iter().map(Sample::lossless).sum();
    let jpeg: u64 = samples.iter().map(|s| s.jpeg).sum();
    let lz4_time: Duration = samples.iter().map(|s| s.lz4_time).sum();
    let jpeg_time: Duration = samples.iter().map(|s| s.jpeg_time).sum();
    // The spread of LZ4 ratios, in tenths.
    let mut spread = [0usize; 11];
    for s in samples {
        spread[((s.lz4_ratio() * 10.0) as usize).min(10)] += 1;
    }
    let jpeg_smaller = samples.iter().filter(|s| s.jpeg < s.lossless()).count();
    println!(
        "  {label:>6}: n={:>5} lz4={:.3} lossless={:.3} jpeg={:.3} jpeg<lossless={:>5.1}% \
         lz4 {:>7.0} MB/s jpeg {:>5.0} MB/s (jpeg/lz4 time x{:.1})",
        samples.len(),
        lz4 as f64 / raw as f64,
        lossless as f64 / raw as f64,
        jpeg as f64 / raw as f64,
        100.0 * jpeg_smaller as f64 / samples.len() as f64,
        rate(raw, lz4_time),
        rate(raw, jpeg_time),
        jpeg_time.as_secs_f64() / lz4_time.as_secs_f64().max(1e-9),
    );
    let spread: Vec<String> = spread.iter().map(|n| n.to_string()).collect();
    println!("          lz4 ratio by tenths: {}", spread.join(" "));
}

fn classify(label: &str, samples: &[Sample], candidate: Candidate) -> String {
    let chosen: Vec<&Sample> = samples
        .iter()
        .filter(|s| candidate.chooses_jpeg(s))
        .collect();
    let lossless: u64 = samples.iter().map(Sample::lossless).sum();
    let sent: u64 = samples
        .iter()
        .map(|s| {
            if candidate.chooses_jpeg(s) {
                s.jpeg
            } else {
                s.lossless()
            }
        })
        .sum();
    let area: u64 = samples.iter().map(|s| s.area).sum();
    let jpeg_area: u64 = chosen.iter().map(|s| s.area).sum();
    format!(
        "{label}:{:>5.1}%n {:>5.1}%a {:>5.2}x",
        100.0 * chosen.len() as f64 / samples.len() as f64,
        100.0 * jpeg_area as f64 / area as f64,
        lossless as f64 / sent.max(1) as f64,
    )
}

#[test]
#[ignore = "measures a private corpus; run by hand"]
fn corpus() {
    let sides: [(&str, Option<u32>); 4] = [
        ("64", Some(64)),
        ("256", Some(256)),
        ("512", Some(512)),
        ("whole", None),
    ];
    let candidates: Vec<(String, Candidate)> = {
        let mut out = vec![("andris".to_string(), Candidate::Andris)];
        for min_area in [64 * 64, 128 * 128] {
            for min_ratio in [0.3, 0.4, 0.5, 0.6] {
                for max_share in [1.0, 0.5] {
                    let side = (min_area as f64).sqrt() as u64;
                    out.push((
                        format!("a{side} r{min_ratio} s{max_share}"),
                        Candidate::Trial {
                            min_area,
                            min_ratio,
                            max_share,
                        },
                    ));
                }
            }
        }
        out
    };
    for (name, w, h, pixels) in images() {
        println!("{name} ({w}x{h})");
        let mut all = Vec::new();
        for (label, side) in sides {
            let rects = match side {
                Some(side) => grid(w, h, side),
                None => vec![(0, 0, w, h)],
            };
            let samples: Vec<Sample> = rects.into_iter().map(|r| measure(&pixels, w, r)).collect();
            summarise(label, &samples);
            all.push((label, samples));
        }
        for (cname, candidate) in &candidates {
            let row: Vec<String> = all
                .iter()
                .map(|(label, samples)| classify(label, samples, *candidate))
                .collect();
            println!("  [{cname:>16}] {}", row.join(" | "));
        }
    }
}
