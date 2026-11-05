//! The ICC profile writer.

use std::collections::{HashMap, HashSet};

use crate::bytes::pad_to_4;
use crate::error::{IccError, Result};
use crate::header::ProfileId;
use crate::primitives::Signature;
use crate::profile::IccProfile;
use crate::tag_types::encode_tag;
use crate::tags::tag_table_end;

/// Writer for an ICC profile, with serialization options.
///
/// `IccWriter::new().write(profile)` is equivalent to [`IccProfile::to_bytes`]. Enable
/// [`recompute_profile_id`](IccWriter::recompute_profile_id) to stamp a freshly computed MD5 ID
/// (ICC.1:2022 §7.2.18) into the output instead of preserving the header's stored ID.
#[derive(Debug, Clone, Default)]
pub struct IccWriter {
    recompute_profile_id: bool,
}

impl IccWriter {
    /// A writer that preserves the profile's stored ID.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets whether to recompute and stamp the profile ID into the output.
    #[must_use]
    pub fn recompute_profile_id(mut self, yes: bool) -> Self {
        self.recompute_profile_id = yes;
        self
    }

    /// Serializes `profile` to a fresh, spec-valid byte vector.
    ///
    /// # Errors
    ///
    /// Returns [`IccError::Malformed`] if the model violates an invariant serialization relies on
    /// — a duplicate tag signature, or tag data whose shape contradicts its declared counts (see
    /// [`IccProfile::to_bytes`]). A profile produced by parsing always serializes.
    pub fn write(&self, profile: &IccProfile) -> Result<Vec<u8>> {
        write_profile(profile, self.recompute_profile_id)
    }
}

/// Two-pass serialization (ICC.1:2022 §7): encode each element, lay out the header, the tag table,
/// and the 4-byte-aligned element data (sharing byte-identical elements), then patch the size and
/// optionally the profile ID.
pub(crate) fn write_profile(profile: &IccProfile, recompute_id: bool) -> Result<Vec<u8>> {
    // The parser rejects duplicate tag signatures (§7.3), so serializing them would produce a
    // profile that cannot be re-read; catch the hand-built case here.
    let mut seen_sigs = HashSet::with_capacity(profile.tags.len());
    if !profile
        .tags
        .iter()
        .all(|tag| seen_sigs.insert(tag.signature.0))
    {
        return Err(IccError::Malformed("icc: duplicate tag signature"));
    }

    // Pass 1: encode every tag element.
    let mut elements: Vec<(Signature, Vec<u8>)> = Vec::with_capacity(profile.tags.len());
    for tag in &profile.tags {
        let mut bytes = Vec::new();
        encode_tag(&tag.data, &mut bytes)?;
        elements.push((tag.signature, bytes));
    }

    // Pass 2: place element data after the header and tag table, 4-byte aligned, de-duplicating
    // byte-identical elements (as real writers share e.g. the three `*TRC` curves).
    let data_start = tag_table_end(elements.len()).next_multiple_of(4);
    let mut blob = Vec::new();
    let mut layout: Vec<(u32, u32)> = Vec::with_capacity(elements.len());
    let mut seen: HashMap<&[u8], u32> = HashMap::new();
    for (_, bytes) in &elements {
        let size = bytes.len() as u32;
        if let Some(&offset) = seen.get(bytes.as_slice()) {
            layout.push((offset, size));
        } else {
            let offset = (data_start + blob.len()) as u32;
            seen.insert(bytes.as_slice(), offset);
            layout.push((offset, size));
            blob.extend_from_slice(bytes);
            pad_to_4(&mut blob);
        }
    }

    // Assemble: header, tag table, then the element-data region.
    let mut out = Vec::with_capacity(data_start + blob.len());
    profile.header.write(&mut out);
    out.extend_from_slice(&(elements.len() as u32).to_be_bytes());
    for ((sig, _), (offset, size)) in elements.iter().zip(&layout) {
        out.extend_from_slice(&sig.0);
        out.extend_from_slice(&offset.to_be_bytes());
        out.extend_from_slice(&size.to_be_bytes());
    }
    out.resize(data_start, 0); // pad the tag table up to the element-data start
    out.extend_from_slice(&blob);

    // Patch the total size, then optionally the profile ID.
    let total = out.len() as u32;
    out[0..4].copy_from_slice(&total.to_be_bytes());
    if recompute_id {
        let id = ProfileId::compute(&out);
        out[84..100].copy_from_slice(&id.0);
    }
    Ok(out)
}
