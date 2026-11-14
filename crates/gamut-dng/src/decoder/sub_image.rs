//! Sub-image decoding: every non-raw image IFD — preview, transparency or semantic mask, depth
//! map, enhanced image — as a [`SubImage`], plus IFD 0's depth-map description tags.

use gamut_core::Dimensions;
use gamut_ifd::{ByteOrder, Value, extent_slices};

use super::TrackedIfd;
use super::image_data::decode_image_data;
use super::profile_tags::ascii_value;
use crate::subimage::{
    DepthInfo, MaskSubArea, SemanticMaskInfo, SubImage, SubImageData, SubImageKind,
};
use crate::tags;

/// Extracts the first `(numerator, denominator)` of an unsigned `RATIONAL` value.
fn rational_pair(value: Option<&Value>) -> Option<(u32, u32)> {
    value?.as_rationals()?.first().copied()
}

/// Decodes one non-raw image IFD into a [`SubImage`], or `None` when the IFD carries no image
/// data (or its geometry/chunks are too malformed to represent — `deconstruct` diagnoses those).
///
/// Sub-images are auxiliary, so pixel decoding is **best-effort**: a scheme or stream outside
/// decode scope (baseline-DCT JPEG previews, lossy JPEG, float JXL) falls back to carrying the
/// compressed chunks verbatim rather than failing the whole decode.
pub(super) fn decode_sub_image(
    ifd: &TrackedIfd,
    data: &[u8],
    order: ByteOrder,
) -> Option<SubImage> {
    if ifd.get(tags::STRIP_OFFSETS).is_none() && ifd.get(tags::TILE_OFFSETS).is_none() {
        return None;
    }
    let width = ifd.get_u32(tags::IMAGE_WIDTH)?;
    let height = ifd.get_u32(tags::IMAGE_LENGTH)?;
    let dimensions = Dimensions::new(width, height).ok()?;
    let spp = u16::try_from(ifd.get_u32(tags::SAMPLES_PER_PIXEL).unwrap_or(1)).ok()?;
    let bits = ifd
        .get_u32_vec(tags::BITS_PER_SAMPLE)
        .and_then(|v| v.first().copied())
        .unwrap_or(1) as u16;
    let photometric = ifd
        .get_u32(tags::PHOTOMETRIC_INTERPRETATION)
        .and_then(|c| u16::try_from(c).ok())
        .unwrap_or(0);
    let kind = SubImageKind::from_code(ifd.get_u32(tags::NEW_SUBFILE_TYPE).unwrap_or(0));
    let compression = ifd.get_u32(tags::COMPRESSION).unwrap_or(1) as u16;

    let data_payload =
        match decode_image_data(ifd, data, order, width, height, usize::from(spp), bits) {
            Ok(samples) => SubImageData::Decoded(samples),
            Err(_) => SubImageData::Undecoded {
                compression,
                chunks: undecoded_chunks(ifd, data)?,
            },
        };

    let semantic = semantic_mask_info(ifd, dimensions);
    Some(SubImage {
        kind,
        photometric,
        dimensions,
        bits_per_sample: bits,
        samples_per_pixel: spp,
        data: data_payload,
        semantic: if kind == SubImageKind::SemanticMask {
            Some(semantic.unwrap_or_default())
        } else {
            semantic
        },
        // Filled in by `decode` once every consumer has run.
        extra_tags: Vec::new(),
    })
}

/// The stored chunks of an image IFD, verbatim (strips or tiles, in offset order). Only the
/// offset/count tags are consumed; the rest of the layout and interpretation tags
/// (`RowsPerStrip`, `TileWidth`/`TileLength`, `SampleFormat`, interleave factors, …) stay in the
/// extras — the consumer of undecoded chunks needs them to interpret the data.
fn undecoded_chunks(ifd: &TrackedIfd, data: &[u8]) -> Option<Vec<Vec<u8>>> {
    let (offset_tag, count_tag) = if ifd.get(tags::TILE_OFFSETS).is_some() {
        (tags::TILE_OFFSETS, tags::TILE_BYTE_COUNTS)
    } else {
        (tags::STRIP_OFFSETS, tags::STRIP_BYTE_COUNTS)
    };
    let offsets = ifd.get_u64_vec(offset_tag)?;
    let counts = ifd.get_u64_vec(count_tag)?;
    let chunks = extent_slices(&offsets, &counts, data, env!("CARGO_PKG_NAME")).ok()?;
    Some(chunks.into_iter().map(<[u8]>::to_vec).collect())
}

/// Reads the semantic-mask tags, when any is present. `MaskSubArea` is validated by pairing top
/// with the mask height and left with the mask width (as the SDK does — the spec's own
/// inequality text transposes the axes) and ignored when invalid, per spec.
fn semantic_mask_info(ifd: &TrackedIfd, mask_dims: Dimensions) -> Option<SemanticMaskInfo> {
    let name = ascii_value(ifd.get(tags::SEMANTIC_NAME));
    let instance_id = ascii_value(ifd.get(tags::SEMANTIC_INSTANCE_ID));
    let sub_area = ifd
        .get_u32_vec(tags::MASK_SUB_AREA)
        .filter(|v| v.len() == 4)
        .and_then(|v| {
            let area = MaskSubArea {
                top: v[0],
                left: v[1],
                full_width: v[2],
                full_height: v[3],
            };
            let fits = u64::from(area.top) + u64::from(mask_dims.height())
                <= u64::from(area.full_height)
                && u64::from(area.left) + u64::from(mask_dims.width())
                    <= u64::from(area.full_width);
            if !fits {
                // Ignored per spec, but not dropped: the rejected value surfaces in the extras.
                ifd.untouch(tags::MASK_SUB_AREA);
            }
            fits.then_some(area)
        });
    if name.is_none() && instance_id.is_none() && sub_area.is_none() {
        return None;
    }
    Some(SemanticMaskInfo {
        name,
        instance_id,
        sub_area,
    })
}

/// Reads IFD 0's depth-map description tags, when any is present.
pub(super) fn decode_depth_info(ifd0: &TrackedIfd) -> Option<DepthInfo> {
    let short = |tag: u16| ifd0.get_u32(tag).and_then(|v| u16::try_from(v).ok());
    let rational = |tag: u16| rational_pair(ifd0.get(tag));
    let info = DepthInfo {
        format: short(tags::DEPTH_FORMAT),
        near: rational(tags::DEPTH_NEAR),
        far: rational(tags::DEPTH_FAR),
        units: short(tags::DEPTH_UNITS),
        measure_type: short(tags::DEPTH_MEASURE_TYPE),
    };
    (info != DepthInfo::default()).then_some(info)
}

#[cfg(test)]
mod tests {
    use gamut_ifd::Ifd;

    use super::*;

    /// `MaskSubArea` validity pairs top with the mask *height* and left with the mask *width*
    /// (the SDK's reading; the spec's inequality text transposes the axes), each axis rejecting
    /// on its own. Boundary values distinguish the additions from any other arithmetic.
    #[test]
    fn mask_sub_area_validation_pairs_axes_exactly() {
        let dims = Dimensions::new(2, 2).unwrap(); // a 2x2 stored mask
        let area_of = |vals: Vec<u32>| {
            let mut ifd = Ifd::new();
            ifd.set(tags::SEMANTIC_NAME, Value::Ascii("m".into()));
            ifd.set(tags::MASK_SUB_AREA, Value::Long(vals));
            semantic_mask_info(&TrackedIfd::new(&ifd), dims).and_then(|s| s.sub_area)
        };
        // top + height against H_full, exactly at the boundary (3 + 2 == 5).
        assert!(area_of(vec![3, 0, 4, 5]).is_some());
        assert!(area_of(vec![4, 0, 4, 5]).is_none());
        // left + width against W_full, exactly at the boundary (3 + 2 == 5).
        assert!(area_of(vec![0, 3, 5, 4]).is_some());
        assert!(area_of(vec![0, 4, 5, 4]).is_none());
        // One failing axis alone invalidates (top overflows, left fits).
        assert!(area_of(vec![3, 0, 4, 4]).is_none());
        // A tall, narrow full mask fits a 2x2 crop — the transposed pairing would reject it.
        assert!(area_of(vec![0, 0, 2, 6]).is_some());
    }

    #[test]
    fn decode_depth_info_reads_when_present() {
        let mut ifd = Ifd::new();
        assert_eq!(decode_depth_info(&TrackedIfd::new(&ifd)), None);
        ifd.set(tags::DEPTH_FORMAT, Value::Short(vec![2]));
        ifd.set(tags::DEPTH_NEAR, Value::Rational(vec![(1, 10)]));
        ifd.set(tags::DEPTH_UNITS, Value::Short(vec![1]));
        let info = decode_depth_info(&TrackedIfd::new(&ifd)).expect("depth info");
        assert_eq!(info.format, Some(2));
        assert_eq!(info.near, Some((1, 10)));
        assert_eq!(info.far, None);
        assert_eq!(info.units, Some(1));
        assert_eq!(info.measure_type, None);
    }
}
