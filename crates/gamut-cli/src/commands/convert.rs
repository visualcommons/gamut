//! `gamut convert` — decode an image and re-encode it with a gamut codec.

use std::io::Write;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use clap::{Args, ValueEnum};
use gamut::avif::{AvifEncodeReport, AvifEncoder};
use gamut::core::{EncodeImage, ImageRef, Rgb8, Rgba8};
use gamut::jpeg::{ChromaSubsampling as JpegChroma, JpegEncoder};
use gamut::jxl::{
    Container as JxlContainer, Distance as JxlDistance, Effort as JxlEffort, JxlEncoder,
    ModularMode as JxlModularMode,
};
use gamut::png::{PngEncoder, Preset as PngCodecPreset};
use gamut::tiff::{Compression as TiffCompression, TiffEncoder};
use gamut::webp::{Effort as WebpEffort, NearLossless as WebpNearLossless, WebpEncoder};

use crate::error::CliError;
use crate::input::{InputMetadata, NativeImage, STDIO, decode_bytes, decode_native, read_input};

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
    /// AVIF mode selector: `0` keeps the lossless default; any nonzero value selects lossy AVIF at
    /// `--quality` (the encoder now takes a `0..=100` quality rather than a raw `base_q_idx`).
    #[arg(long, default_value_t = 0)]
    qindex: u8,
    /// Encode lossy (WebP VP8 intra) instead of lossless. For AVIF, select lossy with `--qindex`.
    #[arg(long)]
    lossy: bool,
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
    /// PNG effort preset: one setting for the five knobs that trade encoding time for output size
    /// (compression level, DEFLATE effort, filter strategy, optimal-parse span, and lossless
    /// colour-type reduction). `small` is the default and is what this command has always done;
    /// `smallest` adds the whole-image filter search, which is much slower. Ignored for other
    /// output formats.
    #[arg(long = "png-preset", value_enum, default_value = "small")]
    png_preset: PngPreset,
    /// PNG DEFLATE effort: optimal-parse refinement passes, overriding whatever `--png-preset`
    /// chose (0 = lazy parse only; zopfli's default budget is 15). Only the best compression level
    /// consults it, so it does nothing under `--png-preset fast` or `balanced`. Omitting it keeps
    /// the preset's own budget. Ignored for other output formats.
    #[arg(long)]
    png_effort: Option<u8>,
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
    /// Drop the input's metadata instead of carrying it into the output. By default a PNG input
    /// re-encoded to PNG keeps its EXIF, ICC profile, XMP packet, text annotations and colour
    /// chunks; a stripped file is smaller, an unstripped one is colour-accurate, so the default
    /// is the one that loses nothing. Anything that cannot be carried — the C2PA manifest store,
    /// signed over the bytes of the file it was made for — and anything carried in a shape the
    /// PNG specification does not endorse is reported on stderr rather than passed over in
    /// silence. Applies to the PNG output path with a PNG input, and to AVIF output, which carries
    /// the ICC profile and Exif of a PNG, JPEG, WebP or JPEG XL input (the ICC profile only where
    /// it describes the decoded pixels — not for a lossy, XYB-coded JPEG XL, and not for a grey
    /// image with alpha or 16-bit samples, which AVIF codes as colour); every other pair drops
    /// metadata regardless.
    #[arg(long)]
    strip_metadata: bool,
}

/// Output container/codec for `gamut convert`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum OutputFormat {
    /// AVIF — lossless or lossy intra via `--qindex`; keeps the input's alpha and grey, and codes
    /// a 16-bit input at 12 bits.
    Avif,
    /// WebP — lossless (VP8L) or lossy (VP8, with `--lossy`); transparency is preserved.
    Webp,
    /// TIFF (8-bit RGB; uncompressed, or PackBits with `--packbits`).
    Tiff,
    /// PNG — lossless; transparency preserved, with automatic lossless colour-type reduction at
    /// the default `--png-preset`.
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

/// PNG effort preset for `--png-preset`, naming the rungs of `gamut_png::Preset`.
///
/// The discriminants **are** the codec's ladder levels, and `to_codec` resolves them through
/// [`PngCodecPreset::from_level`] rather than re-deciding the mapping — the same route
/// `--webp-effort` and `--jxl-effort` take through their own ladders. A named value enum rather
/// than the siblings' bare integer only because clap prints a rung's doc comment in `--help`,
/// which a number cannot.
#[derive(Clone, Copy, ValueEnum)]
#[repr(u8)]
pub(crate) enum PngPreset {
    /// Fastest: greedy matching and one fixed filter, accepting a larger file.
    Fast = 0,
    /// The `gamut-png` library default — the balanced speed/size point.
    Balanced = 1,
    /// The optimal parse and lossless reduction on one filter heuristic; this command's default.
    Small = 2,
    /// Adds the whole-image filter search, zopfli's own refinement budget and a wider
    /// optimal-parse span. Six times slower than `small` and worth about 3.6% on gamut's corpus,
    /// but nothing at all on three of its nine rows: measure your own material.
    Smallest = 3,
}

impl PngPreset {
    /// Maps the CLI choice onto the codec's [`PngCodecPreset`] rung.
    fn to_codec(self) -> PngCodecPreset {
        // `the_png_preset_flag_offers_exactly_the_codec_ladder` pins that every discriminant here
        // is a level the codec admits, so the fallback is unreachable; it exists because this is
        // a CLI and a panic is not an error report.
        PngCodecPreset::from_level(self as u8).unwrap_or_default()
    }
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
    let (raw_len, dims) = match format {
        OutputFormat::Avif => {
            // The input's own layout, so alpha and 16-bit precision reach the encoder (issue
            // #663); a 16-bit input codes at the encoder's default depth, 12 bits.
            let (image, metadata) = decode_native(input, bytes)?;
            let dims = image.dimensions();
            tracing::info!(
                width = dims.width,
                height = dims.height,
                bytes = image.byte_len(),
                "decoded input"
            );
            // `AvifEncoder` migrated from a raw `base_q_idx` to a lossless()/lossy(quality) model;
            // qindex 0 keeps the lossless default, any nonzero value selects lossy at --quality.
            let mut encoder = if args.qindex == 0 {
                AvifEncoder::lossless()
            } else {
                AvifEncoder::lossy(args.quality)
            };
            if !args.strip_metadata {
                encoder = with_avif_metadata(encoder, metadata);
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
            let encoder = if args.lossy {
                WebpEncoder::lossy(args.quality)
            } else {
                WebpEncoder::lossless()
            };
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
            let image = ImageRef::<Rgb8>::new(&rgb, dims)?;
            TiffEncoder::new()
                .with_compression(compression)
                .encode_image(image, &mut out)?;
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
            // The preset sets every size/time knob; `--png-effort` still overrides the one it
            // names, so it must be applied after.
            let mut encoder = PngEncoder::new().with_preset(args.png_preset.to_codec());
            if let Some(effort) = args.png_effort {
                encoder = encoder.with_effort(effort);
            }
            // Carry the input's metadata rather than dropping it (issue #483). `png_metadata`
            // walks the bytes already read, which is cheap (it skips IDAT by length and never
            // inflates a pixel); it yields nothing for an input that is not a PNG.
            let metadata = (!args.strip_metadata)
                .then(|| png_metadata(bytes))
                .flatten();
            if let Some(metadata) = &metadata {
                tracing::info!(
                    texts = metadata.texts.len(),
                    exif = metadata.exif.is_some(),
                    icc = metadata.icc_profile.is_some(),
                    xmp = metadata.xmp.is_some(),
                    "carrying input metadata"
                );
                encoder = encoder.with_metadata(metadata);
                // Say what could not come along, and what came along with a caveat. Silent loss
                // is the defect this path exists to remove, and a payload the spec forbids
                // carrying is still a payload the caller had.
                for notice in encoder.metadata_notices() {
                    if notice.carried() {
                        tracing::warn!("input metadata carried with a caveat — {notice}");
                    } else {
                        tracing::warn!("input metadata not carried — {notice}");
                    }
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
            let encoder = match args.jxl_distance {
                Some(distance) => JxlEncoder::lossy(JxlDistance::new(distance)?),
                None => JxlEncoder::lossless(),
            };
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
            encoder
                .with_effort(effort)
                .with_modular(args.jxl_modular.to_codec())
                .with_container(container)
                .encode_image(ImageRef::<Rgba8>::new(&rgba, dims)?, &mut out)?;
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
            encoder.encode_image(ImageRef::<Rgb8>::new(&rgb, dims)?, &mut out)?;
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

/// The metadata the input `bytes` carry, or `None` when they are not a PNG.
///
/// Deliberately total: the input has already been decoded successfully by the time this is
/// called, so an error here means the file is simply not a PNG — a JPEG or WebP input has
/// metadata of its own, but mapping that into PNG chunks is a cross-format job this command does
/// not do yet. Failing to *read* metadata must never fail a conversion whose pixels are fine.
fn png_metadata(bytes: &[u8]) -> Option<gamut::png::PngMetadata> {
    gamut::png::metadata(bytes).ok()
}

/// Carries the input's ICC profile and Exif into an AVIF encode, both verbatim.
///
/// The encoder does not validate the Exif stream, and libavif rejects a whole file whose Exif item
/// is not a TIFF stream — so a payload that does not open with a TIFF byte-order header (`II*\0` or
/// `MM\0*`, TIFF 6.0 §2) is left out with a warning rather than written into a file a reader then
/// refuses. Every input decoder already strips JPEG's `Exif\0\0` prefix; one still present is a
/// malformed WebP `EXIF` chunk, and it falls under the same rule.
fn with_avif_metadata(mut encoder: AvifEncoder, metadata: InputMetadata) -> AvifEncoder {
    if let Some(icc) = &metadata.icc {
        tracing::info!(bytes = icc.len(), "carrying input ICC profile");
        encoder = encoder.with_icc_profile(icc);
    }
    if let Some(exif) = &metadata.exif {
        if exif.starts_with(b"II*\0") || exif.starts_with(b"MM\0*") {
            tracing::info!(bytes = exif.len(), "carrying input Exif");
            encoder = encoder.with_exif(exif);
        } else {
            tracing::warn!("input metadata not carried — the Exif payload is not a TIFF stream");
        }
    }
    encoder
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
    use std::path::PathBuf;

    use clap::{Parser, ValueEnum};

    use super::{ConvertArgs, Job, OutputFormat, Plan, PngCodecPreset, PngPreset, plan};
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
    fn the_png_preset_flag_offers_exactly_the_codec_ladder() {
        // The flag's values and the codec's rungs are two lists that must stay the same list.
        // Walking `from_level` from zero is the codec's own enumeration of its ladder, so a rung
        // added to `gamut_png::Preset` fails here instead of being silently unreachable from the
        // command line — which is what a hand-written arm-per-variant mapping could not catch.
        let from_flag: Vec<u8> = PngPreset::value_variants()
            .iter()
            .map(|preset| preset.to_codec().level())
            .collect();
        let from_codec: Vec<u8> = (0..=u8::MAX)
            .map_while(PngCodecPreset::from_level)
            .map(PngCodecPreset::level)
            .collect();
        assert!(
            from_flag == from_codec,
            "--png-preset offers levels {from_flag:?} against the codec ladder {from_codec:?}"
        );
    }
}
