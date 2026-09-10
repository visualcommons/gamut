//! The [`JxlEncoder`]: a typed front end over a stack of JPEG XL codestream encoders — any backend
//! pushed with [`JxlEncoder::push_backend`], and, last, the built-in libjxl wrapper
//! ([`crate::ffi`]).
//!
//! Construct one with a mode — [`JxlEncoder::lossless`] (the default) or [`JxlEncoder::lossy`] — then
//! refine it with the chainable [`JxlEncoder::with_effort`] / [`JxlEncoder::with_modular`] /
//! [`JxlEncoder::with_container`] builders, and drive it through the [`EncodeImage`] trait for any
//! of the eight supported pixel layouts.

use gamut_core::{
    EncodeImage, Error, Gray8, Gray16, GrayAlpha8, GrayAlpha16, ImageRef, Pixel, Result, Rgb8,
    Rgb16, Rgba8, Rgba16,
};

use crate::backend::{JxlCodestreamEncoder, JxlEncodeRequest, JxlImageRef, JxlSamples, Registry};
use crate::config::{ColorSpec, Container, Distance, Effort, Mode, ModularMode, Orientation};

/// The refusal returned when no backend can encode: nothing was pushed, and the built-in libjxl
/// tail is not compiled into this build (no `encode` feature, or a `wasm32` target it cannot be
/// built for).
///
/// Always compiled (and unit-tested) even where the tail *is* present, so its message stays pinned
/// on every build rather than only on the targets that can return it.
#[cfg_attr(
    all(
        feature = "encode",
        any(not(target_arch = "wasm32"), target_os = "emscripten")
    ),
    allow(dead_code)
)]
pub(crate) fn no_encode_backend() -> Error {
    Error::unsupported(
        env!("CARGO_PKG_NAME"),
        "JXL: no encode backend (enable the `encode` feature or push a codestream backend)",
    )
}

/// Resolves the **coded** bit depth to declare for a raster whose samples are stored
/// `bits_per_sample` wide: the storage width, unless [`JxlEncoder::with_bit_depth`] narrowed it.
///
/// Shared by the built-in FFI driver and the backend request builder so both see one value.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if the override is `0` or wider than the storage width — neither
/// can mean anything coherent.
pub(crate) fn resolve_coded_bits(cfg: &JxlEncoder, bits_per_sample: u32) -> Result<u32> {
    match cfg.bit_depth() {
        None => Ok(bits_per_sample),
        Some(bits) => {
            let bits = u32::from(bits);
            if bits == 0 || bits > bits_per_sample {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JXL: coded bit depth must be 1..= the sample width",
                ));
            }
            Ok(bits)
        }
    }
}

/// A JPEG XL encoder.
///
/// Encodes 8- and 16-bit grayscale, gray+alpha, RGB and RGBA images. Pick a mode at construction —
/// [`JxlEncoder::lossless`] (bit-exact; also [`JxlEncoder::new`] and [`Default`]) or
/// [`JxlEncoder::lossy`] with a Butteraugli [`Distance`] — then optionally set the [`Effort`],
/// [`ModularMode`] and output [`Container`] with the `with_*` builders. Encode through the
/// [`EncodeImage`](gamut_core::EncodeImage) trait, which appends the JPEG XL stream to the caller's
/// buffer.
///
/// # Backends
///
/// The codestream itself is produced by a [`JxlCodestreamEncoder`]. With the `encode` feature (and
/// an encoder-capable target) the reference libjxl wrapper is the implicit **last** backend, so the
/// default encoder needs no wiring at all. [`JxlEncoder::push_backend`] inserts a platform or
/// alternate encoder *ahead* of it — and is what supplies the encode direction on `wasm32`, where
/// the libjxl tail cannot be built. See [`crate::backend`] for the fallback contract and the
/// container-feature veto.
///
/// The type is `Clone` but deliberately not `Copy`: a [`ColorSpec::Icc`] configuration owns the
/// profile bytes, and the backend registry is shared. Cloning shares one registry: a backend pushed
/// onto a clone is visible through the original.
///
/// `PartialEq` compares the **configuration** only — backends are opaque and have no equality.
#[derive(Debug, Clone)]
pub struct JxlEncoder {
    /// Lossless, or lossy at a validated distance.
    mode: Mode,
    /// The speed/density effort level.
    effort: Effort,
    /// Which coding tool (VarDCT or Modular) the encoder is told to use, if either.
    modular: ModularMode,
    /// Codestream vs. ISO BMFF container framing.
    container: Container,
    /// The colour interpretation signalled for the pixel samples.
    color: ColorSpec,
    /// The display orientation signalled for the coded samples.
    orientation: Orientation,
    /// Raw EXIF payload for an `Exif` container box, if any.
    exif: Option<Vec<u8>>,
    /// XMP (XML) payload for an `xml ` container box, if any.
    xmp: Option<Vec<u8>>,
    /// The declared coded bit depth override, if any (see [`JxlEncoder::with_bit_depth`]).
    bit_depth: Option<u8>,
    /// Pushed codestream backends, tried in push order ahead of the built-in libjxl tail.
    backends: Registry<dyn JxlCodestreamEncoder>,
}

impl PartialEq for JxlEncoder {
    /// Compares the encoder **configuration**; the backend registries are ignored, since a
    /// [`JxlCodestreamEncoder`] is an opaque trait object with no notion of equality.
    fn eq(&self, other: &Self) -> bool {
        self.mode == other.mode
            && self.effort == other.effort
            && self.modular == other.modular
            && self.container == other.container
            && self.color == other.color
            && self.orientation == other.orientation
            && self.exif == other.exif
            && self.xmp == other.xmp
            && self.bit_depth == other.bit_depth
    }
}

impl Default for JxlEncoder {
    /// The default encoder is **lossless** — identical to [`JxlEncoder::lossless`], which (with
    /// [`JxlEncoder::new`]) is an intent-revealing alias for this canonical construction.
    fn default() -> Self {
        Self {
            mode: Mode::Lossless,
            effort: Effort::default(),
            modular: ModularMode::default(),
            container: Container::default(),
            color: ColorSpec::default(),
            orientation: Orientation::default(),
            exif: None,
            xmp: None,
            bit_depth: None,
            backends: Registry::default(),
        }
    }
}

impl JxlEncoder {
    /// Creates an encoder with the default configuration; equivalent to [`JxlEncoder::lossless`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an encoder that produces a **lossless** stream — the decoded image is bit-exact to the
    /// input. This is the default mode, so [`JxlEncoder::new`] and [`JxlEncoder::default`] return the
    /// same encoder; it exists to pair with [`JxlEncoder::lossy`] and make intent explicit.
    #[must_use]
    pub fn lossless() -> Self {
        Self::default()
    }

    /// Creates an encoder that produces a **lossy** stream at the given Butteraugli [`Distance`]
    /// (`1.0` = visually lossless; larger = smaller file, lower quality).
    #[must_use]
    pub fn lossy(distance: Distance) -> Self {
        Self {
            mode: Mode::Lossy(distance),
            ..Self::default()
        }
    }

    /// Sets the [`Effort`] (speed/density trade-off). Returns the updated encoder for chaining.
    #[must_use]
    pub fn with_effort(mut self, effort: Effort) -> Self {
        self.effort = effort;
        self
    }

    /// Sets the [`ModularMode`] (which coding tool libjxl uses, or [`ModularMode::Auto`] to let it
    /// choose). Returns the updated encoder for chaining.
    ///
    /// [`ModularMode::VarDct`] on a **lossless** encoder is a contradiction — libjxl always codes a
    /// lossless frame with Modular — and is rejected when encoding rather than silently ignored.
    #[must_use]
    pub fn with_modular(mut self, modular: ModularMode) -> Self {
        self.modular = modular;
        self
    }

    /// Sets the output [`Container`] framing (bare codestream vs. ISO BMFF). Returns the updated
    /// encoder for chaining.
    #[must_use]
    pub fn with_container(mut self, container: Container) -> Self {
        self.container = container;
        self
    }

    /// Sets the [`ColorSpec`] signalled for the pixel samples (sRGB by default). Returns the
    /// updated encoder for chaining.
    ///
    /// The encoder never converts pixels between colour spaces — this only declares how the
    /// caller's samples are to be interpreted. An [`ColorSpec::Icc`] profile is validated against
    /// the image's colour family when encoding.
    #[must_use]
    pub fn with_color(mut self, color: ColorSpec) -> Self {
        self.color = color;
        self
    }

    /// Sets the display [`Orientation`] signalled for the coded samples ([`Orientation::Identity`]
    /// by default). Returns the updated encoder for chaining.
    ///
    /// Orientation is metadata: the samples are stored exactly as given, and decoders apply the
    /// transform on output (the transposing variants swap the displayed width and height).
    #[must_use]
    pub fn with_orientation(mut self, orientation: Orientation) -> Self {
        self.orientation = orientation;
        self
    }

    /// Attaches a raw EXIF payload, stored as an `Exif` box in the ISO BMFF container. Returns the
    /// updated encoder for chaining.
    ///
    /// `exif` is the TIFF-structured EXIF data itself (starting with the `II`/`MM` byte-order
    /// mark); the 4-byte tiff-header offset the `Exif` box format requires is prepended
    /// automatically. Because metadata lives in container boxes, encoding requires
    /// [`Container::IsoBmff`] — combining metadata with [`Container::Codestream`] is a typed error
    /// rather than a silent framing change.
    #[must_use]
    pub fn with_exif(mut self, exif: &[u8]) -> Self {
        self.exif = Some(exif.to_vec());
        self
    }

    /// Attaches an XMP (XML) packet, stored as an `xml ` box in the ISO BMFF container. Returns
    /// the updated encoder for chaining.
    ///
    /// As with [`JxlEncoder::with_exif`], encoding then requires [`Container::IsoBmff`].
    #[must_use]
    pub fn with_xmp(mut self, xmp: &str) -> Self {
        self.xmp = Some(xmp.as_bytes().to_vec());
        self
    }

    /// Embeds a unified [`Metadata`](gamut_metadata::Metadata) model: serializes it with the
    /// default [`MetadataEmbedder`](gamut_metadata::MetadataEmbedder) and routes each carrier to
    /// the matching setter — EXIF to [`with_exif`](Self::with_exif) (the facade's `Exif\0\0`
    /// signature stripped, since the `Exif` box carries the TIFF stream), XMP to
    /// [`with_xmp`](Self::with_xmp), and the ICC profile to [`with_color`](Self::with_color) as
    /// [`ColorSpec::Icc`] — in JPEG XL the profile *is* the codestream's colour encoding, not a
    /// container box, and it is validated against the image's colour family at encode time. Returns
    /// the updated encoder for chaining.
    ///
    /// Encoding then requires [`Container::IsoBmff`](crate::Container::IsoBmff) whenever an EXIF or
    /// XMP carrier was present, exactly as for the raw setters. The default embedder emits no legacy
    /// IPTC-IIM block and **drops** a C2PA manifest store (a store is signed over the file it came
    /// from; see [`gamut_metadata::C2paPolicy`]); a caller that must be told about either
    /// configures the embedder itself and calls
    /// [`with_encoded_metadata`](Self::with_encoded_metadata).
    ///
    /// # Precedence
    ///
    /// A carrier **absent** from the model leaves any earlier setting untouched. A carrier
    /// **present** in it overwrites one, because each is routed to the raw setter — and for ICC
    /// that setter is [`with_color`](Self::with_color), so a profile in the model replaces a
    /// [`ColorSpec`] chosen before this call, not merely an earlier profile. Call this before
    /// [`with_color`](Self::with_color) when the explicit colour choice is meant to win. Whether
    /// last-write-wins is the right rule for a colour encoding, or the conflict should be refused,
    /// is open (issue #626); this documents what it does today rather than settling it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] when the model does not serialize (the facade's message is
    /// carried as [`Error::detail`]), and whatever
    /// [`with_encoded_metadata`](Self::with_encoded_metadata) returns.
    #[cfg(feature = "metadata")]
    pub fn with_metadata(self, meta: &gamut_metadata::Metadata) -> Result<Self> {
        let encoded = gamut_metadata::MetadataEmbedder::new()
            .embed(meta)
            .map_err(|e| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "JXL: metadata does not serialize")
                    .with_detail(e.to_string())
            })?;
        self.with_encoded_metadata(&encoded)
    }

    /// Embeds already-serialized facade blocks — the output of a
    /// [`MetadataEmbedder`](gamut_metadata::MetadataEmbedder) configured by the caller — routing
    /// each present carrier as [`with_metadata`](Self::with_metadata) does. Returns the updated
    /// encoder for chaining.
    ///
    /// Only the carriers JPEG XL can write are accepted: EXIF (`Exif` box), XMP (`xml ` box) and
    /// ICC (the codestream colour encoding). Fields that are `None` leave any earlier setting
    /// untouched; a field that is `Some` overwrites one, and for ICC that means replacing the
    /// encoder's [`ColorSpec`] — see the precedence note on
    /// [`with_metadata`](Self::with_metadata).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] when the blocks carry a legacy IPTC-IIM stream (JPEG XL has
    /// no carrier for it) or a C2PA manifest store (the `jumb` box is not written by this crate,
    /// and a store must not be copied forward in any case), and [`Error::InvalidInput`] when the
    /// XMP packet is not UTF-8 (the `xml ` box holds text). Nothing is applied when an error is
    /// returned.
    #[cfg(feature = "metadata")]
    pub fn with_encoded_metadata(
        mut self,
        encoded: &gamut_metadata::EncodedMetadata,
    ) -> Result<Self> {
        if encoded.iptc_iim.is_some() {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "JXL: IPTC-IIM has no container carrier",
            ));
        }
        if encoded.c2pa.is_some() {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "JXL: C2PA manifest store (jumb box) embedding is not supported",
            ));
        }
        let xmp = encoded
            .xmp
            .as_deref()
            .map(std::str::from_utf8)
            .transpose()
            .map_err(|_| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "JXL: XMP packet is not UTF-8")
            })?;
        if let Some(exif) = &encoded.exif {
            self = self.with_exif(exif.strip_prefix(b"Exif\0\0").unwrap_or(exif));
        }
        if let Some(xmp) = xmp {
            self = self.with_xmp(xmp);
        }
        if let Some(icc) = &encoded.icc {
            self = self.with_color(ColorSpec::Icc(icc.clone()));
        }
        Ok(self)
    }

    /// Declares the samples' **coded bit depth** N, making a 16-bit pixel buffer carry N-bit code
    /// values (`0 ..= 2^N - 1`) instead of full-range 16-bit. Returns the updated encoder for
    /// chaining.
    ///
    /// The stream's header then declares N bits per sample and libjxl reads the buffer at that
    /// depth (`JxlEncoderSetFrameBitDepth`, from-codestream semantics) — the framing an N-bit raw
    /// consumer (e.g. a 10/12/14-bit DNG tile) round-trips exactly with
    /// [`JxlDecoder::with_codestream_bit_depth`](crate::JxlDecoder::with_codestream_bit_depth).
    /// `bits` must be `1..=16` and no wider than the pixel layout's sample width; encoding
    /// validates this with a typed error.
    #[must_use]
    pub fn with_bit_depth(mut self, bits: u8) -> Self {
        self.bit_depth = Some(bits);
        self
    }

    /// The declared coded bit depth override, if any (see [`JxlEncoder::with_bit_depth`]).
    #[must_use]
    pub fn bit_depth(&self) -> Option<u8> {
        self.bit_depth
    }

    /// Whether this encoder is in lossless mode.
    #[must_use]
    pub fn is_lossless(&self) -> bool {
        matches!(self.mode, Mode::Lossless)
    }

    /// The configured lossy [`Distance`], or `None` in lossless mode.
    #[must_use]
    pub fn distance(&self) -> Option<Distance> {
        match self.mode {
            Mode::Lossless => None,
            Mode::Lossy(d) => Some(d),
        }
    }

    /// The configured [`Effort`].
    #[must_use]
    pub fn effort(&self) -> Effort {
        self.effort
    }

    /// The configured [`ModularMode`].
    #[must_use]
    pub fn modular(&self) -> ModularMode {
        self.modular
    }

    /// The configured output [`Container`].
    #[must_use]
    pub fn container(&self) -> Container {
        self.container
    }

    /// The configured [`ColorSpec`].
    #[must_use]
    pub fn color(&self) -> &ColorSpec {
        &self.color
    }

    /// The configured [`Orientation`].
    #[must_use]
    pub fn orientation(&self) -> Orientation {
        self.orientation
    }

    /// The attached raw EXIF payload, if any.
    #[must_use]
    pub fn exif(&self) -> Option<&[u8]> {
        self.exif.as_deref()
    }

    /// The attached XMP packet bytes, if any.
    #[must_use]
    pub fn xmp(&self) -> Option<&[u8]> {
        self.xmp.as_deref()
    }

    /// The internal lossless/lossy mode, for the FFI driver.
    #[cfg(all(
        feature = "encode",
        any(not(target_arch = "wasm32"), target_os = "emscripten")
    ))]
    pub(crate) fn mode(&self) -> Mode {
        self.mode
    }

    /// Pushes a [`JxlCodestreamEncoder`] onto the end of this encoder's backend list, ahead of the
    /// built-in libjxl tail. Returns `&mut self` for chaining.
    ///
    /// Backends are tried in push order; the first whose
    /// [`supports`](JxlCodestreamEncoder::supports) returns `true` produces the codestream, and the
    /// built-in wrapper (when compiled in) is tried last. A backend that accepts a job and then
    /// fails propagates its error rather than falling through — see [`crate::backend`] for the full
    /// contract.
    ///
    /// Two things a pushed backend never sees, because the host vetoes them **before** consulting
    /// the registry: [`Container::IsoBmff`] output and
    /// [`with_exif`](JxlEncoder::with_exif)/[`with_xmp`](JxlEncoder::with_xmp) metadata (both
    /// container-level, outside the codestream seam), and
    /// [`recompress_jpeg`](JxlEncoder::recompress_jpeg) (jbrd reconstruction metadata, likewise a
    /// container box). Those requests always go to the built-in path.
    ///
    /// On `wasm32` targets without emscripten there is no built-in tail, so a pushed backend is the
    /// **only** way to encode; with neither, encoding reports
    /// [`Error::Unsupported`](gamut_core::Error::Unsupported).
    pub fn push_backend(&mut self, backend: impl JxlCodestreamEncoder + 'static) -> &mut Self {
        self.backends.push(Box::new(backend));
        self
    }

    /// Whether this configuration requests a **container-level** feature, which pins the encode to
    /// the built-in path and skips the backend registry entirely.
    ///
    /// The seam is the bare codestream, so ISO BMFF framing and the `Exif`/`xml ` metadata boxes
    /// have no representation a backend could honour; asking one anyway would either drop the
    /// request or produce a mis-framed stream.
    fn uses_container_features(&self) -> bool {
        self.container == Container::IsoBmff || self.exif.is_some() || self.xmp.is_some()
    }

    /// Builds the plain-data [`JxlEncodeRequest`] a backend sees for `image`.
    fn encode_request(&self, image: &JxlImageRef<'_>) -> Result<JxlEncodeRequest> {
        Ok(JxlEncodeRequest::new(
            self.distance(),
            self.effort,
            self.modular,
            resolve_coded_bits(self, image.bits_per_sample())?,
            self.color.clone(),
            self.orientation,
        ))
    }

    /// Rejects a configuration whose coding-tool request cannot be honoured.
    ///
    /// Only one combination is contradictory: a **lossless** encoder with
    /// [`ModularMode::VarDct`]. libjxl codes every lossless frame with Modular regardless of the
    /// frame setting, so honouring the request is impossible — and quietly emitting a Modular stream
    /// for a caller who asked for VarDCT is exactly the silent-drop this crate refuses elsewhere.
    ///
    /// [`ModularMode::Modular`] with lossless is *not* rejected: it agrees with what libjxl does.
    fn validate_modular(&self) -> Result<()> {
        if self.is_lossless() && self.modular == ModularMode::VarDct {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JXL: lossless encoding cannot force VarDCT mode",
            ));
        }
        Ok(())
    }

    /// Encodes `image` through the backend registry, falling back to the built-in tail.
    ///
    /// The registry is consulted only when a backend has actually been pushed *and* no
    /// container-level feature is requested, so a default encoder takes exactly the same code path —
    /// and produces exactly the same bytes — as before backends existed.
    fn dispatch_encode(&self, image: &JxlImageRef<'_>, out: &mut Vec<u8>) -> Result<usize> {
        // Ahead of the registry *and* the tail, so every target and every backend sees one refusal.
        self.validate_modular()?;
        if !self.uses_container_features() && !self.backends.is_empty() {
            let request = self.encode_request(image)?;
            let mut backends = self.backends.lock();
            for backend in backends.iter_mut() {
                if !backend.supports(&request) {
                    continue;
                }
                match backend.encode(&request, image) {
                    Ok(codestream) => {
                        out.extend_from_slice(&codestream);
                        return Ok(codestream.len());
                    }
                    // A late decline: fall through exactly as `supports() == false` would.
                    Err(error) if error.kind() == gamut_core::ErrorKind::Unsupported => continue,
                    // Terminal: the backend accepted the job and failed, so propagate.
                    Err(error) => return Err(error),
                }
            }
        }
        self.builtin_encode(image, out)
    }

    /// The built-in libjxl tail, or a typed refusal where it is not compiled in.
    fn builtin_encode(&self, image: &JxlImageRef<'_>, out: &mut Vec<u8>) -> Result<usize> {
        #[cfg(all(
            feature = "encode",
            any(not(target_arch = "wasm32"), target_os = "emscripten")
        ))]
        {
            crate::ffi::encode(self, image, out)
        }
        #[cfg(not(all(
            feature = "encode",
            any(not(target_arch = "wasm32"), target_os = "emscripten")
        )))]
        {
            let _ = (image, out);
            Err(no_encode_backend())
        }
    }

    /// Losslessly transcodes an existing JPEG bitstream into JPEG XL (the "jbrd" recompression
    /// path), appending to `out` and returning the number of bytes written.
    ///
    /// libjxl reversibly re-packs the JPEG's DCT coefficients and stores reconstruction metadata
    /// (the `jbrd` box), so the original `.jpg` can later be reconstructed bit-for-bit while the
    /// stream also decodes as ordinary JPEG XL pixels. Because that metadata lives in a container
    /// box, the output is **always ISO BMFF container framing** — the configured [`Container`] does
    /// not apply here. The transcode is inherently lossless, so the lossless/lossy mode and
    /// [`Distance`] do not apply either; only the configured [`Effort`] is honoured. [`ModularMode`]
    /// does not apply: the path re-packs the JPEG's own DCT coefficients, so there is no coding-tool
    /// choice to make, and a non-default setting is ignored rather than rejected. Metadata
    /// attached with [`JxlEncoder::with_exif`]/[`JxlEncoder::with_xmp`] is **not** applied on this
    /// path: libjxl already carries the JPEG's own EXIF/XMP into the container automatically, and
    /// duplicating those boxes would corrupt the reconstruction metadata's byte accounting.
    ///
    /// # Errors
    ///
    /// Returns [`gamut_core::Error::InvalidInput`] on an empty or malformed JPEG codestream, and
    /// [`gamut_core::Error::Unsupported`] if the JPEG uses features whose reconstruction metadata
    /// libjxl cannot represent — or if the built-in encoder is not compiled into this build, since
    /// jbrd recompression is **never** delegated to a pushed backend (the reconstruction metadata is
    /// a container box, outside the codestream seam).
    pub fn recompress_jpeg(&self, jpeg: &[u8], out: &mut Vec<u8>) -> Result<usize> {
        #[cfg(all(
            feature = "encode",
            any(not(target_arch = "wasm32"), target_os = "emscripten")
        ))]
        {
            crate::ffi::recompress(self, jpeg, out)
        }
        #[cfg(not(all(
            feature = "encode",
            any(not(target_arch = "wasm32"), target_os = "emscripten")
        )))]
        {
            let _ = (jpeg, out);
            Err(no_encode_backend())
        }
    }
}

/// Implements [`EncodeImage`] for each supported pixel layout by describing the raster as a
/// [`JxlImageRef`] and handing it to the backend dispatcher. The channel layout is derived from the
/// layout brand's [`Pixel::FORMAT`], so there is exactly one table of layout facts in the crate; the
/// macro only names the sample-storage variant.
macro_rules! impl_encode_image {
    ($($pixel:ty => $variant:ident;)*) => {$(
        impl EncodeImage<$pixel> for JxlEncoder {
            fn encode_image(&self, image: ImageRef<'_, $pixel>, out: &mut Vec<u8>) -> Result<usize> {
                let described = JxlImageRef::new(
                    <$pixel as Pixel>::FORMAT,
                    image.dimensions(),
                    JxlSamples::$variant(image.as_samples()),
                )?;
                self.dispatch_encode(&described, out)
            }
        }
    )*};
}

impl_encode_image! {
    Gray8       => U8;
    GrayAlpha8  => U8;
    Rgb8        => U8;
    Rgba8       => U8;
    Gray16      => U16;
    GrayAlpha16 => U16;
    Rgb16       => U16;
    Rgba16      => U16;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn default_and_new_are_lossless() {
        assert!(JxlEncoder::default().is_lossless());
        assert!(JxlEncoder::new().is_lossless());
        assert_eq!(JxlEncoder::new(), JxlEncoder::lossless());
        assert_eq!(JxlEncoder::default(), JxlEncoder::lossless());
        // A lossless encoder exposes no distance.
        assert_eq!(JxlEncoder::lossless().distance(), None);
    }

    #[test]
    fn defaults_are_squirrel_effort_auto_modular_and_codestream() {
        let enc = JxlEncoder::new();
        assert_eq!(enc.effort(), Effort::Squirrel);
        assert_eq!(enc.modular(), ModularMode::Auto);
        assert_eq!(enc.container(), Container::Codestream);
    }

    #[test]
    fn lossy_carries_its_distance_and_is_not_lossless() {
        let d = Distance::new(2.5).unwrap();
        let enc = JxlEncoder::lossy(d);
        assert!(!enc.is_lossless());
        assert_eq!(enc.distance(), Some(d));
    }

    #[test]
    fn builders_are_chainable_and_preserve_mode() {
        let enc = JxlEncoder::lossy(Distance::new(3.0).unwrap())
            .with_effort(Effort::Glacier)
            .with_container(Container::IsoBmff);
        assert_eq!(enc.effort(), Effort::Glacier);
        assert_eq!(enc.container(), Container::IsoBmff);
        // The builders touch only their own field, not the mode.
        assert!(!enc.is_lossless());
        assert_eq!(enc.distance(), Some(Distance::new(3.0).unwrap()));

        // with_effort on a lossless encoder keeps it lossless.
        let l = JxlEncoder::lossless().with_effort(Effort::Lightning);
        assert!(l.is_lossless());
        assert_eq!(l.effort(), Effort::Lightning);
    }

    #[test]
    fn with_modular_is_chainable_and_touches_only_its_own_field() {
        let enc = JxlEncoder::lossy(Distance::new(3.0).unwrap())
            .with_modular(ModularMode::Modular)
            .with_effort(Effort::Kitten);
        assert_eq!(enc.modular(), ModularMode::Modular);
        assert_eq!(enc.effort(), Effort::Kitten);
        assert!(!enc.is_lossless());
        assert_eq!(enc.distance(), Some(Distance::new(3.0).unwrap()));

        // Each variant survives the round trip through the builder, mode untouched.
        for modular in [ModularMode::Auto, ModularMode::VarDct, ModularMode::Modular] {
            let l = JxlEncoder::lossless().with_modular(modular);
            assert_eq!(l.modular(), modular);
            assert!(l.is_lossless());
        }

        // The knob participates in configuration equality.
        assert_ne!(
            JxlEncoder::new().with_modular(ModularMode::Modular),
            JxlEncoder::new()
        );
        assert_eq!(
            JxlEncoder::new().with_modular(ModularMode::Auto),
            JxlEncoder::new()
        );
    }

    #[test]
    fn lossless_rejects_forced_vardct_but_accepts_the_others() {
        let error = JxlEncoder::lossless()
            .with_modular(ModularMode::VarDct)
            .validate_modular()
            .unwrap_err();
        assert_eq!(error.kind(), gamut_core::ErrorKind::InvalidInput);
        assert_eq!(
            error.static_message(),
            Some("JXL: lossless encoding cannot force VarDCT mode")
        );

        // Modular agrees with what libjxl already does for lossless, and Auto is the default.
        assert!(
            JxlEncoder::lossless()
                .with_modular(ModularMode::Modular)
                .validate_modular()
                .is_ok()
        );
        assert!(JxlEncoder::lossless().validate_modular().is_ok());

        // Lossy accepts every coding tool, VarDCT included.
        for modular in [ModularMode::Auto, ModularMode::VarDct, ModularMode::Modular] {
            assert!(
                JxlEncoder::lossy(Distance::new(1.0).unwrap())
                    .with_modular(modular)
                    .validate_modular()
                    .is_ok(),
                "lossy + {modular:?} should be accepted"
            );
        }
    }

    #[cfg(all(
        feature = "encode",
        any(not(target_arch = "wasm32"), target_os = "emscripten")
    ))]
    #[test]
    fn recompress_jpeg_rejects_empty_input() {
        let mut out = Vec::new();
        let err = JxlEncoder::new()
            .recompress_jpeg(&[], &mut out)
            .unwrap_err();
        assert_eq!(err.kind(), gamut_core::ErrorKind::InvalidInput);
        assert_eq!(err.static_message(), Some("JXL: empty JPEG input"));
        // No output was produced on the rejected path.
        assert!(out.is_empty());
    }

    #[cfg(not(all(
        feature = "encode",
        any(not(target_arch = "wasm32"), target_os = "emscripten")
    )))]
    #[test]
    fn recompress_jpeg_without_the_builtin_is_unsupported() {
        // jbrd recompression is never delegated to a pushed backend, so with no built-in tail it is
        // unsupported regardless of the registry.
        let mut out = Vec::new();
        let mut enc = JxlEncoder::new();
        enc.push_backend(FixedBackend::new(vec![0xFF, 0x0A, 0x01]));
        assert!(matches!(
            enc.recompress_jpeg(&[0xFF, 0xD8], &mut out),
            Err(error) if error.kind() == gamut_core::ErrorKind::Unsupported
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn no_encode_backend_is_the_pinned_refusal() {
        let error = no_encode_backend();
        assert_eq!(error.kind(), gamut_core::ErrorKind::Unsupported);
        assert_eq!(
            error.static_message(),
            Some(
                "JXL: no encode backend (enable the `encode` feature or push a codestream backend)"
            )
        );
    }

    #[test]
    fn resolve_coded_bits_defaults_to_the_sample_width() {
        let enc = JxlEncoder::new();
        assert_eq!(resolve_coded_bits(&enc, 8).unwrap(), 8);
        assert_eq!(resolve_coded_bits(&enc, 16).unwrap(), 16);
    }

    #[test]
    fn resolve_coded_bits_honours_and_bounds_the_override() {
        assert_eq!(
            resolve_coded_bits(&JxlEncoder::new().with_bit_depth(10), 16).unwrap(),
            10
        );
        // The storage width itself is the inclusive upper bound; 1 is the inclusive lower bound.
        assert_eq!(
            resolve_coded_bits(&JxlEncoder::new().with_bit_depth(16), 16).unwrap(),
            16
        );
        assert_eq!(
            resolve_coded_bits(&JxlEncoder::new().with_bit_depth(1), 8).unwrap(),
            1
        );
        // Zero, and anything wider than the buffer, are rejected.
        let error = resolve_coded_bits(&JxlEncoder::new().with_bit_depth(0), 16).unwrap_err();
        assert_eq!(error.kind(), gamut_core::ErrorKind::InvalidInput);
        assert_eq!(
            error.static_message(),
            Some("JXL: coded bit depth must be 1..= the sample width")
        );
        assert!(resolve_coded_bits(&JxlEncoder::new().with_bit_depth(17), 16).is_err());
        assert!(resolve_coded_bits(&JxlEncoder::new().with_bit_depth(9), 8).is_err());
    }

    #[test]
    fn container_features_are_exactly_isobmff_exif_and_xmp() {
        assert!(!JxlEncoder::new().uses_container_features());
        assert!(
            !JxlEncoder::new()
                .with_container(Container::Codestream)
                .uses_container_features()
        );
        assert!(
            JxlEncoder::new()
                .with_container(Container::IsoBmff)
                .uses_container_features()
        );
        assert!(
            JxlEncoder::new()
                .with_exif(&[1, 2, 3])
                .uses_container_features()
        );
        assert!(JxlEncoder::new().with_xmp("<x/>").uses_container_features());
        // Codestream-level knobs do not trigger the veto.
        assert!(
            !JxlEncoder::new()
                .with_effort(Effort::Glacier)
                .with_modular(ModularMode::Modular)
                .with_bit_depth(10)
                .with_color(ColorSpec::Pq)
                .with_orientation(Orientation::Rotate180)
                .uses_container_features()
        );
    }

    #[test]
    fn encode_request_mirrors_the_configuration() {
        let enc = JxlEncoder::lossy(Distance::new(4.0).unwrap())
            .with_effort(Effort::Tortoise)
            .with_modular(ModularMode::Modular)
            .with_color(ColorSpec::Hlg)
            .with_orientation(Orientation::FlipVertical)
            .with_bit_depth(12);
        let pixels = [0u16; 4];
        let image = JxlImageRef::new(
            gamut_core::PixelFormat::Gray16,
            gamut_core::Dimensions::new(2, 2).unwrap(),
            JxlSamples::U16(&pixels),
        )
        .unwrap();
        let req = enc.encode_request(&image).unwrap();
        assert_eq!(req.distance(), Some(Distance::new(4.0).unwrap()));
        assert!(!req.is_lossless());
        assert_eq!(req.effort(), Effort::Tortoise);
        assert_eq!(req.modular(), ModularMode::Modular);
        assert_eq!(req.coded_bit_depth(), 12);
        assert_eq!(req.color(), &ColorSpec::Hlg);
        assert_eq!(req.orientation(), Orientation::FlipVertical);

        // A lossless encoder without an override reports no distance and the storage width.
        let req = JxlEncoder::lossless().encode_request(&image).unwrap();
        assert!(req.is_lossless());
        assert_eq!(req.distance(), None);
        assert_eq!(req.coded_bit_depth(), 16);
        assert_eq!(req.modular(), ModularMode::Auto);
    }

    #[test]
    fn encode_request_propagates_a_bad_bit_depth_override() {
        let pixels = [0u8; 4];
        let image = JxlImageRef::new(
            gamut_core::PixelFormat::Gray8,
            gamut_core::Dimensions::new(2, 2).unwrap(),
            JxlSamples::U8(&pixels),
        )
        .unwrap();
        assert!(
            JxlEncoder::new()
                .with_bit_depth(9)
                .encode_request(&image)
                .is_err()
        );
    }

    /// A backend that answers `supports` from a flag and `encode` with a canned outcome, recording
    /// how often each was called.
    struct FixedBackend {
        supported: bool,
        outcome: Result<Vec<u8>>,
        supports_calls: Arc<AtomicUsize>,
        encode_calls: Arc<AtomicUsize>,
    }

    impl FixedBackend {
        /// A backend that accepts everything and returns `bytes`.
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                supported: true,
                outcome: Ok(bytes),
                supports_calls: Arc::new(AtomicUsize::new(0)),
                encode_calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// A backend that declines at `supports` time.
        fn declining() -> Self {
            Self {
                supported: false,
                outcome: Ok(Vec::new()),
                supports_calls: Arc::new(AtomicUsize::new(0)),
                encode_calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// A backend that accepts and then returns `error`.
        fn failing(error: Error) -> Self {
            Self {
                supported: true,
                outcome: Err(error),
                supports_calls: Arc::new(AtomicUsize::new(0)),
                encode_calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// A shared handle to this backend's call counters, readable after it is pushed.
        fn counters(&self) -> (Arc<AtomicUsize>, Arc<AtomicUsize>) {
            (
                Arc::clone(&self.supports_calls),
                Arc::clone(&self.encode_calls),
            )
        }
    }

    impl JxlCodestreamEncoder for FixedBackend {
        fn supports(&mut self, _req: &JxlEncodeRequest) -> bool {
            self.supports_calls.fetch_add(1, Ordering::SeqCst);
            self.supported
        }

        fn encode(&mut self, _req: &JxlEncodeRequest, _image: &JxlImageRef<'_>) -> Result<Vec<u8>> {
            self.encode_calls.fetch_add(1, Ordering::SeqCst);
            match &self.outcome {
                Ok(bytes) => Ok(bytes.clone()),
                Err(error) if error.kind() == gamut_core::ErrorKind::Unsupported => {
                    Err(Error::unsupported(
                        env!("CARGO_PKG_NAME"),
                        error
                            .static_message()
                            .unwrap_or("JXL: test backend refusal"),
                    ))
                }
                Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput => {
                    Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        error
                            .static_message()
                            .unwrap_or("JXL: test backend failure"),
                    ))
                }
                Err(_) => Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JXL: test backend failure",
                )),
            }
        }
    }

    /// A 2×2 Gray8 raster, and its `JxlImageRef`.
    const GRAY_2X2: [u8; 4] = [1, 2, 3, 4];

    /// Describes [`GRAY_2X2`] for the dispatcher.
    fn gray_image() -> JxlImageRef<'static> {
        JxlImageRef::new(
            gamut_core::PixelFormat::Gray8,
            gamut_core::Dimensions::new(2, 2).unwrap(),
            JxlSamples::U8(&GRAY_2X2),
        )
        .expect("valid raster")
    }

    #[test]
    fn first_supporting_backend_wins_and_later_ones_are_untouched() {
        let first = FixedBackend::new(vec![0xFF, 0x0A, 0x11]);
        let second = FixedBackend::new(vec![0xFF, 0x0A, 0x22]);
        let (_, first_encodes) = first.counters();
        let (second_supports, second_encodes) = second.counters();

        let mut enc = JxlEncoder::new();
        enc.push_backend(first).push_backend(second);

        let mut out = vec![0xEE];
        let written = enc.dispatch_encode(&gray_image(), &mut out).unwrap();
        assert_eq!(written, 3);
        // The output is appended after the caller's existing bytes.
        assert_eq!(out, vec![0xEE, 0xFF, 0x0A, 0x11]);
        assert_eq!(first_encodes.load(Ordering::SeqCst), 1);
        // The second backend was never consulted at all.
        assert_eq!(second_supports.load(Ordering::SeqCst), 0);
        assert_eq!(second_encodes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_declining_backend_is_skipped_in_favour_of_the_next() {
        let first = FixedBackend::declining();
        let second = FixedBackend::new(vec![0xFF, 0x0A, 0x22]);
        let (first_supports, first_encodes) = first.counters();
        let (second_supports, second_encodes) = second.counters();

        let mut enc = JxlEncoder::new();
        enc.push_backend(first).push_backend(second);

        let mut out = Vec::new();
        assert_eq!(enc.dispatch_encode(&gray_image(), &mut out).unwrap(), 3);
        assert_eq!(out, vec![0xFF, 0x0A, 0x22]);
        assert_eq!(first_supports.load(Ordering::SeqCst), 1);
        // Declining at `supports` means `encode` is never called on it.
        assert_eq!(first_encodes.load(Ordering::SeqCst), 0);
        assert_eq!(second_supports.load(Ordering::SeqCst), 1);
        assert_eq!(second_encodes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_late_unsupported_falls_through_to_the_next_backend() {
        let first = FixedBackend::failing(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "backend changed its mind",
        ));
        let second = FixedBackend::new(vec![0xFF, 0x0A, 0x33]);
        let (_, first_encodes) = first.counters();
        let (_, second_encodes) = second.counters();

        let mut enc = JxlEncoder::new();
        enc.push_backend(first).push_backend(second);

        let mut out = Vec::new();
        assert_eq!(enc.dispatch_encode(&gray_image(), &mut out).unwrap(), 3);
        assert_eq!(out, vec![0xFF, 0x0A, 0x33]);
        assert_eq!(first_encodes.load(Ordering::SeqCst), 1);
        assert_eq!(second_encodes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_accepted_then_failed_backend_propagates_and_stops_the_chain() {
        let first = FixedBackend::failing(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JXL: test backend failure",
        ));
        let second = FixedBackend::new(vec![0xFF, 0x0A, 0x44]);
        let (_, first_encodes) = first.counters();
        let (second_supports, second_encodes) = second.counters();

        let mut enc = JxlEncoder::new();
        enc.push_backend(first).push_backend(second);

        let mut out = Vec::new();
        let error = enc.dispatch_encode(&gray_image(), &mut out).unwrap_err();
        assert_eq!(error.kind(), gamut_core::ErrorKind::InvalidInput);
        assert_eq!(error.static_message(), Some("JXL: test backend failure"));
        assert_eq!(first_encodes.load(Ordering::SeqCst), 1);
        // No later backend — and no built-in tail — was reached.
        assert_eq!(second_supports.load(Ordering::SeqCst), 0);
        assert_eq!(second_encodes.load(Ordering::SeqCst), 0);
        assert!(out.is_empty());
    }

    #[test]
    fn container_features_skip_the_registry_entirely() {
        // Each container-level request must bypass `supports` altogether, not merely ignore the
        // backend's answer.
        for enc in [
            JxlEncoder::new().with_container(Container::IsoBmff),
            JxlEncoder::new()
                .with_container(Container::IsoBmff)
                .with_exif(&[1, 2, 3]),
            JxlEncoder::new()
                .with_container(Container::IsoBmff)
                .with_xmp("<x/>"),
            // Metadata without the container is a typed error from the built-in path — but the
            // registry must still be skipped, so the error is the built-in's, not a backend's.
            JxlEncoder::new().with_exif(&[1, 2, 3]),
        ] {
            let backend = FixedBackend::new(vec![0xFF, 0x0A, 0x55]);
            let (supports, encodes) = backend.counters();
            let mut enc = enc;
            enc.push_backend(backend);

            let mut out = Vec::new();
            let _ = enc.dispatch_encode(&gray_image(), &mut out);
            assert_eq!(
                supports.load(Ordering::SeqCst),
                0,
                "a container request must not consult the registry"
            );
            assert_eq!(encodes.load(Ordering::SeqCst), 0);
            // Whatever the built-in did, the backend's bytes are certainly not the output.
            assert_ne!(out, vec![0xFF, 0x0A, 0x55]);
        }
    }

    #[test]
    fn all_backends_declining_falls_through_to_the_builtin_tail() {
        let mut enc = JxlEncoder::new();
        let first = FixedBackend::declining();
        let second =
            FixedBackend::failing(Error::unsupported(env!("CARGO_PKG_NAME"), "late decline"));
        let (first_supports, _) = first.counters();
        let (_, second_encodes) = second.counters();
        enc.push_backend(first).push_backend(second);

        let mut out = Vec::new();
        let result = enc.dispatch_encode(&gray_image(), &mut out);
        assert_eq!(first_supports.load(Ordering::SeqCst), 1);
        assert_eq!(second_encodes.load(Ordering::SeqCst), 1);

        if cfg!(all(
            feature = "encode",
            any(not(target_arch = "wasm32"), target_os = "emscripten")
        )) {
            // The libjxl tail encoded it: a real bare codestream, not the backends' bytes.
            let written = result.expect("the built-in tail encodes");
            assert_eq!(written, out.len());
            assert_eq!(&out[..2], &[0xFF, 0x0A]);
        } else {
            // No tail compiled in: the direction is unsupported.
            assert!(matches!(
                result,
                Err(error) if error.kind() == gamut_core::ErrorKind::Unsupported
            ));
            assert!(out.is_empty());
        }
    }

    #[test]
    fn with_no_backend_and_no_builtin_encoding_is_unsupported() {
        // The wasm32 story, asserted directly on the dispatcher so it holds on every host: an empty
        // registry falls to the tail, and the tail's absence is a typed refusal.
        let enc = JxlEncoder::new();
        assert!(enc.backends.is_empty());
        let mut out = Vec::new();
        let result = enc.dispatch_encode(&gray_image(), &mut out);
        assert_eq!(
            result.is_ok(),
            cfg!(all(
                feature = "encode",
                any(not(target_arch = "wasm32"), target_os = "emscripten")
            ))
        );
        if let Err(error) = result {
            assert_eq!(error.kind(), gamut_core::ErrorKind::Unsupported);
        }
    }

    #[test]
    fn a_pushed_backend_supplies_encode_without_any_builtin() {
        // The other half of the wasm32 story: whatever the build, a pushed backend serves the
        // encode, so a target with no libjxl tail still encodes.
        let mut enc = JxlEncoder::new();
        enc.push_backend(FixedBackend::new(vec![0xFF, 0x0A, 0x66]));
        let mut out = Vec::new();
        assert_eq!(enc.dispatch_encode(&gray_image(), &mut out).unwrap(), 3);
        assert_eq!(out, vec![0xFF, 0x0A, 0x66]);
    }

    #[test]
    fn a_clone_shares_the_original_s_backend_registry() {
        // The registry is shared, not copied: a backend pushed after cloning is visible through
        // the clone. A deep copy here would silently give a cloned encoder a stale backend list.
        let mut enc = JxlEncoder::new();
        let clone = enc.clone();

        enc.push_backend(FixedBackend::new(vec![0xFF, 0x0A, 0x77]));

        assert!(!clone.backends.is_empty());
    }

    #[test]
    fn equality_compares_configuration_and_ignores_backends() {
        // Independent of the sharing above: `PartialEq` could compare backends while the registry
        // is shared, or ignore them while it is copied. An encoder with a backend pushed must
        // still equal a fresh one, because only configuration is compared.
        let mut enc = JxlEncoder::new();
        enc.push_backend(FixedBackend::new(vec![0xFF, 0x0A, 0x77]));

        assert_eq!(enc, JxlEncoder::new());
        // ...but a configuration difference is still a difference, or the comparison would be
        // vacuous.
        assert_ne!(enc, JxlEncoder::new().with_effort(Effort::Glacier));
    }

    #[test]
    fn debug_includes_the_backend_count() {
        let mut enc = JxlEncoder::new();
        assert!(format!("{enc:?}").contains("backends: 0"));
        enc.push_backend(FixedBackend::new(Vec::new()));
        assert!(format!("{enc:?}").contains("backends: 1"));
    }
}
