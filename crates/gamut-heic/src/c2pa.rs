//! Locating the C2PA manifest store carried by a HEIF file ([`HeifContainer::c2pa`]) and
//! summarising its C2PA boxes ([`HeifContainer::c2pa_summary`]).
//!
//! C2PA (Coalition for Content Provenance and Authenticity, spec version 2.4) carries its manifest
//! store in an ISOBMFF file inside a top-level `uuid` box — the `ContentProvenanceBox` of §A.5.1 —
//! whose 16-byte extended (user) type is [`C2PA_UUID`](crate::C2PA_UUID). These are lenses only:
//! they find that box, strip the framing the specification puts around the store, and report the
//! store as opaque bytes plus its exact byte range. Nothing inside the store is parsed beyond the
//! outer JUMBF length field that bounds it, no hash is verified, no signature checked, and no
//! verdict reached — validation belongs to a C2PA validator downstream.
//!
//! The locator and the summary are `gamut-heif`'s ([`gamut_heif::c2pa_manifest_stores`],
//! [`C2paSummary::from_segments`]), shared with `gamut-avif`, so the two crates report the same
//! bytes for one store; this module applies them to the container's
//! [`segments`](HeifContainer::segments).

use gamut_heif::{C2paManifestStore, C2paSummary};

use crate::container::HeifContainer;

impl<'a> HeifContainer<'a> {
    /// The first C2PA manifest store in the file, in file order, or `None` if the file carries none.
    ///
    /// A file that is mid-update legitimately carries two stores — an `original` box and an `update`
    /// box (C2PA 2.4 §A.5.3) — and deciding which of them is *active* is a validator's judgement,
    /// not a container reader's. This accessor therefore promises only "the first one"; use
    /// [`c2pa_manifest_stores`](Self::c2pa_manifest_stores) to see them all and their purposes.
    ///
    /// See [`C2paManifestStore`] for exactly what is stripped, what bounds the store, and why the
    /// reported range must not be treated as a BMFF exclusion range.
    #[must_use]
    pub fn c2pa(&self) -> Option<C2paManifestStore<'a>> {
        self.c2pa_manifest_stores().next()
    }

    /// Every C2PA manifest store among the **top-level boxes of the primary stream**, in file
    /// order: [`gamut_heif::c2pa_manifest_stores`] over [`segments`](Self::segments), whose docs
    /// state what is and is not scanned.
    ///
    /// A `uuid` box nested inside `meta` is not a manifest store and is surfaced through
    /// [`unknown_meta_boxes`](Self::unknown_meta_boxes) instead. A box that *is* a C2PA box and
    /// still yields nothing is reported, with the reason, by
    /// [`c2pa_summary`](Self::c2pa_summary), so an empty iterator here does not mean a file
    /// carrying no provenance at all.
    pub fn c2pa_manifest_stores(&self) -> impl Iterator<Item = C2paManifestStore<'a>> + '_ {
        gamut_heif::c2pa_manifest_stores(self.segments())
    }

    /// A non-validating summary of the file's C2PA boxes, in file order: every manifest store,
    /// every C2PA box that yielded none with the reason, every auxiliary `merkle` box, and a count
    /// of the top-level `uuid` boxes of another extended type — [`C2paSummary::from_segments`] over
    /// [`segments`](Self::segments), the same scan as
    /// [`c2pa_manifest_stores`](Self::c2pa_manifest_stores). See [`C2paSummary`] for what it is
    /// for, and [`C2PA_NOT_VALIDATED`](crate::C2PA_NOT_VALIDATED) for what has to be said beside it.
    #[must_use]
    pub fn c2pa_summary(&self) -> C2paSummary {
        C2paSummary::from_segments(self.segments())
    }
}
