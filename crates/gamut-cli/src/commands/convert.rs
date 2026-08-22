//! `gamut convert` — decode an image and re-encode it with a gamut codec.

use std::path::PathBuf;

use clap::{Args, ValueEnum};
use gamut::avif::AvifEncoder;
use gamut::core::{EncodeImage, ImageRef, Rgb8, Rgba8};
use gamut::jpeg::{ChromaSubsampling as JpegChroma, JpegEncoder};
use gamut::jxl::{
    Container as JxlContainer, Distance as JxlDistance, Effort as JxlEffort, JxlEncoder,
    ModularMode as JxlModularMode,
};
use gamut::png::{Level as PngLevel, PngEncoder};
use gamut::tiff::{Compression as TiffCompression, TiffEncoder};
use gamut::webp::{Effort as WebpEffort, NearLossless as WebpNearLossless, WebpEncoder};

use crate::error::CliError;
use crate::input::{decode_rgb8, decode_rgba8};

/// Arguments for `gamut convert`.
#[derive(Args)]
pub(crate) struct ConvertArgs {
    /// Input image (PNG, JPEG, PPM/P6, WebP, or JPEG XL). WebP and JPEG XL are decoded by gamut's
    /// own decoders.
    input: PathBuf,
    /// Output file. The format is inferred from its extension unless `--format` is given.
    output: PathBuf,
    /// Output format. Defaults to the output file's extension.
    #[arg(long, value_enum)]
    format: Option<OutputFormat>,
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
    /// PNG DEFLATE effort: optimal-parse refinement passes at the always-used best compression
    /// level (0 = lazy parse only; zopfli's default budget is 15). Omitting it keeps the encoder
    /// default (6). Ignored for other output formats.
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
}

/// Output container/codec for `gamut convert`.
#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum OutputFormat {
    /// AVIF (8-bit RGB; lossless or lossy intra via `--qindex`).
    Avif,
    /// WebP — lossless (VP8L) or lossy (VP8, with `--lossy`); transparency is preserved.
    Webp,
    /// TIFF (8-bit RGB; uncompressed, or PackBits with `--packbits`).
    Tiff,
    /// PNG — lossless; transparency preserved, with automatic lossless colour-type reduction.
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

/// Runs the `convert` command: decode the input, encode it, and report the result.
pub(crate) fn run(args: &ConvertArgs) -> Result<(), CliError> {
    let format = resolve_format(args)?;

    let mut out = Vec::new();
    let (raw_len, dims) = match format {
        OutputFormat::Avif => {
            let (rgb, dims) = decode_rgb8(&args.input)?;
            tracing::info!(
                width = dims.width,
                height = dims.height,
                bytes = rgb.len(),
                "decoded input"
            );
            // `AvifEncoder` migrated from a raw `base_q_idx` to a lossless()/lossy(quality) model;
            // qindex 0 keeps the lossless default, any nonzero value selects lossy at --quality.
            let encoder = if args.qindex == 0 {
                AvifEncoder::lossless()
            } else {
                AvifEncoder::lossy(args.quality)
            };
            encoder.encode_image(ImageRef::<Rgb8>::new(&rgb, dims)?, &mut out)?;
            (rgb.len(), dims)
        }
        OutputFormat::Webp => {
            // RGBA so transparency survives; `encode_rgba8` emits a simple file when fully opaque.
            let (rgba, dims) = decode_rgba8(&args.input)?;
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
            let (rgb, dims) = decode_rgb8(&args.input)?;
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
            let (rgba, dims) = decode_rgba8(&args.input)?;
            tracing::info!(
                width = dims.width,
                height = dims.height,
                bytes = rgba.len(),
                "decoded input"
            );
            let mut encoder = PngEncoder::new()
                .with_compression(PngLevel::Best)
                .with_auto_reduce(true);
            if let Some(effort) = args.png_effort {
                encoder = encoder.with_effort(effort);
            }
            encoder.encode_image(ImageRef::<Rgba8>::new(&rgba, dims)?, &mut out)?;
            (rgba.len(), dims)
        }
        OutputFormat::Jxl => {
            // RGBA so transparency survives, matching the PNG/WebP paths.
            let (rgba, dims) = decode_rgba8(&args.input)?;
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
            let (rgb, dims) = decode_rgb8(&args.input)?;
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
    tracing::info!(bytes = out.len(), lossy = args.lossy, "encoded output");

    std::fs::write(&args.output, &out).map_err(|source| CliError::Io {
        path: args.output.clone(),
        source,
    })?;

    let ratio = if out.is_empty() {
        0.0
    } else {
        raw_len as f64 / out.len() as f64
    };
    println!(
        "wrote {} ({}x{}, {} bytes, {ratio:.2}x vs raw RGB)",
        args.output.display(),
        dims.width,
        dims.height,
        out.len(),
    );
    Ok(())
}

/// Picks the output format from `--format`, falling back to the output file's extension.
fn resolve_format(args: &ConvertArgs) -> Result<OutputFormat, CliError> {
    if let Some(format) = args.format {
        return Ok(format);
    }
    match args
        .output
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
