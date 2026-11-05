//! Carrier-agnostic metadata blocks, and the carrier vocabulary they share.

/// A located metadata payload, handed over by a container crate for parsing.
///
/// Each variant borrows the raw bytes a container has already extracted — a WebP `EXIF`/`XMP `/
/// `ICCP` chunk, an AVIF/HEIF `Exif`/`mime`/`colr` item payload, a JPEG `APP1`/`APP2`/`APP13`
/// segment. The facade stays container-agnostic: it never parses boxes or chunks, only these
/// payloads. (IPTC Core/Extension arrives inside an [`MetadataBlock::Xmp`] payload; the separate
/// [`MetadataBlock::IptcIim`] is the legacy binary form from a Photoshop IRB.)
///
/// Marked `#[non_exhaustive]`, so a later carrier can add a variant without a breaking change;
/// match with a wildcard arm. There is deliberately **no** variant for
/// [`Metadata::extensions`](crate::Metadata::extensions): extensions hold data no carrier
/// serializes, so no block can produce one. A C2PA manifest store is the opposite case — a
/// container does hold it — so it has a variant, [`MetadataBlock::C2pa`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MetadataBlock<'a> {
    /// An EXIF blob (`Exif\0\0` + TIFF stream, or a bare TIFF stream).
    Exif(&'a [u8]),
    /// An XMP packet (RDF/XML).
    Xmp(&'a [u8]),
    /// An ICC profile blob.
    Icc(&'a [u8]),
    /// A legacy IPTC-IIM dataset stream (e.g. the `0x0404` Photoshop image resource's payload).
    IptcIim(&'a [u8]),
    /// A C2PA manifest store: the JUMBF superbox a container located (C2PA 2.4 §11.1.4.2), taken
    /// verbatim.
    ///
    /// The facade never looks inside it. Extraction hands the bytes to
    /// [`Metadata::c2pa`](crate::Metadata::c2pa); embedding does **not** hand them back — see
    /// [`C2paPolicy`](crate::C2paPolicy) for why a store must not be copied into a rewritten file.
    ///
    /// **Hand over the complete store.** Where a carrier splits it across segments — C2PA's JPEG
    /// carriage spans as many `APP11` segments as the store needs, which is the ordinary case once
    /// a manifest embeds a thumbnail — the container reassembles them first, exactly as it already
    /// does for a multi-segment `APP2 ICC_PROFILE`. One block per segment would not error: a
    /// repeated block kind takes the last occurrence, so the model would quietly hold a fragment.
    C2pa(&'a [u8]),
}

/// A metadata carrier: one genuinely distinct serialization a container holds, named without its
/// payload — one per [`MetadataBlock`] variant.
///
/// The vocabulary a [`MetadataSink`](crate::MetadataSink) states its writable carriers in, and the
/// `gamut` umbrella's capability table states each format crate's in.
///
/// `#[non_exhaustive]` and `#[repr(u8)]` with **permanent, append-only** discriminants: a later
/// carrier is added at the end and never renumbers an existing one, so the value is stable across
/// the C ABI. Match with a wildcard arm; iterate with [`Carrier::ALL`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[repr(u8)]
pub enum Carrier {
    /// An EXIF blob (a TIFF stream, with or without the `Exif\0\0` marker).
    Exif = 0,
    /// An XMP packet — which is also where IPTC Core/Extension lives.
    Xmp = 1,
    /// An ICC profile.
    Icc = 2,
    /// The legacy binary IPTC-IIM dataset stream.
    IptcIim = 3,
    /// A C2PA manifest store (JUMBF superbox). Read-only by nature: the facade never copies a store
    /// forward (see [`C2paPolicy`](crate::C2paPolicy)), so no encoder **writes** one as a carrier.
    /// An encoder that can place a store does so only as a signing workflow — a store computed for
    /// that exact output — which is not carrying metadata from one file to another.
    C2pa = 4,
}

impl Carrier {
    /// Every carrier, in discriminant order — the way to enumerate a `#[non_exhaustive]` enum.
    ///
    /// A slice, not a fixed-length array, precisely because the enum is `#[non_exhaustive]`:
    /// appending a carrier would change an array constant's *type*, breaking every caller who
    /// named it, which is the opposite of what `#[non_exhaustive]` promises.
    pub const ALL: &'static [Self] = &[Self::Exif, Self::Xmp, Self::Icc, Self::IptcIim, Self::C2pa];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carrier_discriminants_are_the_documented_append_only_values() {
        // The `repr(u8)` values are a public contract (C ABI); pin them so a reorder is a failure.
        // Collected rather than `map`ped over an array: `ALL` is a slice, so its length is part of
        // what this compares, and an appended variant without its discriminant fails here.
        assert_eq!(
            Carrier::ALL.iter().map(|&c| c as u8).collect::<Vec<_>>(),
            (0..5).collect::<Vec<u8>>()
        );
    }
}
