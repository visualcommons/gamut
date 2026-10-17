//! `gamut convert` — decode an image and re-encode it with a gamut codec.

use std::io::Write;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use clap::{Args, ValueEnum};
use gamut::avif::{AvifEncodeReport, AvifEncoder, Speed as AvifSpeed};
use gamut::color::{BitDepth, ChromaSubsampling, MatrixCoefficients};
use gamut::core::{Dimensions, EncodeImage, ImageRef, Rgb8, Rgba8};
use gamut::exif::{Exif, ExifTag, ExifWriter, IfdKind, Value as ExifValue};
use gamut::icc::{ColorSpace as IccColorSpace, ProfileHeader};
use gamut::jpeg::{ChromaSubsampling as JpegChroma, JpegEncoder};
use gamut::jxl::{
    ColorSpec as JxlColorSpec, Container as JxlContainer, Distance as JxlDistance,
    Effort as JxlEffort, JxlEncoder, ModularMode as JxlModularMode, Orientation as JxlOrientation,
};
use gamut::metadata::EncodedMetadata;
use gamut::metadata::capability::{Carrier, Direction, Format as MetadataFormat, supports};
use gamut::png::{PngEncoder, PngMetadata, Preset as PngCodecPreset};
use gamut::tiff::{Compression as TiffCompression, IFD0_EXIF_TAGS, TiffEncoder};
use gamut::webp::{Effort as WebpEffort, NearLossless as WebpNearLossless, WebpEncoder};

use crate::error::CliError;
use crate::input::{
    InputMetadata, NativeImage, STDIO, decode_bytes, decode_native, read_input, read_metadata,
};

/// Arguments for `gamut convert`.
#[derive(Args)]
pub(crate) struct ConvertArgs {
    /// Input images (PNG, JPEG, PPM/P6, WebP, or JPEG XL), or `-` for standard input. Without
    /// `-o`, the last of two or more paths is the output, as with `cp`: `gamut convert in.png
    /// out.avif` converts one file, `gamut convert a.png b.png outdir` converts two into a
    /// directory. A lone input with no output is written beside itself, named for the format.
    #[arg(required = true, value_name = "PATH")]
    paths: Vec<PathBuf>,
    /// Output file, directory, or `-` for standard output. With several inputs it is a directory
    /// (created if missing) receiving each input's file name with the format's extension; with one
    /// input it is a directory if one exists there, and otherwise the output file.
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,
    /// Output format. Defaults to the output file's extension when there is a single output file,
    /// and to AVIF otherwise (a directory, standard output, or beside the input).
    #[arg(long, value_enum)]
    format: Option<OutputFormat>,
    /// Replace output files that already exist. Without it, an existing output is left alone and
    /// counted as a failed conversion.
    #[arg(short = 'f', long)]
    overwrite: bool,
    /// Do not print the per-file summary lines. Errors are still reported on stderr.
    #[arg(short, long)]
    quiet: bool,
    /// Deprecated: use `--avif-lossy`. Any nonzero value selects lossy AVIF at `--quality`, exactly
    /// as `--avif-lossy` does, and prints a deprecation warning; `0` changes nothing.
    #[arg(long, default_value_t = 0)]
    qindex: u8,
    /// Encode lossy (WebP VP8 intra) instead of lossless. For AVIF, select lossy with
    /// `--avif-lossy`.
    #[arg(long)]
    lossy: bool,
    /// Encode lossy AVIF at `--quality` instead of the lossless default. Ignored for other output
    /// formats.
    #[arg(long)]
    avif_lossy: bool,
    /// AVIF encoder speed, 0 (the full search) to 3 (the fastest): `gamut-av1`'s presets
    /// thorough, balanced (1, the default), fast and fastest. A time control, not a size one — a
    /// faster preset can come out smaller or larger. Only lossy AVIF is searched; lossless codes
    /// the same bytes at every speed. Ignored for other output formats.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(0..=3))]
    avif_speed: u8,
    /// Most threads one AVIF encode may use; `0` means every available core. The output is the
    /// same at every count. Several inputs already convert in parallel, one per core, so each of
    /// them multiplies this. Ignored for other output formats.
    #[arg(long, default_value_t = 1)]
    avif_threads: usize,
    /// Lossy quality of the AVIF alpha channel, 0–100, independent of the colour's `--quality`
    /// (which it follows when omitted). Ignored by lossless AVIF, by input without alpha, and for
    /// other output formats.
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=100))]
    avif_alpha_quality: Option<u8>,
    /// Replace the colour hidden under transparent pixels with a bleed of the visible edge before
    /// a lossy AVIF encode, so no bits are spent on colour nobody sees. The displayed image does
    /// not change. Ignored by lossless AVIF, by input without alpha, and for other output formats.
    #[arg(long)]
    avif_clean_alpha: bool,
    /// AVIF colour matrix for lossy encoding: `identity` (RGB planes, forces 4:4:4), `bt601`,
    /// `bt709` (the default) or `bt2020`. Lossless AVIF always uses `identity`. Ignored for other
    /// output formats.
    #[arg(long = "avif-matrix", value_enum)]
    avif_matrix: Option<AvifMatrix>,
    /// AVIF coded bit depth: `8`, `10` or `12`. Defaults to the input's own — 8 for an 8-bit
    /// input, 12 for a 16-bit one; `8` is refused for a 16-bit input. An 8-bit input coded at 10
    /// or 12 bits stays exact under lossless and rounds less under lossy. Ignored for other output
    /// formats.
    #[arg(long = "avif-depth", value_enum)]
    avif_depth: Option<AvifDepth>,
    /// AVIF chroma subsampling for lossy colour: `444` (none), `422`, or `420` (the default).
    /// Lossless AVIF and the `identity` matrix always keep 4:4:4, and input with alpha is coded
    /// 4:4:4 whatever is set here. Ignored for other output formats.
    #[arg(long = "avif-chroma", value_enum)]
    avif_chroma: Option<AvifChroma>,
    /// Lossy quality, 0–100 (higher is better but larger). Used with WebP `--lossy`, lossy AVIF,
    /// and JPEG (which is always lossy).
    #[arg(long, default_value_t = 75)]
    quality: u8,
    /// WebP encoder effort, 0 (fastest) to 6 (densest); libwebp's default method is 4. Applies to
    /// both lossless and lossy WebP. Ignored for other output formats.
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u8).range(0..=6))]
    webp_effort: u8,
    /// WebP near-lossless preprocessing on libwebp's scale: 0 (most loss) to 99, or 100 / omitted
    /// for off. Quantizes the source in textured regions before *lossless* coding, leaving alpha
    /// exact. Ignored with `--lossy` and for other output formats.
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=100))]
    webp_near_lossless: Option<u8>,
    /// JPEG chroma subsampling for colour input: `444` (none), `422` (halve horizontally), or `420`
    /// (halve both, the default). Ignored for other output formats and for grayscale.
    #[arg(long = "jpeg-subsampling", value_enum, default_value = "420")]
    jpeg_subsampling: JpegSubsampling,
    /// JPEG restart interval in MCUs: insert an RSTn marker every N MCUs so a decoder can resync
    /// (`0`, the default, disables restarts). Ignored for other output formats.
    #[arg(long = "jpeg-restart-interval", default_value_t = 0)]
    jpeg_restart_interval: u16,
    /// Encode JPEG output progressively (SOF2, spectral-band scans with successive approximation)
    /// instead of baseline sequential. Ignored for other output formats.
    #[arg(long = "jpeg-progressive")]
    jpeg_progressive: bool,
    /// Compress TIFF output with PackBits run-length encoding instead of storing it uncompressed.
    #[arg(long)]
    packbits: bool,
    /// PNG encoder effort, 0 (fastest) to 3 (smallest): one rung of `gamut-png`'s ladder, setting
    /// the five knobs that trade encoding time for output size (compression level, DEFLATE
    /// refinement passes, filter strategy, optimal-parse span, and lossless colour-type
    /// reduction). 0 is greedy matching on one fixed filter; 1 is the library default; 2, the
    /// default here, adds the optimal parse and lossless reduction; 3 adds the whole-image filter
    /// search, about six times slower than 2 for a few percent. Ignored for other output formats.
    #[arg(long, default_value_t = PNG_EFFORT_DEFAULT,
        value_parser = clap::value_parser!(u8).range(0..=i64::from(PNG_EFFORT_MAX)))]
    png_effort: u8,
    /// PNG DEFLATE optimal-parse refinement passes, overriding the one knob `--png-effort` chose
    /// for it (0 = lazy parse only; zopfli's default budget is 15). Only the best compression
    /// level consults it, so it does nothing under `--png-effort 0` or `1`. Omitting it keeps the
    /// rung's own budget. Ignored for other output formats.
    #[arg(long)]
    png_refinement_passes: Option<u8>,
    /// JPEG XL Butteraugli distance for lossy encoding (~1.0 = visually lossless, up to 25.0).
    /// Supplying it selects lossy JXL; omitting it keeps the lossless default. Ignored for other
    /// output formats.
    #[arg(long)]
    jxl_distance: Option<f32>,
    /// JPEG XL encoder effort, 1 (fastest) to 10 (densest); libjxl's default is 7. Ignored for
    /// other output formats.
    #[arg(long, default_value_t = 7, value_parser = clap::value_parser!(u8).range(1..=10))]
    jxl_effort: u8,
    /// JPEG XL coding tool: `auto` (the default) lets libjxl choose, `vardct` forces the DCT path,
    /// `modular` forces the modular path. `vardct` is rejected for lossless JPEG XL (which is always
    /// modular). Ignored for other output formats.
    #[arg(long = "jxl-modular", value_enum, default_value = "auto")]
    jxl_modular: JxlModular,
    /// Emit JPEG XL in the ISO BMFF (`.jxl` box) container instead of a bare codestream. Ignored
    /// for other output formats.
    #[arg(long)]
    jxl_container: bool,
    /// Drop the input's metadata instead of carrying it into the output. By default the Exif,
    /// XMP packet and ICC profile of a PNG, JPEG, WebP or JPEG XL input are carried into every
    /// output that can hold them; a stripped file is smaller, an unstripped one is
    /// colour-accurate, so the default is the one that loses nothing. The Exif claims a
    /// conversion can make false are rewritten to match the output: `Orientation` (1 when the
    /// decoder already oriented the pixels, as JPEG XL's does), `PixelXDimension` /
    /// `PixelYDimension` (the output's size) and `ColorSpace` (sRGB when the pixels were decoded
    /// into sRGB, uncalibrated when their ICC profile is not carried). A PNG input re-encoded to
    /// PNG also keeps its text annotations and colour chunks. Anything that is not carried — a
    /// block the output cannot hold, an ICC profile that cannot describe the output's samples
    /// (a grey profile on colour output), the C2PA manifest store (signed over the bytes of the
    /// file it was made for, so never carried) — and anything carried in a shape the PNG
    /// specification does not endorse is reported on stderr rather than passed over in silence.
    #[arg(long)]
    strip_metadata: bool,
}

/// Output container/codec for `gamut convert`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum OutputFormat {
    /// AVIF — lossless or lossy intra via `--avif-lossy`; keeps the input's alpha and grey, and
    /// codes a 16-bit input at 12 bits.
    Avif,
    /// WebP — lossless (VP8L) or lossy (VP8, with `--lossy`); transparency is preserved.
    Webp,
    /// TIFF (8-bit RGB; uncompressed, or PackBits with `--packbits`).
    Tiff,
    /// PNG — lossless; transparency preserved, with automatic lossless colour-type reduction at
    /// the default `--png-effort`.
    Png,
    /// JPEG XL — lossless by default, or lossy at `--jxl-distance`; transparency preserved.
    Jxl,
    /// JPEG (JPEG-1 baseline) — always lossy at `--quality`, YCbCr with `--jpeg-subsampling`.
    Jpeg,
}

/// Chroma subsampling for JPEG YCbCr output.
#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum JpegSubsampling {
    /// 4:4:4 — no chroma subsampling (full-resolution Cb/Cr).
    #[value(name = "444")]
    S444,
    /// 4:2:2 — halve chroma horizontally.
    #[value(name = "422")]
    S422,
    /// 4:2:0 — halve chroma both horizontally and vertically.
    #[value(name = "420")]
    S420,
}

/// CICP matrix for lossy AVIF output — the four [`MatrixCoefficients`] the AVIF encoder codes.
#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum AvifMatrix {
    /// Identity: the R'G'B' samples as GBR planes, no luma–chroma decorrelation; forces 4:4:4.
    Identity,
    /// BT.601 YCbCr.
    Bt601,
    /// BT.709 YCbCr; the lossy default.
    Bt709,
    /// BT.2020 non-constant-luminance YCbCr.
    Bt2020,
}

impl AvifMatrix {
    /// Maps the CLI choice onto the codec's [`MatrixCoefficients`].
    fn to_codec(self) -> MatrixCoefficients {
        match self {
            AvifMatrix::Identity => MatrixCoefficients::Identity,
            AvifMatrix::Bt601 => MatrixCoefficients::Bt601,
            AvifMatrix::Bt709 => MatrixCoefficients::Bt709,
            AvifMatrix::Bt2020 => MatrixCoefficients::Bt2020Ncl,
        }
    }
}

/// Coded bit depth for AVIF output — the three depths AV1 codes (§6.4.1).
#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum AvifDepth {
    /// 8 bits; 8-bit input only.
    #[value(name = "8")]
    D8,
    /// 10 bits.
    #[value(name = "10")]
    D10,
    /// 12 bits.
    #[value(name = "12")]
    D12,
}

impl AvifDepth {
    /// Maps the CLI choice onto the codec's [`BitDepth`].
    fn to_codec(self) -> BitDepth {
        match self {
            AvifDepth::D8 => BitDepth::Eight,
            AvifDepth::D10 => BitDepth::Ten,
            AvifDepth::D12 => BitDepth::Twelve,
        }
    }
}

/// Chroma subsampling for lossy AVIF output.
#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum AvifChroma {
    /// 4:4:4 — full-resolution chroma (AV1 Profile 1).
    #[value(name = "444")]
    S444,
    /// 4:2:2 — chroma halved horizontally (AV1 Profile 2).
    #[value(name = "422")]
    S422,
    /// 4:2:0 — chroma halved both ways (AV1 Main profile); the lossy default.
    #[value(name = "420")]
    S420,
}

impl AvifChroma {
    /// Maps the CLI choice onto the codec's [`ChromaSubsampling`].
    fn to_codec(self) -> ChromaSubsampling {
        match self {
            AvifChroma::S444 => ChromaSubsampling::Cs444,
            AvifChroma::S422 => ChromaSubsampling::Cs422,
            AvifChroma::S420 => ChromaSubsampling::Cs420,
        }
    }
}

/// Coding-tool selection for JPEG XL output.
#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum JxlModular {
    /// Let libjxl choose between VarDCT and modular; the default.
    Auto,
    /// Force the VarDCT path (photographic material); invalid for lossless output.
    Vardct,
    /// Force the modular path (what lossless output already uses).
    Modular,
}

/// The highest `--png-effort`: the top level of `gamut_png::Preset`'s ladder.
///
/// The flag carries the codec's own ladder level, as `--webp-effort` and `--jxl-effort` carry
/// theirs, so this bound is the one fact the command line restates about the ladder;
/// `the_png_effort_range_is_exactly_the_codec_ladder` holds it to the codec.
const PNG_EFFORT_MAX: u8 = 3;

/// The default `--png-effort`: `gamut_png::Preset::Small`, which is what this command has always
/// encoded with.
const PNG_EFFORT_DEFAULT: u8 = 2;

/// Resolves a `--png-effort` level onto the codec's [`PngCodecPreset`] rung through the codec's
/// own [`PngCodecPreset::from_level`], rather than re-deciding the mapping here.
fn png_preset(level: u8) -> PngCodecPreset {
    // clap admits only `0..=PNG_EFFORT_MAX`, and `the_png_effort_range_is_exactly_the_codec_ladder`
    // pins every one of those as a level the codec admits, so the fallback is unreachable; it
    // exists because this is a CLI and a panic is not an error report.
    PngCodecPreset::from_level(level).unwrap_or_default()
}

impl JpegSubsampling {
    /// Maps the CLI choice onto the codec's [`JpegChroma`] enum.
    fn to_codec(self) -> JpegChroma {
        match self {
            JpegSubsampling::S444 => JpegChroma::Ycbcr444,
            JpegSubsampling::S422 => JpegChroma::Ycbcr422,
            JpegSubsampling::S420 => JpegChroma::Ycbcr420,
        }
    }
}

impl JxlModular {
    /// Maps the CLI choice onto the codec's [`JxlModularMode`] enum.
    fn to_codec(self) -> JxlModularMode {
        match self {
            JxlModular::Auto => JxlModularMode::Auto,
            JxlModular::Vardct => JxlModularMode::VarDct,
            JxlModular::Modular => JxlModularMode::Modular,
        }
    }
}

impl OutputFormat {
    /// The file extension an output of this format is given when `convert` names it.
    fn extension(self) -> &'static str {
        match self {
            OutputFormat::Avif => "avif",
            OutputFormat::Webp => "webp",
            OutputFormat::Tiff => "tiff",
            OutputFormat::Png => "png",
            OutputFormat::Jxl => "jxl",
            OutputFormat::Jpeg => "jpg",
        }
    }

    /// The format's name, as the metadata warnings print it.
    fn name(self) -> &'static str {
        match self {
            OutputFormat::Avif => "AVIF",
            OutputFormat::Webp => "WebP",
            OutputFormat::Tiff => "TIFF",
            OutputFormat::Png => "PNG",
            OutputFormat::Jxl => "JPEG XL",
            OutputFormat::Jpeg => "JPEG",
        }
    }

    /// The format as `gamut-metadata`'s capability table names it, which decides what metadata an
    /// output of this format can hold.
    fn container(self) -> MetadataFormat {
        match self {
            OutputFormat::Avif => MetadataFormat::Avif,
            OutputFormat::Webp => MetadataFormat::WebP,
            OutputFormat::Tiff => MetadataFormat::Tiff,
            OutputFormat::Png => MetadataFormat::Png,
            OutputFormat::Jxl => MetadataFormat::Jxl,
            OutputFormat::Jpeg => MetadataFormat::Jpeg,
        }
    }
}

/// One conversion: where its input comes from, where its output goes, and in what format.
#[derive(Debug, PartialEq, Eq)]
struct Job {
    /// The input file, or [`STDIO`] for standard input.
    input: PathBuf,
    /// The output file, or [`STDIO`] for standard output.
    output: PathBuf,
    /// The format the output is encoded in.
    format: OutputFormat,
}

/// Every conversion one `convert` invocation asks for, decided before any of them runs.
#[derive(Debug, PartialEq, Eq)]
struct Plan {
    /// The conversions, in the order the inputs were given.
    jobs: Vec<Job>,
    /// The output directory to create before the conversions run, when there is one.
    directory: Option<PathBuf>,
}

/// What one successful conversion produced, for its summary line.
struct Encoded {
    /// The encoded file.
    bytes: Vec<u8>,
    /// The size of the decoded samples the encoder was given.
    raw_len: usize,
    /// The image's dimensions.
    dims: gamut::core::Dimensions,
    /// How an AVIF output splits between its codestreams and its container; `None` for every
    /// other format.
    avif: Option<AvifEncodeReport>,
}

/// Runs the `convert` command: decode each input, encode it, and report the result.
///
/// A single conversion fails with its own error. Several run in parallel, one worker per
/// available core; each failure is reported on stderr as it happens, and the command then fails
/// with [`CliError::BatchFailed`] when any did.
pub(crate) fn run(args: &ConvertArgs) -> Result<(), CliError> {
    if args.qindex != 0 {
        tracing::warn!("--qindex is deprecated; use --avif-lossy");
    }
    let plan = plan(args)?;
    if let Some(directory) = &plan.directory {
        std::fs::create_dir_all(directory).map_err(|source| CliError::Io {
            path: directory.clone(),
            source,
        })?;
    }
    if let [job] = plan.jobs.as_slice() {
        let encoded = convert_one(args, job)?;
        report(args, job, &encoded);
        return Ok(());
    }

    let next = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let workers = std::thread::available_parallelism()
        .map_or(1, NonZeroUsize::get)
        .min(plan.jobs.len());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                while let Some(job) = plan.jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                    match convert_one(args, job) {
                        Ok(encoded) => report(args, job, &encoded),
                        Err(err) => {
                            eprintln!("error: {}: {err}", job.input.display());
                            failed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            });
        }
    });

    match failed.into_inner() {
        0 => Ok(()),
        failed => Err(CliError::BatchFailed {
            failed,
            total: plan.jobs.len(),
        }),
    }
}

/// Decides every conversion `args` asks for, refusing an invocation that cannot be carried out as
/// a whole — standard input or output shared between several files, two inputs that would write
/// the same output, or an output that is its own input — before anything is written.
fn plan(args: &ConvertArgs) -> Result<Plan, CliError> {
    let stdio = Path::new(STDIO);
    let (inputs, destination) = match (&args.output, args.paths.split_last()) {
        (Some(output), _) => (args.paths.as_slice(), Some(output.as_path())),
        (None, Some((last, rest))) if !rest.is_empty() => (rest, Some(last.as_path())),
        (None, _) => (args.paths.as_slice(), None),
    };
    if inputs.len() > 1 && inputs.iter().any(|input| input == stdio) {
        return Err(CliError::Usage(
            "`-` (standard input) must be the only input".to_string(),
        ));
    }
    let default_format = args.format.unwrap_or(OutputFormat::Avif);

    let mut plan = Plan {
        jobs: Vec::with_capacity(inputs.len()),
        directory: None,
    };
    match destination {
        None => {
            let input = &inputs[0];
            let output = if input == stdio {
                PathBuf::from(STDIO)
            } else {
                input.with_extension(default_format.extension())
            };
            plan.jobs.push(Job {
                input: input.clone(),
                output,
                format: default_format,
            });
        }
        Some(output) if output == stdio => {
            if inputs.len() > 1 {
                return Err(CliError::Usage(
                    "`-` (standard output) takes exactly one input".to_string(),
                ));
            }
            plan.jobs.push(Job {
                input: inputs[0].clone(),
                output: output.to_path_buf(),
                format: default_format,
            });
        }
        Some(directory) if inputs.len() > 1 || directory.is_dir() => {
            for input in inputs {
                plan.jobs.push(Job {
                    input: input.clone(),
                    output: output_in(directory, input, default_format)?,
                    format: default_format,
                });
            }
            plan.directory = Some(directory.to_path_buf());
        }
        Some(output) => plan.jobs.push(Job {
            input: inputs[0].clone(),
            output: output.to_path_buf(),
            format: match args.format {
                Some(format) => format,
                None => format_of(output)?,
            },
        }),
    }

    for (i, job) in plan.jobs.iter().enumerate() {
        if job.output == job.input && job.input != stdio {
            return Err(CliError::Usage(format!(
                "{} would be written over its own input",
                job.output.display()
            )));
        }
        if let Some(earlier) = plan.jobs[..i].iter().find(|e| e.output == job.output) {
            return Err(CliError::Usage(format!(
                "{} and {} would both be written to {}",
                earlier.input.display(),
                job.input.display(),
                job.output.display()
            )));
        }
    }
    Ok(plan)
}

/// The output `input` gets in `directory`: its file name with the extension replaced by
/// `format`'s (`photo.v2.png` becomes `photo.v2.avif`). Standard input has no name to give it, so
/// it is refused there.
fn output_in(directory: &Path, input: &Path, format: OutputFormat) -> Result<PathBuf, CliError> {
    let name = input
        .file_name()
        .filter(|_| input != Path::new(STDIO))
        .ok_or_else(|| {
            CliError::Usage(format!(
                "{} has no file name to give its output in {}; name the output file instead",
                input.display(),
                directory.display()
            ))
        })?;
    Ok(directory.join(name).with_extension(format.extension()))
}

/// Runs one conversion: read and decode the input, encode it, and write the output.
///
/// An existing output file is left alone unless `--overwrite` was given; it is checked before the
/// work starts, and again when the file is created, so a file that appears meanwhile is not
/// clobbered either.
fn convert_one(args: &ConvertArgs, job: &Job) -> Result<Encoded, CliError> {
    let to_stdout = job.output == Path::new(STDIO);
    if !to_stdout && !args.overwrite && job.output.exists() {
        return Err(CliError::OutputExists {
            path: job.output.clone(),
        });
    }
    let bytes = read_input(&job.input)?;
    let encoded = encode(args, job.format, &job.input, &bytes)?;
    tracing::info!(
        bytes = encoded.bytes.len(),
        lossy = args.lossy,
        "encoded output"
    );

    let io_error = |source| CliError::Io {
        path: job.output.clone(),
        source,
    };
    if to_stdout {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(&encoded.bytes).map_err(io_error)?;
        stdout.flush().map_err(io_error)?;
    } else {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .create_new(!args.overwrite)
            .open(&job.output)
            .map_err(|source| match source.kind() {
                std::io::ErrorKind::AlreadyExists => CliError::OutputExists {
                    path: job.output.clone(),
                },
                _ => io_error(source),
            })?;
        file.write_all(&encoded.bytes).map_err(io_error)?;
    }
    Ok(encoded)
}

/// Prints the summary line for one finished conversion, unless `--quiet`: to stdout, or to stderr
/// when stdout carries the image itself.
fn report(args: &ConvertArgs, job: &Job, encoded: &Encoded) {
    if args.quiet {
        return;
    }
    let len = encoded.bytes.len();
    let ratio = if len == 0 {
        0.0
    } else {
        encoded.raw_len as f64 / len as f64
    };
    let breakdown = encoded.avif.as_ref().map_or_else(String::new, |avif| {
        format!(
            "; colour {} + alpha {} + container {} bytes",
            avif.color_bytes,
            avif.alpha_bytes,
            len.saturating_sub(avif.color_bytes + avif.alpha_bytes)
        )
    });
    let line = format!(
        "wrote {} ({}x{}, {len} bytes, {ratio:.2}x vs raw RGB{breakdown})",
        if job.output == Path::new(STDIO) {
            "<stdout>".into()
        } else {
            job.output.display().to_string()
        },
        encoded.dims.width,
        encoded.dims.height,
    );
    if job.output == Path::new(STDIO) {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
}

/// Decodes `bytes` (read from `input`, which only labels errors) and encodes it as `format`.
fn encode(
    args: &ConvertArgs,
    format: OutputFormat,
    input: &Path,
    bytes: &[u8],
) -> Result<Encoded, CliError> {
    let mut out = Vec::new();
    let mut avif = None;
    // Read without touching the pixels, which decode separately below; every output maps it
    // through `carry` once its samples and size are known (issue #570).
    let metadata = (!args.strip_metadata).then(|| read_metadata(bytes));
    let carry_into = |samples, dims| {
        metadata
            .as_ref()
            .map(|metadata| carry(metadata, format, samples, dims))
            .unwrap_or_default()
    };
    let (raw_len, dims) = match format {
        OutputFormat::Avif => {
            // The input's own layout, so alpha and 16-bit precision reach the encoder (issue
            // #663); a 16-bit input codes at the encoder's default depth, 12 bits.
            let image = decode_native(input, bytes)?;
            let dims = image.dimensions();
            tracing::info!(
                width = dims.width,
                height = dims.height,
                bytes = image.byte_len(),
                "decoded input"
            );
            let samples = match image {
                NativeImage::Gray8(_) => Samples::Grey,
                _ => Samples::Colour,
            };
            let carried = carry_into(Some(samples), dims);
            if carried
                .orientation
                .is_some_and(|orientation| orientation != 1)
            {
                tracing::warn!(
                    "input metadata carried with a caveat — the Exif Orientation is kept, but the \
                     AVIF output signals no orientation of its own (no `irot`/`imir`), so a reader \
                     that orients by the container shows the stored pixels unrotated"
                );
            }
            let mut encoder = avif_encoder(args);
            if let Some(icc) = &carried.icc {
                encoder = encoder.with_icc_profile(icc);
            }
            if let Some(exif) = &carried.exif {
                encoder = encoder.with_exif(exif);
            }
            if let Some(xmp) = &carried.xmp {
                encoder = encoder.with_xmp(xmp);
            }
            // The report splits the file into its colour and alpha codestreams and the container
            // around them, for the summary line's size breakdown.
            let (bytes, report) = match &image {
                NativeImage::Gray8(img) => encoder.encode_with_report(img.as_ref())?,
                NativeImage::Rgb8(img) => encoder.encode_with_report(img.as_ref())?,
                NativeImage::Rgba8(img) => encoder.encode_with_report(img.as_ref())?,
                NativeImage::Rgb16(img) => encoder.encode_with_report(img.as_ref())?,
                NativeImage::Rgba16(img) => encoder.encode_with_report(img.as_ref())?,
            };
            out = bytes;
            avif = Some(report);
            (image.byte_len(), dims)
        }
        OutputFormat::Webp => {
            // RGBA so transparency survives; `encode_rgba8` emits a simple file when fully opaque.
            let (rgba, dims) = decode_bytes(input, bytes, true)?;
            tracing::info!(
                width = dims.width,
                height = dims.height,
                bytes = rgba.len(),
                "decoded input"
            );
            // The clap `0..=6` range guarantees `from_level` returns `Some`; fall back to the
            // default effort rather than unwrap so the path stays panic-free.
            let effort = WebpEffort::from_level(args.webp_effort).unwrap_or_default();
            // `100` is libwebp's "off" sentinel, so it maps to `None` alongside an absent flag —
            // the strength type refuses to represent "off" as a magic value.
            let near_lossless = args
                .webp_near_lossless
                .and_then(WebpNearLossless::from_libwebp_strength);
            if args.lossy && near_lossless.is_some() {
                tracing::warn!("--webp-near-lossless applies to lossless WebP only; ignoring");
            }
            let mut encoder = if args.lossy {
                WebpEncoder::lossy(args.quality)
            } else {
                WebpEncoder::lossless()
            };
            let carried = carry_into(Some(Samples::Colour), dims);
            if let Some(icc) = &carried.icc {
                encoder = encoder.with_icc_profile(icc);
            }
            if let Some(exif) = &carried.exif {
                encoder = encoder.with_exif(exif);
            }
            if let Some(xmp) = &carried.xmp {
                encoder = encoder.with_xmp(xmp);
            }
            encoder
                .with_effort(effort)
                .with_near_lossless(near_lossless)
                .encode_image(ImageRef::<Rgba8>::new(&rgba, dims)?, &mut out)?;
            (rgba.len(), dims)
        }
        OutputFormat::Tiff => {
            let (rgb, dims) = decode_bytes(input, bytes, false)?;
            tracing::info!(
                width = dims.width,
                height = dims.height,
                bytes = rgb.len(),
                "decoded input"
            );
            let compression = if args.packbits {
                TiffCompression::PackBits
            } else {
                TiffCompression::None
            };
            // The Exif `Orientation` is carried as a tag of the TIFF's own IFD 0, so a TIFF output
            // needs no orientation handling of its own.
            let carried = carry_into(Some(Samples::Colour), dims);
            let bare = TiffEncoder::new().with_compression(compression);
            let mut blocks = EncodedMetadata::default();
            blocks.exif = carried.exif;
            blocks.xmp = carried.xmp;
            blocks.icc = carried.icc;
            blocks.iptc_iim = carried.iptc_iim;
            let attached = blocks != EncodedMetadata::default();
            let image = ImageRef::<Rgb8>::new(&rgb, dims)?;
            // The encoder checks the Exif directory it is handed, at the builder and again at the
            // encode. Metadata failing must not fail a conversion whose pixels are fine, as on the
            // JPEG path: say so, and encode without it.
            let encoded = bare
                .clone()
                .with_encoded_metadata(&blocks)
                .and_then(|encoder| encoder.encode_image(image, &mut out));
            if let Err(err) = encoded {
                if !attached {
                    return Err(err.into());
                }
                tracing::warn!("input metadata not carried — the TIFF encoder refused it: {err}");
                out.clear();
                bare.encode_image(image, &mut out)?;
            }
            (rgb.len(), dims)
        }
        OutputFormat::Png => {
            // RGBA so transparency survives; auto-reduce drops it (and chooses grey/palette) when
            // that is lossless.
            let (rgba, dims) = decode_bytes(input, bytes, true)?;
            tracing::info!(
                width = dims.width,
                height = dims.height,
                bytes = rgba.len(),
                "decoded input"
            );
            // The rung sets every size/time knob; `--png-refinement-passes` still overrides the
            // one it names, so it must be applied after.
            let mut encoder = PngEncoder::new().with_preset(png_preset(args.png_effort));
            if let Some(passes) = args.png_refinement_passes {
                encoder = encoder.with_effort(passes);
            }
            // A PNG input keeps every chunk it carries (issue #483), with its Exif rewritten; its
            // ICC profile is the file's own, so it needs no check against the samples here.
            // Any other input carries only what has a `gamut-metadata` block (issue #570).
            let png_input = metadata.as_ref().and_then(|m| m.png.as_ref());
            let samples = if png_input.is_some() {
                None
            } else {
                Some(Samples::Colour)
            };
            let mut carried = carry_into(samples, dims);
            if let Some(png) = png_input {
                tracing::info!(
                    texts = png.texts.len(),
                    exif = carried.exif.is_some(),
                    icc = png.icc_profile.is_some(),
                    xmp = png.xmp.is_some(),
                    "carrying input metadata"
                );
                let mut png: PngMetadata = png.clone();
                png.exif = carried.exif.take();
                // `carry` has already reported the store, which is never carried.
                png.c2pa = None;
                encoder = encoder.with_png_metadata(&png);
            } else {
                if let Some(exif) = &carried.exif {
                    encoder = encoder.with_exif(exif);
                }
                if let Some(icc) = &carried.icc {
                    encoder = encoder.with_icc_profile("ICC profile", icc);
                }
                if let Some(xmp) = carried.xmp.as_deref().and_then(utf8_xmp) {
                    encoder = encoder.with_xmp(xmp);
                }
            }
            // Say what could not come along, and what came along with a caveat. Silent loss is
            // the defect this path exists to remove, and a payload the spec forbids carrying is
            // still a payload the caller had.
            for notice in encoder.metadata_notices() {
                if notice.carried() {
                    tracing::warn!("input metadata carried with a caveat — {notice}");
                } else {
                    tracing::warn!("input metadata not carried — {notice}");
                }
            }
            encoder.encode_image(ImageRef::<Rgba8>::new(&rgba, dims)?, &mut out)?;
            (rgba.len(), dims)
        }
        OutputFormat::Jxl => {
            // RGBA so transparency survives, matching the PNG/WebP paths.
            let (rgba, dims) = decode_bytes(input, bytes, true)?;
            tracing::info!(
                width = dims.width,
                height = dims.height,
                bytes = rgba.len(),
                "decoded input"
            );
            // A `--jxl-distance` selects lossy; `Distance::new` validates the range and surfaces an
            // out-of-range value as the codec's `InvalidInput` through `CliError::Codec`.
            let mut encoder = match args.jxl_distance {
                Some(distance) => JxlEncoder::lossy(JxlDistance::new(distance)?),
                None => JxlEncoder::lossless(),
            };
            let carried = carry_into(Some(Samples::Colour), dims);
            // Orientation and the ICC profile live in the codestream itself, so they are carried
            // in either framing.
            if let Some(orientation) = carried
                .orientation
                .and_then(|value| JxlOrientation::from_exif_value(u8::try_from(value).ok()?))
            {
                encoder = encoder.with_orientation(orientation);
            }
            // The clap `1..=10` range guarantees `from_level` returns `Some`; fall back to the
            // default effort (Squirrel/7) rather than unwrap so the path stays panic-free.
            let effort = JxlEffort::from_level(args.jxl_effort).unwrap_or_default();
            let container = if args.jxl_container {
                JxlContainer::IsoBmff
            } else {
                JxlContainer::Codestream
            };
            // `Auto` leaves the coding tool to libjxl; forcing VarDCT on the lossless default is a
            // contradiction the codec reports as `InvalidInput` through `CliError::Codec`.
            encoder = encoder
                .with_effort(effort)
                .with_modular(args.jxl_modular.to_codec())
                .with_container(container);
            let bare = encoder.clone();
            let mut attached = false;
            if let Some(icc) = carried.icc {
                encoder = encoder.with_color(JxlColorSpec::Icc(icc));
                attached = true;
            }
            if args.jxl_container {
                if let Some(exif) = &carried.exif {
                    encoder = encoder.with_exif(exif);
                    attached = true;
                }
                if let Some(xmp) = carried.xmp.as_deref().and_then(utf8_xmp) {
                    encoder = encoder.with_xmp(xmp);
                    attached = true;
                }
            } else if carried.exif.is_some() || carried.xmp.is_some() {
                // Exif and XMP are container boxes, which a bare codestream has nowhere to put
                // (`JxlEncoder::with_exif` refuses the pair). The framing is the caller's choice,
                // so it is not changed behind their back.
                tracing::warn!(
                    "input metadata not carried — a bare JPEG XL codestream holds no Exif or XMP; \
                     pass --jxl-container to carry them"
                );
            }
            // libjxl refuses an ICC profile it cannot use even where its header parses here.
            // Metadata failing must not fail a conversion whose pixels are fine, as on the JPEG
            // path: say so, and encode without it (the orientation, a header field, stays).
            let image = ImageRef::<Rgba8>::new(&rgba, dims)?;
            if let Err(err) = encoder.encode_image(image, &mut out) {
                if !attached {
                    return Err(err.into());
                }
                tracing::warn!(
                    "input metadata not carried — the JPEG XL encoder refused it: {err}"
                );
                out.clear();
                bare.encode_image(image, &mut out)?;
            }
            (rgba.len(), dims)
        }
        OutputFormat::Jpeg => {
            // JPEG has no alpha channel, so decode to RGB like the AVIF/TIFF paths; the input
            // pipeline does not distinguish grayscale, so colour is always encoded as YCbCr.
            let (rgb, dims) = decode_bytes(input, bytes, false)?;
            tracing::info!(
                width = dims.width,
                height = dims.height,
                bytes = rgb.len(),
                "decoded input"
            );
            // JPEG-1 is inherently lossy; `--quality` (default 75) drives the quantization tables,
            // `--jpeg-subsampling` (default 4:2:0) the chroma resolution, and `--jpeg-progressive`
            // selects the SOF2 progressive process. A restart interval of 0 disables restarts, so
            // only apply a nonzero one.
            let mut encoder = JpegEncoder::new()
                .with_quality(args.quality)
                .with_subsampling(args.jpeg_subsampling.to_codec())
                .with_progressive(args.jpeg_progressive);
            if args.jpeg_restart_interval != 0 {
                encoder = encoder.with_restart_interval(args.jpeg_restart_interval);
            }
            let image = ImageRef::<Rgb8>::new(&rgb, dims)?;
            let carried = carry_into(Some(Samples::Colour), dims);
            let bare = encoder.clone();
            let attached = carried.icc.is_some() || carried.exif.is_some() || carried.xmp.is_some();
            if let Some(icc) = &carried.icc {
                encoder = encoder.with_icc_profile(icc);
            }
            if let Some(exif) = &carried.exif {
                encoder = encoder.with_exif(exif);
            }
            if let Some(xmp) = &carried.xmp {
                encoder = encoder.with_xmp(xmp);
            }
            // A JPEG segment is capped at 64 KiB, so an Exif or XMP payload over its segment's
            // limit is refused by the encode (the encoder owns those limits). Metadata failing
            // must not fail a conversion whose pixels are fine: say so, and encode without it.
            if let Err(err) = encoder.encode_image(image, &mut out) {
                if !attached {
                    return Err(err.into());
                }
                tracing::warn!("input metadata not carried — the JPEG encoder refused it: {err}");
                out.clear();
                bare.encode_image(image, &mut out)?;
            }
            (rgb.len(), dims)
        }
    };
    Ok(Encoded {
        bytes: out,
        raw_len,
        dims,
        avif,
    })
}

/// The AVIF encoder the `--avif-*` flags (and the deprecated `--qindex`) describe, before the
/// input's metadata is attached.
///
/// A flag left unset leaves the encoder's own default in place rather than restating it, so the
/// codec stays the one place a default is decided: the matrix and chroma defaults differ between
/// lossless and lossy, and the depth default between 8- and 16-bit input.
fn avif_encoder(args: &ConvertArgs) -> AvifEncoder {
    // `AvifEncoder` migrated from a raw `base_q_idx` to a lossless()/lossy(quality) model, so
    // `--qindex` survives only as a deprecated spelling of `--avif-lossy`.
    let mut encoder = if args.avif_lossy || args.qindex != 0 {
        AvifEncoder::lossy(args.quality)
    } else {
        AvifEncoder::lossless()
    };
    // The clap `0..=3` range is `Speed::ALL`'s index range, slowest first —
    // `the_avif_speed_flag_offers_exactly_the_codec_presets` pins that they agree — so the
    // fallback is unreachable; it exists because this is a CLI and a panic is not an error report.
    let speed = AvifSpeed::ALL
        .get(usize::from(args.avif_speed))
        .copied()
        .unwrap_or_default();
    // `0` is "every core", which the encoder spells `None`, exactly what `NonZeroUsize::new` gives.
    encoder = encoder
        .with_speed(speed)
        .with_threads(NonZeroUsize::new(args.avif_threads))
        .with_clean_alpha(args.avif_clean_alpha);
    if let Some(quality) = args.avif_alpha_quality {
        encoder = encoder.with_alpha_quality(quality);
    }
    if let Some(matrix) = args.avif_matrix {
        encoder = encoder.with_matrix(matrix.to_codec());
    }
    if let Some(depth) = args.avif_depth {
        encoder = encoder.with_bit_depth(depth.to_codec());
    }
    if let Some(chroma) = args.avif_chroma {
        encoder = encoder.with_chroma(chroma.to_codec());
    }
    encoder
}

/// The samples an output codes, which decides the ICC profile that can describe them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Samples {
    /// One grey channel (plus alpha).
    Grey,
    /// Three colour channels (plus alpha).
    Colour,
}

/// The input metadata one output carries, after [`carry`] rewrote and dropped what it had to.
#[derive(Debug, Default)]
struct Carried {
    /// The Exif payload as a bare TIFF stream (no `Exif\0\0` marker), its conversion-sensitive
    /// claims rewritten (see [`rewrite_exif`]).
    exif: Option<Vec<u8>>,
    /// The XMP packet, verbatim.
    xmp: Option<Vec<u8>>,
    /// The ICC profile, verbatim.
    icc: Option<Vec<u8>>,
    /// The legacy IPTC-IIM dataset stream, verbatim.
    iptc_iim: Option<Vec<u8>>,
    /// The Exif orientation (1–8) the output's stored pixels need for display, after the rewrite.
    /// Read even where the output cannot hold the Exif itself, for a format that signals
    /// orientation in its own header (JPEG XL).
    orientation: Option<u16>,
}

/// Maps the input's metadata onto a `format` output of `dims` whose samples are `samples` —
/// `None` when the encoder fits its colour type to the profile itself, as a PNG input re-encoded to
/// PNG does — reporting on stderr everything left behind.
///
/// What the output can hold is `gamut-metadata`'s capability table, not a judgement made here, so
/// no output arm is handed a carrier the table says its format cannot write. Beyond that table:
///
/// - a **C2PA manifest store** is never carried, into any format: its hard binding covers the
///   bytes of the file it was made for (C2PA 2.4 §9.1), so a copy would be an invalid claim;
/// - an **ICC profile** is carried only where its data colour space (ICC.1:2022 §7.2.6) is the
///   output samples' — a `GRAY` profile cannot describe RGB data, nor an RGB one grey data;
/// - the **Exif** is carried only once it parses, because a payload that does not parse cannot be
///   rewritten (see [`rewrite_exif`]) and copying it would repeat claims the conversion may have
///   made false;
/// - a PNG input's **text annotations and colour chunks** have no block, so only a PNG output
///   holds them.
fn carry(
    meta: &InputMetadata,
    format: OutputFormat,
    samples: Option<Samples>,
    dims: Dimensions,
) -> Carried {
    let holds = |carrier| supports(format.container(), carrier, Direction::Write);
    let not_held = |what: &str| {
        tracing::warn!(
            "input metadata not carried — {} output cannot hold the {what}",
            format.name()
        );
    };

    if meta.c2pa {
        tracing::warn!(
            "input metadata not carried — the C2PA manifest store, which is signed over the bytes \
             of the file it was made for (C2PA 2.4 §9.1)"
        );
    }
    let iptc_iim = match &meta.iptc_iim {
        // IPTC Core also travels inside the XMP packet, so the legacy block is the duplicate a
        // format without a slot for it loses.
        Some(_) if !holds(Carrier::IptcIim) => {
            not_held("IPTC-IIM block");
            None
        }
        iim => iim.clone(),
    };
    if meta.unknown > 0 {
        tracing::warn!(
            "input metadata not carried — {} metadata block(s) of a kind this command does not know",
            meta.unknown
        );
    }
    if let Some(png) = &meta.png
        && format != OutputFormat::Png
    {
        let chunks = png_only_chunks(png);
        if !chunks.is_empty() {
            not_held(&format!("PNG chunks {}", chunks.join(", ")));
        }
    }

    let icc = match (&meta.icc, samples) {
        (None, _) => None,
        (Some(_), _) if !holds(Carrier::Icc) => {
            not_held("ICC profile");
            None
        }
        (Some(icc), Some(samples)) if !profile_describes(icc, samples) => {
            tracing::warn!(
                "input metadata not carried — the ICC profile, whose colour space does not \
                 describe the output's {} samples (ICC.1:2022 §7.2.6)",
                match samples {
                    Samples::Grey => "grey",
                    Samples::Colour => "RGB",
                }
            );
            None
        }
        (Some(icc), _) => Some(icc.clone()),
    };
    let xmp = match &meta.xmp {
        Some(_) if !holds(Carrier::Xmp) => {
            not_held("XMP packet");
            None
        }
        xmp => xmp.clone(),
    };
    let mut carried = Carried {
        exif: None,
        xmp,
        icc,
        iptc_iim,
        orientation: None,
    };

    let Some(raw) = &meta.exif else {
        return carried;
    };
    let mut exif = match Exif::parse(raw) {
        Ok(exif) => exif,
        Err(err) => {
            tracing::warn!(
                "input metadata not carried — the Exif payload, which does not parse, so its \
                 claims about the image cannot be rewritten: {err}"
            );
            return carried;
        }
    };
    rewrite_exif(&mut exif, meta, dims, carried.icc.is_some());
    carried.orientation = exif.orientation();
    if !holds(Carrier::Exif) {
        not_held("Exif payload");
        return carried;
    }
    if format == OutputFormat::Tiff {
        exif = tiff_carried(&exif);
    }
    // `ExifWriter` re-embeds a thumbnail's JPEG bytes and its uncompressed strips, but a thumbnail
    // whose image bytes did not read holds neither. The reader has already removed its
    // `JPEGInterchangeFormat`/`StripOffsets` (#548, #868), so nothing would dangle; the 1st IFD
    // would instead come out as a thumbnail with no image — its sizing tags
    // (`JPEGInterchangeFormatLength`, `StripByteCounts`) measuring bytes that are not there, and
    // the offset tag Exif 3.0 §4.6.9.2 Table 21 marks `M` in its format's column missing. Such a
    // thumbnail is dropped rather than written imageless.
    if exif
        .thumbnail()
        .is_some_and(|thumb| thumb.jpeg().is_none() && thumb.strips().is_none())
    {
        exif.clear_thumbnail();
        tracing::warn!(
            "input metadata not carried — the Exif thumbnail, whose image bytes (neither a JPEG \
             nor uncompressed strips) did not read from the input"
        );
    }
    match ExifWriter::new().marker(false).write(&exif) {
        Ok(bytes) => carried.exif = Some(bytes),
        Err(err) => {
            tracing::warn!(
                "input metadata not carried — the Exif payload does not re-encode: {err}"
            );
        }
    }
    carried
}

/// The part of `exif` a TIFF output holds — everything but the thumbnail and the 0th-IFD fields
/// the TIFF encoder writes itself — reporting the rest as left behind.
///
/// A TIFF has no Exif 0th IFD of its own: that directory's tags are TIFF 6.0 tags of the page's
/// own IFD 0. `gamut-tiff` carries the descriptive ones (`Make`, `Model`, `Orientation`, …,
/// `gamut::tiff::IFD0_EXIF_TAGS`) into it beside the `GPSInfo` and `ExifIFD` sub-IFDs, but writes
/// the layout ones (`XResolution`, `YCbCrPositioning`, …) from the pixels it lays out, and a
/// thumbnail would be a page of its own. It refuses an Exif model holding either rather than
/// dropping it — so the drop happens here, where it is reported.
fn tiff_carried(exif: &Exif) -> Exif {
    let mut kept = exif.clone();
    let mut left = Vec::new();
    for field in exif.image().fields() {
        if !IFD0_EXIF_TAGS.contains(&field.tag) {
            kept.image_mut().remove(field.tag);
            left.push(
                ExifTag::from_id(IfdKind::Image, field.tag)
                    .map_or_else(|| format!("tag {}", field.tag), |tag| tag.name().to_owned()),
            );
        }
    }
    if !left.is_empty() {
        tracing::warn!(
            "input metadata not carried — the Exif 0th-IFD field(s) {}, which a TIFF's IFD 0 \
             holds as the TIFF encoder's own",
            left.join(", ")
        );
    }
    if exif.thumbnail().is_some() {
        kept.clear_thumbnail();
        tracing::warn!(
            "input metadata not carried — the Exif thumbnail, which a TIFF would hold as a page \
             of its own"
        );
    }
    kept
}

/// Rewrites the Exif claims a conversion can make false, each only where the input makes it — a
/// tag the input leaves out is not invented:
///
/// - `Orientation` (0th IFD, CIPA DC-008 §4.6.5) becomes 1 when the decoder already applied it to
///   the pixels, as JPEG XL's does; otherwise the pixels are stored as they were and the value
///   still holds;
/// - `PixelXDimension` / `PixelYDimension` (Exif IFD, §4.6.6) become the output's `dims`;
/// - `ColorSpace` (Exif IFD, §4.6.6) becomes 1 (sRGB) when the decoder rendered the pixels into
///   sRGB, and `0xFFFF` (Uncalibrated) when the input's ICC profile described them and
///   `icc_carried` says the output does not — the pixels are then in a space the file no longer
///   names.
fn rewrite_exif(exif: &mut Exif, meta: &InputMetadata, dims: Dimensions, icc_carried: bool) {
    let present = |exif: &Exif, tag| exif.get_tag(tag).is_some();
    if meta.oriented && present(exif, ExifTag::Orientation) {
        exif.set_tag(ExifTag::Orientation, ExifValue::Short(vec![1]));
    }
    for (tag, length) in [
        (ExifTag::PixelXDimension, dims.width),
        (ExifTag::PixelYDimension, dims.height),
    ] {
        if present(exif, tag) {
            exif.set_tag(tag, ExifValue::Long(vec![length]));
        }
    }
    if present(exif, ExifTag::ColorSpace) {
        if meta.decoded_to_srgb {
            exif.set_tag(ExifTag::ColorSpace, ExifValue::Short(vec![1]));
        } else if meta.icc.is_some() && !icc_carried {
            exif.set_tag(ExifTag::ColorSpace, ExifValue::Short(vec![0xFFFF]));
        }
    }
}

/// Whether `icc`'s data colour space (ICC.1:2022 §7.2.6) is that of `samples`. A profile whose
/// header does not parse describes nothing.
fn profile_describes(icc: &[u8], samples: Samples) -> bool {
    let wanted = match samples {
        Samples::Grey => IccColorSpace::Gray,
        Samples::Colour => IccColorSpace::Rgb,
    };
    ProfileHeader::parse(icc).is_ok_and(|header| header.data_color_space == wanted)
}

/// The chunks of a PNG input that only a PNG output holds, by name, for the warning that they were
/// left behind.
fn png_only_chunks(png: &PngMetadata) -> Vec<&'static str> {
    [
        (!png.texts.is_empty(), "tEXt/zTXt/iTXt"),
        (png.gamma.is_some(), "gAMA"),
        (png.chromaticities.is_some(), "cHRM"),
        (png.srgb.is_some(), "sRGB"),
        (png.cicp.is_some(), "cICP"),
        (png.significant_bits.is_some(), "sBIT"),
        (png.background.is_some(), "bKGD"),
        (png.physical_dimensions.is_some(), "pHYs"),
        (png.modification_time.is_some(), "tIME"),
    ]
    .into_iter()
    .filter_map(|(present, name)| present.then_some(name))
    .collect()
}

/// `xmp` as the `&str` the PNG and JPEG XL encoders take, or `None` — with a warning — when it is
/// not UTF-8, which an XMP packet must be in those formats (PNG §11.3.3.4's `iTXt` is UTF-8).
fn utf8_xmp(xmp: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(xmp).ok();
    if text.is_none() {
        tracing::warn!("input metadata not carried — the XMP packet, which is not UTF-8");
    }
    text
}

/// The output format an output file's extension names, for when `--format` is not given.
fn format_of(output: &Path) -> Result<OutputFormat, CliError> {
    match output
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("avif") => Ok(OutputFormat::Avif),
        Some("webp") => Ok(OutputFormat::Webp),
        Some("tiff" | "tif") => Ok(OutputFormat::Tiff),
        Some("png") => Ok(OutputFormat::Png),
        Some("jxl") => Ok(OutputFormat::Jxl),
        Some("jpg" | "jpeg") => Ok(OutputFormat::Jpeg),
        Some(other) => Err(CliError::UnsupportedOutput(other.to_string())),
        None => Err(CliError::UnsupportedOutput("<none>".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::path::PathBuf;

    use clap::Parser;
    use gamut::avif::AvifMode;

    use super::{
        AvifEncoder, AvifSpeed, BitDepth, ChromaSubsampling, ConvertArgs, Dimensions,
        InputMetadata, Job, MatrixCoefficients, OutputFormat, Plan, PngCodecPreset, Samples,
        avif_encoder, carry, plan, png_preset,
    };
    use crate::error::CliError;

    /// `convert`'s arguments as clap parses them from `argv` (without the subcommand name).
    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: ConvertArgs,
    }

    /// The plan for `gamut convert <argv>`.
    fn plan_of(argv: &[&str]) -> Result<Plan, CliError> {
        plan(
            &Cli::try_parse_from(std::iter::once("convert").chain(argv.iter().copied()))
                .unwrap()
                .args,
        )
    }

    /// `convert in.png out.avif <flags>` parsed, or clap's refusal.
    fn parse_avif(flags: &[&str]) -> Result<ConvertArgs, clap::Error> {
        Cli::try_parse_from(
            ["convert", "in.png", "out.avif"]
                .into_iter()
                .chain(flags.iter().copied()),
        )
        .map(|cli| cli.args)
    }

    /// `convert in.png out.png <flags>` parsed, or clap's refusal.
    fn parse_png(flags: &[&str]) -> Result<ConvertArgs, clap::Error> {
        Cli::try_parse_from(
            ["convert", "in.png", "out.png"]
                .into_iter()
                .chain(flags.iter().copied()),
        )
        .map(|cli| cli.args)
    }

    /// The AVIF encoder `convert in.png out.avif <flags>` builds.
    fn avif_with(flags: &[&str]) -> AvifEncoder {
        avif_encoder(&parse_avif(flags).unwrap())
    }

    /// A job converting `input` to `output` as `format`.
    fn job(input: &str, output: &str, format: OutputFormat) -> Job {
        Job {
            input: PathBuf::from(input),
            output: PathBuf::from(output),
            format,
        }
    }

    #[test]
    fn two_paths_are_an_input_and_its_output_file() {
        // The pre-batch form, kept: the output's extension still picks the format.
        let plan = plan_of(&["in.png", "out.webp"]).unwrap();
        assert_eq!(plan.jobs, [job("in.png", "out.webp", OutputFormat::Webp)]);
        assert_eq!(plan.directory, None);
    }

    #[test]
    fn several_inputs_go_into_the_output_directory_named_for_the_format() {
        // Only the last extension is replaced, so a dotted name keeps its inner dot.
        let plan = plan_of(&["a/x.png", "y.v2.jpg", "-o", "out", "--format", "png"]).unwrap();
        assert_eq!(
            plan.jobs,
            [
                job("a/x.png", "out/x.png", OutputFormat::Png),
                job("y.v2.jpg", "out/y.v2.png", OutputFormat::Png),
            ]
        );
        assert_eq!(plan.directory, Some(PathBuf::from("out")));
    }

    #[test]
    fn a_lone_input_is_written_beside_itself_as_avif() {
        let plan = plan_of(&["dir/in.png"]).unwrap();
        assert_eq!(
            plan.jobs,
            [job("dir/in.png", "dir/in.avif", OutputFormat::Avif)]
        );
    }

    #[test]
    fn standard_input_alone_goes_to_standard_output() {
        let plan = plan_of(&["-"]).unwrap();
        assert_eq!(plan.jobs, [job("-", "-", OutputFormat::Avif)]);
    }

    #[test]
    fn standard_input_cannot_share_a_batch() {
        assert!(matches!(
            plan_of(&["-", "b.png", "-o", "out"]),
            Err(CliError::Usage(_))
        ));
    }

    #[test]
    fn standard_output_takes_one_input() {
        assert!(matches!(
            plan_of(&["a.png", "b.png", "-o", "-"]),
            Err(CliError::Usage(_))
        ));
    }

    #[test]
    fn two_inputs_with_one_output_name_are_refused() {
        assert!(matches!(
            plan_of(&["a/x.png", "b/x.jpg", "-o", "out"]),
            Err(CliError::Usage(_))
        ));
    }

    #[test]
    fn a_lone_input_goes_into_an_existing_directory_named_for_the_format() {
        // The temporary directory always exists, so one input with it as the output is placed
        // inside it rather than written over it as a file.
        let dir = std::env::temp_dir();
        let dir_str = dir.to_str().unwrap();
        let plan = plan_of(&["a/in.png", "-o", dir_str]).unwrap();
        assert_eq!(
            plan.jobs,
            [Job {
                input: PathBuf::from("a/in.png"),
                output: dir.join("in.avif"),
                format: OutputFormat::Avif,
            }]
        );
        assert_eq!(plan.directory, Some(dir));
    }

    #[test]
    fn standard_input_cannot_be_written_into_a_directory() {
        // `-` has no file name to give its output there.
        let dir = std::env::temp_dir();
        assert!(matches!(
            plan_of(&["-", "-o", dir.to_str().unwrap()]),
            Err(CliError::Usage(_))
        ));
    }

    #[test]
    fn an_output_over_its_own_input_is_refused() {
        assert!(matches!(
            plan_of(&["in.png", "--format", "png"]),
            Err(CliError::Usage(_))
        ));
    }

    #[test]
    fn the_png_effort_range_is_exactly_the_codec_ladder() {
        // The levels clap admits and the codec's rungs are two lists that must stay the same
        // list. Walking `from_level` from zero is the codec's own enumeration of its ladder, so a
        // rung added to `gamut_png::Preset` fails here instead of being silently unreachable from
        // the command line, and each admitted level must resolve to the rung of that level.
        let from_flag: Vec<u8> = (0..=u8::MAX)
            .filter(|level| parse_png(&["--png-effort", &level.to_string()]).is_ok())
            .map(|level| png_preset(level).level())
            .collect();
        let from_codec: Vec<u8> = (0..=u8::MAX)
            .map_while(PngCodecPreset::from_level)
            .map(PngCodecPreset::level)
            .collect();
        assert!(
            from_flag == from_codec,
            "--png-effort admits levels {from_flag:?} against the codec ladder {from_codec:?}"
        );
    }

    #[test]
    fn png_effort_defaults_to_the_small_rung() {
        // `small` is what this command encoded with before the flag existed.
        let args = parse_png(&[]).unwrap();
        assert_eq!(png_preset(args.png_effort), PngCodecPreset::Small);
        assert_eq!(args.png_refinement_passes, None);
    }

    #[test]
    fn png_refinement_passes_is_its_own_flag() {
        let args = parse_png(&["--png-effort", "3", "--png-refinement-passes", "0"]).unwrap();
        assert_eq!(png_preset(args.png_effort), PngCodecPreset::Smallest);
        assert_eq!(args.png_refinement_passes, Some(0));
    }

    #[test]
    fn the_retired_png_preset_flag_is_refused() {
        // The rename is a breaking change taken on purpose (issue #630); the old spelling must
        // fail loudly rather than be silently accepted under another meaning.
        let error = parse_png(&["--png-preset", "small"]).err().unwrap();
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn unset_avif_flags_leave_the_encoder_defaults() {
        // Each `--avif-*` flag only overrides; without them the encoder is the codec's own.
        let encoder = avif_with(&[]);
        let default = AvifEncoder::lossless();
        assert_eq!(encoder.config(), default.config());
        assert_eq!(encoder.speed(), default.speed());
        assert_eq!(encoder.threads(), default.threads());
        let lossy = avif_with(&["--avif-lossy", "--quality", "40"]);
        assert_eq!(lossy.config(), AvifEncoder::lossy(40).config());
        // Speed only moves lossy output, so that is where the flag's default must agree.
        assert_eq!(lossy.speed(), AvifEncoder::lossy(40).speed());
    }

    #[test]
    fn avif_lossy_selects_lossy_mode() {
        assert_eq!(avif_with(&["--avif-lossy"]).config().mode, AvifMode::Lossy);
    }

    #[test]
    fn deprecated_qindex_selects_lossy_mode() {
        assert_eq!(avif_with(&["--qindex", "1"]).config().mode, AvifMode::Lossy);
    }

    #[test]
    fn the_avif_speed_flag_offers_exactly_the_codec_presets() {
        // `0..=3` is written into the flag's parser and `Speed::ALL` is the codec's own list, so
        // a preset added to `gamut-av1` fails here instead of being unreachable from the CLI.
        for (level, preset) in AvifSpeed::ALL.iter().enumerate() {
            let level = level.to_string();
            assert_eq!(avif_with(&["--avif-speed", &level]).speed(), *preset);
        }
        let past_the_end = AvifSpeed::ALL.len().to_string();
        assert!(parse_avif(&["--avif-speed", &past_the_end]).is_err());
    }

    #[test]
    fn avif_threads_caps_the_encoder() {
        assert_eq!(
            avif_with(&["--avif-threads", "3"]).threads(),
            NonZeroUsize::new(3)
        );
    }

    #[test]
    fn avif_threads_zero_is_every_core() {
        assert_eq!(avif_with(&["--avif-threads", "0"]).threads(), None);
    }

    #[test]
    fn avif_alpha_quality_reaches_the_encoder() {
        let encoder = avif_with(&["--avif-lossy", "--avif-alpha-quality", "100"]);
        assert_eq!(encoder.config().alpha_quality, Some(100));
    }

    #[test]
    fn avif_alpha_quality_above_100_is_refused() {
        assert!(parse_avif(&["--avif-alpha-quality", "101"]).is_err());
    }

    #[test]
    fn avif_clean_alpha_reaches_the_encoder() {
        assert!(avif_with(&["--avif-clean-alpha"]).config().clean_alpha);
    }

    #[test]
    fn avif_matrix_maps_each_value() {
        for (flag, matrix) in [
            ("identity", MatrixCoefficients::Identity),
            ("bt601", MatrixCoefficients::Bt601),
            ("bt709", MatrixCoefficients::Bt709),
            ("bt2020", MatrixCoefficients::Bt2020Ncl),
        ] {
            assert_eq!(
                avif_with(&["--avif-matrix", flag]).config().matrix,
                matrix,
                "--avif-matrix {flag}"
            );
        }
        assert!(parse_avif(&["--avif-matrix", "ycgco"]).is_err());
    }

    #[test]
    fn avif_depth_maps_each_value() {
        for (flag, depth) in [
            ("8", BitDepth::Eight),
            ("10", BitDepth::Ten),
            ("12", BitDepth::Twelve),
        ] {
            assert_eq!(
                avif_with(&["--avif-depth", flag]).config().bit_depth,
                Some(depth),
                "--avif-depth {flag}"
            );
        }
        assert!(parse_avif(&["--avif-depth", "16"]).is_err());
    }

    #[test]
    fn avif_chroma_maps_each_value() {
        for (flag, chroma) in [
            ("444", ChromaSubsampling::Cs444),
            ("422", ChromaSubsampling::Cs422),
            ("420", ChromaSubsampling::Cs420),
        ] {
            assert_eq!(
                avif_with(&["--avif-chroma", flag]).config().chroma,
                chroma,
                "--avif-chroma {flag}"
            );
        }
        assert!(parse_avif(&["--avif-chroma", "400"]).is_err());
    }

    /// No input this command reads yields an IPTC-IIM block today, so the routing is pinned here:
    /// a TIFF output, whose `IPTC/NAA` tag holds it, keeps the block, and one without a slot for
    /// it does not.
    #[test]
    fn the_iptc_iim_block_is_carried_only_where_the_output_holds_it() {
        let meta = InputMetadata {
            iptc_iim: Some(b"iim".to_vec()),
            ..InputMetadata::default()
        };
        let dims = Dimensions::new(1, 1).unwrap();
        let into = |format| carry(&meta, format, Some(Samples::Colour), dims).iptc_iim;
        assert_eq!(into(OutputFormat::Tiff).as_deref(), Some(&b"iim"[..]));
        assert_eq!(into(OutputFormat::Webp), None);
    }
}
