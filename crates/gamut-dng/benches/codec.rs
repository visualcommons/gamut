//! DNG encode + decode throughput, against the Adobe DNG SDK's decode (issue #163).
//!
//! `cargo bench -p gamut-dng --bench codec` first prints a fixture table — the byte volumes each
//! measured region moves — then runs divan throughput benchmarks over the codec matrix the crate
//! ships: **uncompressed**, **Deflate**, **lossless JPEG** and — behind the `jxl-encode` feature,
//! which this crate's own dev-dependency turns on for every bench build — **JPEG XL**, each for
//! **CFA** and **LinearRaw** photometry. Beside that synthetic matrix, `decode_dng_file` decodes
//! DNGs that *cameras and Adobe* wrote, which no synthetic fixture stands in for (issue #556):
//! Adobe's `01_jxl_linear_raw_integer.dng`, the JPEG XL sample the oracle extracts, and — only when
//! `mise run fetch-dng-samples` has checked the real-camera corpus out — that corpus's largest
//! frames. Every counter is a *pixel volume* in bytes — the raw sample volume (`samples × 2`),
//! plus the IFD-0 preview on the gamut benchmarks that handle it and where charging for it is a
//! measurement rather than a bound. See the counter rule below, and the fixture table's epilogue,
//! which names the rows where the preview correction is applied and the rows where it is not.
//!
//! # What is inside the timed region, and what is not
//!
//! **Inside**, for every benchmark: the codec call itself, the allocation and growth of the buffer
//! it produces, and that buffer's teardown. Every closure below returns `()`, so its result is
//! released where it was made rather than handed back to divan — which defers a returned value's
//! drop until after timing, and would therefore charge gamut nothing for freeing a decoded image
//! while the SDK, whose `dng_negative` destructor runs inside its own call, pays in full.
//!
//! **Outside**, for every benchmark: synthesising the sensor samples, building the [`RawImage`]
//! and [`CameraProfile`], and encoding the DNG (or the bare lossless-JPEG stream) that the decode
//! benchmarks read. Those are fixtures; timing them would measure this file rather than the codec.
//! No timed region touches the filesystem: `decode_dng_file` reads its files once, before divan
//! starts, and times only the decode of the bytes already in memory.
//!
//! # Is the gamut-versus-SDK comparison fair?
//!
//! Every asymmetry between the two implementations is either **removed** or **measured**. None is
//! left as an adjective.
//!
//! **Neither side pays for the FFI boundary on the way in.**
//! [`gamut_dng_oracle::decode_dng_in_memory`] hands the SDK a `dng_stream` over the caller's own
//! bytes, so the reference implementation parses the very buffer gamut parses: no temporary file,
//! no import copy on either side.
//!
//! **Neither side pays for it on the way out, in the container comparison.** That entry point
//! reports the decoded image's *extent* and exports no samples, so the SDK is not charged for a
//! `malloc` + `memcpy` that only exists because the caller is in Rust.
//!
//! **The one asymmetry left in `decode_dng` is the preview, and on the rows where correcting for
//! it is a measurement the throughput column does so.** `DngDecoder::decode` is a *whole-file*
//! decode and `ReadStage1Image` is not: gamut additionally unpacks IFD 0's uncompressed RGB
//! preview and reconstructs the metadata. The preview's volume is exact — see
//! [`preview_decode_bytes`], which models it at the width the *decoder* materialises, not the
//! width the file stores it at — so this file applies the **counter rule** below and the fixture
//! table prints, per case, that volume and whether the correction was applied. It is not
//! normalised away by changing the codec: gamut exposes no raw-image-only decode entry point, and
//! inventing one to make a benchmark look better would be the wrong direction of causation.
//!
//! **The one asymmetry left in `decode_lossless_jpeg` is the export path, and a third arm bounds
//! it.** [`gamut_dng_oracle::decode_lossless_jpeg`] spools into a `std::vector`, copies that into
//! a `malloc`d buffer and copies *that* into a `Vec`; gamut fills one `Vec`. Rather than assert
//! that the difference is small, `adobe-sdk-no-export` runs the identical decode into the
//! identical spool buffer and stops there, so the gap between the two SDK arms **bounds** the
//! export cost, measured on the same box in the same run. Read that gap as a magnitude only: it
//! sits at this harness's measurement floor, where its *sign* is not resolved, so what it
//! supports is "the codestream comparison is fair to within the bound", not "the export path
//! costs the SDK X".
//!
//! **On the two `*/deflate` rows neither arm's inflate is gamut-authored, and both are pinned.**
//! `gamut-deflate` is deliberately encoder-only, so this crate inflates with `miniz_oxide`; the
//! reference arm calls zlib, because the SDK includes `<zlib.h>` unconditionally. A Deflate row is
//! therefore `miniz_oxide` against stock zlib — not gamut's own codec against the SDK's, which is
//! how the wording here once read.
//!
//! The distinction that decides whether such a row is reproducible is **pinning**, not
//! authorship. `miniz_oxide` is pinned by `Cargo.lock` to one version and one checksum. The
//! oracle's zlib is the stock build `zlib-oracle` makes from the `third_party/zlib` submodule,
//! linked **statically** into this executable, so the submodule commit pins it. It used to be the
//! system libz, linked dynamically, and that was pinned by nothing: the loader chose between a
//! copy a dev oracle built under `target/` and whatever the platform installed, cargo's
//! `LD_LIBRARY_PATH` made the choice depend on the launcher, and stock zlib and a zlib-ng-class
//! fork differ by more than the margin that decides which side of 1.0 those rows fall on (issue
//! #618 measured the reference arm moving 1.2–1.3×). Every row now runs only code this repository
//! builds or pins, whether the bench is launched through `cargo` or run directly.
//!
//! The fixture table still prints [`gamut_dng_oracle::zlib_identity`] — the version, and the
//! object the `zlibVersion` symbol resolved in — and refuses to run if
//! [`gamut_dng_oracle::zlib_path`] names a shared library, because that would mean a shared libz
//! is being called again and the Deflate rows are unpinned.
//!
//! # The counter rule
//!
//! Every benchmark's counter is **the pixel volume that implementation actually moves**:
//!
//! - the raw sample volume for every SDK arm and for gamut's bare-codestream decode;
//! - the raw sample volume **plus the preview the encoder derives** for `encode_gamut`, which has
//!   no reference arm and so is not a comparison at all; and
//! - for gamut's whole-file DNG decode, the raw sample volume plus the preview the decoder
//!   materialises — **but only on the rows where charging preview bytes at the raw path's
//!   per-byte rate is a measurement rather than a bound.** `decode_dng_file`, over files gamut
//!   did not write, never applies it: a real file's preview may be compressed or missing, so
//!   each arm there is charged its own raw volume and nothing else.
//!
//! That proviso is the whole of the rule. On the **uncompressed** rows both paths do the same
//! kind of work per byte — unpack a stored integer and store it — so the correction is a
//! measurement, it is applied, and in `decode_dng` the **median-time** column is then the
//! uncorrected comparison while the **throughput** column is the preview-corrected one. On the
//! **compressed** rows a raw byte costs far more than a preview byte (the preview is stored
//! uncompressed whatever the raw scheme is), so the same arithmetic would credit gamut with more
//! than the preview actually costs: a *lower bound* printed where a reader will take a
//! measurement. There the correction is **suppressed** — gamut's counter is the raw volume, both
//! columns say the same uncorrected thing, and the fixture table and its epilogue say which rows
//! those are. In `decode_lossless_jpeg` all three arms share the raw volume, so no correction
//! arises.
//!
//! # Why each pair is one benchmark
//!
//! `decode_dng` and `decode_lossless_jpeg` take the implementation as a benchmark *argument*
//! rather than living in a benchmark each. divan runs benchmarks in name order, so two separate
//! benchmarks would measure every reference case minutes away from its counterpart — and on a
//! shared machine that drifts, a ratio measured minutes apart is not a ratio. As arguments the
//! pair members run back to back, under the same instantaneous load. The argument names are
//! ordered so that divan's own name sort keeps them adjacent.
//!
//! There is no `encode` arm for the SDK: the oracle shim wraps the SDK's reader, not its writer,
//! so no reference encode number exists to compare against and none is fabricated. Encode
//! throughput is reported for gamut alone, across the same matrix.
//!
//! # Alternate the arm order between runs
//!
//! Adjacent is not simultaneous. divan cannot interleave two arms *per sample*, so inside every
//! pair one arm always runs first, and the second inherits whatever the first left in the caches
//! and in the frequency governor. That is a real bias and it points one way for a whole run.
//! divan's sort is reversible, so the control already exists: take one run each way and publish
//! the mean of the two.
//!
//! ```text
//! cargo bench -p gamut-dng --bench codec                   # reference arm first
//! cargo bench -p gamut-dng --bench codec -- --sortr name   # gamut arm first
//! ```
//!
//! The epilogue printed under the fixture table repeats this, because that is where an operator
//! reads it rather than here.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use divan::counter::BytesCount;
use divan::{Bencher, black_box};
use gamut_core::Dimensions;
use gamut_dng::raw::cfa_color;
use gamut_dng::{
    CalibrationIlluminant, CameraProfile, Compression, DngDecoder, DngEncoder, RawImage,
    lossless_jpeg,
};

fn main() {
    print_fixture_table();
    divan::main();
}

/// Fixture frame size, in pixels. Large enough that the codecs dominate per-call overhead, small
/// enough that `cargo bench --workspace` stays affordable: a `LinearRaw` frame at this size is
/// 1.1 MiB of samples and a CFA frame 384 KiB.
const WIDTH: u32 = 512;
const HEIGHT: u32 = 384;

/// Fixture sample depth. 16-bit for every case, so the axis the matrix varies is the compression
/// scheme and the photometry — not the packing. (DNG's Deflate path is restricted to whole-byte
/// depths anyway; see `DngEncoder::encode`.)
const BITS: u16 = 16;

/// Which photometry a fixture carries — the two the encoder writes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Photometry {
    /// A single-plane RGGB Bayer mosaic.
    Cfa,
    /// A demosaiced three-plane linear image.
    LinearRaw,
}

impl Photometry {
    /// Colour planes per pixel.
    fn planes(self) -> u32 {
        match self {
            Photometry::Cfa => 1,
            Photometry::LinearRaw => 3,
        }
    }

    /// The fixture raw image for this photometry.
    fn raw(self) -> RawImage {
        let dims = Dimensions::new(WIDTH, HEIGHT).expect("non-empty fixture dimensions");
        let samples = sensor_samples(self.planes());
        let max = f64::from((1u32 << BITS) - 1);
        match self {
            Photometry::Cfa => {
                let pattern = vec![
                    cfa_color::RED,
                    cfa_color::GREEN,
                    cfa_color::GREEN,
                    cfa_color::BLUE,
                ];
                RawImage::new_cfa(dims, BITS, (2, 2), pattern, samples).expect("valid CFA fixture")
            }
            Photometry::LinearRaw => {
                RawImage::new_linear_raw(dims, BITS, 3, samples).expect("valid LinearRaw fixture")
            }
        }
        .with_black_level(0.0)
        .expect("valid black level")
        .with_white_level(max)
        .expect("valid white level")
        .with_active_area([0, 0, HEIGHT, WIDTH])
        .with_default_crop([0, 0], [WIDTH, HEIGHT])
    }
}

impl std::fmt::Display for Photometry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `pad`, not `write_str`: the latter goes straight to the underlying buffer and drops the
        // formatter's width and alignment, so a `{:<26}` column would not line up.
        f.pad(match self {
            Photometry::Cfa => "cfa",
            Photometry::LinearRaw => "linear-raw",
        })
    }
}

/// One cell of the codec matrix: a photometry crossed with a compression scheme.
#[derive(Clone, Copy)]
struct Case {
    photometry: Photometry,
    compression: Compression,
}

impl Case {
    /// The fixture raw image.
    fn raw(self) -> RawImage {
        self.photometry.raw()
    }

    /// The fixture DNG the decode benchmarks read: this case's raw image, encoded.
    fn encoded(self) -> Vec<u8> {
        let mut out = Vec::new();
        encoder(self.compression)
            .encode(&self.raw(), &profile(), &mut out)
            .expect("fixture DNG encodes");
        out
    }

    /// Raw sample volume in bytes — the denominator every SDK arm is quoted against.
    fn raw_bytes(self) -> usize {
        raw_bytes(self.photometry)
    }

    /// Pixel volume gamut's **encoder** moves for this case: the raw samples plus the preview it
    /// derives. `encode_gamut` has no reference arm, so this is a description of the work, not a
    /// correction to a comparison.
    fn gamut_encode_bytes(self) -> usize {
        self.raw_bytes() + preview_encode_bytes()
    }

    /// Pixel volume gamut's **decoder** is credited with for this case — the counter rule's one
    /// conditional.
    ///
    /// `DngDecoder::decode` always unpacks the IFD-0 preview that `ReadStage1Image` does not, so
    /// the raw volume alone understates its work. But the correction charges preview bytes at the
    /// *raw path's* per-byte rate, and that only holds where the two paths do comparable work per
    /// byte. So the preview is added on the rows where [`Case::preview_correction_is_measured`]
    /// holds and withheld everywhere else, rather than printing a bound a reader would take as a
    /// measurement.
    fn gamut_decode_bytes(self) -> usize {
        if self.preview_correction_is_measured() {
            self.raw_bytes() + preview_decode_bytes()
        } else {
            self.raw_bytes()
        }
    }

    /// Whether charging this case's preview bytes at its raw path's per-byte rate is a
    /// measurement.
    ///
    /// It is exactly when the raw path is uncompressed: then both the raw samples and the
    /// (always uncompressed) preview are unpacked and stored, at comparable cost per byte. Under
    /// Deflate, lossless JPEG or JPEG XL a raw byte carries entropy-decode work the preview byte
    /// does not,
    /// so the same arithmetic would over-credit gamut and the correction is suppressed.
    fn preview_correction_is_measured(self) -> bool {
        matches!(self.compression, Compression::Uncompressed)
    }
}

impl std::fmt::Display for Case {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let scheme = match self.compression {
            Compression::Uncompressed => "uncompressed".to_string(),
            Compression::Deflate => "deflate".to_string(),
            Compression::LosslessJpeg => "lossless-jpeg".to_string(),
            Compression::JpegXl => "jpeg-xl".to_string(),
            other => format!("{other:?}"),
        };
        // `pad`, not `write!`: `write!` writes through to the buffer and ignores the formatter's
        // width, so the fixture table's `{case:<26}` column would not align.
        f.pad(&format!("{}/{scheme}", self.photometry))
    }
}

/// The two photometries, as a benchmark argument list.
const PHOTOMETRIES: [Photometry; 2] = [Photometry::Cfa, Photometry::LinearRaw];

/// The codec matrix: every compression this crate encodes, crossed with both photometries.
///
/// The JPEG XL cells are gated on the `jxl-encode` feature, because encoding their fixture needs
/// the libjxl backend that feature builds (cmake + a C++ toolchain). In practice that gate is open
/// for every bench build: `Cargo.toml` dev-depends on this crate *with* `jxl-encode`, and the
/// Adobe oracle this file links already builds libjxl, so the cells cost no extra toolchain. The
/// gate keeps the file compiling should that self-dependency ever go, rather than turning a
/// missing feature into a fixture panic. JPEG XL *decode* of a file no gamut encoder wrote is
/// measured separately, by [`decode_dng_file`] over Adobe's own sample.
const CASES: &[Case] = &[
    Case {
        photometry: Photometry::Cfa,
        compression: Compression::Uncompressed,
    },
    Case {
        photometry: Photometry::Cfa,
        compression: Compression::Deflate,
    },
    Case {
        photometry: Photometry::Cfa,
        compression: Compression::LosslessJpeg,
    },
    #[cfg(feature = "jxl-encode")]
    Case {
        photometry: Photometry::Cfa,
        compression: Compression::JpegXl,
    },
    Case {
        photometry: Photometry::LinearRaw,
        compression: Compression::Uncompressed,
    },
    Case {
        photometry: Photometry::LinearRaw,
        compression: Compression::Deflate,
    },
    Case {
        photometry: Photometry::LinearRaw,
        compression: Compression::LosslessJpeg,
    },
    #[cfg(feature = "jxl-encode")]
    Case {
        photometry: Photometry::LinearRaw,
        compression: Compression::JpegXl,
    },
];

/// Which implementation one `decode_dng` measurement runs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DngImpl {
    /// gamut's `DngDecoder::decode` — a whole-file decode, preview and metadata included.
    Gamut,
    /// The Adobe DNG SDK: parse → build negative → `ReadStage1Image`, over the same bytes, from
    /// memory, exporting nothing.
    AdobeSdk,
}

impl std::fmt::Display for DngImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `pad`, not `write_str`, so a width on the formatter survives — see `Photometry`.
        f.pad(match self {
            DngImpl::Gamut => "gamut",
            DngImpl::AdobeSdk => "adobe-sdk",
        })
    }
}

/// One `decode_dng` measurement: a matrix cell decoded by one implementation.
#[derive(Clone, Copy)]
struct DngJob {
    case: Case,
    imp: DngImpl,
}

impl std::fmt::Display for DngJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(&format!("{} {}", self.case, self.imp))
    }
}

/// Every case, each decoded by both implementations. The two arms of a pair are emitted next to
/// each other *and* sort next to each other by name, so divan measures them back to back.
const DNG_JOBS: [DngJob; CASES.len() * 2] = dng_jobs();

/// Builds [`DNG_JOBS`]: the cross product of [`CASES`] with both implementations.
const fn dng_jobs() -> [DngJob; CASES.len() * 2] {
    let mut jobs = [DngJob {
        case: CASES[0],
        imp: DngImpl::Gamut,
    }; CASES.len() * 2];
    let mut index = 0;
    while index < CASES.len() {
        jobs[index * 2] = DngJob {
            case: CASES[index],
            imp: DngImpl::Gamut,
        };
        jobs[index * 2 + 1] = DngJob {
            case: CASES[index],
            imp: DngImpl::AdobeSdk,
        };
        index += 1;
    }
    jobs
}

/// Which implementation one `decode_lossless_jpeg` measurement runs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum JpegImpl {
    /// gamut's `lossless_jpeg::decode`, filling one `Vec<u16>`.
    Gamut,
    /// The Adobe DNG SDK's `DecodeLosslessJPEG<Scalar>`, exported across the FFI boundary.
    AdobeSdk,
    /// The same SDK decode, stopping at the spool buffer. The export path is the only difference
    /// between this arm and `AdobeSdk`, so the gap between them bounds its cost — a magnitude, not
    /// a signed price: it sits at this harness's measurement floor.
    AdobeSdkNoExport,
}

impl std::fmt::Display for JpegImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(match self {
            JpegImpl::Gamut => "gamut",
            JpegImpl::AdobeSdk => "adobe-sdk",
            JpegImpl::AdobeSdkNoExport => "adobe-sdk-no-export",
        })
    }
}

/// One `decode_lossless_jpeg` measurement: a photometry decoded by one implementation.
#[derive(Clone, Copy)]
struct JpegJob {
    photometry: Photometry,
    imp: JpegImpl,
}

impl std::fmt::Display for JpegJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(&format!("{} {}", self.photometry, self.imp))
    }
}

/// Both photometries, each decoded by all three arms, adjacent by construction and by name sort.
const JPEG_JOBS: [JpegJob; PHOTOMETRIES.len() * 3] = jpeg_jobs();

/// Builds [`JPEG_JOBS`]: the cross product of [`PHOTOMETRIES`] with all three arms.
const fn jpeg_jobs() -> [JpegJob; PHOTOMETRIES.len() * 3] {
    let mut jobs = [JpegJob {
        photometry: PHOTOMETRIES[0],
        imp: JpegImpl::Gamut,
    }; PHOTOMETRIES.len() * 3];
    let arms = [
        JpegImpl::Gamut,
        JpegImpl::AdobeSdk,
        JpegImpl::AdobeSdkNoExport,
    ];
    let mut photometry = 0;
    while photometry < PHOTOMETRIES.len() {
        let mut arm = 0;
        while arm < arms.len() {
            jobs[photometry * arms.len() + arm] = JpegJob {
                photometry: PHOTOMETRIES[photometry],
                imp: arms[arm],
            };
            arm += 1;
        }
        photometry += 1;
    }
    jobs
}

/// Adobe's own JPEG XL sample: a `LinearRaw` JXL raw written by Adobe's pipeline rather than by
/// gamut's encoder, which the oracle's build extracts from the committed SDK ZIP. Decoding it
/// needs no encoder at all, so this row stands whatever the `jxl-encode` gate says.
const ADOBE_JXL_SAMPLE: &str = "01_jxl_linear_raw_integer.dng";

/// How many real-camera frames `decode_dng_file` measures when the corpus is checked out: the
/// largest files both implementations decode. Large is the point — cache behaviour and strip/tile
/// geometry at a full sensor frame are what the synthetic 512×384 fixture cannot show — and three
/// keeps an opt-in run to minutes rather than the whole ~178 MiB corpus.
const REAL_CAMERA_COUNT: usize = 3;

/// Samples divan takes per `decode_dng_file` arm. A real frame decodes in a large fraction of a
/// second, so divan's default of 100 would spend minutes per arm for no gain in resolution.
const FILE_SAMPLE_COUNT: u32 = 20;

/// Where the real-camera corpus is checked out: the `gamut-dng-samples` submodule, the same
/// location `tooling/gamut-dng-real-conformance` reads.
fn real_camera_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../third_party/gamut-dng-samples")
}

/// A DNG read from disk, already accepted by both implementations, with the raw volume each one
/// reported. Everything here is computed once, before divan starts.
struct FileCase {
    /// Where it came from: `adobe/<name>` or `camera/<path in the corpus>`.
    label: String,
    /// The file's bytes, which every timed region decodes from memory.
    bytes: Vec<u8>,
    /// Raw sample volume gamut's decoder produced, in bytes (`samples × 2`).
    gamut_raw_bytes: usize,
    /// Raw sample volume the SDK's stage-1 read produced, in bytes (`samples × 2`).
    sdk_raw_bytes: usize,
}

impl FileCase {
    /// Reads `path` and decodes it once with each implementation, outside any timed region.
    /// Returns why not when the file cannot be read or either implementation refuses it, so the
    /// fixture table can name a skipped row instead of dropping it silently.
    fn load(label: String, path: &Path) -> Result<FileCase, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("{label}: {e}"))?;
        let gamut = DngDecoder::new()
            .decode(&bytes)
            .map_err(|e| format!("{label}: gamut decode: {e}"))?;
        let sdk = gamut_dng_oracle::decode_dng_in_memory(&bytes)
            .map_err(|e| format!("{label}: SDK decode: {e}"))?;
        Ok(FileCase {
            label,
            gamut_raw_bytes: size_of_val(gamut.raw.samples()),
            sdk_raw_bytes: sdk.samples * size_of::<u16>(),
            bytes,
        })
    }
}

/// The file-backed fixtures, and a line per file or source that yields no row.
struct FileCases {
    /// Every file both implementations decode, Adobe's sample first.
    cases: Vec<FileCase>,
    /// Why each absent row is absent, printed under the fixture table.
    skipped: Vec<String>,
}

/// Loaded on first use — which [`print_fixture_table`] makes happen before divan starts, so no
/// file is read while divan is timing anything.
static FILE_CASES: LazyLock<FileCases> = LazyLock::new(load_file_cases);

/// Loads Adobe's JPEG XL sample and, where the corpus is checked out, its [`REAL_CAMERA_COUNT`]
/// largest frames that both implementations decode.
///
/// An absent corpus is a skip, not a failure: the corpus is an opt-in ~178 MiB submodule, and a
/// default `cargo bench` must stay runnable without it. The skip is printed, not silent.
fn load_file_cases() -> FileCases {
    let mut cases = Vec::new();
    let mut skipped = Vec::new();

    let adobe = gamut_dng_oracle::sample_files_dir().join(ADOBE_JXL_SAMPLE);
    match FileCase::load(format!("adobe/{ADOBE_JXL_SAMPLE}"), &adobe) {
        Ok(case) => cases.push(case),
        Err(why) => skipped.push(why),
    }

    let corpus = real_camera_dir();
    if !corpus.join("MANIFEST.toml").is_file() {
        skipped.push(format!(
            "camera/*: real-camera corpus not checked out at {} (mise run fetch-dng-samples)",
            corpus.display()
        ));
        return FileCases { cases, skipped };
    }
    let mut files: Vec<(u64, PathBuf)> = dng_files_under(&corpus)
        .into_iter()
        .filter_map(|path| Some((std::fs::metadata(&path).ok()?.len(), path)))
        .collect();
    // Largest first; the path breaks ties so the selection is the same on every machine.
    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut taken = 0;
    for (_, path) in files {
        if taken == REAL_CAMERA_COUNT {
            break;
        }
        let relative = path.strip_prefix(&corpus).unwrap_or(&path);
        match FileCase::load(format!("camera/{}", relative.display()), &path) {
            Ok(case) => {
                cases.push(case);
                taken += 1;
            }
            Err(why) => skipped.push(why),
        }
    }
    FileCases { cases, skipped }
}

/// Every `*.dng` (any case) under `root`, recursively. Unreadable directories are passed over: a
/// file this cannot list is a file the bench does not measure, never a reason to stop it.
fn dng_files_under(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for path in entries.filter_map(|entry| entry.ok().map(|entry| entry.path())) {
            if path.is_dir() {
                pending.push(path);
            } else if path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("dng"))
            {
                found.push(path);
            }
        }
    }
    found
}

/// One `decode_dng_file` measurement: a file-backed fixture decoded by one implementation.
#[derive(Clone, Copy)]
struct FileJob {
    case: &'static FileCase,
    imp: DngImpl,
}

impl std::fmt::Display for FileJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(&format!("{} {}", self.case.label, self.imp))
    }
}

/// Every loaded file, each decoded by both implementations, adjacent by construction and by name
/// sort — the same pairing [`DNG_JOBS`] makes. Empty rows are simply absent.
fn file_jobs() -> Vec<FileJob> {
    FILE_CASES
        .cases
        .iter()
        .flat_map(|case| [DngImpl::Gamut, DngImpl::AdobeSdk].map(|imp| FileJob { case, imp }))
        .collect()
}

/// A sensor-like frame: a smooth illumination falloff, a per-CFA-channel gain, and deterministic
/// per-photosite noise, interleaved across `planes`.
///
/// The noise is the point. Raw sensor data is hard to compress *because* of it, so a clean
/// synthetic gradient would flatter every compressor equally and measure nothing a real file
/// would see. This follows the same model `benches/compression.rs` uses for its packed payloads;
/// the two cannot share one generator because they produce different things — that bench needs
/// packed bytes for `gamut-deflate`, this one needs `u16` samples for a [`RawImage`].
fn sensor_samples(planes: u32) -> Vec<u16> {
    /// Per-colour gain, R/G/B, as a fraction of full scale.
    const GAINS: [f64; 3] = [0.42, 0.70, 0.31];

    let max = f64::from((1u32 << BITS) - 1);
    let mut samples = Vec::with_capacity((WIDTH * HEIGHT * planes) as usize);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            for plane in 0..planes {
                // Cosine-fourth-law-ish falloff from the frame centre.
                let dx = f64::from(x) / f64::from(WIDTH) - 0.5;
                let dy = f64::from(y) / f64::from(HEIGHT) - 0.5;
                let falloff = 1.0 - 1.4 * (dx * dx + dy * dy);
                // A green photosite collects roughly twice what red and blue do. In a CFA mosaic
                // the RGGB tile decides the colour; in a linear image the interleaved plane does.
                let colour = if planes == 1 {
                    match (x % 2, y % 2) {
                        (0, 0) => 0, // R
                        (1, 1) => 2, // B
                        _ => 1,      // G
                    }
                } else {
                    plane as usize
                };
                let gain = GAINS[colour.min(GAINS.len() - 1)];
                // Deterministic shot-noise stand-in, a few percent of full scale.
                let hash = ((y * WIDTH + x) * planes + plane).wrapping_mul(2_654_435_761) >> 11;
                let noise = f64::from(hash % 2048) / 2048.0 - 0.5;
                let value = (falloff * gain + noise * 0.05).clamp(0.0, 1.0) * max;
                samples.push(value as u16);
            }
        }
    }
    samples
}

/// A plausible camera colour profile (an illustrative XYZ→camera matrix under D65). The encoder
/// needs one; nothing here measures it.
fn profile() -> CameraProfile {
    CameraProfile::new(
        "gamut BenchCam",
        [
            0.6722, -0.0635, -0.0963, -0.4287, 1.2460, 0.2028, -0.0908, 0.2162, 0.5668,
        ],
        CalibrationIlluminant::D65,
        [0.5128, 1.0, 0.7059],
    )
    .expect("valid profile")
}

/// The encoder under test, at one compression scheme. Everything else is the encoder's default.
fn encoder(compression: Compression) -> DngEncoder {
    DngEncoder::new().with_compression(compression)
}

/// Raw sample volume in bytes for a photometry: `w × h × planes × 2`.
fn raw_bytes(photometry: Photometry) -> usize {
    (WIDTH * HEIGHT * photometry.planes()) as usize * size_of::<u16>()
}

/// IFD-0 preview samples for one of these fixtures: `⌊w/2⌋ × ⌊h/2⌋ × 3`, RGB (the encoder
/// collapses each `2 × 2` block into one pixel, and always writes the preview uncompressed).
fn preview_samples() -> usize {
    (WIDTH / 2 * (HEIGHT / 2) * 3) as usize
}

/// Bytes of preview `DngEncoder::encode` derives: one byte per sample, because `preview::
/// raw_preview` builds a `Vec<u8>` and the encoder writes it at 8 bits per sample.
fn preview_encode_bytes() -> usize {
    preview_samples() * size_of::<u8>()
}

/// Bytes of preview `DngDecoder::decode` **materialises**: two per sample, not one.
///
/// The preview is *stored* at 8 bits, but the decoder surfaces every sub-image as
/// `SubImageData::Decoded(Vec<u16>)` — one `u16` per sample whatever the IFD's bit depth — so the
/// buffer it allocates, fills and tears down is twice the stored size. Modelling the stored width
/// here would under-state the work by half and, because the preview sits on gamut's side of the
/// comparison, would make gamut look slower than it is.
///
/// This is the whole of the `decode_dng` gamut-versus-SDK asymmetry that is attributable to
/// pixels; the rest is IFD and metadata reconstruction, which does not scale with the frame. It
/// is what [`Case::gamut_decode_bytes`] adds to the raw volume where the counter rule allows.
fn preview_decode_bytes() -> usize {
    preview_samples() * size_of::<u16>()
}

/// A bare lossless-JPEG (SOF3) stream over one photometry's fixture samples — one component for
/// CFA, three interleaved for `LinearRaw`.
fn lossless_jpeg_stream(photometry: Photometry) -> Vec<u8> {
    let planes = photometry.planes() as usize;
    lossless_jpeg::encode(
        &sensor_samples(photometry.planes()),
        WIDTH as usize,
        HEIGHT as usize,
        planes,
        BITS,
    )
    .expect("fixture lossless-JPEG stream encodes")
}

/// Prints the byte volumes each measured region moves, so a throughput number can be read against
/// what it is a throughput *of* — including the preview volume that separates gamut's whole-file
/// decode from the SDK's stage-1 read, and, per case, whether that volume is charged into gamut's
/// counter or withheld because charging it would be a bound rather than a measurement.
fn print_fixture_table() {
    println!(
        "\nDNG codec fixtures, {WIDTH}x{HEIGHT} at {BITS}-bit (bytes):\n\n\
         {:<26} {:>12} {:>12} {:>8} {:>12} {:>12} {:>8} {:>11}",
        "case",
        "raw samples",
        "encoded DNG",
        "of raw",
        "preview",
        "decode vol.",
        "/ raw",
        "correction"
    );
    for case in CASES {
        let raw = case.raw_bytes();
        let encoded = case.encoded().len();
        let preview = preview_decode_bytes();
        let decode_volume = case.gamut_decode_bytes();
        let correction = if case.preview_correction_is_measured() {
            "applied"
        } else {
            "SUPPRESSED"
        };
        println!(
            "{case:<26} {raw:>12} {encoded:>12} {:>7.1}% {preview:>12} {decode_volume:>12} \
             {:>8.3} {correction:>11}",
            encoded as f64 / raw as f64 * 100.0,
            decode_volume as f64 / raw as f64,
        );
    }
    print_file_table();
    print_zlib_identity();
    print!("{FIXTURE_TABLE_EPILOGUE}");
}

/// Prints the file-backed fixtures `decode_dng_file` reads — each implementation's own raw
/// volume, which is its counter — and names every row that is absent and why. Forcing
/// [`FILE_CASES`] here is also what reads every file before divan starts timing.
fn print_file_table() {
    println!(
        "\nDNG files decoded from disk (bytes; read before timing starts):\n\n\
         {:<48} {:>12} {:>12} {:>12}",
        "file", "file size", "gamut raw", "SDK raw"
    );
    for case in &FILE_CASES.cases {
        println!(
            "{:<48} {:>12} {:>12} {:>12}",
            case.label,
            case.bytes.len(),
            case.gamut_raw_bytes,
            case.sdk_raw_bytes
        );
    }
    for why in &FILE_CASES.skipped {
        println!("skipped: {why}");
    }
}

/// Prints which zlib the reference arm's Deflate decode calls, and refuses to go on if that zlib
/// is a shared library rather than the copy statically linked into this executable.
///
/// The oracle links the stock zlib built from `third_party/zlib` statically (issue #618), so
/// `zlibVersion` resolves in this executable itself. A shared library — a `libz.so` the loader
/// found on the platform or, through cargo's `LD_LIBRARY_PATH`, under `target/` — means the pin
/// has regressed and the `*/deflate` rows would measure whichever library the launcher happened
/// to supply, which moved the reference arm 1.2–1.3× when it was last allowed to. Publishing such
/// a ratio is worse than publishing none, so this stops the run.
///
/// The test is whether the reported path names a shared object, not whether it equals this
/// executable's path: for the executable itself the loader may report only `argv[0]`, which the
/// shim hands over unresolved when `realpath` fails (a bench launched through `PATH`, or with a
/// rewritten `argv[0]`), so comparing it with `current_exe()` would stop a correctly pinned run.
/// A shared libz is reported as the file the loader opened, so its library name survives.
fn print_zlib_identity() {
    println!(
        "\nThe SDK's Deflate arm calls the pinned stock zlib (third_party/zlib, statically \
         linked): {}.",
        gamut_dng_oracle::zlib_identity()
    );
    if let Some(path) = gamut_dng_oracle::zlib_path() {
        assert!(
            !names_shared_object(&path),
            "the SDK's zlib resolved in the shared library {}, not in this executable: the \
             oracle's static zlib link has regressed to a shared libz, so the */deflate rows \
             would not be reproducible",
            path.display()
        );
    }
}

/// Whether `path`'s file name is a shared library's: `*.so` or versioned `*.so.N…` (ELF),
/// `*.dylib` (Mach-O) or `*.dll` (PE). An executable — the static pin's answer — is none of these.
fn names_shared_object(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|name| {
            name.ends_with(".so")
                || name.contains(".so.")
                || name.ends_with(".dylib")
                || name.ends_with(".dll")
        })
}

/// What an operator has to know to read the table above and the divan output below it, printed
/// where they are read rather than only in this file's header: what the preview correction is,
/// which rows it is applied to, why it is withheld on the rest, and that a published ratio is the
/// mean of two runs taken in opposite arm orders.
const FIXTURE_TABLE_EPILOGUE: &str = "
`decode_dng gamut` decodes the whole file — raw image, the IFD-0 preview and the metadata;
`decode_dng adobe-sdk` reads the raw image only, from the same bytes, and exports nothing. The
`preview` column is the volume gamut's decoder materialises for that preview (two bytes per
sample: every sub-image surfaces as a `Vec<u16>`, whatever the stored depth).

On the `correction: applied` rows that volume is added to gamut's counter, so the median-time
column is the uncorrected comparison and the throughput column is the preview-corrected one. On
the `correction: SUPPRESSED` rows it is not, and BOTH columns are uncorrected. Correcting there
would charge preview bytes at the compressed raw path's per-byte rate — arithmetic that credits
gamut with more than the preview costs, and so yields a lower bound on gamut's true ratio, not a
measurement of it. No number is printed for it, because a printed number is read as measured. Read
the compressed rows as: gamut's figure includes preview and metadata work the SDK arm does not do,
by an amount this harness does not measure.

On the `*/deflate` rows neither arm's inflate is gamut's own: `gamut-deflate` is encoder-only, so
this crate inflates with miniz_oxide, and the SDK calls zlib because it includes <zlib.h>
unconditionally. Read those rows as miniz_oxide against stock zlib. Both are pinned: miniz_oxide
by Cargo.lock, and zlib by the third_party/zlib submodule commit, built by zlib-oracle and linked
statically into this executable -- so the run launched through cargo and the same binary run
directly measure the same library, on any machine. The zlib printed above is that build; do not
compare a Deflate ratio against one taken before the pin (issue #618), when the SDK called
whichever shared libz the loader found. Every row runs only code this repository builds or pins.

`decode_dng_file` decodes files no gamut encoder wrote: Adobe's JPEG XL sample, and the largest
real-camera frames when the corpus is checked out. Its counters are each implementation's own raw
volume from the table above, and no preview correction is applied on any of its rows: a file's
preview may be JPEG-compressed or absent, so charging it at the raw path's rate is not a
measurement. Read those rows like the SUPPRESSED ones: gamut's figure includes preview and
metadata work the SDK arm does not do.

`decode_lossless_jpeg` needs no correction: same stream in, same samples out, one counter for all
three arms. The gap between its `adobe-sdk` and `adobe-sdk-no-export` arms bounds the FFI export
path — a magnitude, not a signed cost; it sits at the measurement floor.

Adjacent is not simultaneous: divan cannot interleave a pair per sample, so one arm always runs
first and the bias points one way for a whole run. Take one run each way and publish the mean:

    cargo bench -p gamut-dng --bench codec                   # reference arm first
    cargo bench -p gamut-dng --bench codec -- --sortr name   # gamut arm first

";

/// Encode: `DngEncoder::encode` over a prepared raw image and profile.
///
/// Timed: preview derivation, sample packing and compression, IFD-tree layout, and the growth and
/// teardown of the output buffer. Not timed: building the raw image and the profile. The counter
/// is the raw volume plus the preview the encoder derives (at the 8-bit width it derives it).
#[divan::bench(args = CASES)]
fn encode_gamut(bencher: Bencher, case: Case) {
    let raw = case.raw();
    let profile = profile();
    let encoder = encoder(case.compression);
    bencher
        .counter(BytesCount::new(case.gamut_encode_bytes()))
        .bench_local(|| {
            let mut out = Vec::new();
            encoder
                .encode(black_box(&raw), black_box(&profile), &mut out)
                .expect("encode");
            drop(black_box(out));
        });
}

/// Whole-file DNG decode, both implementations, interleaved: gamut's `DngDecoder::decode` and the
/// Adobe DNG SDK's parse → `ReadStage1Image`, over the *same* in-memory bytes.
///
/// Timed, gamut: container parse, raw-image decode, IFD-0 preview decode, metadata
/// reconstruction, and the teardown of everything decoded. Timed, SDK: everything the reference
/// implementation does to materialise the raw image, including the negative's teardown, which
/// runs inside the C++ call. Not timed, either side: producing the DNG bytes — and by
/// construction of [`gamut_dng_oracle::decode_dng_in_memory`], no temporary file and no FFI
/// export copy.
///
/// The two counters differ by the preview volume on the uncompressed rows and are identical on
/// the compressed ones, where that correction is suppressed: see this file's counter rule.
#[divan::bench(args = DNG_JOBS)]
fn decode_dng(bencher: Bencher, job: DngJob) {
    let bytes = job.case.encoded();
    match job.imp {
        DngImpl::Gamut => {
            let decoder = DngDecoder::new();
            bencher
                .counter(BytesCount::new(job.case.gamut_decode_bytes()))
                .bench_local(|| {
                    drop(black_box(
                        decoder.decode(black_box(&bytes)).expect("decode"),
                    ));
                });
        }
        DngImpl::AdobeSdk => {
            bencher
                .counter(BytesCount::new(job.case.raw_bytes()))
                .bench_local(|| {
                    // No `drop` to place: `DecodedExtent` is plain `Copy` data, because the SDK's
                    // own teardown already ran — inside the call, and so inside this timed region.
                    black_box(
                        gamut_dng_oracle::decode_dng_in_memory(black_box(&bytes))
                            .expect("SDK decode"),
                    );
                });
        }
    }
}

/// Whole-file decode of DNGs gamut did not write, both implementations, interleaved: Adobe's
/// JPEG XL sample and, when the real-camera corpus is checked out, its largest frames.
///
/// Timed: exactly what [`decode_dng`] times, for each arm. Not timed: reading the file, and the
/// one decode per implementation that selected it and measured its raw volume — all of which
/// happened before divan started. With the corpus absent this benchmark has only Adobe's sample.
#[divan::bench(args = file_jobs(), sample_count = FILE_SAMPLE_COUNT)]
fn decode_dng_file(bencher: Bencher, job: FileJob) {
    let bytes = job.case.bytes.as_slice();
    match job.imp {
        DngImpl::Gamut => {
            let decoder = DngDecoder::new();
            bencher
                .counter(BytesCount::new(job.case.gamut_raw_bytes))
                .bench_local(|| {
                    drop(black_box(decoder.decode(black_box(bytes)).expect("decode")));
                });
        }
        DngImpl::AdobeSdk => {
            bencher
                .counter(BytesCount::new(job.case.sdk_raw_bytes))
                .bench_local(|| {
                    // `DecodedExtent` is `Copy`; the SDK's teardown already ran inside the call.
                    black_box(
                        gamut_dng_oracle::decode_dng_in_memory(black_box(bytes))
                            .expect("SDK decode"),
                    );
                });
        }
    }
}

/// Bare lossless-JPEG (SOF3) codestream decode, all three arms, interleaved: gamut, the Adobe DNG
/// SDK exporting its samples across the FFI boundary, and the same SDK decode stopping at the
/// spool buffer.
///
/// Timed: marker parse, Huffman and predictor decode, and the teardown of the sample buffer —
/// plus, for `adobe-sdk`, the export path (spool → `malloc`d buffer → `Vec`). Not timed:
/// encoding the stream. The export path is the only difference between the two SDK arms, so the
/// gap between them bounds this file's one remaining bias rather than leaving it described.
#[divan::bench(args = JPEG_JOBS)]
fn decode_lossless_jpeg(bencher: Bencher, job: JpegJob) {
    let stream = lossless_jpeg_stream(job.photometry);
    let expected = (WIDTH * HEIGHT * job.photometry.planes()) as usize;
    let bencher = bencher.counter(BytesCount::new(raw_bytes(job.photometry)));
    match job.imp {
        JpegImpl::Gamut => bencher.bench_local(|| {
            drop(black_box(
                lossless_jpeg::decode(black_box(&stream)).expect("decode"),
            ));
        }),
        JpegImpl::AdobeSdk => bencher.bench_local(|| {
            drop(black_box(
                gamut_dng_oracle::decode_lossless_jpeg(black_box(&stream), expected)
                    .expect("SDK decode"),
            ));
        }),
        JpegImpl::AdobeSdkNoExport => bencher.bench_local(|| {
            // Returns a `usize`; there is nothing allocated for the caller to release, which is
            // the whole point of this arm.
            black_box(
                gamut_dng_oracle::decode_lossless_jpeg_extent(black_box(&stream), expected)
                    .expect("SDK decode"),
            );
        }),
    }
}
