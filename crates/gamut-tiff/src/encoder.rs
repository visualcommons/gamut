//! The TIFF encoder.

use gamut_core::{
    Bilevel, Cmyk8, Dimensions, EncodeImage, Error, Gray8, Gray16, ImageRef, Indexed8, Result,
    Rgb8, Rgb16, Rgba8, Rgba16,
};
use gamut_ifd::{ByteOrder, Ifd, Value, Variant};

use crate::compression::{Compression, ccitt, deflate, lzw, packbits, predictor};
use crate::ifd::{PhotometricInterpretation, Predictor};
use crate::metadata::TiffMetadata;
use crate::palette::Palette8;
use crate::{tags, writer};

/// The on-disk sample layout of an image, shared by the 8-bit and bilevel encode paths.
struct SampleLayout {
    spp: usize,
    bits_per_sample: u16,
    stored_row_bytes: usize,
    photometric: PhotometricInterpretation,
}

/// Encoder for baseline TIFF images.
///
/// Writes chunky (`PlanarConfiguration = 1`) strips or tiles using the compression selected by
/// [`Self::with_compression`]. Supports 8- and 16-bit grayscale/RGB/RGBA, 8-bit CMYK/palette, and
/// 1-bit bilevel. 16-bit samples are written in this encoder's byte order; no `SampleFormat` tag is
/// emitted, since unsigned integer is the TIFF default and the only format written.
/// Emits classic TIFF by default, or BigTIFF (64-bit offsets) when [`Self::with_big_tiff`] is set.
#[derive(Debug, Clone)]
pub struct TiffEncoder {
    order: ByteOrder,
    compression: Compression,
    predictor: Predictor,
    tiling: Option<(u32, u32)>,
    big_tiff: bool,
    metadata: TiffMetadata,
}

impl Default for TiffEncoder {
    fn default() -> Self {
        Self {
            order: ByteOrder::LittleEndian,
            compression: Compression::None,
            predictor: Predictor::None,
            tiling: None,
            big_tiff: false,
            metadata: TiffMetadata::new(),
        }
    }
}

impl TiffEncoder {
    /// Creates an encoder that writes little-endian (`II`) TIFF.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a copy of this encoder that writes in the given byte order.
    #[must_use]
    pub fn with_byte_order(mut self, order: ByteOrder) -> Self {
        self.order = order;
        self
    }

    /// Returns a copy of this encoder that compresses image data with `compression`.
    #[must_use]
    pub fn with_compression(mut self, compression: Compression) -> Self {
        self.compression = compression;
        self
    }

    /// Returns a copy of this encoder that applies `predictor` before compression.
    ///
    /// [`Predictor::HorizontalDifferencing`] requires 8- or 16-bit samples and pairs well with LZW
    /// or Deflate. At 16 bits it differences sample *values* (TIFF 6.0 §14), not bytes.
    #[must_use]
    pub fn with_predictor(mut self, predictor: Predictor) -> Self {
        self.predictor = predictor;
        self
    }

    /// Returns a copy of this encoder that writes the image as tiles of `tile_width × tile_height`
    /// pixels instead of strips.
    ///
    /// Both dimensions must be positive multiples of 16. Tiling supports every byte-oriented
    /// compression at 8 and 16 bits; horizontal differencing on encode is currently enabled with
    /// Deflate.
    #[must_use]
    pub fn with_tiling(mut self, tile_width: u32, tile_height: u32) -> Self {
        self.tiling = Some((tile_width, tile_height));
        self
    }

    /// Returns a copy of this encoder that writes BigTIFF (magic `43`, 64-bit offsets) instead of
    /// classic TIFF.
    ///
    /// BigTIFF only widens the container's structural fields; every colour mode, compression
    /// scheme, strip/tile layout, and multi-page feature applies unchanged, so this composes with
    /// the other builders. Its 64-bit offsets let a file exceed the 4 GiB classic limit. A reader
    /// detects the variant from the header magic, so no decoder flag is needed. Defaults to off.
    #[must_use]
    pub fn with_big_tiff(mut self, big_tiff: bool) -> Self {
        self.big_tiff = big_tiff;
        self
    }

    /// Returns a copy of this encoder that embeds `metadata` — an Exif sub-IFD plus opaque
    /// XMP / IPTC-IIM / ICC blocks.
    ///
    /// The blocks and the Exif sub-IFD go in **IFD 0**, which for
    /// [`encode_pages_rgb8`](Self::encode_pages_rgb8) is the first page: they describe the
    /// document, not one of its pages.
    #[must_use]
    pub fn with_metadata(mut self, metadata: TiffMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// The container variant this encoder writes (BigTIFF when [`Self::with_big_tiff`] is set).
    fn variant(&self) -> Variant {
        if self.big_tiff {
            Variant::Big
        } else {
            Variant::Classic
        }
    }

    /// Encodes an 8-bit palette-colour image: one [`Indexed8`] sample per pixel selecting an entry
    /// of `palette`.
    ///
    /// `indices` is the `width * height` index buffer (already validated by [`ImageRef`]); `palette`
    /// is the 256-entry colour table. Returns the number of bytes written. Palette colour does not
    /// fit the single-buffer [`EncodeImage`] shape (it needs the separate colour table), so it stays
    /// an inherent method.
    pub fn encode_palette8(
        &self,
        indices: ImageRef<'_, Indexed8>,
        palette: &Palette8,
        out: &mut Vec<u8>,
    ) -> Result<usize> {
        let w = indices.width() as usize;
        let colormap = palette.to_tiff_colormap();
        self.encode_packed(
            indices.as_samples(),
            indices.dimensions(),
            &SampleLayout {
                spp: 1,
                bits_per_sample: 8,
                stored_row_bytes: w,
                photometric: PhotometricInterpretation::Palette,
            },
            &[(tags::COLOR_MAP, Value::Short(colormap))],
            out,
        )
    }

    fn encode_8bit(
        &self,
        pixels: &[u8],
        dims: Dimensions,
        spp: usize,
        photometric: PhotometricInterpretation,
        out: &mut Vec<u8>,
    ) -> Result<usize> {
        // The caller is an EncodeImage impl handing us an ImageRef-validated buffer, so
        // pixels.len() == width * height * spp holds and the product cannot overflow.
        let row_bytes = dims.width as usize * spp;
        debug_assert_eq!(pixels.len(), row_bytes * dims.height as usize);
        self.encode_packed(
            pixels,
            dims,
            &SampleLayout {
                spp,
                bits_per_sample: 8,
                stored_row_bytes: row_bytes,
                photometric,
            },
            &[],
            out,
        )
    }

    /// Serialises 16-bit samples into this encoder's byte order and lays them out like
    /// [`Self::encode_8bit`].
    ///
    /// No `SampleFormat` tag is written: unsigned integer is the TIFF 6.0 default and the only
    /// format these impls emit, so writing it would be redundant.
    fn encode_16bit(
        &self,
        samples: &[u16],
        dims: Dimensions,
        spp: usize,
        photometric: PhotometricInterpretation,
        extra_fields: &[(u16, Value)],
        out: &mut Vec<u8>,
    ) -> Result<usize> {
        // As in `encode_8bit`, the caller hands us an ImageRef-validated buffer.
        let row_bytes = dims.width as usize * spp * 2;
        debug_assert_eq!(samples.len() * 2, row_bytes * dims.height as usize);
        // The packed buffer must exist in *file* order, and must be owned so the predictor can
        // difference it in place — one allocation is the format's unavoidable serialisation cost.
        let order = self.order;
        let mut packed = Vec::with_capacity(samples.len() * 2);
        for &sample in samples {
            packed.extend_from_slice(&order.pack_u16(sample));
        }
        self.encode_packed(
            &packed,
            dims,
            &SampleLayout {
                spp,
                bits_per_sample: 16,
                stored_row_bytes: row_bytes,
                photometric,
            },
            extra_fields,
            out,
        )
    }

    /// Lays out an image from already-packed sample bytes (`height * stored_row_bytes`), applying
    /// the strip codec and building the directory.
    fn encode_packed(
        &self,
        packed: &[u8],
        dims: Dimensions,
        layout: &SampleLayout,
        extra_fields: &[(u16, Value)],
        out: &mut Vec<u8>,
    ) -> Result<usize> {
        if let Some((tw, tl)) = self.tiling {
            return self.encode_tiled(packed, dims, layout, extra_fields, tw, tl, out);
        }
        let (mut ifd, strips) = self.build_strip_image(packed, dims, layout, extra_fields)?;
        self.metadata.apply(&mut ifd);
        let bytes = writer::write_image(self.order, self.variant(), &ifd, &strips)?;
        out.extend_from_slice(&bytes);
        Ok(bytes.len())
    }

    /// Builds one strip image's directory (without `StripOffsets`/`StripByteCounts`) and its
    /// compressed strips, applying the predictor and strip codec.
    fn build_strip_image(
        &self,
        packed: &[u8],
        dims: Dimensions,
        layout: &SampleLayout,
        extra_fields: &[(u16, Value)],
    ) -> Result<(Ifd, Vec<Vec<u8>>)> {
        let h = dims.height as usize;
        let stored_row_bytes = layout.stored_row_bytes;

        // Apply the horizontal-differencing predictor before compression. §14 differences sample
        // values, so 16-bit samples are differenced as `u16` lanes in the file's byte order — the
        // order they are already packed in here.
        let predicting = self.predictor == Predictor::HorizontalDifferencing;
        if predicting && !matches!(layout.bits_per_sample, 8 | 16) {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "TIFF: predictor requires 8- or 16-bit samples",
            ));
        }
        let predicted = predicting.then(|| {
            let mut buf = packed.to_vec();
            if layout.bits_per_sample == 16 {
                predictor::forward16(&mut buf, stored_row_bytes, layout.spp, self.order);
            } else {
                predictor::forward(&mut buf, stored_row_bytes, layout.spp);
            }
            buf
        });
        let packed: &[u8] = predicted.as_deref().unwrap_or(packed);

        // Partition rows into strips of roughly 8 KB (TIFF 6.0 §7), then apply the strip codec.
        let rows_per_strip = (8192 / stored_row_bytes.max(1)).clamp(1, h);
        let mut strips: Vec<Vec<u8>> = Vec::new();
        let mut row = 0;
        while row < h {
            let rows = rows_per_strip.min(h - row);
            let start = row * stored_row_bytes;
            let raw = &packed[start..start + rows * stored_row_bytes];
            strips.push(self.compress_strip(raw, dims, layout)?);
            row += rows;
        }

        let mut ifd = Ifd::new();
        ifd.set(tags::IMAGE_WIDTH, dim_value(dims.width));
        ifd.set(tags::IMAGE_LENGTH, dim_value(dims.height));
        ifd.set(
            tags::BITS_PER_SAMPLE,
            Value::Short(vec![layout.bits_per_sample; layout.spp]),
        );
        ifd.set(
            tags::COMPRESSION,
            Value::Short(vec![u16::from(self.compression)]),
        );
        ifd.set(
            tags::PHOTOMETRIC_INTERPRETATION,
            Value::Short(vec![u16::from(layout.photometric)]),
        );
        ifd.set(
            tags::SAMPLES_PER_PIXEL,
            Value::Short(vec![layout.spp as u16]),
        );
        ifd.set(tags::ROWS_PER_STRIP, dim_value(rows_per_strip as u32));
        ifd.set(tags::X_RESOLUTION, Value::Rational(vec![(72, 1)]));
        ifd.set(tags::Y_RESOLUTION, Value::Rational(vec![(72, 1)]));
        ifd.set(tags::RESOLUTION_UNIT, Value::Short(vec![2])); // inch
        if predicting {
            ifd.set(
                tags::PREDICTOR,
                Value::Short(vec![u16::from(self.predictor)]),
            );
        }
        for (tag, value) in extra_fields {
            ifd.set(*tag, value.clone());
        }
        Ok((ifd, strips))
    }

    /// Encodes several 8-bit [`Rgb8`] images as the pages of one multi-page TIFF.
    ///
    /// Each page is a validated [`ImageRef`]. Returns the number of bytes written.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if `pages` is empty.
    pub fn encode_pages_rgb8(
        &self,
        pages: &[ImageRef<'_, Rgb8>],
        out: &mut Vec<u8>,
    ) -> Result<usize> {
        if pages.is_empty() {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "TIFF: no pages to encode",
            ));
        }
        let total = pages.len() as u16;
        let mut images: Vec<(Ifd, Vec<Vec<u8>>)> = Vec::with_capacity(pages.len());
        for (i, page) in pages.iter().enumerate() {
            let row_bytes = page.width() as usize * 3;
            let extra = [
                (tags::NEW_SUBFILE_TYPE, Value::Long(vec![2])), // bit 1: page of a multi-page image
                (tags::PAGE_NUMBER, Value::Short(vec![i as u16, total])),
            ];
            images.push(self.build_strip_image(
                page.as_samples(),
                page.dimensions(),
                &SampleLayout {
                    spp: 3,
                    bits_per_sample: 8,
                    stored_row_bytes: row_bytes,
                    photometric: PhotometricInterpretation::Rgb,
                },
                &extra,
            )?);
        }
        // The blocks describe the document, not one of its pages, so they go in IFD 0 alone.
        if let Some((ifd0, _)) = images.first_mut() {
            self.metadata.apply(ifd0);
        }
        let bytes = writer::write_multipage(self.order, self.variant(), &images)?;
        out.extend_from_slice(&bytes);
        Ok(bytes.len())
    }

    /// Applies the selected compression to one strip's already-packed bytes.
    fn compress_strip(
        &self,
        raw: &[u8],
        dims: Dimensions,
        layout: &SampleLayout,
    ) -> Result<Vec<u8>> {
        let row_bytes = layout.stored_row_bytes;
        match self.compression {
            Compression::CcittRle => {
                if layout.bits_per_sample != 1 {
                    return Err(Error::unsupported(
                        env!("CARGO_PKG_NAME"),
                        "TIFF: Modified Huffman requires a bilevel image",
                    ));
                }
                ccitt::mh_encode_strip(raw, row_bytes, dims.width as usize)
            }
            Compression::CcittGroup4Fax => {
                if layout.bits_per_sample != 1 {
                    return Err(Error::unsupported(
                        env!("CARGO_PKG_NAME"),
                        "TIFF: Group 4 fax requires a bilevel image",
                    ));
                }
                let rows = raw.len() / row_bytes;
                ccitt::g4_encode_strip(raw, row_bytes, rows, dims.width as usize)
            }
            _ => self.compress_bytes(raw, row_bytes),
        }
    }

    /// Byte-level compression of one strip/tile (the schemes that work on raw bytes).
    fn compress_bytes(&self, raw: &[u8], row_bytes: usize) -> Result<Vec<u8>> {
        match self.compression {
            Compression::None => Ok(raw.to_vec()),
            Compression::PackBits => {
                let mut out = Vec::new();
                for row in raw.chunks(row_bytes) {
                    packbits::encode_row(row, &mut out);
                }
                Ok(out)
            }
            Compression::Lzw => Ok(lzw::encode(raw)),
            Compression::Deflate => Ok(deflate::encode(raw)),
            _ => Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "TIFF: unsupported compression for encoding",
            )),
        }
    }

    /// Lays out an 8-bit image as a grid of `tile_w × tile_h` tiles (edge tiles zero-padded).
    #[allow(clippy::too_many_arguments)]
    fn encode_tiled(
        &self,
        packed: &[u8],
        dims: Dimensions,
        layout: &SampleLayout,
        extra_fields: &[(u16, Value)],
        tile_w: u32,
        tile_h: u32,
        out: &mut Vec<u8>,
    ) -> Result<usize> {
        if !matches!(layout.bits_per_sample, 8 | 16) {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "TIFF: tiling requires 8- or 16-bit images",
            ));
        }
        let predicting = self.predictor == Predictor::HorizontalDifferencing;
        if predicting && self.compression != Compression::Deflate {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "TIFF: tiled predictor is supported only with Deflate",
            ));
        }
        let (tw, th) = (tile_w as usize, tile_h as usize);
        if tw == 0 || th == 0 || tw % 16 != 0 || th % 16 != 0 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "TIFF: tile dimensions must be positive multiples of 16",
            ));
        }
        let (w, h, spp) = (dims.width as usize, dims.height as usize, layout.spp);
        let stored_row_bytes = layout.stored_row_bytes;
        // Every offset below is a byte offset, so the per-pixel stride carries the sample width too.
        let pixel_bytes = spp * (layout.bits_per_sample as usize / 8);
        let tile_row_bytes = tw * pixel_bytes;
        let tiles_across = w.div_ceil(tw);
        let tiles_down = h.div_ceil(th);

        let mut tiles: Vec<Vec<u8>> = Vec::with_capacity(tiles_across * tiles_down);
        for ty in 0..tiles_down {
            for tx in 0..tiles_across {
                let mut tile = vec![0u8; th * tile_row_bytes];
                for r in 0..th {
                    let src_row = ty * th + r;
                    if src_row >= h {
                        break;
                    }
                    let copy_cols = tw.min(w - tx * tw);
                    let src = (src_row * stored_row_bytes) + (tx * tw) * pixel_bytes;
                    let dst = r * tile_row_bytes;
                    tile[dst..dst + copy_cols * pixel_bytes]
                        .copy_from_slice(&packed[src..src + copy_cols * pixel_bytes]);
                }
                if predicting {
                    if layout.bits_per_sample == 16 {
                        predictor::forward16(&mut tile, tile_row_bytes, spp, self.order);
                    } else {
                        predictor::forward(&mut tile, tile_row_bytes, spp);
                    }
                }
                tiles.push(self.compress_bytes(&tile, tile_row_bytes)?);
            }
        }

        let mut ifd = Ifd::new();
        ifd.set(tags::IMAGE_WIDTH, dim_value(dims.width));
        ifd.set(tags::IMAGE_LENGTH, dim_value(dims.height));
        ifd.set(
            tags::BITS_PER_SAMPLE,
            Value::Short(vec![layout.bits_per_sample; spp]),
        );
        ifd.set(
            tags::COMPRESSION,
            Value::Short(vec![u16::from(self.compression)]),
        );
        ifd.set(
            tags::PHOTOMETRIC_INTERPRETATION,
            Value::Short(vec![u16::from(layout.photometric)]),
        );
        ifd.set(tags::SAMPLES_PER_PIXEL, Value::Short(vec![spp as u16]));
        ifd.set(tags::TILE_WIDTH, dim_value(tile_w));
        ifd.set(tags::TILE_LENGTH, dim_value(tile_h));
        ifd.set(tags::X_RESOLUTION, Value::Rational(vec![(72, 1)]));
        ifd.set(tags::Y_RESOLUTION, Value::Rational(vec![(72, 1)]));
        ifd.set(tags::RESOLUTION_UNIT, Value::Short(vec![2])); // inch
        if predicting {
            ifd.set(
                tags::PREDICTOR,
                Value::Short(vec![u16::from(self.predictor)]),
            );
        }
        for (tag, value) in extra_fields {
            ifd.set(*tag, value.clone());
        }
        self.metadata.apply(&mut ifd);

        let bytes = writer::write_image_tiled(self.order, self.variant(), &ifd, &tiles)?;
        out.extend_from_slice(&bytes);
        Ok(bytes.len())
    }
}

impl EncodeImage<Gray8> for TiffEncoder {
    fn encode_image(&self, image: ImageRef<'_, Gray8>, out: &mut Vec<u8>) -> Result<usize> {
        self.encode_8bit(
            image.as_samples(),
            image.dimensions(),
            1,
            PhotometricInterpretation::BlackIsZero,
            out,
        )
    }
}

impl EncodeImage<Rgb8> for TiffEncoder {
    fn encode_image(&self, image: ImageRef<'_, Rgb8>, out: &mut Vec<u8>) -> Result<usize> {
        self.encode_8bit(
            image.as_samples(),
            image.dimensions(),
            3,
            PhotometricInterpretation::Rgb,
            out,
        )
    }
}

impl EncodeImage<Cmyk8> for TiffEncoder {
    /// `PhotometricInterpretation = Separated` (5); each sample is ink coverage (0 = 0 %, 255 = 100 %).
    fn encode_image(&self, image: ImageRef<'_, Cmyk8>, out: &mut Vec<u8>) -> Result<usize> {
        self.encode_8bit(
            image.as_samples(),
            image.dimensions(),
            4,
            PhotometricInterpretation::Cmyk,
            out,
        )
    }
}

impl EncodeImage<Rgba8> for TiffEncoder {
    /// Stores the fourth sample as *unassociated* alpha (`ExtraSamples = 2`, not premultiplied).
    fn encode_image(&self, image: ImageRef<'_, Rgba8>, out: &mut Vec<u8>) -> Result<usize> {
        let row_bytes = image.width() as usize * 4;
        self.encode_packed(
            image.as_samples(),
            image.dimensions(),
            &SampleLayout {
                spp: 4,
                bits_per_sample: 8,
                stored_row_bytes: row_bytes,
                photometric: PhotometricInterpretation::Rgb,
            },
            &[(tags::EXTRA_SAMPLES, Value::Short(vec![2]))],
            out,
        )
    }
}

impl EncodeImage<Gray16> for TiffEncoder {
    /// `PhotometricInterpretation = BlackIsZero` (1), one 16-bit sample per pixel written in the
    /// encoder's byte order.
    fn encode_image(&self, image: ImageRef<'_, Gray16>, out: &mut Vec<u8>) -> Result<usize> {
        self.encode_16bit(
            image.as_samples(),
            image.dimensions(),
            1,
            PhotometricInterpretation::BlackIsZero,
            &[],
            out,
        )
    }
}

impl EncodeImage<Rgb16> for TiffEncoder {
    /// `PhotometricInterpretation = RGB` (2), three 16-bit samples per pixel written in the
    /// encoder's byte order.
    fn encode_image(&self, image: ImageRef<'_, Rgb16>, out: &mut Vec<u8>) -> Result<usize> {
        self.encode_16bit(
            image.as_samples(),
            image.dimensions(),
            3,
            PhotometricInterpretation::Rgb,
            &[],
            out,
        )
    }
}

impl EncodeImage<Rgba16> for TiffEncoder {
    /// Stores the fourth sample as *unassociated* alpha (`ExtraSamples = 2`, not premultiplied),
    /// matching the 8-bit [`Rgba8`] impl.
    fn encode_image(&self, image: ImageRef<'_, Rgba16>, out: &mut Vec<u8>) -> Result<usize> {
        self.encode_16bit(
            image.as_samples(),
            image.dimensions(),
            4,
            PhotometricInterpretation::Rgb,
            &[(tags::EXTRA_SAMPLES, Value::Short(vec![2]))],
            out,
        )
    }
}

impl EncodeImage<Bilevel> for TiffEncoder {
    /// Packs one byte per pixel (`0` = black, non-zero = white) MSB-first into bits, `BlackIsZero`.
    fn encode_image(&self, image: ImageRef<'_, Bilevel>, out: &mut Vec<u8>) -> Result<usize> {
        let (w, h) = (image.width() as usize, image.height() as usize);
        let pixels = image.as_samples();
        let stored_row_bytes = w.div_ceil(8);
        let mut packed = vec![0u8; stored_row_bytes * h];
        for y in 0..h {
            let row = &pixels[y * w..(y + 1) * w];
            let dst = &mut packed[y * stored_row_bytes..(y + 1) * stored_row_bytes];
            for (x, &p) in row.iter().enumerate() {
                if p != 0 {
                    dst[x / 8] |= 0x80 >> (x % 8);
                }
            }
        }
        self.encode_packed(
            &packed,
            image.dimensions(),
            &SampleLayout {
                spp: 1,
                bits_per_sample: 1,
                stored_row_bytes,
                photometric: PhotometricInterpretation::BlackIsZero,
            },
            &[],
            out,
        )
    }
}

/// Stores a dimension/count as `SHORT` when it fits, else `LONG` (both are valid per §2).
fn dim_value(n: u32) -> Value {
    if n <= u32::from(u16::MAX) {
        Value::Short(vec![n as u16])
    } else {
        Value::Long(vec![n])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dimension_is_stored_as_short_only_while_it_fits() {
        // Both types are valid per TIFF 6.0 §2, so a decoder -- ours or libtiff's -- accepts
        // either and no round-trip or differential test can see the difference. What it changes is
        // size: SHORT is 2 bytes inline, LONG is 4. The boundary is the whole claim, so it is
        // asserted at the two values that straddle it rather than at a typical dimension.
        assert!(matches!(dim_value(1), Value::Short(_)));
        assert!(matches!(dim_value(u32::from(u16::MAX)), Value::Short(_)));
        assert!(matches!(dim_value(u32::from(u16::MAX) + 1), Value::Long(_)));
    }

    #[test]
    fn image_ref_rejects_mismatched_buffer() {
        // Validation now lives at the ImageRef boundary, so a wrong-length or zero-sized buffer
        // can't even be constructed for the encoder's pixel types.
        let dims = Dimensions {
            width: 2,
            height: 2,
        };
        assert!(ImageRef::<Rgb8>::new(&[0; 11], dims).is_err());
        assert!(ImageRef::<Gray8>::new(&[0; 3], dims).is_err());
        assert!(ImageRef::<Bilevel>::new(&[0; 3], dims).is_err());
        assert!(
            ImageRef::<Rgb8>::new(
                &[],
                Dimensions {
                    width: 0,
                    height: 1
                }
            )
            .is_err()
        );
    }

    #[test]
    fn writes_a_well_formed_header() {
        let enc = TiffEncoder::new();
        let mut out = Vec::new();
        let n = enc
            .encode_image(
                ImageRef::<Rgb8>::new(
                    &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
                    Dimensions {
                        width: 2,
                        height: 2,
                    },
                )
                .unwrap(),
                &mut out,
            )
            .expect("encode");
        assert_eq!(n, out.len());
        assert_eq!(&out[0..2], b"II");
        // Classic TIFF by default: magic 42.
        assert_eq!(out[2], 42);
    }

    #[test]
    fn with_big_tiff_emits_bigtiff_header() {
        let mut out = Vec::new();
        TiffEncoder::new()
            .with_big_tiff(true)
            .encode_image(
                ImageRef::<Rgb8>::new(
                    &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
                    Dimensions {
                        width: 2,
                        height: 2,
                    },
                )
                .unwrap(),
                &mut out,
            )
            .expect("encode");
        // Magic 43, the fixed offset-size 8, and a 16-byte header (first IFD at offset >= 16).
        let (order, variant, first) = gamut_ifd::read_header(&out).expect("header");
        assert_eq!(order, ByteOrder::LittleEndian);
        assert_eq!(variant, Variant::Big);
        assert_eq!(out[2], 0x2b);
        assert!(first >= 16);
    }
}
