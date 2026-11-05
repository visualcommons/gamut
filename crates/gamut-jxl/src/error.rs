//! Mapping from the reference implementations' error/status codes to the shared
//! [`gamut_core::Error`] surface.
//!
//! Kept out of the `unsafe` [`crate::ffi`] module because it is pure value translation: it matches
//! on plain code enums / `#[repr(transparent)]` newtypes and returns a static-string error,
//! dereferencing nothing. That keeps every arm unit-testable without a live encoder or decoder.
//!
//! The module has two independently feature-gated halves: [`map_encoder_error`] (libjxl statuses,
//! `encode`) and [`map_decode_error`] (jxl-rs errors, `decode`).

#[cfg(all(
    feature = "encode",
    any(not(target_arch = "wasm32"), target_os = "emscripten")
))]
mod encode {
    use gamut_core::Error;
    use gamut_jxl_sys::encode::JxlEncoderError;

    /// Translates the detailed [`JxlEncoderError`] behind a libjxl `JXL_ENC_ERROR` status into a
    /// [`gamut_core::Error`] with a static `"JXL: …"` message.
    ///
    /// The mapping mirrors libjxl's `JxlEncoderError` categories: out-of-memory and rejected input
    /// map to [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput), an unsupported
    /// configuration to [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported), and API
    /// misuse (a bug in this crate — the wrapper is supposed to make every call in the right order)
    /// to [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) after a `debug_assert!`
    /// that trips in debug builds. Any other code, including the generic error, falls through to a
    /// generic [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error.
    pub(crate) fn map_encoder_error(err: JxlEncoderError) -> Error {
        let detail = format!("{err:?}");
        let classified = match err {
            JxlEncoderError::OOM => {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "JXL: encoder ran out of memory")
            }
            JxlEncoderError::BAD_INPUT => {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "JXL: encoder rejected the input")
            }
            JxlEncoderError::NOT_SUPPORTED => Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "JXL: encoder does not support this configuration",
            ),
            JxlEncoderError::API_USAGE => {
                debug_assert!(false, "JXL: internal encoder API misuse (bug in gamut-jxl)");
                Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JXL: internal encoder API misuse (bug in gamut-jxl)",
                )
            }
            // JPEG-reconstruction metadata could not represent the input on the jbrd
            // recompression path (e.g. exotic progressive scan scripts).
            JxlEncoderError::JBRD => Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "JXL: JPEG reconstruction metadata cannot represent this JPEG",
            ),
            // GENERIC is the catch-all, and any future/unknown code is handled conservatively.
            _ => Error::invalid_input(env!("CARGO_PKG_NAME"), "JXL: encoding failed"),
        };
        classified.with_detail(detail)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn assert_mapped(error: Error, kind: gamut_core::ErrorKind, message: &'static str) {
            assert_eq!(error.kind(), kind);
            assert_eq!(error.static_message(), Some(message));
            assert!(error.detail().is_some());
        }

        #[test]
        fn oom_maps_to_invalid_input() {
            assert_mapped(
                map_encoder_error(JxlEncoderError::OOM),
                gamut_core::ErrorKind::InvalidInput,
                "JXL: encoder ran out of memory",
            );
        }

        #[test]
        fn bad_input_maps_to_invalid_input() {
            assert_mapped(
                map_encoder_error(JxlEncoderError::BAD_INPUT),
                gamut_core::ErrorKind::InvalidInput,
                "JXL: encoder rejected the input",
            );
        }

        #[test]
        fn not_supported_maps_to_unsupported() {
            assert_mapped(
                map_encoder_error(JxlEncoderError::NOT_SUPPORTED),
                gamut_core::ErrorKind::Unsupported,
                "JXL: encoder does not support this configuration",
            );
        }

        #[test]
        fn jbrd_maps_to_unsupported() {
            assert_mapped(
                map_encoder_error(JxlEncoderError::JBRD),
                gamut_core::ErrorKind::Unsupported,
                "JXL: JPEG reconstruction metadata cannot represent this JPEG",
            );
        }

        #[test]
        fn generic_and_unknown_map_to_generic_failure() {
            assert_mapped(
                map_encoder_error(JxlEncoderError::GENERIC),
                gamut_core::ErrorKind::InvalidInput,
                "JXL: encoding failed",
            );
            // Any unlisted code (ABI-representable via the transparent newtype) falls through.
            assert_mapped(
                map_encoder_error(JxlEncoderError(9999)),
                gamut_core::ErrorKind::InvalidInput,
                "JXL: encoding failed",
            );
        }

        // API_USAGE fires a `debug_assert!` (an internal API-ordering bug in this crate). Under a
        // debug build that panics, so it is pinned with a `should_panic` test — which also kills the
        // "delete the API_USAGE arm" mutant, since deleting it drops the assertion. Gated to debug
        // builds because the assertion (and thus the panic) is compiled out under `--release`.
        #[cfg(debug_assertions)]
        #[test]
        #[should_panic = "internal encoder API misuse"]
        fn api_usage_debug_asserts() {
            let _ = map_encoder_error(JxlEncoderError::API_USAGE);
        }
    }
}

#[cfg(all(
    feature = "encode",
    any(not(target_arch = "wasm32"), target_os = "emscripten")
))]
pub(crate) use encode::map_encoder_error;

#[cfg(feature = "decode")]
mod decode {
    use gamut_core::Error;
    use jxl::error::Error as JxlError;

    /// Translates a jxl-rs decode [`JxlError`] into a [`gamut_core::Error`] with a static `"JXL: …"`
    /// message.
    ///
    /// jxl-rs's `Error` is a large, `#[non_exhaustive]` enum of specific decode failures; gamut's
    /// surface is deliberately coarse, so this collapses it to a handful of distinguishable classes
    /// and a generic fallback:
    ///
    /// - an invalid magic/signature → [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput)
    ///   (not a JPEG XL stream);
    /// - the pixel-limit guard tripping →
    ///   [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) (image too large);
    /// - features gamut can't present — a colour image requested as grayscale, or output that would
    ///   need a colour-management transform we don't run — →
    ///   [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported);
    /// - buffer-count/-size mismatches, which can only be a bug in this wrapper's own driver → a
    ///   `debug_assert!` plus [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput);
    /// - everything else (malformed codestream, unsupported-but-valid feature, allocation failure) →
    ///   the generic [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) fallback.
    ///
    /// Truncation is *not* handled here: jxl-rs signals "needs more input" as a
    /// [`ProcessingResult::NeedsMoreInput`](jxl::api::ProcessingResult), never an `Err`, so the
    /// decoder maps truncation to an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput)
    /// error itself (see [`crate::decoder`]).
    pub(crate) fn map_decode_error(err: JxlError) -> Error {
        let detail = err.to_string();
        let classified = match err {
            JxlError::InvalidSignature => Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JXL: not a valid JPEG XL codestream",
            ),
            // The decoder's `pixel_limit` guard rejects oversized images with this error.
            JxlError::ImageSizeTooLarge(..)
            | JxlError::ImageDimensionTooLarge(..)
            | JxlError::InvalidImageSize(..)
            | JxlError::SizeOverflow => Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JXL: image exceeds the decoder pixel limit",
            ),
            // A colour image cannot be presented as grayscale. gamut rejects this up front with a
            // clearer message, so reaching it here would be a jxl-rs-side surprise; map it anyway.
            JxlError::NotGrayscale => Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "JXL: cannot decode a color image as grayscale",
            ),
            // Output paths that would require a colour-management (ICC/CMS) transform, which gamut
            // does not configure.
            JxlError::ICCOutputNoCMS
            | JxlError::NonXybOutputNoCMS
            | JxlError::IccUnsupportedTransferFunction
            | JxlError::TransferFunctionUnknown
            | JxlError::CmsError(_)
            | JxlError::CmsChannelCountIncrease { .. }
            | JxlError::CmsConsumedChannelRequested { .. } => Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "JXL: color management (ICC/CMS) is not supported",
            ),
            // The wrapper always hands jxl-rs exactly one correctly sized output buffer; a mismatch
            // is a bug in `crate::decoder`, not bad input.
            JxlError::WrongBufferCount(..) | JxlError::InvalidOutputBufferSize(..) => {
                debug_assert!(
                    false,
                    "JXL: internal decoder buffer mismatch (bug in gamut-jxl)"
                );
                Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JXL: internal decoder buffer mismatch (bug in gamut-jxl)",
                )
            }
            _ => Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JXL: invalid or unsupported codestream",
            ),
        };
        classified.with_detail(detail)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn assert_mapped(error: Error, kind: gamut_core::ErrorKind, message: &'static str) {
            assert_eq!(error.kind(), kind);
            assert_eq!(error.static_message(), Some(message));
            assert!(error.detail().is_some());
        }

        #[test]
        fn invalid_signature_maps_to_invalid_input() {
            assert_mapped(
                map_decode_error(JxlError::InvalidSignature),
                gamut_core::ErrorKind::InvalidInput,
                "JXL: not a valid JPEG XL codestream",
            );
        }

        #[test]
        fn oversized_maps_to_pixel_limit() {
            // The `pixel_limit` decoder option rejects oversized images with `ImageSizeTooLarge`.
            for error in [
                JxlError::ImageSizeTooLarge(1 << 20, 1 << 20),
                JxlError::ImageDimensionTooLarge(1 << 40),
                JxlError::SizeOverflow,
            ] {
                assert_mapped(
                    map_decode_error(error),
                    gamut_core::ErrorKind::InvalidInput,
                    "JXL: image exceeds the decoder pixel limit",
                );
            }
        }

        #[test]
        fn not_grayscale_maps_to_unsupported() {
            assert_mapped(
                map_decode_error(JxlError::NotGrayscale),
                gamut_core::ErrorKind::Unsupported,
                "JXL: cannot decode a color image as grayscale",
            );
        }

        #[test]
        fn color_management_errors_map_to_unsupported() {
            for error in [
                JxlError::ICCOutputNoCMS,
                JxlError::NonXybOutputNoCMS,
                JxlError::CmsError("boom".into()),
            ] {
                assert_mapped(
                    map_decode_error(error),
                    gamut_core::ErrorKind::Unsupported,
                    "JXL: color management (ICC/CMS) is not supported",
                );
            }
        }

        #[test]
        fn unknown_errors_fall_through_to_generic() {
            for error in [JxlError::NoGlobalTree, JxlError::PointListEmpty] {
                assert_mapped(
                    map_decode_error(error),
                    gamut_core::ErrorKind::InvalidInput,
                    "JXL: invalid or unsupported codestream",
                );
            }
        }

        // `WrongBufferCount` / `InvalidOutputBufferSize` fire a `debug_assert!` (an internal
        // buffer-sizing bug in this crate's own decoder driver). As with the encoder half, the debug
        // panic is pinned with a `should_panic` test — which also kills the "delete this arm" mutant.
        // Gated to debug builds because the assertion is compiled out under `--release`.
        #[cfg(debug_assertions)]
        #[test]
        #[should_panic = "internal decoder buffer mismatch"]
        fn wrong_buffer_count_debug_asserts() {
            let _ = map_decode_error(JxlError::WrongBufferCount(1, 2));
        }
    }
}

#[cfg(feature = "decode")]
pub(crate) use decode::map_decode_error;
