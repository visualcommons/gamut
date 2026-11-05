//! Extracting a unified model from metadata blocks.

use gamut_exif::Exif;
use gamut_icc::IccProfile;
use gamut_iptc::{ConflictPolicy, FieldConflict, IimBlock, IptcReader, Sources};
use gamut_xmp::XmpMeta;

use crate::error::Result;
use crate::metadata::Metadata;
use crate::source::MetadataBlock;

/// Parses a set of [`MetadataBlock`]s into a unified [`Metadata`].
///
/// Each block is dispatched to the matching parser (EXIF → [`gamut_exif`], XMP → [`gamut_xmp`],
/// ICC → [`gamut_icc`]); a [`MetadataBlock::C2pa`] manifest store has no parser and is carried
/// through verbatim into [`Metadata::c2pa`]. The two IPTC carriers — the legacy binary IIM block
/// and the IPTC-Core properties inside the XMP packet — are reconciled into the **single** XMP
/// graph via [`gamut_iptc`], applying the extractor's [`ConflictPolicy`]; IPTC data therefore
/// always lands in [`Metadata::xmp`] and is read back through [`Metadata::iptc`].
///
/// Configure it fluently, then call [`extract`](Self::extract):
///
/// ```no_run
/// use gamut_metadata::iptc::ConflictPolicy;
/// use gamut_metadata::{MetadataBlock, MetadataExtractor};
///
/// # fn demo(exif: &[u8], xmp: &[u8]) -> Result<(), gamut_metadata::MetadataError> {
/// let meta = MetadataExtractor::new()
///     .policy(ConflictPolicy::IimWins)
///     .extract(&[MetadataBlock::Exif(exif), MetadataBlock::Xmp(xmp)])?;
/// # let _ = meta;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct MetadataExtractor {
    policy: ConflictPolicy,
}

impl MetadataExtractor {
    /// Creates an extractor with the default IPTC conflict policy ([`ConflictPolicy::XmpWins`]).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the [`ConflictPolicy`] used when the legacy IIM and XMP carriers disagree on a mapped
    /// IPTC field, and returns the extractor.
    #[must_use]
    pub fn policy(mut self, policy: ConflictPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Parses the blocks into a unified [`Metadata`].
    ///
    /// A repeated block kind takes the last occurrence. When an IPTC-IIM block is present it is
    /// reconciled into the XMP graph (see the type docs), so IPTC data always ends up in
    /// [`Metadata::xmp`]. An XMP graph that ends up empty is reported as absent. A
    /// [`MetadataBlock::C2pa`] store is copied into [`Metadata::c2pa`] byte-for-byte, unparsed.
    ///
    /// # Errors
    ///
    /// Returns a [`MetadataError`](crate::MetadataError) naming the carrier whose parse failed.
    pub fn extract(&self, blocks: &[MetadataBlock<'_>]) -> Result<Metadata> {
        let mut exif_bytes = None;
        let mut xmp_bytes = None;
        let mut icc_bytes = None;
        let mut iim_bytes = None;
        let mut c2pa_bytes = None;
        for block in blocks {
            match *block {
                MetadataBlock::Exif(b) => exif_bytes = Some(b),
                MetadataBlock::Xmp(b) => xmp_bytes = Some(b),
                MetadataBlock::Icc(b) => icc_bytes = Some(b),
                MetadataBlock::IptcIim(b) => iim_bytes = Some(b),
                MetadataBlock::C2pa(b) => c2pa_bytes = Some(b),
            }
        }

        let exif = exif_bytes.map(Exif::parse).transpose()?;
        let icc = icc_bytes.map(IccProfile::parse).transpose()?;
        let mut xmp = xmp_bytes.map(XmpMeta::parse).transpose()?;

        // Reconcile the legacy IIM carrier into the XMP graph — the single home for IPTC data.
        if let Some(iim) = iim_bytes {
            let iim = IimBlock::parse(iim)?;
            let reconciled = IptcReader::new().with_policy(self.policy).read(Sources {
                iim: Some(&iim),
                xmp: xmp.as_ref(),
            })?;
            let graph = xmp.get_or_insert_with(XmpMeta::new);
            for property in reconciled.to_xmp().properties {
                graph.set(property);
            }
        }

        // Keep `Metadata::xmp.is_none()` meaningful: an empty graph is no XMP at all.
        if xmp.as_ref().is_some_and(|g| g.properties.is_empty()) {
            xmp = None;
        }

        // Extraction parses carriers only: a block never yields an extension.
        let mut meta = Metadata::from_carriers(exif, xmp, icc);
        // The manifest store has no parser here — it is handed on exactly as the container found
        // it, so nothing about the signed bytes can be perturbed by this crate.
        meta.c2pa = c2pa_bytes.map(<[u8]>::to_vec);
        Ok(meta)
    }

    /// Reports the mapped IPTC fields on which the legacy IIM and XMP carriers disagree, without
    /// resolving them — the diagnostic companion to [`extract`](Self::extract).
    ///
    /// Returns an empty vector unless the blocks carry **both** an IPTC-IIM block and an XMP packet
    /// with IPTC properties.
    ///
    /// # Errors
    ///
    /// As [`extract`](Self::extract) for the IPTC and XMP carriers.
    pub fn conflicts(&self, blocks: &[MetadataBlock<'_>]) -> Result<Vec<FieldConflict>> {
        let mut xmp_bytes = None;
        let mut iim_bytes = None;
        for block in blocks {
            match *block {
                MetadataBlock::Xmp(b) => xmp_bytes = Some(b),
                MetadataBlock::IptcIim(b) => iim_bytes = Some(b),
                MetadataBlock::Exif(_) | MetadataBlock::Icc(_) | MetadataBlock::C2pa(_) => {}
            }
        }
        let (Some(iim), Some(xmp)) = (iim_bytes, xmp_bytes) else {
            return Ok(Vec::new());
        };
        let iim = IimBlock::parse(iim)?;
        let xmp = XmpMeta::parse(xmp)?;
        Ok(IptcReader::new()
            .with_policy(self.policy)
            .conflicts(&iim, &xmp)?)
    }
}
