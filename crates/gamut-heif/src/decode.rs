//! The decode surfaces: the derivation walk that turns an item into planar samples, and the
//! presentation pipeline that turns those into interleaved RGBA.
//!
//! # Three surfaces
//!
//! - [`HeifImage::decode_item_planar`] — the **raw** surface. It resolves an item through its
//!   derivation (coded → backend; `iden` → source; `grid` → tile assembly in the plane domain)
//!   and returns a [`DecodedFrame`] untouched by colour or transforms, at full bit depth and
//!   native chroma. `iovl` compositing is *not* on this surface (it is defined over RGBA).
//! - [`HeifImage::decode_item_rgba8`] — the **presentation** surface: planar decode (or `iovl`
//!   compositing), colour conversion, alpha merge, then the transformative properties in `ipma`
//!   order. It presents **8-bit coded frames only**.
//! - [`HeifImage::decode_item_rgba16`] — the same pipeline over 16-bit samples, accepting every
//!   coded depth a [`DecodedFrame`] admits.
//!
//! Each is generic over the [`Codestream`] binding whose coded items it decodes, and takes that
//! binding's backend [`Registry`].

use gamut_codec_abi::Registry;
use gamut_color::{ChromaSubsampling, ColorRange};
use gamut_core::{Cicp, Dimensions, Error, ImageBuf, Result, Rgba8, Rgba16};
use gamut_isobmff::ColourInformation;

use crate::codestream::{self, Codestream};
use crate::convert::{
    Rgba, RgbaSample, apply_alpha, apply_transforms, composite_over, frame_to_rgba,
};
use crate::frame::DecodedFrame;
use crate::image::{HeifImage, HeifItem, ItemKind};
use crate::limits::{
    DECODED_ITEM_LIMIT, DERIVED_ITEM_LIMIT, DecodeLimits, DecodeOptions, charge, try_filled,
};

/// The maximum derivation-reference nesting depth. A derived item (`grid`/`iden`/`iovl`) may
/// reference other derived items; this bounds the recursion so a deep (or, together with the
/// per-branch cycle check, a self-referential) derivation graph errors instead of overflowing the
/// stack.
pub(crate) const MAX_DERIVATION_DEPTH: usize = 8;

impl HeifImage {
    /// Decodes an item to a raw planar [`DecodedFrame`] — the surface that delivers everything,
    /// with no colour conversion or transformative properties applied (drive those from the
    /// item's [`HeifItem`] accessors).
    ///
    /// The item is resolved through its derivation, recursively, depth-limited (a self- or
    /// mutually-referential `dimg` graph errors rather than recursing forever) and work-limited
    /// (a repeated reference is evaluated again and counted again against `limits`):
    ///
    /// - **Coded** (an item [`C::is_coded_item`](Codestream::is_coded_item) claims): its
    ///   configuration is parsed, the payload checked by
    ///   [`C::validate_still_payload`](Codestream::validate_still_payload), the declared `ispe`
    ///   checked against `limits`, and the payload decoded by the first backend in `backends`
    ///   that [`supports`](Codestream::supports) it.
    /// - **`iden`**: the frame of its single `dimg` source.
    /// - **`grid`**: every tile is decoded (row-major `dimg` order) and assembled in the plane
    ///   domain onto a `columns·tile_w × rows·tile_h` canvas, then cropped to the grid's output
    ///   size (ISO/IEC 23008-12 §6.6.2.3.2). All tiles must share chroma layout, bit depth and
    ///   dimensions.
    /// - **`iovl`**: refused — overlay compositing is defined over RGBA.
    ///
    /// # Errors
    ///
    /// Under `C`'s [`ORIGIN`](Codestream::ORIGIN): an
    /// [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error for a missing item,
    /// a coded item with no configuration, a derivation cycle, or a malformed derivation; an
    /// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for an `iovl`, a coded
    /// item of another codec, an item with an unrecognised essential property, a non-uniform
    /// grid, a derivation nested past the depth limit, an image or a count past `limits`, an
    /// allocation the system refuses, or no backend accepting the item
    /// ([`NO_BACKEND`](crate::NO_BACKEND)). The binding's and the backend's own errors propagate
    /// unchanged.
    pub fn decode_item_planar<C: Codestream>(
        &self,
        id: u32,
        backends: &Registry<C::Backend>,
        limits: DecodeLimits,
    ) -> Result<DecodedFrame> {
        let options = DecodeOptions::default().with_limits(limits);
        Walk::<C>::new(self, backends, options).planar(id)
    }

    /// Decodes an item to a presentation-ready [`ImageBuf<Rgba8>`]: planar decode (or `iovl`
    /// compositing), colour conversion, alpha-auxiliary merge, then the item's transformative
    /// properties applied in `ipma` order.
    ///
    /// **Colour policy.** Shared with [`decode_item_rgba16`](Self::decode_item_rgba16); this
    /// surface additionally presents **8-bit coded frames only**. The matrix and range come from
    /// the item's [`presentation_colour`](HeifItem::presentation_colour) — its `nclx` `colr`
    /// wherever it sits beside an ICC one — or, with no `colr` at all, from
    /// [`C::signalled_colour`](Codestream::signalled_colour) of the coded item whose samples it
    /// presents (for a `grid` its first tile, for an `iden` its source). An ICC-only `colr` is
    /// refused here; the planar surface still delivers the samples.
    ///
    /// - **Matrix 0 (identity / GBR)** — requires 4:4:4; samples are mapped directly
    ///   (`R=Cr, G=Y, B=Cb`) with no range expansion.
    /// - **Matrix 1 (BT.709), 2 (unspecified, presented as BT.601), 5/6 (BT.601, BT.470 B,G),
    ///   9 (BT.2020 non-constant luminance)** — with the `full_range` flag selecting
    ///   [`ColorRange`].
    /// - **Monochrome** — luma replicated to gray, range-expanded for limited range.
    /// - Anything else — YCgCo, the chromaticity-derived matrices, or a coded depth CICP does not
    ///   model — is refused.
    ///
    /// **Which conversion runs.** Interpolated 4:2:0 or 4:2:2 chroma goes through the
    /// H.273-derived [`YcbcrMatrix`](gamut_color::YcbcrMatrix), with the interpolated chroma kept
    /// to sixteenths of a code value. Otherwise BT.601 (or unspecified) at 8 bits uses
    /// [`gamut_color::bt601_ycbcr_to_rgb`], the libwebp-exact integer inverse, and every other
    /// (matrix, depth) pair uses [`YcbcrMatrix`](gamut_color::YcbcrMatrix).
    ///
    /// **Chroma upsampling** follows `options.chroma_upsampling`:
    /// [`Interpolated`](gamut_color::ChromaUpsampling::Interpolated) is libavif's bilinear filter
    /// ([`gamut_dsp::upsample::libavif`]) on 4:2:0 and 4:2:2;
    /// [`Nearest`](gamut_color::ChromaUpsampling::Nearest) replicates each chroma sample.
    ///
    /// **Alpha.** The item's alpha auxiliary (if any) is decoded through the same planar path,
    /// must match the master's **pre-transform** dimensions, and its luma plane is rescaled to
    /// the surface; [`C::ALPHA`](Codestream::ALPHA) says whether a coloured auxiliary is
    /// accepted. An absent alpha is opaque. Premultiplication is **not** undone — query
    /// [`is_premultiplied`](Self::is_premultiplied).
    ///
    /// **Transforms** are applied in `ipma` order even when it violates the MIAF constraint.
    /// `clap` crops per ISO/IEC 14496-12 §12.1.4, `irot` rotates anti-clockwise, and `imir`
    /// mirrors per ISO/IEC 23008-12:2022 §6.5.12.
    ///
    /// # Errors
    ///
    /// As [`decode_item_planar`](Self::decode_item_planar), plus an
    /// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for an unsupported
    /// colour configuration or a frame deeper than 8 bits, and an
    /// [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error for a mismatched
    /// alpha auxiliary or an out-of-range or non-integer `clap`.
    pub fn decode_item_rgba8<C: Codestream>(
        &self,
        id: u32,
        backends: &Registry<C::Backend>,
        options: DecodeOptions,
    ) -> Result<ImageBuf<Rgba8>> {
        let (rgba, dims) = Walk::<C>::new(self, backends, options).rgba::<u8>(id)?;
        ImageBuf::<Rgba8>::new(rgba, dims)
    }

    /// Decodes an item to a presentation-ready **high-bit-depth** [`ImageBuf<Rgba16>`]: the
    /// pipeline of [`decode_item_rgba8`](Self::decode_item_rgba8) over 16-bit samples.
    ///
    /// **Sample scale.** Samples are normalized to the **full 16-bit range** whatever the coded
    /// depth: a 10-bit sample `s` becomes `(s·65535 + 511)/1023`, a 12-bit sample
    /// `(s·65535 + 2047)/4095`, and an 8-bit sample exactly `s·257`. Read the coded depth from the
    /// planar surface's [`DecodedFrame::bit_depth`].
    ///
    /// **Coded depth.** Any depth [`DecodedFrame`] admits (`8..=16`) is accepted. The matrixed
    /// paths additionally require a CICP-modeled depth (8, 10, 12 or 16); monochrome and identity
    /// work at every depth. Beyond 8 bits every matrix goes through
    /// [`YcbcrMatrix`](gamut_color::YcbcrMatrix).
    ///
    /// # Errors
    ///
    /// As [`decode_item_rgba8`](Self::decode_item_rgba8), except that a frame deeper than 8 bits
    /// is not refused.
    pub fn decode_item_rgba16<C: Codestream>(
        &self,
        id: u32,
        backends: &Registry<C::Backend>,
        options: DecodeOptions,
    ) -> Result<ImageBuf<Rgba16>> {
        let (rgba, dims) = Walk::<C>::new(self, backends, options).rgba::<u16>(id)?;
        ImageBuf::<Rgba16>::new(rgba, dims)
    }
}

/// One top-level decode's walk through the derivation graph: the image and backends it reads,
/// the current branch (for the cycle and depth guard), and the work spent so far against the
/// [`DecodeLimits`] work caps. One is created per public call and threaded through every
/// recursive step, so the alpha auxiliary and every repeated reference draw on the same budget.
struct Walk<'a, C: Codestream> {
    image: &'a HeifImage,
    backends: &'a Registry<C::Backend>,
    options: DecodeOptions,
    /// The items being evaluated on the current branch, outermost first.
    stack: Vec<u32>,
    /// Backend decodes made so far.
    decoded: u32,
    /// Derived-item evaluations started so far.
    derived: u32,
}

impl<'a, C: Codestream> Walk<'a, C> {
    fn new(
        image: &'a HeifImage,
        backends: &'a Registry<C::Backend>,
        options: DecodeOptions,
    ) -> Self {
        Self {
            image,
            backends,
            options,
            stack: Vec::new(),
            decoded: 0,
            derived: 0,
        }
    }

    fn limits(&self) -> DecodeLimits {
        self.options.limits
    }

    fn invalid(message: &'static str) -> Error {
        Error::invalid_input(C::ORIGIN, message)
    }

    fn unsupported(message: &'static str) -> Error {
        Error::unsupported(C::ORIGIN, message)
    }

    /// The item `id`, refusing a missing one and one carrying an essential property this reader
    /// does not recognise (MIAF §7.3.6).
    fn renderable(&self, id: u32) -> Result<HeifItem<'a>> {
        let item = self
            .image
            .item(id)
            .ok_or_else(|| Self::invalid("HEIF: item id names no item"))?;
        if item.has_unsupported_essential_property() {
            return Err(Self::unsupported(
                "HEIF: item has an unrecognised essential property",
            ));
        }
        Ok(item)
    }

    /// Runs `step` with `id` on the branch: cycle- and depth-checked on the way in, popped on the
    /// way out whatever `step` returns.
    fn nested<R>(&mut self, id: u32, step: impl FnOnce(&mut Self) -> Result<R>) -> Result<R> {
        if self.stack.contains(&id) {
            return Err(Self::invalid("HEIF: derivation reference cycle"));
        }
        if self.stack.len() >= MAX_DERIVATION_DEPTH {
            return Err(Self::unsupported("HEIF: derivation nested too deeply"));
        }
        self.stack.push(id);
        let out = step(self);
        self.stack.pop();
        out
    }

    /// Counts one derived-item evaluation, refusing it past the cap.
    fn charge_derivation(&mut self) -> Result<()> {
        self.derived = charge(
            self.derived,
            self.limits().max_derived_items,
            DERIVED_ITEM_LIMIT,
            C::ORIGIN,
        )?;
        Ok(())
    }

    // ---- planar pipeline ---------------------------------------------------------------------

    fn planar(&mut self, id: u32) -> Result<DecodedFrame> {
        let item = self.renderable(id)?;
        self.nested(id, |walk| walk.planar_kind(id, item))
    }

    fn planar_kind(&mut self, id: u32, item: HeifItem<'a>) -> Result<DecodedFrame> {
        match item.kind() {
            ItemKind::CodedImage { codec } if C::is_coded_item(&codec) => self.coded(item),
            ItemKind::CodedImage { .. } => Err(Self::unsupported(
                "HEIF: coded item is not of this binding's codec",
            )),
            ItemKind::Identity => {
                let [source] = item.derivation_target_ids() else {
                    return Err(Self::invalid(
                        "HEIF: iden item must reference exactly one source",
                    ));
                };
                self.charge_derivation()?;
                self.planar(*source)
            }
            ItemKind::Grid => {
                self.charge_derivation()?;
                self.grid(id, item)
            }
            ItemKind::Overlay => Err(Self::unsupported(
                "HEIF: overlay compositing is not available on the planar surface (use the RGBA \
                 path)",
            )),
            _ => Err(Self::invalid("HEIF: item is not a decodable image")),
        }
    }

    /// The configuration and payload of a coded item of this codec.
    fn config(item: HeifItem<'a>) -> Result<(C::Config, &'a [u8])> {
        let (_, data) = item
            .codec_configuration()
            .filter(|(kind, _)| **kind == C::CONFIGURATION_BOX)
            .ok_or_else(|| Self::invalid("HEIF: coded item has no codec configuration"))?;
        Ok((C::parse_config(data)?, &item.as_isobmff_item().payload))
    }

    fn coded(&mut self, item: HeifItem<'a>) -> Result<DecodedFrame> {
        let (config, payload) = Self::config(item)?;
        // The pipeline enforces the still-image constraints before a backend sees the payload.
        C::validate_still_payload(&config, payload)?;
        // The declared size is refused before a backend is asked to allocate it, and the
        // delivered one before anything is derived from it: a backend need not honour `ispe` (or
        // the file need not carry one), so the second check is not redundant.
        let limits = self.limits();
        if let Some(declared) = item.dimensions() {
            limits.check(declared.width(), declared.height(), C::ORIGIN)?;
        }
        self.decoded = charge(
            self.decoded,
            limits.max_decoded_items,
            DECODED_ITEM_LIMIT,
            C::ORIGIN,
        )?;
        let frame = codestream::run::<C>(self.backends, &config, payload)?;
        limits.check(frame.width(), frame.height(), C::ORIGIN)?;
        Ok(frame)
    }

    fn grid(&mut self, id: u32, item: HeifItem<'a>) -> Result<DecodedFrame> {
        // `grid_for` validates the payload and that the tile count equals rows * columns.
        let grid = self.image.grid_for(id, C::ORIGIN)?;
        let cols = usize::from(grid.columns);
        let rows = usize::from(grid.rows);
        let output = (grid.output_width, grid.output_height);
        // The declared output is refused before a single tile is decoded.
        self.limits().check(output.0, output.1, C::ORIGIN)?;

        // A grid always has >= 1 tile (rows, columns are >= 1), and the first fixes the tile
        // size, so the canvas the tiles fill is checked before a second tile is decoded and
        // every later tile is held to the first's shape as it arrives: the tiles held at once
        // never outgrow the limits.
        let mut tiles: Vec<DecodedFrame> = Vec::with_capacity(cols * rows);
        for &tile_id in item.derivation_target_ids() {
            let tile = self.planar(tile_id)?;
            match tiles.first() {
                None => self.check_grid_canvas((cols, rows), tile.dimensions(), output)?,
                Some(first) if !same_shape(first, &tile) => {
                    return Err(Self::unsupported(
                        "HEIF: grid tiles are not uniform in dimensions, chroma, or bit depth",
                    ));
                }
                Some(_) => {}
            }
            tiles.push(tile);
        }
        assemble_grid(&tiles, cols, output, C::ORIGIN)
    }

    /// Checks a grid's tiled canvas — `columns·tile_w × rows·tile_h`, the samples its tiles hold
    /// before the crop — once the first tile has fixed the tile size: the arithmetic is checked,
    /// the declared output must be non-empty and fit within the canvas (ISO/IEC 23008-12
    /// §6.6.2.3.2), and the canvas itself must be within the limits.
    ///
    /// Holding the canvas, not only the output, to the limits is what bounds the memory the
    /// decoded tiles occupy together. It refuses a grid whose tiles overhang its output by enough
    /// to cross a limit the output itself respects; raise the limits to accept one.
    fn check_grid_canvas(
        &self,
        (cols, rows): (usize, usize),
        tile: Dimensions,
        (ow, oh): (u32, u32),
    ) -> Result<()> {
        let canvas_w = (cols as u32)
            .checked_mul(tile.width())
            .ok_or_else(|| Self::invalid("HEIF: grid canvas width overflow"))?;
        let canvas_h = (rows as u32)
            .checked_mul(tile.height())
            .ok_or_else(|| Self::invalid("HEIF: grid canvas height overflow"))?;
        if ow == 0 || oh == 0 || ow > canvas_w || oh > canvas_h {
            return Err(Self::invalid(
                "HEIF: grid output dimensions exceed the tiled canvas",
            ));
        }
        self.limits().check(canvas_w, canvas_h, C::ORIGIN)
    }

    // ---- RGBA pipeline -----------------------------------------------------------------------

    /// The shared RGBA pipeline, generic over the presentation sample width. Returns the
    /// interleaved buffer and its post-transform dimensions rather than an `ImageBuf`, so the
    /// overlay recursion can composite sub-images without allocating and revalidating one per
    /// sub-item.
    fn rgba<T: RgbaSample>(&mut self, id: u32) -> Result<(Vec<T>, Dimensions)> {
        let item = self.renderable(id)?;
        let decoded = if item.kind() == ItemKind::Overlay {
            self.charge_derivation()?;
            self.nested(id, |walk| walk.overlay::<T>(id, item))?
        } else {
            // The planar path applies its own cycle/depth guard for `id`, and its own limits: a
            // frame it returns is within them, so the RGBA buffer sized from it is too.
            let frame = self.planar(id)?;
            let colour = self.presentation_colour(item)?;
            let mut rgba =
                frame_to_rgba::<T>(&frame, colour, self.options.chroma_upsampling, C::ORIGIN)?;
            let size = (frame.width(), frame.height());
            if let Some(alpha) = self.image.alpha_auxiliary_of(id) {
                let alpha_frame = self.planar(alpha.id())?;
                apply_alpha(&mut rgba, size, &alpha_frame, C::ALPHA, C::ORIGIN)?;
            }
            (rgba, size.0, size.1)
        };
        let (rgba, w, h) = apply_transforms(item.transformative_properties(), decoded, C::ORIGIN)?;
        Ok((rgba, Dimensions::new(w, h)?))
    }

    fn overlay<T: RgbaSample>(&mut self, id: u32, item: HeifItem<'a>) -> Result<Rgba<T>> {
        // `overlay_for` validates the payload holds exactly one offset per `dimg` reference.
        let overlay = self.image.overlay_for(id, C::ORIGIN)?;
        let (ow, oh) = (overlay.output_width, overlay.output_height);
        // The declared canvas is refused before it is allocated (or any input decoded).
        self.limits().check(ow, oh, C::ORIGIN)?;
        let len = (ow as usize)
            .checked_mul(oh as usize)
            .and_then(|p| p.checked_mul(4))
            .filter(|_| ow != 0 && oh != 0)
            .ok_or_else(|| Self::invalid("HEIF: overlay canvas is empty or too large"))?;
        // The `canvas_fill_value` channels are 16-bit; narrowing to a smaller sample takes the
        // most significant bits (ISO/IEC 23008-12 §6.6.2.4), deliberately *not* the
        // round-to-nearest rescale used for coded samples.
        let fill = overlay.canvas_fill_value.map(T::from_canvas_fill);
        let mut canvas = try_filled(len, T::default(), C::ORIGIN)?;
        for px in canvas.as_chunks_mut::<4>().0 {
            *px = fill;
        }
        for (&input_id, offset) in item.derivation_target_ids().iter().zip(&overlay.offsets) {
            let (src, dims) = self.rgba::<T>(input_id)?;
            composite_over(
                &mut canvas,
                (ow, oh),
                &src,
                (dims.width(), dims.height()),
                (offset.horizontal, offset.vertical),
            );
        }
        Ok((canvas, ow, oh))
    }

    /// The matrix coefficients and range the RGBA surfaces de-matrix `item` with: its
    /// [`presentation_colour`](HeifItem::presentation_colour), else the colour its codestream
    /// signals.
    fn presentation_colour(&self, item: HeifItem<'a>) -> Result<(u16, ColorRange)> {
        let cicp = match item.presentation_colour() {
            None => self.signalled_colour(item.id())?,
            Some(ColourInformation::Nclx(nclx)) => *nclx,
            Some(_) => {
                return Err(Self::unsupported(
                    "HEIF: ICC-profile colr is not supported on the RGBA surface (use the planar \
                     surface)",
                ));
            }
        };
        let range = if cicp.full_range {
            ColorRange::Full
        } else {
            ColorRange::Limited
        };
        Ok((cicp.matrix_coefficients, range))
    }

    /// The colour signalled by the coded item whose samples item `id` presents: `id` itself when
    /// it is coded; for a `grid`, its **first** tile (the one libavif harvests an absent `colr`
    /// from); for an `iden`, its sole source — followed down to a coded item.
    ///
    /// It is only reached after the planar walk has followed these same `dimg` edges under its
    /// cycle and depth guard, so the chain is at most [`MAX_DERIVATION_DEPTH`] items long and the
    /// hop bound is never what stops it. The bound is there so termination does not rest on that
    /// argument alone (#890).
    fn signalled_colour(&self, mut id: u32) -> Result<Cicp> {
        for _ in 0..MAX_DERIVATION_DEPTH {
            match self
                .image
                .item(id)
                .map(|item| (item.kind(), item.derivation_target_ids()))
            {
                Some((ItemKind::Grid | ItemKind::Identity, [first, ..])) => id = *first,
                _ => break,
            }
        }
        let item = self
            .image
            .item(id)
            .ok_or_else(|| Self::invalid("HEIF: item id names no item"))?;
        let (config, payload) = Self::config(item)?;
        C::signalled_colour(&config, payload)
    }
}

/// Whether two tiles share dimensions, chroma layout and bit depth.
fn same_shape(a: &DecodedFrame, b: &DecodedFrame) -> bool {
    (a.dimensions(), a.chroma(), a.bit_depth()) == (b.dimensions(), b.chroma(), b.bit_depth())
}

/// Assembles uniform `tiles` (row-major, `cols` per row) into a frame cropped to `output`, which
/// the caller has checked lies within the tiled canvas.
fn assemble_grid(
    tiles: &[DecodedFrame],
    cols: usize,
    (ow, oh): (u32, u32),
    origin: &'static str,
) -> Result<DecodedFrame> {
    let first = &tiles[0];
    let (chroma, bit_depth, tile) = (first.chroma(), first.bit_depth(), first.dimensions());
    let plane = |pick: fn(&DecodedFrame) -> &[u16], (tw, th): (u32, u32), (pw, ph): (u32, u32)| {
        assemble_plane(
            tiles,
            pick,
            cols,
            (tw as usize, th as usize),
            (pw as usize, ph as usize),
            origin,
        )
    };
    let y = plane(DecodedFrame::y, (tile.width(), tile.height()), (ow, oh))?;
    let (cb, cr) = if chroma == ChromaSubsampling::Cs400 {
        (Vec::new(), Vec::new())
    } else {
        let tile_chroma = chroma.chroma_dimensions(tile.width(), tile.height());
        let out_chroma = chroma.chroma_dimensions(ow, oh);
        (
            plane(DecodedFrame::cb, tile_chroma, out_chroma)?,
            plane(DecodedFrame::cr, tile_chroma, out_chroma)?,
        )
    };
    DecodedFrame::new(Dimensions::new(ow, oh)?, bit_depth, chroma, y, cb, cr)
}

/// Assembles one plane of a grid directly into its cropped output: each output sample reads the
/// covering tile's plane, so the tile canvas (`columns·tile_pw × rows·tile_ph`) is sampled and
/// cropped to `out_pw × out_ph` in a single pass. `out_pw <= columns·tile_pw` and
/// `out_ph <= rows·tile_ph` are guaranteed by the caller, so the tile index is always in range.
fn assemble_plane(
    tiles: &[DecodedFrame],
    plane: fn(&DecodedFrame) -> &[u16],
    cols: usize,
    (tile_pw, tile_ph): (usize, usize),
    (out_pw, out_ph): (usize, usize),
    origin: &'static str,
) -> Result<Vec<u16>> {
    let mut out = try_filled(out_pw * out_ph, 0u16, origin)?;
    for (i, sample) in out.iter_mut().enumerate() {
        let (ox, oy) = (i % out_pw, i / out_pw);
        let tile = &tiles[(oy / tile_ph) * cols + ox / tile_pw];
        *sample = plane(tile)[(oy % tile_ph) * tile_pw + ox % tile_pw];
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use gamut_codec_abi::Registry;
    use gamut_core::{ErrorKind, Orientation};
    use gamut_isobmff::{
        ImageGrid, ImageOverlay, IsoBmffImage, Item, ItemReference, OverlayOffset, Property,
        PropertyKind,
    };

    use super::*;
    use crate::invariants::{STUB_ORIGIN, StubBackend, StubCodec};
    use crate::{BACKEND_POISONED, DIMENSION_LIMIT, NO_BACKEND};

    /// The stub's `hvcC` body: `chroma` code (0 4:4:4, 3 monochrome), 8-bit, signalling
    /// `matrix` at full range.
    fn hvcc(chroma: u8, matrix: u8) -> Property {
        Property::new(
            true,
            PropertyKind::CodecConfiguration {
                kind: *b"hvcC",
                data: vec![chroma, 8, matrix, 1],
            },
        )
    }

    /// A monochrome coded item the stub decodes from `payload` (its first two bytes the size).
    fn coded(id: u32, payload: &[u8]) -> Item {
        coded_as(id, payload, hvcc(3, 1))
    }

    fn coded_as(id: u32, payload: &[u8], config: Property) -> Item {
        Item::new(id, *b"hvc1")
            .with_properties(vec![config])
            .with_payload(payload.to_vec())
    }

    fn with(item: Item, kind: PropertyKind) -> Item {
        let mut properties = item.properties.clone();
        properties.push(Property::new(false, kind));
        item.with_properties(properties)
    }

    fn derived(id: u32, item_type: [u8; 4], payload: Vec<u8>, sources: &[u32]) -> Item {
        Item::new(id, item_type)
            .with_references(vec![ItemReference::new(*b"dimg", sources.to_vec())])
            .with_payload(payload)
    }

    fn grid(id: u32, (columns, rows): (u16, u16), output: (u32, u32), tiles: &[u32]) -> Item {
        let payload = ImageGrid {
            rows,
            columns,
            output_width: output.0,
            output_height: output.1,
        }
        .to_bytes()
        .unwrap();
        derived(id, *b"grid", payload, tiles)
    }

    fn image(items: Vec<Item>) -> HeifImage {
        let primary = items[0].id;
        HeifImage::new(IsoBmffImage::new(*b"heic", vec![*b"mif1"], primary, items)).unwrap()
    }

    fn planar(image: &HeifImage, id: u32, limits: DecodeLimits) -> Result<DecodedFrame> {
        image.decode_item_planar::<StubCodec>(id, &StubBackend::registry(), limits)
    }

    fn rgba8(image: &HeifImage, id: u32) -> Result<ImageBuf<Rgba8>> {
        image.decode_item_rgba8::<StubCodec>(id, &StubBackend::registry(), DecodeOptions::default())
    }

    /// The refusal `result` carries, checking it names the binding.
    fn refusal<T: core::fmt::Debug>(result: Result<T>) -> (ErrorKind, Option<&'static str>) {
        let error = result.unwrap_err();
        assert_eq!(error.origin(), Some(STUB_ORIGIN), "{error}");
        (error.kind(), error.static_message())
    }

    fn slot(declines: bool) -> Arc<Mutex<StubBackend>> {
        Arc::new(Mutex::new(StubBackend {
            declines,
            decoded: 0,
        }))
    }

    #[test]
    fn a_coded_item_decodes_through_the_first_accepting_backend() {
        let (declining, accepting, later) = (slot(true), slot(false), slot(false));
        let mut backends = Registry::new();
        for s in [&declining, &accepting, &later] {
            backends.push(Arc::clone(s));
        }
        let image = image(vec![coded(1, &[2, 1, 50])]);
        let frame = image
            .decode_item_planar::<StubCodec>(1, &backends, DecodeLimits::default())
            .unwrap();
        assert_eq!((frame.width(), frame.height()), (3, 2));
        assert_eq!(frame.y(), [2, 1, 50, 2, 1, 50]);
        let decoded = |s: &Arc<Mutex<StubBackend>>| s.lock().unwrap().decoded;
        assert_eq!(
            (decoded(&declining), decoded(&accepting), decoded(&later)),
            (0, 1, 0)
        );
    }

    #[test]
    fn no_accepting_or_a_poisoned_backend_is_the_bindings_refusal() {
        let image = image(vec![coded(1, &[0, 0])]);
        let mut declining = Registry::new();
        declining.push(slot(true));
        let none = image.decode_item_planar::<StubCodec>(1, &declining, DecodeLimits::default());
        assert_eq!(refusal(none), (ErrorKind::Unsupported, Some(NO_BACKEND)));

        let poisoned = slot(false);
        let held = Arc::clone(&poisoned);
        let _ = std::thread::spawn(move || {
            let _guard = held.lock().unwrap();
            panic!("poison the backend's lock");
        })
        .join();
        let mut backends = Registry::new();
        backends.push(poisoned);
        let result = image.decode_item_planar::<StubCodec>(1, &backends, DecodeLimits::default());
        assert_eq!(
            refusal(result),
            (ErrorKind::Unsupported, Some(BACKEND_POISONED))
        );
    }

    #[test]
    fn a_backends_own_error_is_returned_unchanged() {
        // The stub hands a 7-bit depth to `DecodedFrame::new`, which refuses it under its own
        // origin: the pipeline must not re-attribute it.
        let config = Property::new(
            true,
            PropertyKind::CodecConfiguration {
                kind: *b"hvcC",
                data: vec![3, 7, 1, 1],
            },
        );
        let error = planar(
            &image(vec![coded_as(1, &[0, 0], config)]),
            1,
            DecodeLimits::default(),
        )
        .unwrap_err();
        assert_eq!(error.origin(), Some("gamut-heif"));
        assert_eq!(
            error.static_message(),
            Some("HEIF: frame bit depth out of range (8..=16)")
        );
    }

    #[test]
    fn items_the_planar_surface_cannot_decode_are_refused_by_kind() {
        let unknown_essential = coded(2, &[0, 0]).with_properties(vec![
            hvcc(3, 1),
            Property::new(
                true,
                PropertyKind::Other {
                    kind: *b"a1op",
                    data: vec![],
                },
            ),
        ]);
        let overlay = ImageOverlay {
            canvas_fill_value: [0; 4],
            output_width: 1,
            output_height: 1,
            offsets: vec![OverlayOffset {
                horizontal: 0,
                vertical: 0,
            }],
        }
        .to_bytes()
        .unwrap();
        let image = image(vec![
            coded(1, &[0, 0]),
            unknown_essential,
            Item::new(3, *b"av01").with_properties(vec![hvcc(3, 1)]),
            Item::new(4, *b"hvc1").with_payload(vec![0]),
            derived(5, *b"iden", vec![], &[1, 1]),
            derived(6, *b"iovl", overlay, &[1]),
            Item::new(7, *b"Exif"),
            coded(8, &[]),
        ]);
        let refused = |id| refusal(planar(&image, id, DecodeLimits::default()));
        assert_eq!(
            refused(99),
            (ErrorKind::InvalidInput, Some("HEIF: item id names no item"))
        );
        assert_eq!(
            refused(2),
            (
                ErrorKind::Unsupported,
                Some("HEIF: item has an unrecognised essential property")
            )
        );
        assert_eq!(
            refused(3),
            (
                ErrorKind::Unsupported,
                Some("HEIF: coded item is not of this binding's codec")
            )
        );
        assert_eq!(
            refused(4),
            (
                ErrorKind::InvalidInput,
                Some("HEIF: coded item has no codec configuration")
            )
        );
        assert_eq!(
            refused(5),
            (
                ErrorKind::InvalidInput,
                Some("HEIF: iden item must reference exactly one source")
            )
        );
        assert_eq!(refused(6).0, ErrorKind::Unsupported);
        assert_eq!(
            refused(7),
            (
                ErrorKind::InvalidInput,
                Some("HEIF: item is not a decodable image")
            )
        );
        // The binding's own still-payload check runs before any backend.
        assert_eq!(refused(8).1, Some("stub: empty payload"));
    }

    #[test]
    fn a_configuration_of_another_box_type_is_not_this_codecs() {
        let foreign = Property::new(
            true,
            PropertyKind::CodecConfiguration {
                kind: *b"av1C",
                data: vec![3, 8, 1, 1],
            },
        );
        let image = image(vec![coded_as(1, &[0, 0], foreign)]);
        assert_eq!(
            refusal(planar(&image, 1, DecodeLimits::default())).1,
            Some("HEIF: coded item has no codec configuration")
        );
    }

    #[test]
    fn a_derivation_cycle_and_an_overdeep_chain_are_refused() {
        let cycle = image(vec![
            derived(1, *b"iden", vec![], &[2]),
            derived(2, *b"iden", vec![], &[1]),
        ]);
        assert_eq!(
            refusal(planar(&cycle, 1, DecodeLimits::default())),
            (
                ErrorKind::InvalidInput,
                Some("HEIF: derivation reference cycle")
            )
        );
        // MAX_DERIVATION_DEPTH items on the branch is the most: a chain of that many `iden`s
        // above a coded item puts one item too many on it.
        let chain = |idens: u32| {
            let mut items: Vec<Item> = (1..=idens)
                .map(|id| derived(id, *b"iden", vec![], &[id + 1]))
                .collect();
            items.push(coded(idens + 1, &[0, 0]));
            image(items)
        };
        let depth = MAX_DERIVATION_DEPTH as u32;
        assert!(planar(&chain(depth - 1), 1, DecodeLimits::default()).is_ok());
        assert_eq!(
            refusal(planar(&chain(depth), 1, DecodeLimits::default())),
            (
                ErrorKind::Unsupported,
                Some("HEIF: derivation nested too deeply")
            )
        );
    }

    #[test]
    fn a_grid_assembles_its_tiles_row_major_and_crops_every_plane() {
        // Two 2×1 4:4:4 tiles side by side, cropped to 3×1. The stub's planes are the payload
        // read cyclically: luma from byte 0, Cb from byte 1, Cr from byte 2.
        let config = || hvcc(0, 1);
        let image = image(vec![
            grid(1, (2, 1), (3, 1), &[2, 3]),
            coded_as(2, &[1, 0], config()),
            coded_as(3, &[17, 0], config()),
        ]);
        let frame = planar(&image, 1, DecodeLimits::default()).unwrap();
        assert_eq!((frame.width(), frame.height()), (3, 1));
        assert_eq!(frame.y(), [1, 0, 17]);
        assert_eq!(frame.cb(), [0, 1, 0]);
        assert_eq!(frame.cr(), [1, 0, 17]);
        // Two rows of one column take the tiles top to bottom.
        let image = self::image(vec![
            grid(1, (1, 2), (2, 2), &[2, 3]),
            coded(2, &[1, 0]),
            coded(3, &[17, 0]),
        ]);
        assert_eq!(
            planar(&image, 1, DecodeLimits::default()).unwrap().y(),
            [1, 0, 17, 0]
        );
    }

    #[test]
    fn a_grid_of_multi_row_tiles_takes_each_tiles_rows_in_turn() {
        // A 2×2 grid of 2×2 tiles: the stub's luma for payload [1, 1, a, b, c] is [1, 1, a, b].
        let tile = |id: u32, t: u8| coded(id, &[1, 1, 10 + t, 20 + t, 0]);
        let image = image(vec![
            grid(1, (2, 2), (3, 4), &[2, 3, 4, 5]),
            tile(2, 0),
            tile(3, 1),
            tile(4, 2),
            tile(5, 3),
        ]);
        let frame = planar(&image, 1, DecodeLimits::default()).unwrap();
        assert_eq!(
            frame.y(),
            [1, 1, 1, 10, 20, 11, 1, 1, 1, 12, 22, 13],
            "rows: tile row 0 (top halves, bottom halves), then tile row 1"
        );
    }

    #[test]
    fn a_grid_of_mismatched_tiles_or_an_overhanging_output_is_refused() {
        let mismatched = image(vec![
            grid(1, (2, 1), (2, 1), &[2, 3]),
            coded(2, &[0, 0]),
            coded_as(3, &[0, 0], hvcc(0, 1)),
        ]);
        assert_eq!(
            refusal(planar(&mismatched, 1, DecodeLimits::default())).1,
            Some("HEIF: grid tiles are not uniform in dimensions, chroma, or bit depth")
        );
        // Two 1×1 tiles cannot fill a 3-wide output.
        let overhang = image(vec![
            grid(1, (2, 1), (3, 1), &[2, 3]),
            coded(2, &[0, 0]),
            coded(3, &[0, 0]),
        ]);
        assert_eq!(
            refusal(planar(&overhang, 1, DecodeLimits::default())).1,
            Some("HEIF: grid output dimensions exceed the tiled canvas")
        );
        // An empty output on either axis is refused by the same check, inside the canvas.
        for output in [(0, 1), (1, 0)] {
            let empty = image(vec![
                grid(1, (2, 1), output, &[2, 3]),
                coded(2, &[0, 0]),
                coded(3, &[0, 0]),
            ]);
            assert_eq!(
                refusal(planar(&empty, 1, DecodeLimits::default())).1,
                Some("HEIF: grid output dimensions exceed the tiled canvas"),
                "{output:?}"
            );
        }
    }

    #[test]
    fn a_grid_canvas_past_the_limits_is_refused_though_its_output_is_inside_them() {
        // Two 6×1 tiles make a 12×1 canvas cropped to 7×1: the output respects a 10-pixel cap,
        // the canvas the tiles fill does not.
        let image = |tile: u8| {
            self::image(vec![
                grid(1, (2, 1), (7, 1), &[2, 3]),
                coded(2, &[tile, 0]),
                coded(3, &[tile, 0]),
            ])
        };
        assert_eq!(
            refusal(planar(&image(5), 1, DecodeLimits::new(10, 100))),
            (ErrorKind::Unsupported, Some(DIMENSION_LIMIT))
        );
        assert!(planar(&image(4), 1, DecodeLimits::new(10, 100)).is_ok());
        // The declared output itself is checked before any tile is decoded.
        assert_eq!(
            refusal(planar(&image(5), 1, DecodeLimits::new(6, 100))).1,
            Some(DIMENSION_LIMIT)
        );
    }

    #[test]
    fn a_declared_size_past_the_limits_is_refused_before_any_backend_runs() {
        let backend = slot(false);
        let mut backends = Registry::new();
        backends.push(Arc::clone(&backend));
        let declared = with(
            coded(1, &[0, 0]),
            PropertyKind::ImageSpatialExtents {
                width: 11,
                height: 1,
            },
        );
        let result = image(vec![declared]).decode_item_planar::<StubCodec>(
            1,
            &backends,
            DecodeLimits::new(10, 100),
        );
        assert_eq!(refusal(result).1, Some(DIMENSION_LIMIT));
        assert_eq!(backend.lock().unwrap().decoded, 0);
        // A delivered frame past the limits is refused too, whatever `ispe` said.
        let result = planar(
            &image(vec![coded(1, &[10, 0])]),
            1,
            DecodeLimits::new(10, 100),
        );
        assert_eq!(refusal(result).1, Some(DIMENSION_LIMIT));
    }

    #[test]
    fn repeated_references_are_charged_against_the_work_caps() {
        // A grid naming one tile twice decodes it twice.
        let repeated = image(vec![grid(1, (2, 1), (2, 1), &[2, 2]), coded(2, &[0, 0])]);
        let caps = |decoded, derived| DecodeLimits::default().with_item_limits(decoded, derived);
        assert!(planar(&repeated, 1, caps(2, 1)).is_ok());
        assert_eq!(
            refusal(planar(&repeated, 1, caps(1, 1))).1,
            Some(DECODED_ITEM_LIMIT)
        );
        assert_eq!(
            refusal(planar(&repeated, 1, caps(2, 0))).1,
            Some(DERIVED_ITEM_LIMIT)
        );
        // An `iden` is a derivation too.
        let iden = image(vec![derived(1, *b"iden", vec![], &[2]), coded(2, &[0, 0])]);
        assert!(planar(&iden, 1, caps(1, 1)).is_ok());
        assert_eq!(
            refusal(planar(&iden, 1, caps(1, 0))).1,
            Some(DERIVED_ITEM_LIMIT)
        );
    }

    /// A 1×1 4:4:4 item decoding to Y 0, Cb 0, Cr 10, its codestream signalling `matrix`.
    fn pixel(id: u32, matrix: u8) -> Item {
        coded_as(id, &[0, 0, 10], hvcc(0, matrix))
    }

    fn nclx(matrix_coefficients: u16) -> PropertyKind {
        PropertyKind::Colour(ColourInformation::Nclx(Cicp {
            colour_primaries: 1,
            transfer_characteristics: 13,
            matrix_coefficients,
            full_range: true,
        }))
    }

    /// What [`pixel`] presents as under identity (GBR) and under full-range BT.601.
    const AS_IDENTITY: [u8; 4] = [10, 0, 0, 255];

    fn as_bt601() -> Vec<u8> {
        let (r, g, b) = gamut_color::bt601_ycbcr_to_rgb(0, 0, 10, ColorRange::Full);
        vec![r, g, b, 255]
    }

    #[test]
    fn with_no_colr_the_codestreams_signalled_colour_presents() {
        assert_eq!(
            rgba8(&image(vec![pixel(1, 0)]), 1).unwrap().as_samples(),
            AS_IDENTITY
        );
        assert_eq!(
            rgba8(&image(vec![pixel(1, 6)]), 1).unwrap().as_samples(),
            as_bt601()
        );
        // A grid presents with its first tile's signal, an `iden` with its source's.
        let grid = image(vec![
            grid(1, (2, 1), (1, 1), &[2, 3]),
            pixel(2, 0),
            pixel(3, 6),
        ]);
        assert_eq!(rgba8(&grid, 1).unwrap().as_samples(), AS_IDENTITY);
        let iden = image(vec![derived(1, *b"iden", vec![], &[2]), pixel(2, 6)]);
        assert_eq!(rgba8(&iden, 1).unwrap().as_samples(), as_bt601());
    }

    #[test]
    fn an_nclx_colr_wins_over_the_signal_and_over_an_icc_colr_in_either_order() {
        let icc = || PropertyKind::Colour(ColourInformation::UnrestrictedIcc(vec![1, 2]));
        let nclx_first = with(with(pixel(1, 0), nclx(6)), icc());
        let icc_first = with(with(pixel(1, 0), icc()), nclx(6));
        for item in [nclx_first, icc_first] {
            assert_eq!(
                rgba8(&image(vec![item]), 1).unwrap().as_samples(),
                as_bt601()
            );
        }
        // An ICC-only colr is refused rather than presented under the signal.
        assert_eq!(
            refusal(rgba8(&image(vec![with(pixel(1, 0), icc())]), 1)),
            (
                ErrorKind::Unsupported,
                Some(
                    "HEIF: ICC-profile colr is not supported on the RGBA surface (use the planar \
                     surface)"
                )
            )
        );
    }

    /// An alpha auxiliary of `master` coded under `config`.
    fn alpha_of(id: u32, master: u32, payload: &[u8], config: Property) -> Item {
        let aux = PropertyKind::AuxiliaryType {
            aux_type: "urn:mpeg:mpegB:cicp:systems:auxiliary:alpha".to_owned(),
            aux_subtype: vec![],
        };
        with(coded_as(id, payload, config), aux)
            .with_hidden(true)
            .with_references(vec![ItemReference::new(*b"auxl", vec![master])])
    }

    #[test]
    fn the_alpha_auxiliary_fills_the_alpha_channel_under_the_bindings_policy() {
        let master = coded(1, &[1, 0, 77]);
        let alpha = alpha_of(2, 1, &[1, 0, 200], hvcc(3, 1));
        let rgba = rgba8(&image(vec![master.clone(), alpha]), 1).unwrap();
        // Luma [1, 0] presents as gray 1 and 0; the alpha luma [1, 0] becomes the alpha.
        assert_eq!(rgba.as_samples(), [1, 1, 1, 1, 0, 0, 0, 0]);
        // The stub binding accepts only a monochrome auxiliary.
        let coloured = alpha_of(2, 1, &[1, 0, 200], hvcc(0, 1));
        assert_eq!(
            refusal(rgba8(&image(vec![master.clone(), coloured]), 1)).1,
            Some("HEIF: alpha auxiliary must be monochrome")
        );
        let small = alpha_of(2, 1, &[0, 0], hvcc(3, 1));
        assert_eq!(
            refusal(rgba8(&image(vec![master, small]), 1)).1,
            Some("HEIF: alpha auxiliary dimensions do not match the master image")
        );
    }

    #[test]
    fn transforms_apply_after_presentation() {
        let rotated = with(coded(1, &[2, 0, 5]), PropertyKind::Rotation(1));
        let image = image(vec![rotated]);
        let rgba = rgba8(&image, 1).unwrap();
        assert_eq!((rgba.width(), rgba.height()), (1, 3));
        assert_eq!(image.primary_item().orientation(), Orientation::Rotate90Ccw);
        // The planar surface does not transform.
        let frame = planar(&image, 1, DecodeLimits::default()).unwrap();
        assert_eq!((frame.width(), frame.height()), (3, 1));
    }

    fn overlay(id: u32, fill: [u16; 4], output: (u32, u32), inputs: &[(u32, i32, i32)]) -> Item {
        let payload = ImageOverlay {
            canvas_fill_value: fill,
            output_width: output.0,
            output_height: output.1,
            offsets: inputs
                .iter()
                .map(|&(_, horizontal, vertical)| OverlayOffset {
                    horizontal,
                    vertical,
                })
                .collect(),
        }
        .to_bytes()
        .unwrap();
        let ids: Vec<u32> = inputs.iter().map(|&(id, _, _)| id).collect();
        derived(id, *b"iovl", payload, &ids)
    }

    #[test]
    fn an_overlay_composites_its_inputs_over_its_fill_on_the_rgba_surfaces() {
        let image = image(vec![
            overlay(1, [0x1000, 0x2000, 0x3000, 0xFFFF], (3, 1), &[(2, 1, 0)]),
            coded(2, &[0, 0, 99]),
        ]);
        // The fill's top byte, then the opaque 1×1 input at x = 1.
        assert_eq!(
            rgba8(&image, 1).unwrap().as_samples(),
            [0x10, 0x20, 0x30, 255, 0, 0, 0, 255, 0x10, 0x20, 0x30, 255]
        );
        let wide = image
            .decode_item_rgba16::<StubCodec>(1, &StubBackend::registry(), DecodeOptions::default())
            .unwrap();
        assert_eq!(&wide.as_samples()[..4], &[0x1000, 0x2000, 0x3000, 0xFFFF]);
        // Not on the planar surface, and not past the limits.
        assert_eq!(
            refusal(planar(&image, 1, DecodeLimits::default())).0,
            ErrorKind::Unsupported
        );
        let tight = DecodeOptions::default().with_limits(DecodeLimits::new(2, 100));
        let refused = image.decode_item_rgba8::<StubCodec>(1, &StubBackend::registry(), tight);
        assert_eq!(refusal(refused).1, Some(DIMENSION_LIMIT));
        let no_derivations =
            DecodeOptions::default().with_limits(DecodeLimits::default().with_item_limits(10, 0));
        let refused =
            image.decode_item_rgba8::<StubCodec>(1, &StubBackend::registry(), no_derivations);
        assert_eq!(refusal(refused).1, Some(DERIVED_ITEM_LIMIT));
    }

    #[test]
    fn an_empty_overlay_canvas_is_refused() {
        for output in [(0, 1), (1, 0)] {
            let image = image(vec![
                overlay(1, [0; 4], output, &[(2, 0, 0)]),
                coded(2, &[0, 0]),
            ]);
            assert_eq!(
                refusal(rgba8(&image, 1)).1,
                Some("HEIF: overlay canvas is empty or too large"),
                "{output:?}"
            );
        }
    }

    #[test]
    fn an_overlay_naming_itself_is_a_cycle() {
        let image = image(vec![overlay(1, [0; 4], (1, 1), &[(1, 0, 0)])]);
        assert_eq!(
            refusal(rgba8(&image, 1)).1,
            Some("HEIF: derivation reference cycle")
        );
    }

    /// The pinned-seed driver of `invariants::decoding_stays_within_limits` over generated
    /// coded, `grid` and `iovl` files.
    mod limits_law {
        use proptest::collection;
        use proptest::prelude::{Strategy, prop_assert_eq, prop_oneof, proptest};
        use proptest::test_runner::{Config, RngSeed};

        use super::*;
        use crate::invariants::decoding_stays_within_limits;

        /// The looser run's limits: well past anything generated here.
        const CEILING: DecodeLimits = DecodeLimits::new(1024, 1 << 20);

        /// The workspace property-test configuration (`docs/testing.md`): pinned seed, bounded
        /// shrinking, no failure persistence.
        fn config() -> Config {
            Config {
                cases: 512,
                max_shrink_iters: 2048,
                max_shrink_time: 10_000,
                rng_seed: RngSeed::Fixed(0x6761_6D75_745F_6866),
                failure_persistence: None,
                ..Config::default()
            }
        }

        /// A monochrome coded item the stub decodes to `(1 + w % 16) × (1 + h % 16)`,
        /// optionally declaring `ispe`.
        fn coded_with(id: u32, (w, h): (u8, u8), ispe: Option<(u32, u32)>, hidden: bool) -> Item {
            let mut item = coded(id, &[w, h, 0x80]).with_hidden(hidden);
            if let Some((width, height)) = ispe {
                item = with(item, PropertyKind::ImageSpatialExtents { width, height });
            }
            item
        }

        fn file(items: Vec<Item>) -> Vec<u8> {
            IsoBmffImage::new(*b"heic", vec![*b"mif1"], 1, items)
                .to_bytes()
                .expect("the generated model is writable")
        }

        fn frame_bytes() -> impl Strategy<Value = (u8, u8)> {
            (0u8..=255, 0u8..=255)
        }

        fn coded_file() -> impl Strategy<Value = Vec<u8>> {
            let ispe = proptest::option::of((1u32..=48, 1u32..=48));
            (frame_bytes(), ispe)
                .prop_map(|(frame, ispe)| file(vec![coded_with(1, frame, ispe, false)]))
        }

        fn grid_file() -> impl Strategy<Value = Vec<u8>> {
            (1u16..=3, 1u16..=3, frame_bytes(), 1u32..=48, 1u32..=48).prop_map(
                |(rows, columns, frame, ow, oh)| {
                    let tiles: Vec<u32> = (2..2 + u32::from(rows * columns)).collect();
                    let mut items = vec![grid(1, (columns, rows), (ow, oh), &tiles)];
                    items.extend(tiles.iter().map(|&id| coded_with(id, frame, None, true)));
                    file(items)
                },
            )
        }

        fn overlay_file() -> impl Strategy<Value = Vec<u8>> {
            let input = (frame_bytes(), -8i32..=40, -8i32..=40);
            (1u32..=48, 1u32..=48, collection::vec(input, 1..=3)).prop_map(|(ow, oh, inputs)| {
                let placed: Vec<(u32, i32, i32)> = inputs
                    .iter()
                    .zip(2..)
                    .map(|(&(_, x, y), id)| (id, x, y))
                    .collect();
                let mut items = vec![overlay(1, [1000, 2000, 3000, 65535], (ow, oh), &placed)];
                items.extend(
                    placed
                        .iter()
                        .zip(&inputs)
                        .map(|(&(id, _, _), &(frame, _, _))| coded_with(id, frame, None, true)),
                );
                file(items)
            })
        }

        /// The property is vacuous if the generated files never decode: pin that one of each
        /// shape does, at the size the stub and the derivation give it.
        #[test]
        fn the_generated_files_decode() {
            let sizes = |bytes: Vec<u8>| {
                let image = HeifImage::new(IsoBmffImage::parse(&bytes).unwrap()).unwrap();
                let options = DecodeOptions::default().with_limits(CEILING);
                let rgba = image
                    .decode_item_rgba16::<StubCodec>(1, &StubBackend::registry(), options)
                    .unwrap();
                (rgba.width(), rgba.height())
            };
            assert_eq!(
                sizes(file(vec![coded_with(1, (3, 4), None, false)])),
                (4, 5)
            );
            assert_eq!(
                sizes(file(vec![
                    grid(1, (2, 1), (7, 5), &[2, 3]),
                    coded_with(2, (3, 4), None, true),
                    coded_with(3, (3, 4), None, true),
                ])),
                (7, 5)
            );
            assert_eq!(
                sizes(file(vec![
                    overlay(1, [0; 4], (9, 2), &[(2, 0, 0)]),
                    coded_with(2, (3, 4), None, true),
                ])),
                (9, 2)
            );
        }

        proptest! {
            #![proptest_config(config())]

            #[test]
            fn decoding_never_returns_an_image_past_its_limits(
                bytes in prop_oneof![coded_file(), grid_file(), overlay_file()],
                max_dimension in 1u32..=40,
                max_pixels in 1u64..=1600,
            ) {
                let limits = DecodeLimits::new(max_dimension, max_pixels);
                prop_assert_eq!(decoding_stays_within_limits(&bytes, limits, CEILING), Ok(()));
            }
        }
    }
}
