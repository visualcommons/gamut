//! The AV1 **still-encode** backend seam: the typed [`Av1StillEncoder`] trait a caller plugs an
//! alternate AV1 encoder into, the [`Av1EncodeRequest`] describing one encode job, and the
//! [`AbiAv1StillEncoder`] adapter that bridges a [`gamut_codec_abi::Encoder`] (and, through
//! [`gamut_codec_abi::bridge::ForeignEncoder`], a C/`-sys` backend) onto it.
//!
//! This is the write-direction mirror of the [`Av1StillDecoder`](crate::Av1StillDecoder) seam:
//! `gamut-avif` owns the container and everything around the coded picture, while the AV1
//! codestream itself may come from a platform or reference encoder (libaom, SVT-AV1, a hardware
//! encoder, …). The shape and the fallback contract are the workspace-wide ones defined by
//! [`gamut_codec_abi`] (issue #241/#272):
//!
//! - Backends are tried in **push order** ([`AvifEncoder::push_backend`](crate::AvifEncoder::push_backend)).
//! - [`Av1StillEncoder::supports`] returning `false` — or, across the C seam,
//!   [`Status::UNSUPPORTED`](gamut_codec_abi::Status::UNSUPPORTED) — is the **only** fall-through
//!   signal.
//! - `gamut-av1`'s [`encode_still_intra_with`](gamut_av1::encode_still_intra_with) is the
//!   **implicit tail**,
//!   used when every pushed backend declines. `gamut-av1` itself is unaware of this seam.
//! - A backend that *accepts* a job and then fails propagates its error; the tail is **not** retried,
//!   because silently substituting a different encoder would make the output non-deterministic.
//!
//! # The `av1C` record for a backend-supplied stream
//!
//! The container's `av1C`/`colr` boxes must mirror the sequence header the item payload actually
//! carries (AV1-ISOBMFF v1.3.0 §2.3.4). For the built-in tail those values come back from
//! `gamut-av1`; for a backend they are recovered from the returned OBUs themselves —
//! `seq_profile`, `seq_level_idx[0]`, the coded dimensions and `color_config()` are read from the
//! sequence header, the colour configuration is checked against the one the request asked for (so
//! the `colr` box can never disagree with the payload), and the stream is then checked against the
//! AVIF still-image item constraints
//! ([`Av1Config::validate_still_payload`](crate::Av1Config::validate_still_payload)). The rest of
//! the pixel parameters (8-bit, 4:4:4) are the v1 surface's fixed contract, stated on
//! [`Av1StillEncoder::encode_still`] and enforced by the `seq_profile` check.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use gamut_av1::{Av1Colour, Av1StillConfig, EncodedStill, SequenceHeader, Speed, Subsampling};
use gamut_codec_abi::{EncodeConfig, Encoder, ImageDesc, Status};
use gamut_color::{
    BitDepth, ChromaSubsampling, ColorRange, ColourPrimaries, MatrixCoefficients, Planar8,
    Planar16, TransferCharacteristics,
};
use gamut_core::{Dimensions, Error, PixelFormat, Result};

use crate::av1c::{Av1Config, first_sequence_header};

/// The `codec_id` `gamut-avif` stamps into a [`EncodeConfig`] when it drives a
/// [`gamut_codec_abi`] backend: the big-endian `av01` FourCC — the same four bytes AV1-ISOBMFF
/// v1.3.0 §2.2 assigns the AV1 image item type — so a backend can dispatch on the codec without a
/// gamut-specific table.
pub const AV1_CODEC_ID: u32 = u32::from_be_bytes(*b"av01");

/// One AV1 still-encode job, as handed to an [`Av1StillEncoder`].
///
/// Constructed by `gamut-avif` (never by a caller) and read through its getters, so the crate can
/// grow the request without a breaking change. It carries the **already-derived**
/// [`base_q_idx`](Self::base_q_idx): the `quality → base_q_idx` mapping is part of the frozen v1
/// guarantee (see `STATUS.md`) and lives inside the encoder, so a backend never sees — and can
/// never reinterpret — the `0..=100` quality scale.
///
/// `#[non_exhaustive]`, so the deferred pixel-format work (M2: 10/12-bit, 4:2:0/4:2:2, limited
/// range) extends it additively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Av1EncodeRequest {
    /// Display dimensions of the image to encode.
    dimensions: Dimensions,
    /// The AV1 `base_q_idx` (AV1 §5.9.12), `0..=255`; `0` selects the lossless path.
    base_q_idx: u8,
    /// The colour signalling the returned stream must carry — and the layout `planes` is in.
    colour: Av1Colour,
    /// The chroma sampling of `planes`, which the returned stream must also code.
    chroma: ChromaSubsampling,
    /// The depth the samples are coded at: 8, 10 or 12 bits.
    bit_depth: BitDepth,
    /// The search preset the caller chose with
    /// [`AvifEncoder::with_speed`](crate::AvifEncoder::with_speed).
    speed: Speed,
    /// The share of [`AvifEncoder::with_threads`](crate::AvifEncoder::with_threads)'s budget this
    /// encode may use.
    threads: NonZeroUsize,
}

impl Av1EncodeRequest {
    /// Builds a request. Crate-internal: the `base_q_idx` must already have been derived through
    /// the encoder's frozen quality mapping.
    pub(crate) fn new(
        dimensions: Dimensions,
        base_q_idx: u8,
        colour: Av1Colour,
        chroma: ChromaSubsampling,
        bit_depth: BitDepth,
        speed: Speed,
        threads: NonZeroUsize,
    ) -> Self {
        Self {
            chroma,
            dimensions,
            base_q_idx,
            colour,
            bit_depth,
            speed,
            threads,
        }
    }

    /// The most threads this encode may use: the share of
    /// [`AvifEncoder::with_threads`](crate::AvifEncoder::with_threads)'s budget the colour item
    /// gets — all of it for an image without alpha, and the larger half when the alpha auxiliary
    /// is coded beside it on another thread. One unless the caller asked for more.
    ///
    /// Like [`speed`](Self::speed) this is a **hint**: a backend may run on fewer threads or ignore
    /// it. The built-in encoder's output does not depend on it; whether a backend's does is the
    /// backend's own contract. It does not cross the C seam — [`AbiAv1StillEncoder`]'s `extra`
    /// blob is unchanged — since a C backend schedules its own threads.
    #[must_use]
    pub fn threads(&self) -> NonZeroUsize {
        self.threads
    }

    /// The encode-speed preset the caller chose with
    /// [`AvifEncoder::with_speed`](crate::AvifEncoder::with_speed) — [`Speed::default`]
    /// ([`Speed::Balanced`]) unless they chose another.
    ///
    /// Unlike [`colour`](Self::colour) and [`chroma`](Self::chroma) this is a **hint**, not an
    /// obligation: it names how much search time the caller is willing to spend, and a backend
    /// maps it onto its own effort scale or ignores it. Nothing in the returned stream is checked
    /// against it.
    #[must_use]
    pub fn speed(&self) -> Speed {
        self.speed
    }

    /// The display dimensions of the image to encode.
    #[must_use]
    pub fn dimensions(&self) -> Dimensions {
        self.dimensions
    }

    /// The display width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.dimensions.width
    }

    /// The display height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.dimensions.height
    }

    /// The AV1 `base_q_idx` (AV1 §5.9.12) to encode at, `0..=255` — lower is finer. This is the
    /// authoritative quantizer for the job; it is already derived from the encoder's quality
    /// setting, which backends do not see.
    #[must_use]
    pub fn base_q_idx(&self) -> u8 {
        self.base_q_idx
    }

    /// Whether this is a **lossless** encode — i.e. [`base_q_idx`](Self::base_q_idx) is `0`, the
    /// AV1 lossless path, for which the decoded output must be bit-exact to the input.
    #[must_use]
    pub fn is_lossless(&self) -> bool {
        self.base_q_idx == 0
    }

    /// The colour encoding the job is in: the CICP triple plus the signal range.
    ///
    /// This is **both** an input and an obligation. It says what the `planes` handed to
    /// [`Av1StillEncoder::encode_still`] contain — GBR for
    /// [`MatrixCoefficients::Identity`], `Y/Cb/Cr`
    /// otherwise — and the returned stream's `color_config()` must declare exactly it, because the
    /// container mirrors these values into `colr` and they must agree with the payload
    /// (AV1-ISOBMFF v1.3.0 §2.3.4). A mismatch is rejected.
    #[must_use]
    pub fn colour(&self) -> Av1Colour {
        self.colour
    }

    /// The chroma sampling of the planes passed to [`Av1StillEncoder::encode_still`].
    ///
    /// Like [`colour`](Self::colour) this is both an input and an obligation: the planes are in
    /// this layout, and the returned stream's `seq_profile` must declare it, because `av1C` mirrors
    /// the sequence header and the two must agree (AV1-ISOBMFF v1.3.0 §2.3.4). A mismatch is
    /// rejected.
    #[must_use]
    pub fn chroma(&self) -> ChromaSubsampling {
        self.chroma
    }

    /// The depth the samples are coded at.
    ///
    /// The same obligation as [`chroma`](Self::chroma), and checked the same way: `seq_profile`
    /// and the `high_bitdepth`/`twelve_bit` pair must declare this depth, because `av1C` mirrors
    /// them.
    ///
    /// [`BitDepth::Eight`] is the only depth [`Av1StillEncoder::encode_still`] is ever handed;
    /// [`BitDepth::Ten`] and [`BitDepth::Twelve`] arrive through
    /// [`encode_still16`](Av1StillEncoder::encode_still16), whose default implementation declines.
    #[must_use]
    pub fn bit_depth(&self) -> BitDepth {
        self.bit_depth
    }
}

/// A pluggable AV1 **still-image encoder** backend.
///
/// Implement this to route [`AvifEncoder`](crate::AvifEncoder)'s codestream through an alternate
/// AV1 encoder (a platform/hardware encoder, libaom, SVT-AV1, …) and register it with
/// [`AvifEncoder::push_backend`](crate::AvifEncoder::push_backend). A C or `-sys` backend reaches
/// this trait through [`AbiAv1StillEncoder`] rather than implementing it directly.
///
/// `Send` is a supertrait because the registry stores backends behind an
/// [`Arc`]`<`[`Mutex`]`<…>>` so [`AvifEncoder`](crate::AvifEncoder) stays [`Clone`] and encodes
/// through `&self`.
pub trait Av1StillEncoder: Send {
    /// Reports whether this backend can satisfy `req`. Returning `false` is the **only** signal
    /// that lets the encoder fall through to the next backend (and finally to the built-in
    /// `gamut-av1` tail); a backend that returns `true` owns the job outright.
    fn supports(&mut self, req: &Av1EncodeRequest) -> bool;

    /// Encodes `planes` and returns the **AV1 OBU byte stream** for the `av01` item payload — the
    /// low-overhead OBU syntax of AV1 §5.3, with no temporal delimiter, exactly as
    /// [`gamut_av1::EncodedStill::obus`] carries it.
    ///
    /// `planes` are `gamut-color` planes, each `width * height` 8-bit samples, in the layout
    /// [`Av1EncodeRequest::colour`] describes: identity GBR (`Y = G`, `U = B`, `V = R`) or
    /// `Y/Cb/Cr` through that matrix. The v1 surface fixes the rest of the coding parameters: the
    /// returned stream must be a **still picture** with `seq_profile = 1` (High), 8-bit, 4:4:4,
    /// whose sequence header carries `reduced_still_picture_header = 1`, the request's dimensions,
    /// and a `color_config()` matching the request's colour. The crate re-derives the
    /// `av1C`/`colr` boxes from that sequence header and rejects a stream that does not meet the
    /// contract.
    ///
    /// # Errors
    ///
    /// Any error a backend returns after accepting the job **propagates** to the caller of
    /// [`encode_image`](gamut_core::EncodeImage::encode_image); the built-in encoder is not used
    /// as a silent fallback. Decline the job from [`supports`](Self::supports) instead.
    fn encode_still(&mut self, req: &Av1EncodeRequest, planes: &Planar8) -> Result<Vec<u8>>;

    /// Encodes **10- or 12-bit** planes, returning the AV1 OBU byte stream exactly as
    /// [`encode_still`](Self::encode_still) does for 8-bit.
    ///
    /// The default implementation **declines**, and the host falls through to the next backend and
    /// finally to the built-in `gamut-av1` tail — so a backend written against the 8-bit v1
    /// contract keeps compiling *and* keeps its meaning: it is never handed samples it did not
    /// agree to encode. Override it to opt into high bit depth.
    ///
    /// Declining here is the one late fall-through the seam allows, and it exists because
    /// [`supports`](Self::supports) is a single answer for a job whose depth a pre-existing
    /// implementation does not inspect. Every *other* failure after accepting a job still
    /// propagates.
    ///
    /// The returned stream's `seq_profile` must match the depth §6.4.1 requires — 1 (or 0
    /// monochrome) at 10 bits, 2 at 12 — and its `color_config()` must declare
    /// [`Av1EncodeRequest::bit_depth`] and [`Av1EncodeRequest::colour`]; the crate re-derives the
    /// `av1C`/`colr` boxes from it and rejects a disagreement.
    ///
    /// # Errors
    ///
    /// As [`encode_still`](Self::encode_still).
    fn encode_still16(&mut self, req: &Av1EncodeRequest, planes: &Planar16) -> Result<Vec<u8>> {
        let _ = (req, planes);
        Err(LateDecline::error())
    }
}

/// The registry entry type: a shared, interior-mutable backend.
///
/// [`Arc`] so cloning an [`AvifEncoder`](crate::AvifEncoder) **shares** backends rather than
/// duplicating (a backend is typically a stateful, non-`Clone` encoder handle), and [`Mutex`] so
/// `&self` encoding can call the `&mut self` trait methods.
pub(crate) type BackendSlot = Arc<Mutex<dyn Av1StillEncoder + Send>>;

/// The planes one registry run offers a backend, at whichever width the job actually has.
pub(crate) enum BackendPlanes<'a> {
    /// 8-bit planes, offered through [`Av1StillEncoder::encode_still`].
    Eight(&'a Planar8),
    /// 10/12-bit planes, offered through [`Av1StillEncoder::encode_still16`].
    High(&'a Planar16),
}

/// Runs the registry for one request: tries `backends` in push order, returning the first
/// acceptance's OBUs, or `None` when every backend declines (the caller then uses the built-in
/// `gamut-av1` tail).
pub(crate) fn run_backends(
    backends: &[BackendSlot],
    req: &Av1EncodeRequest,
    planes: BackendPlanes<'_>,
) -> Result<Option<Vec<u8>>> {
    for slot in backends {
        let mut backend = slot.lock().map_err(|_| {
            Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "AVIF: AV1 encode backend is poisoned",
            )
        })?;
        if backend.supports(req) {
            // Accepted: this backend owns the job, so its error propagates — falling back to a
            // different encoder here would silently change the output bytes. The sole exception is
            // the ABI adapter's late-`UNSUPPORTED` sentinel, which *is* a decline.
            let encoded = match planes {
                BackendPlanes::Eight(p) => backend.encode_still(req, p),
                BackendPlanes::High(p) => backend.encode_still16(req, p),
            };
            return match encoded {
                Err(e) if LateDecline::is(&e) => continue,
                other => other.map(Some),
            };
        }
    }
    Ok(None)
}

/// Rebuilds the [`EncodedStill`] — OBUs plus the sequence-header values the container stamps into
/// `av1C`/`colr` — for a stream a backend produced, validating it against the v1 contract and the
/// AVIF still-image item constraints.
///
/// # Errors
///
/// - [`Error::InvalidInput`] if the stream has no sequence header `gamut-av1` can parse, or if its
///   coded dimensions disagree with `dims` (which the container's `ispe` states).
/// - [`Error::Unsupported`] if the sequence header is not `reduced_still_picture_header = 1`, is
///   monochrome, or leaves its colour undescribed.
/// - [`Error::InvalidInput`] if the stream's coded depth is not the one the request asked for —
///   the container's `av1C` and `pixi` would otherwise lie about the payload.
/// - Whatever [`Av1Config::validate_still_payload`] reports for a non-conformant item payload.
pub(crate) fn still_from_backend_obus(
    obus: Vec<u8>,
    dims: Dimensions,
    colour: Av1Colour,
    chroma: ChromaSubsampling,
    bit_depth: BitDepth,
) -> Result<EncodedStill> {
    let header = SeqHeaderParams::parse(&obus)?;
    if header.bit_depth != bit_depth {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "AVIF: AV1 backend stream is coded at a different bit depth than requested",
        ));
    }
    if (header.width, header.height) != (dims.width, dims.height) {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "AVIF: AV1 backend stream dimensions differ from the image",
        ));
    }
    // The container mirrors these into `colr`, so they must be what the payload actually declares
    // — not what the request asked for. Read them back and reject a disagreement rather than stamp
    // a `colr` box that lies about the samples (AV1-ISOBMFF v1.3.0 §2.3.4).
    let requested = (
        colour.primaries.code_point(),
        colour.transfer.code_point(),
        colour.matrix.code_point(),
        matches!(colour.range, ColorRange::Full),
    );
    if header.colour != requested {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "AVIF: AV1 backend stream signals a different colour configuration than requested",
        ));
    }
    if header.chroma != chroma {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "AVIF: AV1 backend stream signals a different chroma format than requested",
        ));
    }
    let (color_primaries, transfer_characteristics, matrix_coefficients, full_range) =
        header.colour;
    let (chroma_x, chroma_y) = chroma.subsampling();
    let config = Av1StillConfig {
        seq_profile: header.seq_profile,
        seq_level_idx_0: header.seq_level_idx_0,
        seq_tier_0: 0,
        high_bitdepth: bit_depth != BitDepth::Eight,
        twelve_bit: bit_depth == BitDepth::Twelve,
        monochrome: false,
        chroma_subsampling_x: chroma_x,
        chroma_subsampling_y: chroma_y,
        // Mirrored, not required to match the request: §2.3.4 obliges `av1C` to agree with the
        // sequence header, and a backend whose downsampler is co-sited may legitimately signal a
        // position of its own.
        chroma_sample_position: header.chroma_sample_position,
        color_primaries,
        transfer_characteristics,
        matrix_coefficients,
        full_range,
    };
    // Re-read the record we are about to stamp and validate the payload through the crate's own
    // reader, so a backend stream faces exactly the checks a third-party AVIF file would.
    Av1Config::parse(&crate::encoder::av1c_record(&config))?.validate_still_payload(&obus)?;
    Ok(EncodedStill { obus, config })
}

/// The sequence-header fields the container needs from a backend-supplied stream.
struct SeqHeaderParams {
    /// `seq_profile` (3 bits).
    seq_profile: u8,
    /// `seq_level_idx[0]` (5 bits).
    seq_level_idx_0: u8,
    /// `max_frame_width_minus_1 + 1`.
    width: u32,
    /// `max_frame_height_minus_1 + 1`.
    height: u32,
    /// `color_config()`: `(color_primaries, transfer_characteristics, matrix_coefficients,
    /// color_range == full)`.
    colour: (u16, u16, u16, bool),
    /// Chroma sampling — inferred from `seq_profile`, except at 12-bit where §5.5.2 codes it.
    chroma: ChromaSubsampling,
    /// `chroma_sample_position`, coded for 4:2:0 only.
    chroma_sample_position: u8,
    /// `BitDepth`, as §5.5.2 derives it from `high_bitdepth`, `twelve_bit` and `seq_profile`.
    bit_depth: BitDepth,
}

impl SeqHeaderParams {
    /// Reads the first sequence header OBU in `payload` through `gamut-av1`'s
    /// [`SequenceHeader::parse`] — the workspace's one AV1 §5.5 parser, the same one
    /// [`Av1Config::sequence_header`] reads a decoded item through, so the encode and decode paths
    /// cannot disagree about a field — and projects it onto what the container mirrors.
    ///
    /// The projection refuses the headers this surface cannot describe: anything but
    /// `reduced_still_picture_header = 1` (the shape AVIF still images use), a monochrome stream
    /// (the item being built is the three-plane colour item), undescribed colour, and the sRGB
    /// shortcut on a profile that fixes subsampled chroma.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidInput`] if the stream has no sequence header OBU, does not split into
    ///   OBUs, or carries a header [`SequenceHeader::parse`] rejects (truncated, a reserved
    ///   `seq_profile`, or not ending in the §5.3.1 `trailing_bits()` its size declares) — the
    ///   parser's own error is kept as the detail.
    /// - [`Error::Unsupported`] for the first three refusals above.
    /// - [`Error::InvalidInput`] for the shortcut/profile contradiction.
    fn parse(payload: &[u8]) -> Result<Self> {
        let seq = first_sequence_header(payload)?.ok_or_else(|| {
            Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "AVIF: AV1 backend stream has no sequence header OBU",
            )
        })?;
        // Wrapped rather than propagated, so every refusal of a backend stream stays this crate's
        // own error with a stable message, whichever layer found the defect.
        let header = SequenceHeader::parse(seq).map_err(|e| {
            Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "AVIF: AV1 backend sequence header is malformed",
            )
            .with_detail(e.to_string())
        })?;
        if !header.reduced_still_picture_header {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "AVIF: AV1 backend stream must set reduced_still_picture_header",
            ));
        }
        let color = header.color;
        let chroma = match color.subsampling {
            Subsampling::Monochrome => {
                return Err(Error::unsupported(
                    env!("CARGO_PKG_NAME"),
                    "AVIF: AV1 backend stream must be three-plane (mono_chrome = 0)",
                ));
            }
            Subsampling::Yuv444 => ChromaSubsampling::Cs444,
            Subsampling::Yuv422 => ChromaSubsampling::Cs422,
            Subsampling::Yuv420 => ChromaSubsampling::Cs420,
        };
        let colour = (
            u16::from(color.color_primaries),
            u16::from(color.transfer_characteristics),
            u16::from(color.matrix_coefficients),
            color.full_range,
        );
        // `color_description_present_flag = 0` leaves all three code points UNSPECIFIED, which the
        // container would have to stamp verbatim into `colr` — and which can never match the
        // concrete triple a three-plane request carries (the encoder refuses an unspecified
        // matrix). Reject it here, with a message that says why, rather than let it fall out of
        // the colour comparison as a confusing mismatch.
        //
        // The parsed header does not keep the flag, and does not need to: §5.5.2 gives an absent
        // description exactly the semantics of coding CP/TC/MC_UNSPECIFIED, so a stream that
        // codes that triple explicitly says the same thing and is refused the same way.
        if (colour.0, colour.1, colour.2)
            == (
                ColourPrimaries::Unspecified.code_point(),
                TransferCharacteristics::Unspecified.code_point(),
                MatrixCoefficients::Unspecified.code_point(),
            )
        {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "AVIF: AV1 backend stream must set color_description_present_flag",
            ));
        }
        // `gamut-av1` derives `BitDepth` as 8, 10 or 12 (§5.5.2), so the `None` arm is
        // unreachable; it is an error rather than a panic because library code may not unwrap.
        let bit_depth = BitDepth::from_bits(color.bit_depth).ok_or_else(|| {
            Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "AVIF: AV1 backend sequence header is malformed",
            )
        })?;
        // The §5.5.2 sRGB shortcut (BT.709 + sRGB + identity) infers 4:4:4 with no coded bit,
        // whatever the profile, so a stream whose profile *forces* a subsampled layout asserts two
        // chroma formats at once. libaom asserts against exactly that construction, so no
        // conformant encoder emits it. Under such a profile 4:4:4 arises *only* through the
        // shortcut, so the inferred layout alone detects it.
        //
        // Which profiles force one depends on the depth, and that is why this is not simply
        // `seq_profile != 1`: profile 0 is always 4:2:0, and profile 2 is fixed at 4:2:2 only
        // *below* 12 bits. At 12 bits profile 2 codes the pair instead — and the shortcut codes
        // nothing, so it infers 4:4:4 and the two agree. A 12-bit 4:4:4 identity stream is exactly
        // what a lossless high-bit-depth encode produces, so rejecting it here would refuse the
        // most ordinary stream on this path.
        let forces_subsampled =
            header.seq_profile == 0 || (header.seq_profile == 2 && bit_depth != BitDepth::Twelve);
        if chroma == ChromaSubsampling::Cs444 && forces_subsampled {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "AVIF: AV1 backend stream takes the sRGB color_config shortcut, which infers 4:4:4, \
                 but its seq_profile declares subsampled chroma",
            ));
        }
        Ok(Self {
            bit_depth,
            seq_profile: header.seq_profile,
            seq_level_idx_0: header.seq_level_idx,
            width: header.max_frame_width,
            height: header.max_frame_height,
            colour,
            chroma,
            chroma_sample_position: color.chroma_sample_position,
        })
    }
}

/// Bridges a [`gamut_codec_abi::Encoder`] — the workspace-wide codestream seam, and hence any C /
/// `-sys` backend reached through [`gamut_codec_abi::bridge::ForeignEncoder`] — onto
/// [`Av1StillEncoder`].
///
/// The adapter lowers an [`Av1EncodeRequest`] into an [`EncodeConfig`] tagged with
/// [`AV1_CODEC_ID`], presents the [`Planar8`] input as a three-plane [`ImageDesc`], and collects
/// the encoder's sink chunks into the returned OBU buffer.
///
/// # The quantizer, not the quality
///
/// [`EncodeConfig::quality`] is a `0..=100` scale, but `gamut-avif`'s `quality → base_q_idx`
/// mapping is frozen and one-way: synthesizing a `0..=100` value back out of the derived
/// `base_q_idx` would invent a second, contradictory mapping. The adapter therefore leaves
/// `quality` at `0` and passes the authoritative quantizer out-of-band, as the codec-specific
/// [`EncodeConfig::extra`] blob, whose first byte is the AV1 `base_q_idx`. A backend registered
/// for [`AV1_CODEC_ID`] must read `extra`, and must not read `quality`.
///
/// # The speed preset
///
/// [`EncodeConfig`] has no effort field, but the `extra` blob is length-carrying, so the preset
/// travels there: when [`Av1EncodeRequest::speed`] is anything but [`Speed::Thorough`], `extra`
/// is **two bytes** and the second is the [`Speed`] discriminant (`1` = `Balanced`, `2` = `Fast`,
/// `3` = `Fastest`; permanent values, and a later preset takes the next free one). A full-search
/// job keeps the one-byte blob, so a backend reads byte 1 only when `extra_len >= 2`, and treats
/// its absence as `Thorough` — which is also every job a pre-speed backend was ever handed. That
/// absence rule is a persisted wire convention, not the encoder's default: an
/// [`AvifEncoder`](crate::AvifEncoder) left at its default preset, [`Speed::Balanced`], therefore
/// hands a backend the **two-byte** blob `[base_q_idx, 1]`. A backend maps the preset onto its own effort scale or ignores it; either way the stream it
/// returns is held to the same contract.
///
/// # Bit depth
///
/// Both directions are lowered: an 8-bit job through [`Av1StillEncoder::encode_still`] and a 10- or
/// 12-bit one through [`encode_still16`](Av1StillEncoder::encode_still16), so a wrapped hardware or
/// `-sys` encoder owns the job at every depth rather than silently losing the high-bit-depth ones to
/// the software tail.
///
/// [`ImageDesc`] describes the two identically except in three places, and a backend reading the
/// descriptor must honour all three:
///
/// - [`ImageDesc::depth`] is the **coded** depth — `8`, `10` or `12` — never the storage width.
/// - [`ImageDesc::strides`] are in **bytes**, so a high-bit-depth row is twice its sample count.
/// - Above 8 bits the plane pointers address native-endian `u16` samples, **right-justified** at
///   the coded depth (a 10-bit sample occupies `0..=1023`, not the top ten bits of a `u16`).
///   [`ImageDesc::pixel_format`] carries `Rgb16` rather than `Rgb8` to say so, and `Planar16`
///   validates every sample against its depth, so the range is a guarantee and not a convention.
///
/// # Status handling
///
/// [`Status::UNSUPPORTED`] is the fall-through code in both places it can appear: from `supports`
/// (as `false`) and — for a backend that changes its mind — from `encode`, where it is treated as
/// a late decline and the encoder moves on to the next backend. Any other non-OK status is a
/// terminal failure and propagates as [`Error::InvalidInput`].
pub struct AbiAv1StillEncoder<E: Encoder + Send> {
    /// The wrapped ABI encoder.
    inner: E,
}

impl<E: Encoder + Send> AbiAv1StillEncoder<E> {
    /// Wraps a [`gamut_codec_abi::Encoder`] as an [`Av1StillEncoder`].
    #[must_use]
    pub fn new(inner: E) -> Self {
        Self { inner }
    }

    /// Consumes the adapter, returning the wrapped encoder.
    #[must_use]
    pub fn into_inner(self) -> E {
        self.inner
    }

    /// The [`EncodeConfig`] for a job: [`AV1_CODEC_ID`], `quality = 0`, and `extra` pointing at
    /// `extra` (the job's [`extra_blob`], borrowed for the duration of the call).
    fn config(extra: &[u8]) -> EncodeConfig {
        let mut cfg = EncodeConfig::new(AV1_CODEC_ID, 0);
        cfg.extra = extra.as_ptr().cast();
        cfg.extra_len = extra.len();
        cfg
    }
}

/// The codec-specific `extra` blob [`AbiAv1StillEncoder`] lowers `req` to: the `base_q_idx`,
/// then — only when the job runs at a preset other than [`Speed::Thorough`] — the [`Speed`]
/// discriminant. A full-search job therefore lowers to exactly the one-byte blob the adapter
/// wrote before speed presets existed. The test is against `Thorough`, not against
/// [`Speed::default`]: what the blob's absence means is fixed on the wire, while the default may
/// move.
fn extra_blob(req: &Av1EncodeRequest) -> Vec<u8> {
    let mut blob = vec![req.base_q_idx()];
    if req.speed() != Speed::Thorough {
        blob.push(req.speed() as u8);
    }
    blob
}

impl<E: Encoder + Send> std::fmt::Debug for AbiAv1StillEncoder<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AbiAv1StillEncoder").finish_non_exhaustive()
    }
}

impl<E: Encoder + Send> Av1StillEncoder for AbiAv1StillEncoder<E> {
    fn supports(&mut self, req: &Av1EncodeRequest) -> bool {
        let extra = extra_blob(req);
        self.inner.supports(&Self::config(&extra))
    }

    fn encode_still(&mut self, req: &Av1EncodeRequest, planes: &Planar8) -> Result<Vec<u8>> {
        let extra = extra_blob(req);
        let cfg = Self::config(&extra);
        // Per plane, not one luma stride for all three: `Planar8` carries its chroma geometry, and
        // `ImageDesc` has a stride slot per plane precisely so a subsampled buffer is expressible.
        // At 4:4:4 — everything this crate encodes today — all three are the luma width.
        let stride = |i: usize| planes.plane_dimensions(i).0 as usize;
        // Encode inputs are read-only per `ImageDesc`'s contract; the `*mut` is the ABI's single
        // descriptor shape shared with the decode (write) direction.
        let plane_ptr = |i: usize| planes.plane(i).as_ptr().cast_mut();
        let image = ImageDesc::new(
            // Three 8-bit planes in gamut's identity order (Y = G, U = B, V = R) — the planar
            // form of `PixelFormat::Rgb8`, which is what the AVIF v1 surface encodes.
            PixelFormat::Rgb8 as u32,
            planes.width(),
            planes.height(),
            8,
            3,
            [
                plane_ptr(0),
                plane_ptr(1),
                plane_ptr(2),
                std::ptr::null_mut(),
            ],
            [stride(0), stride(1), stride(2), 0],
        );
        let mut obus = Vec::new();
        let status = self.inner.encode(&cfg, &image, &mut |chunk: &[u8]| {
            obus.extend_from_slice(chunk);
            Status::OK
        });
        if status.is_ok() {
            Ok(obus)
        } else if status.is_unsupported() {
            Err(LateDecline::error().with_detail(format!("codec-abi status {}", status.0)))
        } else {
            Err(
                Error::invalid_input(env!("CARGO_PKG_NAME"), "AVIF: AV1 encode backend failed")
                    .with_detail(format!("codec-abi status {}", status.0)),
            )
        }
    }

    fn encode_still16(&mut self, req: &Av1EncodeRequest, planes: &Planar16) -> Result<Vec<u8>> {
        let extra = extra_blob(req);
        let cfg = Self::config(&extra);
        // Bytes, not samples: `ImageDesc::strides` is a byte stride, and a `u16` row is twice as
        // wide as its sample count. Deriving it from the sample count instead would hand the
        // backend every other row.
        let stride = |i: usize| planes.plane_dimensions(i).0 as usize * size_of::<u16>();
        // `Vec<u16>` is 2-byte aligned, which is what a backend reinterpreting the pointer as
        // `uint16_t *` needs; the `*mut` is the ABI's one descriptor shape, shared with the decode
        // (write) direction, and encode inputs stay read-only per its contract.
        let plane_ptr = |i: usize| planes.plane(i).as_ptr().cast::<u8>().cast_mut();
        let image = ImageDesc::new(
            // `Rgb16` rather than `Rgb8`: the samples are `u16`, and the tag is what tells a
            // backend how wide to read before `depth` tells it how many bits carry signal.
            PixelFormat::Rgb16 as u32,
            planes.width(),
            planes.height(),
            // The *coded* depth (10 or 12), not the 16-bit storage width — `av1C` mirrors this and
            // a backend coding to the storage width would produce a stream the container's own
            // depth check rejects.
            u32::from(planes.bit_depth().bits()),
            3,
            [
                plane_ptr(0),
                plane_ptr(1),
                plane_ptr(2),
                std::ptr::null_mut(),
            ],
            [stride(0), stride(1), stride(2), 0],
        );
        let mut obus = Vec::new();
        let status = self.inner.encode(&cfg, &image, &mut |chunk: &[u8]| {
            obus.extend_from_slice(chunk);
            Status::OK
        });
        if status.is_ok() {
            Ok(obus)
        } else if status.is_unsupported() {
            Err(LateDecline::error().with_detail(format!("codec-abi status {}", status.0)))
        } else {
            Err(
                Error::invalid_input(env!("CARGO_PKG_NAME"), "AVIF: AV1 encode backend failed")
                    .with_detail(format!("codec-abi status {}", status.0)),
            )
        }
    }
}

/// The sentinel for a **late** decline — a backend that answered `supports` affirmatively and then
/// declined at `encode` time.
///
/// The typed trait has no third outcome between "handled" and "declined", so the late decline is
/// this exact [`Error::Unsupported`] payload, which [`run_backends`] maps back to a fall-through.
/// Two things raise it: [`AbiAv1StillEncoder`] translating a late [`Status::UNSUPPORTED`] from a C
/// backend, and the default [`Av1StillEncoder::encode_still16`], which is how a backend written
/// against the 8-bit contract declines a high-bit-depth job it never agreed to. A hand-written
/// backend otherwise signals a decline from [`supports`](Av1StillEncoder::supports).
pub(crate) struct LateDecline;

impl LateDecline {
    /// The sentinel error value.
    pub(crate) const MESSAGE: &'static str = "AVIF: AV1 encode backend declined late (UNSUPPORTED)";
    /// Builds the sentinel error.
    pub(crate) fn error() -> Error {
        Error::unsupported(env!("CARGO_PKG_NAME"), Self::MESSAGE)
    }

    /// Whether `err` is the late-decline sentinel.
    pub(crate) fn is(err: &Error) -> bool {
        err.kind() == gamut_core::ErrorKind::Unsupported
            && err.static_message() == Some(Self::MESSAGE)
    }
}

/// Direct tests of the private sequence-header projection.
///
/// [`SeqHeaderParams::parse`] is reachable from the integration suite only through
/// [`still_from_backend_obus`], which checks the coded **bit depth before the chroma format**. Every
/// 12-bit stream that suite can build is paired with an 8-bit request, so it stops at the depth
/// error and never observes the derived layout at all — and the layout is what §5.5.2 makes
/// subtle. Two of the derivations below have no end-to-end route whatever: `chroma_sample_position`
/// because `gamut-av1` fixes it at 0 and exposes no entry point taking an
/// [`Av1StillConfig`](gamut_av1::Av1StillConfig), and the coded `subsampling_y` because the encoder
/// takes a job's chroma from the buffer it was handed and no *subsampled* 12-bit request reaches
/// this parser. So these call `parse` directly and assert what it returned.
///
/// The bit writer duplicates the one in `tests/backend.rs` deliberately: an integration-test binary
/// and a unit-test module cannot share code, and the crate may not dev-depend on `gamut-bitstream`
/// (a publishable crate must not dev-depend on another without a normal edge — `check-release-deps`).
#[cfg(test)]
mod tests {
    use super::*;

    /// The dimensions every test stream carries; both need more than one bit to code, so a
    /// `dimension_bits` mistake cannot hide behind a zero-width field.
    const W: u32 = 34;
    const H: u32 = 18;

    #[test]
    fn extra_blob_appends_the_speed_discriminant_only_off_the_full_search() {
        let blob = |speed| {
            extra_blob(&Av1EncodeRequest::new(
                Dimensions {
                    width: W,
                    height: H,
                },
                77,
                Av1Colour::default(),
                ChromaSubsampling::Cs444,
                BitDepth::Eight,
                speed,
                NonZeroUsize::MIN,
            ))
        };
        assert_eq!(blob(Speed::Thorough), [77]);
        assert_eq!(blob(Speed::Balanced), [77, 1]);
        assert_eq!(blob(Speed::Fast), [77, 2]);
        assert_eq!(blob(Speed::Fastest), [77, 3]);
    }

    /// An MSB-first bit writer. The byte vector *is* the payload — a partial final byte is already
    /// zero-padded, which is what `trailing_bits` requires.
    #[derive(Default)]
    struct BitWriter {
        bytes: Vec<u8>,
        bit: u32,
    }

    impl BitWriter {
        /// Appends the low `n` bits of `value`, most significant first.
        fn put(&mut self, value: u32, n: u32) -> &mut Self {
            for i in (0..n).rev() {
                if self.bit == 0 {
                    self.bytes.push(0);
                }
                let last = self.bytes.len() - 1;
                self.bytes[last] |= (((value >> i) & 1) as u8) << (7 - self.bit);
                self.bit = (self.bit + 1) % 8;
            }
            self
        }
    }

    /// The bit width `dimension_bits` gives `value`.
    fn dim_bits(value: u32) -> u32 {
        32 - (value - 1).leading_zeros()
    }

    /// A `reduced_still_picture_header` sequence-header OBU carrying the §5.5.2 colour tail the
    /// arguments describe.
    ///
    /// `mc` selects the matrix code point, and with it whether the sRGB shortcut applies (`0` is
    /// `MC_IDENTITY`, which alongside BT.709 primaries and sRGB transfer takes it). `coded_pair` is
    /// the explicit `subsampling_x`/`subsampling_y` that only profile 2 at 12 bits codes, and `csp`
    /// the `chroma_sample_position` that follows only when both axes are subsampled.
    fn seq_header_obus(
        seq_profile: u8,
        high_bitdepth: bool,
        twelve_bit: bool,
        mc: u32,
        coded_pair: Option<(u32, u32)>,
        csp: u32,
    ) -> Vec<u8> {
        let (wbits, hbits) = (dim_bits(W), dim_bits(H));
        let mut b = BitWriter::default();
        b.put(u32::from(seq_profile), 3)
            .put(1, 1) // still_picture
            .put(1, 1) // reduced_still_picture_header
            .put(0, 5) // seq_level_idx[0]
            .put(wbits - 1, 4)
            .put(hbits - 1, 4)
            .put(W - 1, wbits)
            .put(H - 1, hbits)
            .put(0, 6) // the six enable flags
            .put(u32::from(high_bitdepth), 1);
        if seq_profile == 2 && high_bitdepth {
            b.put(u32::from(twelve_bit), 1);
        }
        if seq_profile != 1 {
            b.put(0, 1); // mono_chrome
        }
        b.put(1, 1) // color_description_present_flag
            .put(1, 8) // color_primaries = BT.709
            .put(13, 8) // transfer_characteristics = sRGB
            .put(mc, 8);
        if mc != 0 {
            b.put(1, 1); // color_range = full
            let (sx, sy) = match coded_pair {
                Some((sx, sy)) => {
                    b.put(sx, 1);
                    if sx == 1 {
                        b.put(sy, 1);
                    }
                    (sx, sy)
                }
                None if seq_profile == 0 => (1, 1),
                None if seq_profile == 1 => (0, 0),
                None => (1, 0),
            };
            if (sx, sy) == (1, 1) {
                b.put(csp, 2);
            }
        }
        // The tail §5.5.1/§5.5.2 code after the chroma fields — under the shortcut too, which
        // skips only `color_range`, the subsampling pair and the sample position — then the §5.3.1
        // `trailing_bits()` that `SequenceHeader::parse` requires to close the OBU.
        b.put(0, 1) // separate_uv_delta_q
            .put(0, 1) // film_grain_params_present
            .put(1, 1); // trailing_one_bit; the zero pad is the writer's own
        let payload = std::mem::take(&mut b.bytes);
        let mut obus = vec![0x0A, payload.len() as u8];
        obus.extend_from_slice(&payload);
        obus
    }

    /// Parses one stream, asserting every field the container mirrors — not just the one under
    /// test. A misparse that happens to land on the expected chroma is then still caught.
    #[track_caller]
    fn assert_parse(
        obus: &[u8],
        bit_depth: BitDepth,
        seq_profile: u8,
        chroma: ChromaSubsampling,
        chroma_sample_position: u8,
    ) {
        let h = SeqHeaderParams::parse(obus).expect("the stream is a well-formed sequence header");
        assert_eq!(h.bit_depth, bit_depth, "bit_depth");
        assert_eq!(h.seq_profile, seq_profile, "seq_profile");
        assert_eq!(h.seq_level_idx_0, 0, "seq_level_idx_0");
        assert_eq!((h.width, h.height), (W, H), "coded dimensions");
        assert_eq!(h.colour, (1, 13, 1, true), "colour");
        assert_eq!(h.chroma, chroma, "derived chroma");
        assert_eq!(
            h.chroma_sample_position, chroma_sample_position,
            "chroma_sample_position"
        );
    }

    /// §5.5.2 infers 4:4:4 for profile 1 with no bit coding it. The depth cannot reach 12 here —
    /// `twelve_bit` is coded only under profile 2 — so the layout comes from the profile alone.
    #[test]
    fn profile_one_infers_four_four_four() {
        let obus = seq_header_obus(1, false, false, 1, None, 0);
        assert_parse(&obus, BitDepth::Eight, 1, ChromaSubsampling::Cs444, 0);
    }

    /// Profile 2 at 12 bits is the one configuration that *codes* the pair, and `subsampling_x = 0`
    /// is 4:4:4 — the layout an ordinary lossless high-bit-depth encode produces. `subsampling_y`
    /// is not coded, so a parser reading it here would consume `separate_uv_delta_q`.
    #[test]
    fn twelve_bit_profile_two_codes_four_four_four() {
        let obus = seq_header_obus(2, true, true, 1, Some((0, 0)), 0);
        assert_parse(&obus, BitDepth::Twelve, 2, ChromaSubsampling::Cs444, 0);
    }

    /// `subsampling_x = 1, subsampling_y = 0` is 4:2:2 — and, unlike every other profile-2 stream,
    /// it is 4:2:2 because the bits say so rather than because §5.5.2 fixes it.
    #[test]
    fn twelve_bit_profile_two_codes_four_two_two() {
        let obus = seq_header_obus(2, true, true, 1, Some((1, 0)), 0);
        assert_parse(&obus, BitDepth::Twelve, 2, ChromaSubsampling::Cs422, 0);
    }

    /// Below 12 bits profile 2 is fixed at 4:2:2 with no bit coding it, so the pair must not be
    /// read — the depth, not the profile alone, decides whether the syntax codes it.
    #[test]
    fn profile_two_below_twelve_bits_infers_four_two_two() {
        let obus = seq_header_obus(2, true, false, 1, None, 0);
        assert_parse(&obus, BitDepth::Ten, 2, ChromaSubsampling::Cs422, 0);
    }

    /// Profile 0 infers 4:2:0, and `chroma_sample_position` is coded exactly when both axes are
    /// subsampled. A parser that skipped the read would report `CSP_UNKNOWN` for a stream that
    /// named a real position.
    #[test]
    fn profile_zero_infers_four_two_zero_and_reads_its_sample_position() {
        let obus = seq_header_obus(0, false, false, 1, None, 1);
        assert_parse(&obus, BitDepth::Eight, 0, ChromaSubsampling::Cs420, 1);
    }

    /// The coded `(1, 1)` pair is 4:2:0, which then codes a sample position of its own — the one
    /// stream in which both the coded pair and the position are read.
    #[test]
    fn twelve_bit_profile_two_codes_four_two_zero_with_a_sample_position() {
        let obus = seq_header_obus(2, true, true, 1, Some((1, 1)), 2);
        assert_parse(&obus, BitDepth::Twelve, 2, ChromaSubsampling::Cs420, 2);
    }

    /// The §5.5.2 sRGB shortcut infers 4:4:4 and codes no sample position — so
    /// `chroma_sample_position` stays 0.
    #[test]
    fn the_srgb_shortcut_infers_four_four_four_and_codes_no_sample_position() {
        let obus = seq_header_obus(1, false, false, 0, None, 0);
        let h = SeqHeaderParams::parse(&obus).expect("the shortcut stream parses");
        assert_eq!(h.chroma, ChromaSubsampling::Cs444);
        assert_eq!(h.chroma_sample_position, 0);
        assert_eq!(h.colour, (1, 13, 0, true), "the shortcut infers full range");
    }

    /// An explicitly coded CP/TC/MC_UNSPECIFIED triple is refused exactly as an absent colour
    /// description is: §5.5.2 gives the absent flag that very triple, so the two streams say the
    /// same thing, and the parsed header keeps no flag to tell them apart.
    #[test]
    fn an_explicitly_unspecified_triple_is_refused_as_undescribed() {
        let (wbits, hbits) = (dim_bits(W), dim_bits(H));
        let mut b = BitWriter::default();
        b.put(1, 3) // seq_profile = High, so no mono_chrome bit
            .put(1, 1) // still_picture
            .put(1, 1) // reduced_still_picture_header
            .put(0, 5) // seq_level_idx[0]
            .put(wbits - 1, 4)
            .put(hbits - 1, 4)
            .put(W - 1, wbits)
            .put(H - 1, hbits)
            .put(0, 6) // the six enable flags
            .put(0, 1) // high_bitdepth
            .put(1, 1) // color_description_present_flag
            .put(2, 8) // color_primaries = UNSPECIFIED
            .put(2, 8) // transfer_characteristics = UNSPECIFIED
            .put(2, 8) // matrix_coefficients = UNSPECIFIED
            .put(1, 1) // color_range
            .put(0, 1) // separate_uv_delta_q
            .put(0, 1) // film_grain_params_present
            .put(1, 1); // trailing_one_bit
        let payload = std::mem::take(&mut b.bytes);
        let mut obus = vec![0x0A, payload.len() as u8];
        obus.extend_from_slice(&payload);
        let Err(err) = SeqHeaderParams::parse(&obus) else {
            panic!("an unspecified colour description is refused");
        };
        assert_eq!(err.kind(), gamut_core::ErrorKind::Unsupported);
        assert_eq!(
            err.static_message(),
            Some("AVIF: AV1 backend stream must set color_description_present_flag")
        );
    }
}
