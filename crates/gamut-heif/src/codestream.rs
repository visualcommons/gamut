//! [`Codestream`]: how a codec crate binds its coded items into the shared decode pipeline.

use gamut_codec_abi::{Registry, RegistryError};
use gamut_core::{Cicp, Error, Result};

use crate::frame::DecodedFrame;

/// The [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) message raised when no
/// pushed backend's `supports` accepted a coded item (or none was pushed).
pub const NO_BACKEND: &str = "HEIF: no codestream backend accepted the coded item";

/// The [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) message raised when a
/// backend's lock was poisoned by a panic while it was held.
pub const BACKEND_POISONED: &str = "HEIF: a codestream backend's lock was poisoned by a panic";

/// A codec's binding into the shared HEIF pipeline: the questions about a coded item only its
/// codec can answer, and the typed backend trait its decodes run through.
///
/// gamut-heif owns everything around the coded picture — the derivation walk, the limits, colour
/// conversion, alpha, the transformative properties — and nothing inside it. A codec crate
/// (`gamut-avif` for `av01`, `gamut-heic` for `hvc1`/`hev1`) implements this once, on a type of
/// its own that is never instantiated, and every decode surface becomes available for its items:
/// `image.decode_item_rgba8::<Binding>(id, &backends, options)`.
///
/// The backends are a [`Registry`] of the codec's own **typed** backend trait —
/// [`Backend`](Self::Backend) is that trait as an object, `dyn HevcDecoder + Send` for instance —
/// so a pure-Rust backend and a `gamut-codec-abi` adapter over a C one enter by the same door, and
/// the registry's contract holds unchanged: backends are tried in push order,
/// [`supports`](Self::supports) returning `false` is the only fall-through, and a backend that
/// accepts a job and then fails returns its error unchanged.
///
/// Every function is an associated function, so a binding carries no state; the state lives in
/// the backends.
pub trait Codestream {
    /// The codec's typed configuration record (`av1C`, `hvcC`, …).
    type Config;

    /// The codec's typed backend trait, as the object the [`Registry`] holds.
    type Backend: ?Sized;

    /// The error origin of every refusal the pipeline raises while decoding for this binding —
    /// the codec crate's package name — so an error names the format the caller asked for
    /// rather than this shared layer.
    const ORIGIN: &'static str;

    /// The four-character type of the item property carrying [`Config`](Self::Config)
    /// (`*b"av1C"`, `*b"hvcC"`).
    const CONFIGURATION_BOX: [u8; 4];

    /// Which alpha auxiliaries the RGBA surfaces accept.
    const ALPHA: AlphaChroma;

    /// Whether `item_type` names a coded item of this codec (`av01`; `hvc1`/`hev1`).
    fn is_coded_item(item_type: &[u8; 4]) -> bool;

    /// Parses the [`CONFIGURATION_BOX`](Self::CONFIGURATION_BOX) property body.
    ///
    /// # Errors
    ///
    /// The codec's own refusal of a malformed record; the pipeline propagates it unchanged.
    fn parse_config(data: &[u8]) -> Result<Self::Config>;

    /// Checks that `payload` is a still picture this codec's still-image profile admits, before
    /// any backend sees it.
    ///
    /// # Errors
    ///
    /// The codec's own refusal; the pipeline propagates it unchanged.
    fn validate_still_payload(config: &Self::Config, payload: &[u8]) -> Result<()>;

    /// The colour a coded item signals in its codestream, which the RGBA surfaces de-matrix with
    /// when the presented item carries no `colr`: AV1's sequence-header `color_config`, or the
    /// codec's documented default when its codestream signals none.
    ///
    /// # Errors
    ///
    /// The codec's own refusal of a codestream it cannot read the colour from.
    fn signalled_colour(config: &Self::Config, payload: &[u8]) -> Result<Cicp>;

    /// Whether `backend` will decode a picture under `config` — the registry's only
    /// fall-through signal.
    fn supports(backend: &mut Self::Backend, config: &Self::Config) -> bool;

    /// Decodes one still picture through `backend`.
    ///
    /// # Errors
    ///
    /// The backend's own error, which the pipeline returns unchanged.
    fn decode(
        backend: &mut Self::Backend,
        config: &Self::Config,
        payload: &[u8],
    ) -> Result<DecodedFrame>;
}

/// Which alpha auxiliaries a binding's RGBA surfaces accept. Only the auxiliary's luma plane is
/// ever read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AlphaChroma {
    /// The auxiliary must be monochrome; a coloured one is refused.
    Monochrome,
    /// Any chroma layout; the chroma planes are ignored. Real-world AVIF alpha items are
    /// routinely coded 4:2:0 or 4:4:4 with meaningless chroma, and libavif reads only Y.
    Any,
}

/// Runs one coded-item decode through `backends` under `C`'s contract, turning a registry
/// failure into `C`'s error.
pub(crate) fn run<C: Codestream>(
    backends: &Registry<C::Backend>,
    config: &C::Config,
    payload: &[u8],
) -> Result<DecodedFrame> {
    backends
        .run(
            |backend| C::supports(backend, config),
            |backend| C::decode(backend, config, payload),
        )
        .map_err(|error| match error {
            RegistryError::Backend(error) => error,
            RegistryError::Unsupported => Error::unsupported(C::ORIGIN, NO_BACKEND),
            RegistryError::Poisoned => Error::unsupported(C::ORIGIN, BACKEND_POISONED),
        })
}
