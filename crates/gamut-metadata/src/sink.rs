//! The one implementation of facade-to-encoder embedding: [`MetadataSink`].

use gamut_core::Error;

use crate::embed::{EncodedMetadata, MetadataEmbedder};
use crate::metadata::Metadata;
use crate::source::Carrier;

/// The `Exif\0\0` marker [`EncodedMetadata::exif`] opens with, which no sink setter receives.
const EXIF_MARKER: &[u8] = b"Exif\0\0";

/// An encoder builder that writes the facade's carriers: implement the raw carrier setters and
/// state which carriers it writes, and [`with_metadata`](Self::with_metadata) /
/// [`with_encoded_metadata`](Self::with_encoded_metadata) come with the precedence every gamut
/// encoder shares.
///
/// The trait is by-value, like the builders it extends: each method consumes the builder and
/// hands it back, or an error. It is not object-safe and is not meant to be — a caller names the
/// concrete encoder.
///
/// # Precedence
///
/// Implemented once, here, for every encoder:
///
/// 1. **Refusal first.** Before any setter runs, a block whose carrier the encoder does not
///    write — absent from [`CARRIERS`](Self::CARRIERS) — is refused with
///    [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported), checked in [`Carrier`]
///    order (EXIF, XMP, ICC, IPTC-IIM, C2PA), so an IPTC-IIM stream is reported before a manifest
///    store. A [C2PA manifest store](EncodedMetadata::c2pa) is **always** refused, whatever
///    `CARRIERS` lists: a store is signed over the bytes of the one file it was made for (C2PA 2.4
///    §9.1, §15.12.1.1), so an encoder that can place one takes a store computed for its own
///    output through an API of its own, never from metadata blocks.
/// 2. **Absent leaves alone.** A carrier the blocks do not hold calls no setter, so an earlier raw
///    setting survives.
/// 3. **Present overwrites.** Each present carrier goes to its setter, in [`Carrier`] order; the
///    setter decides what replacing an earlier value means for its format, and may itself refuse
///    (an encoder whose ICC profile is its colour encoding, say, refusing to override an explicit
///    colour choice).
///
/// ```
/// use gamut_core::Result;
/// use gamut_metadata::xmp::{WellKnownNs, XmpMeta};
/// use gamut_metadata::{Carrier, Metadata, MetadataSink};
///
/// /// An encoder that keeps only an XMP packet.
/// #[derive(Default)]
/// struct XmpOnly {
///     xmp: Option<Vec<u8>>,
/// }
///
/// impl MetadataSink for XmpOnly {
///     const CARRIERS: &'static [Carrier] = &[Carrier::Xmp];
///
///     fn write_exif(self, _tiff: &[u8]) -> Result<Self> {
///         Ok(self) // never called: `CARRIERS` refuses EXIF first
///     }
///     fn write_xmp(self, packet: &[u8]) -> Result<Self> {
///         Ok(Self { xmp: Some(packet.to_vec()) })
///     }
///     fn write_icc(self, _profile: &[u8]) -> Result<Self> {
///         Ok(self) // never called: `CARRIERS` refuses ICC first
///     }
/// }
///
/// let mut graph = XmpMeta::new();
/// graph.set_text(WellKnownNs::Xmp.uri(), "CreatorTool", "gamut");
/// let meta = Metadata::from_carriers(None, Some(graph), None);
///
/// let encoder = XmpOnly::default().with_metadata(&meta)?;
/// assert_eq!(encoder.xmp, meta.to_blocks()?.xmp);
/// # Ok::<(), gamut_core::Error>(())
/// ```
pub trait MetadataSink: Sized {
    /// The carriers this encoder writes from metadata blocks. A present block whose carrier is not
    /// listed is refused before any setter runs. [`Carrier::C2pa`] is refused whether listed or
    /// not (see [Precedence](Self#precedence)).
    const CARRIERS: &'static [Carrier];

    /// Writes an EXIF carrier: the bare TIFF stream (`II`/`MM` first), the `Exif\0\0` marker of
    /// [`EncodedMetadata::exif`] already stripped. Returns the updated encoder.
    ///
    /// # Errors
    ///
    /// The encoder's own refusal of the payload.
    fn write_exif(self, tiff: &[u8]) -> gamut_core::Result<Self>;

    /// Writes an XMP packet. Returns the updated encoder.
    ///
    /// # Errors
    ///
    /// The encoder's own refusal of the payload (a packet that is not UTF-8 where the carrier holds
    /// text, say).
    fn write_xmp(self, packet: &[u8]) -> gamut_core::Result<Self>;

    /// Writes an ICC profile. Returns the updated encoder.
    ///
    /// # Errors
    ///
    /// The encoder's own refusal of the payload.
    fn write_icc(self, profile: &[u8]) -> gamut_core::Result<Self>;

    /// Writes a legacy IPTC-IIM dataset stream. Returns the updated encoder.
    ///
    /// Override it in an encoder whose [`CARRIERS`](Self::CARRIERS) lists [`Carrier::IptcIim`];
    /// for every other encoder it is never called.
    ///
    /// # Errors
    ///
    /// The default refuses with [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported), the
    /// same error an unlisted IPTC-IIM carrier gets.
    fn write_iptc_iim(self, datasets: &[u8]) -> gamut_core::Result<Self> {
        let _ = datasets;
        Err(unwritable(Carrier::IptcIim))
    }

    /// Embeds already-serialized facade blocks — the output of a [`MetadataEmbedder`] the caller
    /// configured — routing each present carrier to its setter under the shared
    /// [precedence](Self#precedence). Returns the updated encoder.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) for the first present carrier
    /// the encoder does not write, and for any manifest store; otherwise whatever a setter returns.
    fn with_encoded_metadata(self, encoded: &EncodedMetadata) -> gamut_core::Result<Self> {
        let present = [
            (Carrier::Exif, encoded.exif.is_some()),
            (Carrier::Xmp, encoded.xmp.is_some()),
            (Carrier::Icc, encoded.icc.is_some()),
            (Carrier::IptcIim, encoded.iptc_iim.is_some()),
            (Carrier::C2pa, encoded.c2pa.is_some()),
        ];
        if let Some(&(carrier, _)) = present.iter().find(|&&(carrier, is_present)| {
            is_present && (carrier == Carrier::C2pa || !Self::CARRIERS.contains(&carrier))
        }) {
            return Err(unwritable(carrier));
        }

        let mut sink = self;
        if let Some(exif) = &encoded.exif {
            sink = sink.write_exif(exif.strip_prefix(EXIF_MARKER).unwrap_or(exif))?;
        }
        if let Some(xmp) = &encoded.xmp {
            sink = sink.write_xmp(xmp)?;
        }
        if let Some(icc) = &encoded.icc {
            sink = sink.write_icc(icc)?;
        }
        if let Some(iim) = &encoded.iptc_iim {
            sink = sink.write_iptc_iim(iim)?;
        }
        Ok(sink)
    }

    /// Embeds a unified [`Metadata`] model: serializes it with the default [`MetadataEmbedder`]
    /// and hands the blocks to [`with_encoded_metadata`](Self::with_encoded_metadata). Returns the
    /// updated encoder.
    ///
    /// The default embedder emits no legacy IPTC-IIM block (IPTC lives inside the XMP packet) and
    /// **drops** a C2PA manifest store (see [`C2paPolicy`](crate::C2paPolicy)). A caller that must
    /// be told about either configures an embedder itself and calls
    /// [`with_encoded_metadata`](Self::with_encoded_metadata).
    ///
    /// # Errors
    ///
    /// The model's serialization failure, converted by
    /// `From<`[`MetadataError`](crate::MetadataError)`> for gamut_core::Error`, and whatever
    /// [`with_encoded_metadata`](Self::with_encoded_metadata) returns.
    fn with_metadata(self, meta: &Metadata) -> gamut_core::Result<Self> {
        let encoded = MetadataEmbedder::new().embed(meta)?;
        self.with_encoded_metadata(&encoded)
    }
}

/// The refusal for a present carrier the encoder does not write.
fn unwritable(carrier: Carrier) -> Error {
    let message = match carrier {
        Carrier::Exif => "metadata: the encoder writes no EXIF carrier",
        Carrier::Xmp => "metadata: the encoder writes no XMP carrier",
        Carrier::Icc => "metadata: the encoder writes no ICC carrier",
        Carrier::IptcIim => "metadata: the encoder writes no IPTC-IIM carrier",
        Carrier::C2pa => "metadata: a C2PA manifest store is not embedded from metadata blocks",
    };
    Error::unsupported(env!("CARGO_PKG_NAME"), message)
}

#[cfg(test)]
mod tests {
    use gamut_core::ErrorKind;
    use gamut_exif::{ByteOrder, Exif};
    use gamut_icc::{ColorSpace, DeviceClass, IccProfile, ProfileHeader, Tag, TagData};

    use super::*;

    /// Records every setter call, in order, so a test reads what the provided methods routed.
    #[derive(Debug, Default)]
    struct Recorder<const IIM: bool> {
        calls: Vec<(Carrier, Vec<u8>)>,
    }

    impl<const IIM: bool> Recorder<IIM> {
        fn record(mut self, carrier: Carrier, bytes: &[u8]) -> gamut_core::Result<Self> {
            self.calls.push((carrier, bytes.to_vec()));
            Ok(self)
        }
    }

    impl MetadataSink for Recorder<false> {
        const CARRIERS: &'static [Carrier] = &[Carrier::Exif, Carrier::Xmp, Carrier::Icc];

        fn write_exif(self, tiff: &[u8]) -> gamut_core::Result<Self> {
            self.record(Carrier::Exif, tiff)
        }
        fn write_xmp(self, packet: &[u8]) -> gamut_core::Result<Self> {
            self.record(Carrier::Xmp, packet)
        }
        fn write_icc(self, profile: &[u8]) -> gamut_core::Result<Self> {
            self.record(Carrier::Icc, profile)
        }
    }

    /// Lists every carrier, C2PA included, and overrides the IPTC-IIM setter.
    impl MetadataSink for Recorder<true> {
        const CARRIERS: &'static [Carrier] = Carrier::ALL;

        fn write_exif(self, tiff: &[u8]) -> gamut_core::Result<Self> {
            self.record(Carrier::Exif, tiff)
        }
        fn write_xmp(self, packet: &[u8]) -> gamut_core::Result<Self> {
            self.record(Carrier::Xmp, packet)
        }
        fn write_icc(self, profile: &[u8]) -> gamut_core::Result<Self> {
            self.record(Carrier::Icc, profile)
        }
        fn write_iptc_iim(self, datasets: &[u8]) -> gamut_core::Result<Self> {
            self.record(Carrier::IptcIim, datasets)
        }
    }

    /// Lists IPTC-IIM but keeps the default setter.
    #[derive(Debug)]
    struct ListsIimWithoutWriting;

    impl MetadataSink for ListsIimWithoutWriting {
        const CARRIERS: &'static [Carrier] = &[Carrier::IptcIim];

        fn write_exif(self, _: &[u8]) -> gamut_core::Result<Self> {
            Ok(self)
        }
        fn write_xmp(self, _: &[u8]) -> gamut_core::Result<Self> {
            Ok(self)
        }
        fn write_icc(self, _: &[u8]) -> gamut_core::Result<Self> {
            Ok(self)
        }
    }

    fn blocks(
        exif: Option<&[u8]>,
        xmp: Option<&[u8]>,
        icc: Option<&[u8]>,
        iptc_iim: Option<&[u8]>,
        c2pa: Option<&[u8]>,
    ) -> EncodedMetadata {
        // A struct literal is available here, inside the defining crate.
        EncodedMetadata {
            exif: exif.map(<[u8]>::to_vec),
            xmp: xmp.map(<[u8]>::to_vec),
            icc: icc.map(<[u8]>::to_vec),
            iptc_iim: iptc_iim.map(<[u8]>::to_vec),
            c2pa: c2pa.map(<[u8]>::to_vec),
        }
    }

    fn refusal<T: core::fmt::Debug>(result: gamut_core::Result<T>) -> &'static str {
        let err = result.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Unsupported, "{err}");
        err.static_message().unwrap()
    }

    #[test]
    fn present_carriers_reach_their_setters_in_carrier_order_with_the_exif_marker_stripped() {
        let sink = Recorder::<true>::default()
            .with_encoded_metadata(&blocks(
                Some(b"Exif\0\0II*\0"),
                Some(b"<x/>"),
                Some(b"icc"),
                Some(b"iim"),
                None,
            ))
            .unwrap();
        assert_eq!(
            sink.calls,
            vec![
                (Carrier::Exif, b"II*\0".to_vec()),
                (Carrier::Xmp, b"<x/>".to_vec()),
                (Carrier::Icc, b"icc".to_vec()),
                (Carrier::IptcIim, b"iim".to_vec()),
            ]
        );
    }

    #[test]
    fn a_bare_tiff_stream_reaches_the_exif_setter_unchanged() {
        let sink = Recorder::<false>::default()
            .with_encoded_metadata(&blocks(Some(b"MM\0*"), None, None, None, None))
            .unwrap();
        assert_eq!(sink.calls, vec![(Carrier::Exif, b"MM\0*".to_vec())]);
    }

    #[test]
    fn absent_carriers_call_no_setter() {
        let sink = Recorder::<true>::default()
            .with_encoded_metadata(&EncodedMetadata::default())
            .unwrap();
        assert!(sink.calls.is_empty());
    }

    #[test]
    fn each_unlisted_carrier_is_refused_by_name() {
        // `Recorder<false>` writes EXIF/XMP/ICC only; the refusal names the carrier.
        assert_eq!(
            refusal(Recorder::<false>::default().with_encoded_metadata(&blocks(
                None,
                None,
                None,
                Some(b"iim"),
                None
            ))),
            "metadata: the encoder writes no IPTC-IIM carrier"
        );
        let none = ListsIimWithoutWriting;
        assert_eq!(
            refusal(none.with_encoded_metadata(&blocks(Some(b"II*\0"), None, None, None, None))),
            "metadata: the encoder writes no EXIF carrier"
        );
        assert_eq!(
            refusal(ListsIimWithoutWriting.with_encoded_metadata(&blocks(
                None,
                Some(b"<x/>"),
                None,
                None,
                None
            ))),
            "metadata: the encoder writes no XMP carrier"
        );
        assert_eq!(
            refusal(ListsIimWithoutWriting.with_encoded_metadata(&blocks(
                None,
                None,
                Some(b"icc"),
                None,
                None
            ))),
            "metadata: the encoder writes no ICC carrier"
        );
    }

    #[test]
    fn a_refusal_comes_before_any_setter_and_iim_before_a_manifest_store() {
        // EXIF is writable and present, yet nothing is written: the refusal is checked first.
        // With both an IIM stream and a store present, the IIM refusal is the one reported.
        assert_eq!(
            refusal(Recorder::<false>::default().with_encoded_metadata(&blocks(
                Some(b"II*\0"),
                None,
                None,
                Some(b"iim"),
                Some(b"jumb"),
            ))),
            "metadata: the encoder writes no IPTC-IIM carrier"
        );
    }

    #[test]
    fn a_manifest_store_is_refused_even_by_a_sink_that_lists_it() {
        assert_eq!(
            refusal(Recorder::<true>::default().with_encoded_metadata(&blocks(
                None,
                None,
                None,
                None,
                Some(b"jumb")
            ))),
            "metadata: a C2PA manifest store is not embedded from metadata blocks"
        );
    }

    #[test]
    fn the_default_iim_setter_refuses_like_an_unlisted_carrier() {
        assert_eq!(
            refusal(ListsIimWithoutWriting.with_encoded_metadata(&blocks(
                None,
                None,
                None,
                Some(b"iim"),
                None
            ))),
            "metadata: the encoder writes no IPTC-IIM carrier"
        );
    }

    #[test]
    fn with_metadata_routes_the_default_embedders_blocks() {
        let meta = Metadata::from_carriers(Some(Exif::new(ByteOrder::LittleEndian)), None, None);
        let tiff = meta.to_blocks().unwrap().exif.unwrap();
        let sink = Recorder::<false>::default().with_metadata(&meta).unwrap();
        assert_eq!(
            sink.calls,
            vec![(
                Carrier::Exif,
                tiff.strip_prefix(EXIF_MARKER).unwrap().to_vec()
            )]
        );
    }

    #[test]
    fn with_metadata_converts_a_serialization_failure_to_the_core_error() {
        // A duplicate tag signature is rejected by `gamut-icc`'s writer (ICC.1:2022 §7.3); the
        // error the sink returns is that crate's own conversion, not a facade-level rewrap.
        let duplicate = Tag::new(*b"wtpt", TagData::Xyz(Vec::new()));
        let icc = IccProfile {
            header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Rgb),
            tags: vec![duplicate.clone(), duplicate],
        };
        let meta = Metadata::from_carriers(None, None, Some(icc));
        let err = Recorder::<false>::default()
            .with_metadata(&meta)
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput, "{err}");
        assert_eq!(err.origin(), Some("gamut-icc"), "{err}");
    }
}
