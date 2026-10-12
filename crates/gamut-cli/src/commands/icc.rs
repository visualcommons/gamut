//! `gamut icc` — extract and inspect the ICC colour profile embedded in an image.
//!
//! Reads the raw ICC blob from a JPEG `APP2` segment, a PNG `iCCP` chunk (both via the `image`
//! crate, which reassembles/inflates them), or a TIFF/DNG `ICCProfile` tag (34675, via `gamut-tiff`)
//! — or treats the whole file as a standalone `.icc`/`.icm` profile — then parses it with
//! [`gamut::icc`] and prints the header, the decoded tags, and the ICC.1:2022 conformance verdict
//! (the §8 required tags, and §10.3's `cicp` MatrixCoefficients rule).
//! This is the end-to-end check that gamut-icc parses real profiles from real cameras.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use clap::{Args, ValueEnum};
use gamut::icc::{
    DeviceClass, IccProfile, IccReader, KnownTag, ProfileHeader, ProfileId, RenderingIntent,
    Signature, TagData,
};
use image::ImageDecoder;

use crate::error::CliError;

/// The TIFF/DNG `ICCProfile` tag (ICC blob stored as an IFD field).
const ICC_PROFILE_TAG: u16 = 34675;

/// The most tags to print before truncating.
const MAX_TAGS: usize = 64;

/// Arguments for `gamut icc`.
#[derive(Args)]
pub(crate) struct IccArgs {
    /// Input image (JPEG/PNG/TIFF/DNG) or a standalone `.icc` profile.
    input: PathBuf,
    /// Force the container instead of auto-detecting it (`raw` = a standalone ICC profile).
    #[arg(long, value_enum)]
    format: Option<Format>,
    /// Reject non-conformant profiles the lenient parser tolerates (see `gamut_icc::IccReader`).
    #[arg(long)]
    strict: bool,
    /// Recompute the profile's MD5 ID and report whether it matches the stored one.
    #[arg(long)]
    verify_id: bool,
}

/// The container to extract the ICC profile from.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum Format {
    /// JPEG (`APP2` `ICC_PROFILE` segments).
    Jpeg,
    /// PNG (`iCCP` chunk).
    Png,
    /// TIFF (`ICCProfile` tag 34675).
    Tiff,
    /// DNG (same `ICCProfile` tag as TIFF).
    Dng,
    /// A standalone ICC profile file (the whole file is the profile).
    Raw,
}

/// Runs the `icc` command: extract the embedded profile, parse it, and print a report.
pub(crate) fn run(args: &IccArgs) -> Result<(), CliError> {
    let data = std::fs::read(&args.input).map_err(|source| CliError::Io {
        path: args.input.clone(),
        source,
    })?;
    let format = args.format.unwrap_or_else(|| sniff(&data));
    let blob = extract(&data, format, &args.input)?;

    let profile = if args.strict {
        IccReader::new().strict(true).parse(&blob)?
    } else {
        IccProfile::parse(&blob)?
    };

    print_report(&args.input, format, &blob, &profile, args.verify_id);
    Ok(())
}

/// Detects the container from its magic bytes; anything unrecognised is treated as a raw profile.
fn sniff(data: &[u8]) -> Format {
    match data {
        [0xFF, 0xD8, 0xFF, ..] => Format::Jpeg,
        [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, ..] => Format::Png,
        [0x49, 0x49, 0x2A, 0x00, ..] | [0x4D, 0x4D, 0x00, 0x2A, ..] => Format::Tiff,
        _ => Format::Raw,
    }
}

/// Extracts the raw ICC blob for `format`, erroring if the container carries none.
fn extract(data: &[u8], format: Format, path: &Path) -> Result<Vec<u8>, CliError> {
    let blob = match format {
        Format::Raw => Some(data.to_vec()),
        Format::Jpeg => icc_from_image(
            path,
            image::codecs::jpeg::JpegDecoder::new(Cursor::new(data)),
        )?,
        Format::Png => {
            icc_from_image(path, image::codecs::png::PngDecoder::new(Cursor::new(data)))?
        }
        Format::Tiff | Format::Dng => icc_from_tiff(data)?,
    };
    blob.ok_or_else(|| CliError::NoIccProfile {
        path: path.to_path_buf(),
    })
}

/// Reads the ICC profile from an `image`-crate decoder (JPEG/PNG), mapping decode errors.
fn icc_from_image<D: ImageDecoder>(
    path: &Path,
    decoder: Result<D, image::ImageError>,
) -> Result<Option<Vec<u8>>, CliError> {
    let map = |source| CliError::Decode {
        path: path.to_path_buf(),
        source,
    };
    let mut decoder = decoder.map_err(map)?;
    decoder.icc_profile().map_err(map)
}

/// Reads the ICC profile from a TIFF/DNG `ICCProfile` tag (34675) in the first IFD.
fn icc_from_tiff(data: &[u8]) -> Result<Option<Vec<u8>>, CliError> {
    let file = gamut::tiff::read(data)?;
    Ok(file
        .ifds
        .first()
        .and_then(|ifd| ifd.get(ICC_PROFILE_TAG))
        .and_then(|value| match value {
            gamut::ifd::Value::Byte(bytes) | gamut::ifd::Value::Undefined(bytes) => {
                Some(bytes.clone())
            }
            _ => None,
        }))
}

/// Prints the header, tags, and conformance verdict for a parsed profile.
fn print_report(path: &Path, format: Format, blob: &[u8], profile: &IccProfile, verify_id: bool) {
    let h = &profile.header;
    println!(
        "icc profile from {} ({}, {} bytes)",
        path.display(),
        format_name(format),
        blob.len()
    );
    println!("  version:          {}", h.version);
    println!("  device class:     {}", device_class_name(h.device_class));
    println!(
        "  data space:       {}",
        Signature::from(h.data_color_space)
    );
    println!("  pcs:              {}", Signature::from(h.pcs));
    println!(
        "  pcs illuminant:   {}",
        illuminant_summary(h.pcs_illuminant.to_f64())
    );
    println!(
        "  rendering intent: {}",
        rendering_intent_name(h.rendering_intent)
    );
    println!("  flags:            {}", flags_summary(h));
    println!("  attributes:       {}", attributes_summary(h));
    if h.preferred_cmm != Signature::ZERO {
        println!("  preferred cmm:    {}", h.preferred_cmm);
    }
    if h.manufacturer != Signature::ZERO {
        println!("  manufacturer:     {}", h.manufacturer);
    }
    if verify_id {
        println!("  profile id:       {}", profile_id_summary(h, blob));
    }

    println!("  tags ({}):", profile.tags.len());
    for (sig, data) in profile.tags.iter().take(MAX_TAGS) {
        match KnownTag::from_signature(*sig) {
            Some(known) => println!("    - {sig} — {known:?} — {}", summarize(data)),
            None => println!("    - {sig} — {}", summarize(data)),
        }
    }
    if profile.tags.len() > MAX_TAGS {
        println!("    … and {} more", profile.tags.len() - MAX_TAGS);
    }

    let issues = profile.validate();
    if issues.is_empty() {
        println!("  conformance:      OK (§8 required tags present, §10.3 cicp rule met)");
    } else {
        println!("  conformance:      {} unmet requirement(s):", issues.len());
        for issue in &issues {
            println!("    - {}", issue.requirement);
        }
    }
}

/// A one-line summary of a tag's decoded data.
fn summarize(data: &TagData) -> String {
    match data {
        TagData::Text(text) => format!("text {:?}", truncate(text)),
        TagData::MultiLocalizedUnicode(mluc) => {
            format!("mluc {:?}", truncate(mluc.first().unwrap_or("")))
        }
        TagData::TextDescription(desc) => format!("desc {:?}", truncate(&desc.ascii)),
        TagData::Xyz(values) => match values.first() {
            Some(xyz) => {
                let [x, y, z] = xyz.to_f64();
                format!("XYZ [{x:.4}, {y:.4}, {z:.4}]")
            }
            None => "XYZ (empty)".to_owned(),
        },
        TagData::Curve(_) | TagData::ParametricCurve(_) => "tone curve".to_owned(),
        TagData::Signature(sig) => format!("signature {sig}"),
        TagData::DateTime(dt) => format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            dt.year, dt.month, dt.day, dt.hours, dt.minutes, dt.seconds
        ),
        TagData::S15Fixed16Array(v) => format!("s15Fixed16[{}]", v.len()),
        TagData::U16Fixed16Array(v) => format!("u16Fixed16[{}]", v.len()),
        TagData::UInt8Array(v) => format!("uInt8[{}]", v.len()),
        TagData::UInt16Array(v) => format!("uInt16[{}]", v.len()),
        TagData::UInt32Array(v) => format!("uInt32[{}]", v.len()),
        TagData::UInt64Array(v) => format!("uInt64[{}]", v.len()),
        TagData::Lut8(l) => format!("lut8 {}→{}", l.input_channels, l.output_channels),
        TagData::Lut16(l) => format!("lut16 {}→{}", l.input_channels, l.output_channels),
        TagData::LutAToB(l) => format!("lutAToB {}→{}", l.input_channels, l.output_channels),
        TagData::LutBToA(l) => format!("lutBToA {}→{}", l.input_channels, l.output_channels),
        TagData::NamedColor2(n) => format!("namedColor2 ({} colours)", n.colors.len()),
        TagData::Chromaticity(c) => format!("chromaticity ({} channels)", c.channels.len()),
        TagData::Cicp(c) => format!(
            "cicp {}/{}/{}/{}",
            c.colour_primaries,
            c.transfer_characteristics,
            c.matrix_coefficients,
            c.video_full_range_flag
        ),
        TagData::Measurement(_) => "measurement".to_owned(),
        TagData::ViewingConditions(_) => "viewing conditions".to_owned(),
        TagData::Data(d) => format!(
            "data ({}, {} bytes)",
            if d.is_ascii() { "ascii" } else { "binary" },
            d.data.len()
        ),
        TagData::ColorantOrder(o) => format!("colorant order ({} colorants)", o.order.len()),
        TagData::ColorantTable(t) => format!("colorant table ({} colorants)", t.colorants.len()),
        TagData::ProfileSequenceDesc(p) => {
            format!("profile sequence ({} entries)", p.entries.len())
        }
        TagData::ProfileSequenceIdentifier(p) => {
            format!("profile sequence ids ({} entries)", p.entries.len())
        }
        TagData::ResponseCurveSet16(r) => format!("response curves ({} sets)", r.curves.len()),
        TagData::Dict(d) => format!("dict ({} entries)", d.entries.len()),
        TagData::Raw { type_sig, bytes } => format!("raw {type_sig} ({} bytes)", bytes.len()),
        _ => "unmodelled".to_owned(),
    }
}

/// Truncates a string to a readable length for one-line summaries.
fn truncate(text: &str) -> String {
    const MAX: usize = 60;
    if text.chars().count() <= MAX {
        text.to_owned()
    } else {
        format!("{}…", text.chars().take(MAX).collect::<String>())
    }
}

/// The display name of a device class (with its signature).
fn device_class_name(class: DeviceClass) -> String {
    let name = match class {
        DeviceClass::Input => "Input",
        DeviceClass::Display => "Display",
        DeviceClass::Output => "Output",
        DeviceClass::DeviceLink => "DeviceLink",
        DeviceClass::ColorSpace => "ColorSpace",
        DeviceClass::Abstract => "Abstract",
        DeviceClass::NamedColor => "NamedColor",
    };
    format!("{name} ({})", Signature::from(class))
}

/// The display name of a rendering intent.
fn rendering_intent_name(intent: RenderingIntent) -> &'static str {
    match intent {
        RenderingIntent::Perceptual => "perceptual",
        RenderingIntent::MediaRelativeColorimetric => "media-relative colorimetric",
        RenderingIntent::Saturation => "saturation",
        RenderingIntent::IccAbsoluteColorimetric => "ICC-absolute colorimetric",
    }
}

/// Summarizes the PCS illuminant, flagging whether it is the mandated D50.
fn illuminant_summary(xyz: [f64; 3]) -> String {
    const D50: [f64; 3] = [0.9642, 1.0, 0.8249];
    let is_d50 = xyz
        .iter()
        .zip(D50)
        .all(|(got, want)| (got - want).abs() < 1.0e-3);
    let tag = if is_d50 { "D50" } else { "non-D50!" };
    format!("({:.4}, {:.4}, {:.4}) {tag}", xyz[0], xyz[1], xyz[2])
}

/// Renders the decoded profile-flag bits.
fn flags_summary(h: &ProfileHeader) -> String {
    let mut parts = Vec::new();
    parts.push(if h.is_embedded() {
        "embedded"
    } else {
        "not embedded"
    });
    if h.cannot_be_used_independently() {
        parts.push("dependent");
    }
    parts.join(", ")
}

/// Renders the decoded device-attribute bits.
fn attributes_summary(h: &ProfileHeader) -> String {
    [
        if h.is_transparency() {
            "transparency"
        } else {
            "reflective"
        },
        if h.is_matte() { "matte" } else { "glossy" },
        if h.is_negative_polarity() {
            "negative"
        } else {
            "positive"
        },
        if h.is_black_and_white() {
            "black & white"
        } else {
            "colour"
        },
    ]
    .join(", ")
}

/// Renders the stored profile ID and whether it matches the recomputed MD5.
fn profile_id_summary(h: &ProfileHeader, blob: &[u8]) -> String {
    if h.profile_id.is_zero() {
        return "not set".to_owned();
    }
    let matches = ProfileId::compute(blob) == h.profile_id;
    format!(
        "{} (matches recomputed MD5: {})",
        h.profile_id,
        yes_no(matches)
    )
}

/// The display name of a container format.
fn format_name(format: Format) -> &'static str {
    match format {
        Format::Jpeg => "JPEG",
        Format::Png => "PNG",
        Format::Tiff => "TIFF",
        Format::Dng => "DNG",
        Format::Raw => "raw ICC",
    }
}

/// `yes`/`no` for a boolean.
fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}
