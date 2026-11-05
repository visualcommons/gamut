//! The encode pipeline: the backend hand-off, the prologue and baseline-frame writers, the XYB
//! colour mode, and the [`EncodeImage`] impls that colour-convert, subsample and route each image
//! to the baseline ([`super::entropy`]) or progressive ([`crate::progressive`]) coder.

#[cfg(doc)]
use gamut_color::ChromaSubsampling;
use gamut_color::transfer::srgb_eotf;
use gamut_color::{ColorRange, bt601_rgb_to_ycbcr, xyb};
use gamut_core::{EncodeImage, Error, Gray8, ImageRef, PixelFormat, Result, Rgb8};

use super::block::{Plane, downsample};
use super::entropy::{
    BaselineCoder, Component, Histograms, emit_huffman_tables, encode_scan, optimized_tables,
    standard_tables,
};
use super::{
    JpegColorMode, JpegEncoder, Process, ProgressiveScript, RdOptimization, XYB_ICC_PROFILE,
    luma_factors,
};
use crate::backend::{self, JpegEncodeRequest, RasterRef};
#[cfg(doc)]
use crate::quant::QuantTables;
use crate::{appmeta, marker, progressive, quant};

impl JpegEncoder {
    /// Offers the raster to the encode registry; on acceptance the returned stream has this
    /// encoder's metadata patched into it and is appended to `out`, and the written length is
    /// returned. `None` means no backend accepted and the caller runs the built-in encoder.
    ///
    /// `width`/`height` have already passed [`Self::check_dimensions`] and the metadata
    /// [`Self::check_metadata`], so a backend never sees a job the built-in path would reject.
    fn encode_via_backend(
        &self,
        width: u16,
        height: u16,
        format: PixelFormat,
        samples: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<Option<usize>> {
        // Caller-supplied quantization tables, RD optimization, the XYB colour mode, and a
        // non-default progressive scan script cannot ride a `JpegEncodeRequest`, so any of them
        // pins the encode to the built-in path rather than let a backend silently encode with a
        // different configuration (the same host-side veto gamut-jxl applies to its container
        // features).
        if self.quant_tables.is_some()
            || self.rd != RdOptimization::None
            || self.color_mode != JpegColorMode::Ycbcr
            || matches!(self.process, Process::Progressive(script) if script != ProgressiveScript::Libjpeg)
        {
            return Ok(None);
        }
        if self.backends.is_empty() {
            return Ok(None);
        }
        let (w, h) = (u32::from(width), u32::from(height));
        let req = JpegEncodeRequest::new(
            w,
            h,
            format,
            self.quality,
            self.subsampling,
            self.process,
            self.restart_interval,
        );
        let raster = RasterRef::new(w, h, format, samples)?;
        let Some(stream) = backend::encode_with_backends(&self.backends, &req, &raster)? else {
            return Ok(None);
        };
        let patched = appmeta::patch_stream(
            &stream,
            appmeta::AppMetadata {
                exif: self.exif.as_deref(),
                xmp: self.xmp.as_deref(),
                icc: self.icc.as_deref(),
            },
        )?;
        // Validation ownership: the produced stream must declare a frame of exactly the image the
        // caller asked to encode, so a backend cannot quietly resize or drop it.
        let info = crate::JpegStreamInfo::parse(&patched)?;
        if (info.width(), info.height()) != (w, Some(h)) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: backend stream declares different dimensions than the encoded image",
            ));
        }
        out.extend_from_slice(&patched);
        Ok(Some(patched.len()))
    }

    /// Writes the leading markers common to every stream: SOI, JFIF APP0, the configured metadata
    /// APP segments, and the DQT segment.
    ///
    /// JFIF mandates its APP0 first while Exif 3.0 §4.7.2.1 wants its APP1 immediately after SOI,
    /// and neither spec references the other; APP0 then APP1 is the libjpeg-family convention that
    /// XMP Part 3 §1.1.3 records readers must accept. EXIF, XMP, then ICC — all before the first
    /// SOF, as XMP Part 3 requires.
    fn write_prologue(&self, out: &mut Vec<u8>, quant_tables: &[(u8, &[u8; 64])]) {
        marker::write_marker(out, marker::code::SOI);
        marker::write_app0_jfif(out, self.density_unit, self.x_density, self.y_density);
        if let Some(exif) = &self.exif {
            appmeta::write_app1_exif(out, exif);
        }
        if let Some(xmp) = &self.xmp {
            appmeta::write_app1_xmp(out, xmp);
        }
        if let Some(icc) = &self.icc {
            appmeta::write_app2_icc(out, icc);
        }
        quant::emit_dqt(out, quant_tables);
    }

    /// Writes the whole baseline (SOF0) frame: the frame header, one DHT segment, an optional DRI,
    /// the scan header, and the entropy-coded scan. The caller has already written the SOI/APP0/DQT
    /// prologue and appends EOI afterward.
    ///
    /// With [`Process::Optimized`] the scan is walked **twice**: a gather pass
    /// counts the symbols each entropy destination will code, those counts drive the Annex K.2
    /// optimal-table construction, and the emit pass writes the entropy data with the resulting
    /// tables. Marker order and segment count are identical either way — only the DHT contents and
    /// the entropy bytes differ.
    fn write_baseline_frame(
        &self,
        out: &mut Vec<u8>,
        width: u16,
        height: u16,
        sof: &[(u8, u8, u8, u8)],
        sos: &[(u8, u8, u8)],
        components: &[Component],
    ) {
        let (w, h) = (u32::from(width), u32::from(height));
        marker::write_sof0(out, width, height, sof);

        let tables = if self.process == Process::Optimized {
            let mut freq = Histograms::default();
            let mut coder = BaselineCoder::gather(&mut freq);
            encode_scan(components, w, h, self.restart_interval, &mut coder);
            coder.finish();
            optimized_tables(out, &freq)
        } else {
            let color = components.len() > 1;
            emit_huffman_tables(out, color);
            standard_tables(color)
        };

        if self.restart_interval != 0 {
            marker::write_dri(out, self.restart_interval);
        }
        marker::write_sos(out, sos);

        let mut coder = BaselineCoder::emit(out, &tables);
        encode_scan(components, w, h, self.restart_interval, &mut coder);
        coder.finish();
    }

    /// Encodes `rgb` as an XYB stream ([`JpegColorMode::Xyb`]): sRGB → linear (EOTF LUT) → XYB →
    /// scaled-XYB bytes into three planes (X, Y, B−Y), the B−Y plane box-subsampled by the
    /// configured [`ChromaSubsampling`] ratio, `(hx, vx)` from [`luma_factors`] (X and Y carry that
    /// ratio as their sampling factors,
    /// B stays 1×1 — jpegli's B-only layout), coded with component
    /// ids `R`,`G`,`B` under a no-APP0 / APP14 `transform = 0` prologue that embeds
    /// [`XYB_ICC_PROFILE`]. X and Y quantize with the luminance table (destination 0), the stored
    /// B−Y with the chrominance table (destination 1) — Annex K tables are YCbCr-tuned, so this
    /// pairing is an honest placeholder, not XYB-tuned (see STATUS.md). A caller X table
    /// ([`QuantTables::with_xyb_x`]) moves X alone to destination 2. The caller writes EOI.
    fn encode_xyb(
        &self,
        rgb: &[u8],
        width: u16,
        height: u16,
        (hx, vx): (u8, u8),
        out: &mut Vec<u8>,
    ) {
        let (w, h) = (usize::from(width), usize::from(height));

        // 256-entry EOTF LUT: the only per-pixel float work left is the XYB transform itself.
        let mut eotf = [0f64; 256];
        for (i, v) in eotf.iter_mut().enumerate() {
            *v = srgb_eotf(i as f64 / 255.0);
        }

        let mut planes = [
            vec![0u8; w * h], // X
            vec![0u8; w * h], // Y
            vec![0u8; w * h], // stored B − Y
        ];
        for i in 0..w * h {
            let linear = [
                eotf[usize::from(rgb[i * 3])],
                eotf[usize::from(rgb[i * 3 + 1])],
                eotf[usize::from(rgb[i * 3 + 2])],
            ];
            let scaled = xyb::scale_xyb(xyb::linear_srgb_to_xyb(linear));
            for (plane, &s) in planes.iter_mut().zip(scaled.iter()) {
                // scale_xyb clamps to [0, 1], so the rounded value is already in 0..=255.
                plane[i] = (s * 255.0).round() as u8;
            }
        }
        let [x, y, b] = planes;
        let x_plane = Plane {
            data: x,
            width: w,
            height: h,
        };
        let y_plane = Plane {
            data: y,
            width: w,
            height: h,
        };
        // Only B is subsampled: X and Y take the ratio as their sampling factors (`hx`, `vx`), B
        // stays 1×1 — the same frame shape the YCbCr path gives luma and chroma.
        let b_plane = downsample(&b, w, h, usize::from(hx), usize::from(vx));

        let luma_quant = self.luma_quant();
        let chroma_quant = self.chroma_quant();
        let luma_rd = self.rd_ctx(false);
        let chroma_rd = self.rd_ctx(true);

        // Prologue: SOI, Adobe APP14 transform = 0 (no JFIF APP0 — T.871 defines the 3-component
        // JFIF stream as YCbCr), EXIF/XMP if configured, the XYB ICC profile, DQT.
        marker::write_marker(out, marker::code::SOI);
        marker::write_app14_adobe(out, 0);
        if let Some(exif) = &self.exif {
            appmeta::write_app1_exif(out, exif);
        }
        if let Some(xmp) = &self.xmp {
            appmeta::write_app1_xmp(out, xmp);
        }
        appmeta::write_app2_icc(out, XYB_ICC_PROFILE);
        // X quantizes with its own table at destination 2 when the caller supplied one; otherwise
        // it shares Y's luminance table at destination 0 (the default two-table stream).
        let x_table = self.xyb_x_quant();
        let x_tq = match &x_table {
            Some(x) => {
                quant::emit_dqt(out, &[(0, &luma_quant), (1, &chroma_quant), (2, x)]);
                2
            }
            None => {
                quant::emit_dqt(out, &[(0, &luma_quant), (1, &chroma_quant)]);
                0
            }
        };
        let x_quant = x_table.as_ref().unwrap_or(&luma_quant);

        // Component ids are the bytes 'R','G','B' (the jpegli convention for XYB streams),
        // belt-and-braces beside the APP14: either signal alone keeps a decoder from applying a
        // YCbCr inverse.
        let ids: [u8; 3] = *b"RGB";
        if let Process::Progressive(script) = self.process {
            let comps = [
                progressive::ProgComponent {
                    id: ids[0],
                    h: hx,
                    v: vx,
                    tq: x_tq,
                    plane: &x_plane,
                    quant: x_quant,
                    rd: luma_rd.as_ref(),
                },
                progressive::ProgComponent {
                    id: ids[1],
                    h: hx,
                    v: vx,
                    tq: 0,
                    plane: &y_plane,
                    quant: &luma_quant,
                    rd: luma_rd.as_ref(),
                },
                progressive::ProgComponent {
                    id: ids[2],
                    h: 1,
                    v: 1,
                    tq: 1,
                    plane: &b_plane,
                    quant: &chroma_quant,
                    rd: chroma_rd.as_ref(),
                },
            ];
            // XYB is not YCbCr, so mozjpeg's script choice takes its all-purpose branch.
            progressive::encode(
                out,
                width,
                height,
                &comps,
                self.restart_interval,
                script,
                false,
            );
        } else {
            let components = [
                Component {
                    h: hx,
                    v: vx,
                    plane: &x_plane,
                    quant: x_quant,
                    dest: 0,
                    rd: luma_rd.as_ref(),
                },
                Component {
                    h: hx,
                    v: vx,
                    plane: &y_plane,
                    quant: &luma_quant,
                    dest: 0,
                    rd: luma_rd.as_ref(),
                },
                Component {
                    h: 1,
                    v: 1,
                    plane: &b_plane,
                    quant: &chroma_quant,
                    dest: 1,
                    rd: chroma_rd.as_ref(),
                },
            ];
            self.write_baseline_frame(
                out,
                width,
                height,
                &[
                    (ids[0], hx, vx, x_tq),
                    (ids[1], hx, vx, 0),
                    (ids[2], 1, 1, 1),
                ],
                &[(ids[0], 0, 0), (ids[1], 0, 0), (ids[2], 1, 1)],
                &components,
            );
        }
    }
}
impl EncodeImage<Gray8> for JpegEncoder {
    /// Encodes a grayscale image as a single-component (Y) baseline JPEG. Subsampling does not apply
    /// to a one-component image; a JFIF APP0 segment is still written.
    fn encode_image(&self, image: ImageRef<'_, Gray8>, out: &mut Vec<u8>) -> Result<usize> {
        if self.color_mode == JpegColorMode::Xyb {
            // A single-channel image has no XYB representation; silently encoding plain
            // grayscale under an "XYB" setting would misdescribe the output.
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "JPEG: XYB colour mode requires RGB input",
            ));
        }
        let (width, height) = Self::check_dimensions(image.dimensions())?;
        self.check_metadata()?;
        if let Some(written) =
            self.encode_via_backend(width, height, PixelFormat::Gray8, image.as_samples(), out)?
        {
            return Ok(written);
        }
        let start = out.len();

        let plane = Plane {
            data: image.as_samples().to_vec(),
            width: usize::from(width),
            height: usize::from(height),
        };
        let luma_quant = self.luma_quant();
        let luma_rd = self.rd_ctx(false);

        self.write_prologue(out, &[(0, &luma_quant)]);
        if let Process::Progressive(script) = self.process {
            let comps = [progressive::ProgComponent {
                id: 1,
                h: 1,
                v: 1,
                tq: 0,
                plane: &plane,
                quant: &luma_quant,
                rd: luma_rd.as_ref(),
            }];
            progressive::encode(
                out,
                width,
                height,
                &comps,
                self.restart_interval,
                script,
                false,
            );
        } else {
            let comp = Component {
                h: 1,
                v: 1,
                plane: &plane,
                quant: &luma_quant,
                dest: 0,
                rd: luma_rd.as_ref(),
            };
            self.write_baseline_frame(out, width, height, &[(1, 1, 1, 0)], &[(1, 0, 0)], &[comp]);
        }

        marker::write_marker(out, marker::code::EOI);
        Ok(out.len() - start)
    }
}

impl EncodeImage<Rgb8> for JpegEncoder {
    /// Encodes an RGB image as a three-component YCbCr baseline JPEG: RGB is converted to full-range
    /// (JFIF) BT.601 YCbCr per T.871 §7, and the chroma planes are subsampled per the configured
    /// [`ChromaSubsampling`].
    fn encode_image(&self, image: ImageRef<'_, Rgb8>, out: &mut Vec<u8>) -> Result<usize> {
        let (width, height) = Self::check_dimensions(image.dimensions())?;
        self.check_metadata()?;
        // Checked before any backend runs, so a backend is never offered a layout the frame
        // cannot carry.
        let (yh, yv) = luma_factors(self.subsampling)?;
        if let Some(written) =
            self.encode_via_backend(width, height, PixelFormat::Rgb8, image.as_samples(), out)?
        {
            return Ok(written);
        }
        let start = out.len();
        let (w, h) = (usize::from(width), usize::from(height));

        if self.color_mode == JpegColorMode::Xyb {
            self.encode_xyb(image.as_samples(), width, height, (yh, yv), out);
            marker::write_marker(out, marker::code::EOI);
            return Ok(out.len() - start);
        }

        // RGB → full-resolution Y/Cb/Cr planes (T.871 §7 full-range BT.601, fixed-point).
        let rgb = image.as_samples();
        let mut y = vec![0u8; w * h];
        let mut cb = vec![0u8; w * h];
        let mut cr = vec![0u8; w * h];
        for i in 0..w * h {
            let (yy, u, v) =
                bt601_rgb_to_ycbcr(rgb[i * 3], rgb[i * 3 + 1], rgb[i * 3 + 2], ColorRange::Full);
            y[i] = yy;
            cb[i] = u;
            cr[i] = v;
        }
        let (sx, sy) = (usize::from(yh), usize::from(yv));
        let luma_plane = Plane {
            data: y,
            width: w,
            height: h,
        };
        let cb_plane = downsample(&cb, w, h, sx, sy);
        let cr_plane = downsample(&cr, w, h, sx, sy);

        let luma_quant = self.luma_quant();
        let chroma_quant = self.chroma_quant();
        let luma_rd = self.rd_ctx(false);
        let chroma_rd = self.rd_ctx(true);

        self.write_prologue(out, &[(0, &luma_quant), (1, &chroma_quant)]);
        if let Process::Progressive(script) = self.process {
            let comps = [
                progressive::ProgComponent {
                    id: 1,
                    h: yh,
                    v: yv,
                    tq: 0,
                    plane: &luma_plane,
                    quant: &luma_quant,
                    rd: luma_rd.as_ref(),
                },
                progressive::ProgComponent {
                    id: 2,
                    h: 1,
                    v: 1,
                    tq: 1,
                    plane: &cb_plane,
                    quant: &chroma_quant,
                    rd: chroma_rd.as_ref(),
                },
                progressive::ProgComponent {
                    id: 3,
                    h: 1,
                    v: 1,
                    tq: 1,
                    plane: &cr_plane,
                    quant: &chroma_quant,
                    rd: chroma_rd.as_ref(),
                },
            ];
            progressive::encode(
                out,
                width,
                height,
                &comps,
                self.restart_interval,
                script,
                true,
            );
        } else {
            let components = [
                Component {
                    h: yh,
                    v: yv,
                    plane: &luma_plane,
                    quant: &luma_quant,
                    dest: 0,
                    rd: luma_rd.as_ref(),
                },
                Component {
                    h: 1,
                    v: 1,
                    plane: &cb_plane,
                    quant: &chroma_quant,
                    dest: 1,
                    rd: chroma_rd.as_ref(),
                },
                Component {
                    h: 1,
                    v: 1,
                    plane: &cr_plane,
                    quant: &chroma_quant,
                    dest: 1,
                    rd: chroma_rd.as_ref(),
                },
            ];
            self.write_baseline_frame(
                out,
                width,
                height,
                &[(1, yh, yv, 0), (2, 1, 1, 1), (3, 1, 1, 1)],
                &[(1, 0, 0), (2, 1, 1), (3, 1, 1)],
                &components,
            );
        }

        marker::write_marker(out, marker::code::EOI);
        Ok(out.len() - start)
    }
}
