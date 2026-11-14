//! What of an input's metadata a `convert` output carries: each carrier routed by
//! `gamut::capability`, the Exif claims a conversion can make false rewritten, and everything left
//! behind reported on stderr.

use gamut::capability::{Direction, supports};
use gamut::core::Dimensions;
use gamut::exif::{Exif, ExifTag, ExifWriter, IfdKind, Value as ExifValue};
use gamut::icc::{ColorSpace as IccColorSpace, ProfileHeader};
use gamut::metadata::Carrier;
use gamut::png::PngMetadata;
use gamut::tiff::MetadataNotice as TiffNotice;

use super::OutputFormat;
use crate::input::InputMetadata;

/// The samples an output codes, which decides the ICC profile that can describe them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Samples {
    /// One grey channel (plus alpha).
    Grey,
    /// Three colour channels (plus alpha).
    Colour,
}

/// The input metadata one output carries, after [`carry`] rewrote and dropped what it had to.
#[derive(Debug, Default)]
pub(super) struct Carried {
    /// The Exif payload as a bare TIFF stream (no `Exif\0\0` marker), its conversion-sensitive
    /// claims rewritten (see [`rewrite_exif`]).
    pub(super) exif: Option<Vec<u8>>,
    /// The XMP packet, verbatim.
    pub(super) xmp: Option<Vec<u8>>,
    /// The ICC profile, verbatim.
    pub(super) icc: Option<Vec<u8>>,
    /// The legacy IPTC-IIM dataset stream, verbatim.
    pub(super) iptc_iim: Option<Vec<u8>>,
    /// The Exif orientation (1–8) the output's stored pixels need for display, after the rewrite.
    /// Read even where the output cannot hold the Exif itself, for a format that signals
    /// orientation in its own header (JPEG XL).
    pub(super) orientation: Option<u16>,
}

/// Maps the input's metadata onto a `format` output of `dims` whose samples are `samples` —
/// `None` when the encoder fits its colour type to the profile itself, as a PNG input re-encoded to
/// PNG does — reporting on stderr everything left behind.
///
/// What the output can hold is `gamut::capability` table, not a judgement made here, so
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
pub(super) fn carry(
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
    match ExifWriter::new().with_marker(false).write(&exif) {
        Ok(bytes) => carried.exif = Some(bytes),
        Err(err) => {
            tracing::warn!(
                "input metadata not carried — the Exif payload does not re-encode: {err}"
            );
        }
    }
    carried
}

/// Reports what a TIFF encoder left behind of the input's metadata.
///
/// A TIFF has no Exif 0th IFD of its own: that directory's tags are TIFF 6.0 tags of the page's
/// own IFD 0. `gamut-tiff` carries the descriptive ones (`Make`, `Model`, `Orientation`, …) into it
/// beside the `GPSInfo` and `ExifIFD` sub-IFDs, writes the layout ones (`XResolution`,
/// `YCbCrPositioning`, …) from the pixels it lays out, and would hold a thumbnail as a page of its
/// own — so it leaves those behind and names each with a notice, which this renders, the 0th-IFD
/// fields together by name.
pub(super) fn report_tiff_notices(notices: &[TiffNotice]) {
    let mut fields = Vec::new();
    for notice in notices {
        match *notice {
            TiffNotice::ExifImageField { tag } | TiffNotice::ExifImageGroup { tag } => fields.push(
                ExifTag::from_id(IfdKind::Image, tag)
                    .map_or_else(|| format!("tag {tag}"), |tag| tag.name().to_owned()),
            ),
            TiffNotice::ExifThumbnail => tracing::warn!(
                "input metadata not carried — the Exif thumbnail, which a TIFF would hold as a \
                 page of its own"
            ),
            other => tracing::warn!("input metadata not carried — {other}"),
        }
    }
    if !fields.is_empty() {
        tracing::warn!(
            "input metadata not carried — the Exif 0th-IFD field(s) {}, which a TIFF's IFD 0 \
             holds as the TIFF encoder's own",
            fields.join(", ")
        );
    }
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
        (ExifTag::PixelXDimension, dims.width()),
        (ExifTag::PixelYDimension, dims.height()),
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

/// `xmp` as text, or `None` — with a warning — when it is not UTF-8, which an XMP packet must be in
/// the PNG and JPEG XL outputs (PNG §11.3.3.4's `iTXt` is UTF-8; JPEG XL's `xml ` box holds text).
pub(super) fn utf8_xmp(xmp: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(xmp).ok();
    if text.is_none() {
        tracing::warn!("input metadata not carried — the XMP packet, which is not UTF-8");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::{Dimensions, InputMetadata, OutputFormat, Samples, carry};

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
