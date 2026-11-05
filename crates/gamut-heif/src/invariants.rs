//! Executable laws for this crate's decode budget, and the stub codec binding they decode
//! through.
//!
//! Each law takes plain data, checks one claim, and returns a [`Violation`] naming what broke it.
//! Nothing here panics on a well-formed input, so the same body runs under a pinned-seed
//! property test in the per-PR gate (`decode.rs`) and under the fuzz tier's `heif_decode_limits`
//! target (`tooling/gamut-fuzz`, `docs/testing.md`).
//!
//! **Decode limits:** [`decoding_stays_within_limits`] decodes every item of a file on all three
//! surfaces twice — under the limits being tested and under a looser ceiling — with
//! [`StubCodec`] standing in for a codec. Every image the tighter run returns is within its limits
//! and identical to the looser run's, and every refusal the tighter limits add is a limit refusal.
//! Since each full-image buffer the pipeline allocates is sized like an image it checked, the
//! first clause is the observable face of "decode never allocates past limits × channels".
//!
//! This module is the test oracle, not the system under test, so `.cargo/mutants.toml` excludes
//! `crates/*/src/invariants.rs` from mutation.

use std::sync::{Arc, Mutex};

use gamut_codec_abi::Registry;
use gamut_color::ChromaSubsampling;
use gamut_core::{Cicp, Dimensions, Error, Result as CoreResult};
use gamut_isobmff::IsoBmffImage;

use crate::{
    AlphaChroma, Codestream, DECODED_ITEM_LIMIT, DERIVED_ITEM_LIMIT, DIMENSION_LIMIT, DecodeLimits,
    DecodeOptions, DecodedFrame, HeifImage, PIXEL_LIMIT,
};

/// A law that did not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    law: &'static str,
    detail: String,
}

impl Violation {
    fn new(law: &'static str, detail: String) -> Self {
        Self { law, detail }
    }

    /// The name of the law that was broken.
    #[must_use]
    pub fn law(&self) -> &'static str {
        self.law
    }

    /// What broke it, in terms the failing case can be read from.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl core::fmt::Display for Violation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: {}", self.law, self.detail)
    }
}

/// The largest frame side [`StubBackend`] returns, in pixels.
pub const STUB_MAX_SIDE: u8 = 16;

/// The error origin of every refusal raised for [`StubCodec`].
pub const STUB_ORIGIN: &str = "gamut-heif-stub";

/// The configuration record [`StubCodec`] reads from an `hvcC` property body of four bytes:
/// the chroma layout (`0` 4:4:4, `1` 4:2:2, `2` 4:2:0, `3` monochrome), the bit depth, the
/// matrix coefficients and the full-range flag its codestream "signals".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StubConfig {
    /// The chroma layout every frame is returned in.
    pub chroma: ChromaSubsampling,
    /// The bit depth every frame is returned at.
    pub bit_depth: u8,
    /// What [`Codestream::signalled_colour`] reports.
    pub colour: Cicp,
}

impl StubConfig {
    /// The four-byte `hvcC` body that parses to this configuration.
    #[must_use]
    pub fn to_bytes(self) -> Vec<u8> {
        let chroma = match self.chroma {
            ChromaSubsampling::Cs444 => 0,
            ChromaSubsampling::Cs422 => 1,
            ChromaSubsampling::Cs420 => 2,
            _ => 3,
        };
        vec![
            chroma,
            self.bit_depth,
            self.colour.matrix_coefficients as u8,
            u8::from(self.colour.full_range),
        ]
    }
}

/// A stand-in codec binding: `hvc1` items, an `hvcC` [`StubConfig`], and frames from a
/// [`StubBackend`]. Accepts only monochrome alpha, as HEIC does.
#[derive(Debug)]
pub struct StubCodec;

/// The backend [`StubCodec`] decodes through. It ignores the coded picture and returns a frame
/// whose size the payload's first two bytes choose (`1..=STUB_MAX_SIDE` each, width then height),
/// at the configuration's chroma and depth, with samples drawn from the payload.
#[derive(Debug, Clone, Default)]
pub struct StubBackend {
    /// Whether `supports` declines every job.
    pub declines: bool,
    /// How many frames this backend has decoded.
    pub decoded: u32,
}

impl StubBackend {
    /// A registry holding one accepting backend.
    #[must_use]
    pub fn registry() -> Registry<StubBackend> {
        let mut registry = Registry::new();
        registry.push(Arc::new(Mutex::new(StubBackend::default())));
        registry
    }
}

impl Codestream for StubCodec {
    type Config = StubConfig;
    type Backend = StubBackend;
    const ORIGIN: &'static str = STUB_ORIGIN;
    const CONFIGURATION_BOX: [u8; 4] = *b"hvcC";
    const ALPHA: AlphaChroma = AlphaChroma::Monochrome;

    fn is_coded_item(item_type: &[u8; 4]) -> bool {
        item_type == b"hvc1"
    }

    fn parse_config(data: &[u8]) -> CoreResult<StubConfig> {
        let &[chroma, bit_depth, matrix, full] = data else {
            return Err(Error::invalid_input(
                STUB_ORIGIN,
                "stub: config is not 4 bytes",
            ));
        };
        let chroma = match chroma {
            0 => ChromaSubsampling::Cs444,
            1 => ChromaSubsampling::Cs422,
            2 => ChromaSubsampling::Cs420,
            _ => ChromaSubsampling::Cs400,
        };
        Ok(StubConfig {
            chroma,
            bit_depth,
            colour: Cicp {
                colour_primaries: 2,
                transfer_characteristics: 2,
                matrix_coefficients: u16::from(matrix),
                full_range: full != 0,
            },
        })
    }

    fn validate_still_payload(_: &StubConfig, payload: &[u8]) -> CoreResult<()> {
        if payload.is_empty() {
            return Err(Error::invalid_input(STUB_ORIGIN, "stub: empty payload"));
        }
        Ok(())
    }

    fn signalled_colour(config: &StubConfig, _: &[u8]) -> CoreResult<Cicp> {
        Ok(config.colour)
    }

    fn supports(backend: &mut StubBackend, _: &StubConfig) -> bool {
        !backend.declines
    }

    fn decode(
        backend: &mut StubBackend,
        config: &StubConfig,
        payload: &[u8],
    ) -> CoreResult<DecodedFrame> {
        backend.decoded += 1;
        let side = u32::from(STUB_MAX_SIDE);
        let byte = |i: usize| payload.get(i).map_or(0, |&b| u32::from(b));
        let dimensions = Dimensions::new(1 + byte(0) % side, 1 + byte(1) % side)?;
        let sample = |i: usize| u16::from(payload[i % payload.len()]);
        let luma = dimensions.width() as usize * dimensions.height() as usize;
        let (cw, ch) = config
            .chroma
            .chroma_dimensions(dimensions.width(), dimensions.height());
        let chroma_len = cw as usize * ch as usize;
        DecodedFrame::new(
            dimensions,
            config.bit_depth,
            config.chroma,
            (0..luma).map(sample).collect(),
            (0..chroma_len).map(|i| sample(i + 1)).collect(),
            (0..chroma_len).map(|i| sample(i + 2)).collect(),
        )
    }
}

/// One surface's output reduced to what the law compares: dimensions and samples.
type Decoded = (u32, u32, Vec<u16>);

/// Checks one surface's two runs of item `id` against the law; `limits` are the tighter run's.
pub(crate) fn runs_agree_within_limits(
    surface: &str,
    id: u32,
    limits: DecodeLimits,
    tight: CoreResult<Decoded>,
    loose: CoreResult<Decoded>,
) -> Result<(), Violation> {
    const LAW: &str = "decoding_stays_within_limits";
    match (tight, loose) {
        (Ok(tight), Ok(loose)) => {
            let (w, h, _) = tight;
            if w.max(h) > limits.max_dimension || u64::from(w) * u64::from(h) > limits.max_pixels {
                return Err(Violation::new(
                    LAW,
                    format!("{surface} item {id}: {w}x{h} returned under {limits:?}"),
                ));
            }
            if tight != loose {
                return Err(Violation::new(
                    LAW,
                    format!("{surface} item {id}: the limits changed the decoded image"),
                ));
            }
            Ok(())
        }
        (Ok(_), Err(error)) => Err(Violation::new(
            LAW,
            format!("{surface} item {id}: decoded under {limits:?} but refused looser: {error}"),
        )),
        (Err(error), Ok(_))
            if !matches!(
                error.static_message(),
                Some(DIMENSION_LIMIT | PIXEL_LIMIT | DECODED_ITEM_LIMIT | DERIVED_ITEM_LIMIT)
            ) =>
        {
            Err(Violation::new(
                LAW,
                format!("{surface} item {id}: tighter limits raised a non-limit error: {error}"),
            ))
        }
        _ => Ok(()),
    }
}

/// The law: decoding any item of `file` on any surface under `limits` either refuses or returns
/// an image within `limits` — the very image a decode under `ceiling` returns — and any refusal
/// `limits` add over `ceiling` is a limit refusal. See [the module docs](self).
///
/// `limits` are clamped into `ceiling` field by field, so a driver may pass any pair. A file that
/// does not parse satisfies the law vacuously; parsing is `gamut-isobmff`'s subject.
pub fn decoding_stays_within_limits(
    file: &[u8],
    limits: DecodeLimits,
    ceiling: DecodeLimits,
) -> Result<(), Violation> {
    let Ok(image) = IsoBmffImage::parse(file).and_then(HeifImage::new) else {
        return Ok(());
    };
    let limits = DecodeLimits::new(
        limits.max_dimension.min(ceiling.max_dimension),
        limits.max_pixels.min(ceiling.max_pixels),
    )
    .with_item_limits(
        limits.max_decoded_items.min(ceiling.max_decoded_items),
        limits.max_derived_items.min(ceiling.max_derived_items),
    );
    let backends = StubBackend::registry();
    let options = |l| DecodeOptions::default().with_limits(l);
    for item in image.items() {
        let id = item.id();
        let planar = |l| {
            image
                .decode_item_planar::<StubCodec>(id, &backends, l)
                .map(|f| (f.width(), f.height(), [f.y(), f.cb(), f.cr()].concat()))
        };
        runs_agree_within_limits("planar", id, limits, planar(limits), planar(ceiling))?;
        let rgba8 = |l| {
            image
                .decode_item_rgba8::<StubCodec>(id, &backends, options(l))
                .map(|b| {
                    let samples = b.as_samples().iter().map(|&s| u16::from(s)).collect();
                    (b.width(), b.height(), samples)
                })
        };
        runs_agree_within_limits("rgba8", id, limits, rgba8(limits), rgba8(ceiling))?;
        let rgba16 = |l| {
            image
                .decode_item_rgba16::<StubCodec>(id, &backends, options(l))
                .map(|b| (b.width(), b.height(), b.as_samples().to_vec()))
        };
        runs_agree_within_limits("rgba16", id, limits, rgba16(limits), rgba16(ceiling))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // `invariants.rs` is excluded from mutation, so these are not mutation-driven. They exist so
    // that a law which can no longer *fail* is caught: a law stuck at `Ok(())` would make every
    // property over it vacuous while still passing.

    use gamut_core::Error;

    use super::runs_agree_within_limits;
    use crate::{
        DECODED_ITEM_LIMIT, DERIVED_ITEM_LIMIT, DIMENSION_LIMIT, DecodeLimits, PIXEL_LIMIT,
    };

    const LIMITS: DecodeLimits = DecodeLimits::new(4, 12);

    #[test]
    fn an_image_past_the_limits_violates_the_decode_law() {
        let wide = || Ok((5, 1, vec![0; 5]));
        assert!(runs_agree_within_limits("t", 1, LIMITS, wide(), wide()).is_err());
        let large = || Ok((4, 4, vec![0; 16]));
        assert!(runs_agree_within_limits("t", 1, LIMITS, large(), large()).is_err());
    }

    #[test]
    fn a_changed_image_violates_the_decode_law() {
        let v = runs_agree_within_limits("t", 1, LIMITS, Ok((1, 1, vec![0])), Ok((1, 1, vec![1])))
            .unwrap_err();
        assert!(v.detail().contains("changed"), "{v}");
    }

    #[test]
    fn success_only_under_the_tighter_limits_violates_the_decode_law() {
        let refused = Err(Error::invalid_input(env!("CARGO_PKG_NAME"), "x"));
        assert!(runs_agree_within_limits("t", 1, LIMITS, Ok((1, 1, vec![0])), refused).is_err());
    }

    #[test]
    fn only_a_limit_refusal_may_be_added_by_the_tighter_limits() {
        let ok = || Ok((1, 1, vec![0]));
        let other = Err(Error::invalid_input(env!("CARGO_PKG_NAME"), "x"));
        assert!(runs_agree_within_limits("t", 1, LIMITS, other, ok()).is_err());
        for message in [
            DIMENSION_LIMIT,
            PIXEL_LIMIT,
            DECODED_ITEM_LIMIT,
            DERIVED_ITEM_LIMIT,
        ] {
            let limit = Err(Error::unsupported("gamut-heif", message));
            assert_eq!(
                runs_agree_within_limits("t", 1, LIMITS, limit, ok()),
                Ok(())
            );
        }
    }
}
