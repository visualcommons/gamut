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
