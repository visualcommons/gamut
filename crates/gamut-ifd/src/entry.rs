//! The container variant, the IFD entries, and the directory that holds them.

use crate::value::Value;

/// Which variant of the TIFF container a stream uses, distinguished by the header magic number.
///
/// The two variants share an identical tag/IFD model; they differ only in the width of the
/// structural fields — BigTIFF widens every file offset (and the IFD entry count and per-field
/// value count) from 32 to 64 bits so a file may exceed 4 GiB (`references/tiff/bigtiff.html`).
/// The [`Variant::Big`] arm — and every 64-bit width it selects — exists only when the `bigtiff`
/// feature is enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Variant {
    /// Classic TIFF 6.0: magic `42`, an 8-byte header, 12-byte IFD entries, 32-bit offsets.
    Classic,
    /// BigTIFF: magic `43`, a 16-byte header, 20-byte IFD entries, 64-bit offsets.
    #[cfg(feature = "bigtiff")]
    Big,
}

impl Variant {
    /// The header magic number (`42` for classic, `43` for BigTIFF).
    #[must_use]
    pub fn magic(self) -> u16 {
        match self {
            Variant::Classic => 42,
            #[cfg(feature = "bigtiff")]
            Variant::Big => 43,
        }
    }

    /// The size of the image file header in bytes (8 classic, 16 BigTIFF).
    #[must_use]
    pub fn header_size(self) -> usize {
        match self {
            Variant::Classic => 8,
            #[cfg(feature = "bigtiff")]
            Variant::Big => 16,
        }
    }

    /// The on-disk size of one IFD entry in bytes (12 classic, 20 BigTIFF).
    #[must_use]
    pub fn entry_size(self) -> usize {
        match self {
            Variant::Classic => 12,
            #[cfg(feature = "bigtiff")]
            Variant::Big => 20,
        }
    }

    /// The size of an IFD's leading entry-count field in bytes (2 classic, 8 BigTIFF).
    #[must_use]
    pub fn count_size(self) -> usize {
        match self {
            Variant::Classic => 2,
            #[cfg(feature = "bigtiff")]
            Variant::Big => 8,
        }
    }

    /// The size of a file offset (first-IFD pointer, next-IFD pointer, value offset) in bytes
    /// (4 classic, 8 BigTIFF).
    #[must_use]
    pub fn offset_size(self) -> usize {
        match self {
            Variant::Classic => 4,
            #[cfg(feature = "bigtiff")]
            Variant::Big => 8,
        }
    }

    /// The largest value, in bytes, that is stored inline in an IFD entry rather than out of line
    /// (4 classic, 8 BigTIFF — equal to [`Self::offset_size`]).
    #[must_use]
    pub fn inline_threshold(self) -> usize {
        self.offset_size()
    }
}

/// One field (entry) of an Image File Directory: a tag and its value.
///
/// On disk this is a 12-byte (classic) or 20-byte (BigTIFF) record — tag, field type, value count,
/// and a value-or-offset word — but once decoded only the tag and the resolved [`Value`] matter;
/// the field type and count are recoverable from the value.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    /// The 16-bit tag identifying the field (e.g. `256` for `ImageWidth`).
    pub tag: u16,
    /// The field's value.
    pub value: Value,
}

/// A sub-IFD pointer: a tag whose value is the file offset (or array of offsets) of one or more
/// nested IFDs that are **not** part of the top-level next-IFD chain.
///
/// This is how TIFF/EP and DNG attach child directories. `SubIFDs` (330) points at the
/// full-resolution raw image IFD(s); `ExifIFD` (34665) and `GPSInfo` (34853) point at the EXIF and
/// GPS sub-directories. [`write`](crate::write) lays the children out and synthesises the pointer
/// field, patching it with their absolute offset(s). The generic [`read`](crate::read) does **not**
/// follow these — it cannot know which arbitrary `LONG` tags are offsets — so a decoder names the
/// pointer tags to [`read_tree`](crate::read_tree) (which rebuilds the groups exactly as written),
/// or follows a pointer by hand with [`read_ifd_at`](crate::read_ifd_at).
#[derive(Debug, Clone, PartialEq)]
pub struct SubIfd {
    /// The pointer tag (e.g. `330` for `SubIFDs`).
    pub tag: u16,
    /// The child IFD(s) the tag points at — exactly one for `ExifIFD`/`GPSInfo`, one or more for
    /// `SubIFDs`.
    pub ifds: Vec<Ifd>,
}

/// A parsed Image File Directory — one node in the IFD chain (a TIFF page, or an EXIF/GPS/Interop
/// sub-directory).
///
/// Fields are kept sorted in ascending tag order, as required on disk (TIFF 6.0 §2); the accessors
/// preserve that invariant. A directory may also carry [`sub_ifds`](Self::sub_ifds) — child
/// directories referenced by a pointer tag rather than the next-IFD chain (e.g. a DNG's raw
/// sub-IFD or its EXIF sub-IFD); the writer lays those out and synthesises their pointer fields.
///
/// # One tag, one store
///
/// A tag is held **either** as a field **or** as a sub-IFD group, never both: the writer emits one
/// entry per field and one per group, so a tag in both stores would reach disk as two entries
/// under one tag, which TIFF 6.0 §2 does not allow. The setters keep this by construction, and the
/// **later call wins**: [`set`](Self::set) drops any group under its tag, and
/// [`set_sub_ifd`](Self::set_sub_ifd) / [`add_sub_ifd`](Self::add_sub_ifd) drop any field under
/// theirs. The one way past it is editing a group's `tag` through
/// [`sub_ifds_mut`](Self::sub_ifds_mut), which that method forbids.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Ifd {
    fields: Vec<Field>,
    sub_ifds: Vec<SubIfd>,
}

impl Ifd {
    /// Creates an empty directory.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the directory's fields, sorted by ascending tag.
    #[must_use]
    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    /// Returns the value of `tag`, or `None` if absent.
    #[must_use]
    pub fn get(&self, tag: u16) -> Option<&Value> {
        self.fields
            .binary_search_by_key(&tag, |f| f.tag)
            .ok()
            .map(|i| &self.fields[i].value)
    }

    /// Returns `tag` coerced to a single `u32` (accepting `BYTE`/`SHORT`/`LONG`).
    #[must_use]
    pub fn get_u32(&self, tag: u16) -> Option<u32> {
        self.get(tag).and_then(Value::as_u32)
    }

    /// Returns `tag` coerced to a `Vec<u32>` (accepting `BYTE`/`SHORT`/`LONG`).
    #[must_use]
    pub fn get_u32_vec(&self, tag: u16) -> Option<Vec<u32>> {
        self.get(tag).and_then(Value::as_u32_vec)
    }

    /// Returns `tag` coerced to a single `u64`, without any width clamp (see [`Value::as_u64`]) —
    /// the form BigTIFF-scale offsets need.
    #[must_use]
    pub fn get_u64(&self, tag: u16) -> Option<u64> {
        self.get(tag).and_then(Value::as_u64)
    }

    /// Returns `tag` coerced to a `Vec<u64>`, without any width clamp (see
    /// [`Value::as_u64_vec`]).
    #[must_use]
    pub fn get_u64_vec(&self, tag: u16) -> Option<Vec<u64>> {
        self.get(tag).and_then(Value::as_u64_vec)
    }

    /// Inserts or replaces the value of `tag`, keeping the fields sorted.
    ///
    /// Any sub-IFD group under `tag` is dropped, children and all: a tag is a field or a group,
    /// never both, and the later call wins (see [`Ifd`]'s *One tag, one store*).
    pub fn set(&mut self, tag: u16, value: Value) {
        if let Ok(i) = self.sub_ifds.binary_search_by_key(&tag, |s| s.tag) {
            self.sub_ifds.remove(i);
        }
        match self.fields.binary_search_by_key(&tag, |f| f.tag) {
            Ok(i) => self.fields[i].value = value,
            Err(i) => self.fields.insert(i, Field { tag, value }),
        }
    }

    /// Removes `tag`, returning its value if it was present.
    ///
    /// This is how a consumer strips a field it has represented elsewhere — e.g. a sub-IFD
    /// pointer tag it followed on read, which must not survive as a stale offset when the
    /// directory is re-written (the writer synthesises pointer fields from
    /// [`sub_ifds`](Self::sub_ifds)).
    pub fn remove(&mut self, tag: u16) -> Option<Value> {
        self.fields
            .binary_search_by_key(&tag, |f| f.tag)
            .ok()
            .map(|i| self.fields.remove(i).value)
    }

    /// Returns the directory's sub-IFD pointer groups (see [`SubIfd`]), sorted by ascending
    /// pointer tag — one group per tag, like [`fields`](Self::fields), so a reconstructed tree
    /// ([`read_tree`](crate::read_tree)) compares equal to the one that was written regardless of
    /// attachment order.
    #[must_use]
    pub fn sub_ifds(&self) -> &[SubIfd] {
        &self.sub_ifds
    }

    /// Returns the sub-IFD groups mutably — how a tree-preserving rewrite edits child
    /// directories in place.
    ///
    /// The groups must stay sorted by ascending pointer tag (the [`sub_ifds`](Self::sub_ifds)
    /// invariant): edit each group's [`ifds`](SubIfd::ifds), not its `tag` — change a group's
    /// tag by removing and re-attaching it with [`set_sub_ifd`](Self::set_sub_ifd). Editing the
    /// tag in place also bypasses the one-tag-one-store rule (see [`Ifd`]), which only the setters
    /// enforce.
    pub fn sub_ifds_mut(&mut self) -> &mut [SubIfd] {
        &mut self.sub_ifds
    }

    /// Attaches `ifds` as the child directories of pointer `tag`, replacing any existing group for
    /// that tag.
    ///
    /// The pointer field (a `LONG`/`LONG8` array of the children's offsets) is synthesised by the
    /// writer, so any regular field under `tag` is dropped: a tag is a field or a group, never
    /// both, and the later call wins (see [`Ifd`]'s *One tag, one store*).
    pub fn set_sub_ifd(&mut self, tag: u16, ifds: Vec<Ifd>) {
        self.remove(tag);
        match self.sub_ifds.binary_search_by_key(&tag, |s| s.tag) {
            Ok(i) => self.sub_ifds[i].ifds = ifds,
            Err(i) => self.sub_ifds.insert(i, SubIfd { tag, ifds }),
        }
    }

    /// Appends a single child directory to pointer `tag`, creating the group if it does not exist.
    ///
    /// Like [`set_sub_ifd`](Self::set_sub_ifd), this drops any regular field under `tag`.
    pub fn add_sub_ifd(&mut self, tag: u16, ifd: Ifd) {
        self.remove(tag);
        match self.sub_ifds.binary_search_by_key(&tag, |s| s.tag) {
            Ok(i) => self.sub_ifds[i].ifds.push(ifd),
            Err(i) => self.sub_ifds.insert(
                i,
                SubIfd {
                    tag,
                    ifds: vec![ifd],
                },
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variant_layout_constants() {
        assert_eq!(Variant::Classic.magic(), 42);
        assert_eq!(Variant::Classic.header_size(), 8);
        assert_eq!(Variant::Classic.entry_size(), 12);
        assert_eq!(Variant::Classic.count_size(), 2);
        assert_eq!(Variant::Classic.offset_size(), 4);
        // The inline threshold equals the offset size: values up to that many bytes pack inline.
        assert_eq!(Variant::Classic.inline_threshold(), 4);
    }

    #[cfg(feature = "bigtiff")]
    #[test]
    fn bigtiff_variant_layout_constants() {
        assert_eq!(Variant::Big.magic(), 43);
        assert_eq!(Variant::Big.header_size(), 16);
        assert_eq!(Variant::Big.entry_size(), 20);
        assert_eq!(Variant::Big.count_size(), 8);
        assert_eq!(Variant::Big.offset_size(), 8);
        assert_eq!(Variant::Big.inline_threshold(), 8);
    }

    #[test]
    fn ifd_keeps_fields_sorted_and_replaces() {
        // Tag numbers are used literally here: tag semantics live in the consuming codec
        // (e.g. gamut-tiff's `tags` module), not in this structural core. 256/257/259 are
        // ImageWidth/ImageLength/Compression.
        let mut ifd = Ifd::new();
        ifd.set(259, Value::Short(vec![1]));
        ifd.set(256, Value::Short(vec![4]));
        ifd.set(257, Value::Short(vec![3]));
        let order: Vec<u16> = ifd.fields().iter().map(|f| f.tag).collect();
        assert_eq!(order, vec![256, 257, 259]);
        ifd.set(256, Value::Short(vec![8]));
        assert_eq!(ifd.get_u32(256), Some(8));
        assert_eq!(ifd.fields().len(), 3);
    }

    #[test]
    fn remove_returns_value_and_keeps_order() {
        let mut ifd = Ifd::new();
        ifd.set(256, Value::Short(vec![4]));
        ifd.set(257, Value::Short(vec![3]));
        ifd.set(259, Value::Short(vec![1]));
        assert_eq!(ifd.remove(257), Some(Value::Short(vec![3])));
        assert_eq!(ifd.remove(257), None); // second removal misses
        assert_eq!(ifd.get(257), None);
        // The remaining fields keep the sorted invariant (a later set still lands mid-list).
        ifd.set(258, Value::Short(vec![8]));
        let order: Vec<u16> = ifd.fields().iter().map(|f| f.tag).collect();
        assert_eq!(order, vec![256, 258, 259]);
    }

    #[test]
    fn get_u32_vec_reads_arrays_and_misses() {
        let mut ifd = Ifd::new();
        ifd.set(258, Value::Short(vec![8, 8, 8]));
        assert_eq!(ifd.get_u32_vec(258), Some(vec![8, 8, 8]));
        // Absent tag is None (distinguishes the real coercion from a constant Some/None).
        assert_eq!(ifd.get_u32_vec(999), None);
    }

    #[test]
    fn get_u64_reads_scalars_and_arrays() {
        let mut ifd = Ifd::new();
        ifd.set(273, Value::Long(vec![64, 128]));
        ifd.set(279, Value::Long(vec![100]));
        assert_eq!(ifd.get_u64(279), Some(100));
        assert_eq!(ifd.get_u64_vec(273), Some(vec![64, 128]));
        assert_eq!(ifd.get_u64(999), None);
        assert_eq!(ifd.get_u64_vec(999), None);
    }

    /// `sub_ifds_mut` exposes the real groups (not a detached view): an in-place child edit is
    /// visible through the shared accessors.
    #[test]
    fn sub_ifds_mut_edits_children_in_place() {
        let mut ifd = Ifd::new();
        let mut child = Ifd::new();
        child.set(256, Value::Short(vec![1]));
        ifd.set_sub_ifd(330, vec![child]);
        assert_eq!(ifd.sub_ifds_mut().len(), 1);
        ifd.sub_ifds_mut()[0].ifds[0].set(256, Value::Short(vec![9]));
        assert_eq!(ifd.sub_ifds()[0].ifds[0].get_u32(256), Some(9));
    }

    // One tag, one store (issue #601): each setter evicts the other store's entry under its tag
    // and leaves every other tag alone. 330 is the pointer under test, 256 a bystander in each
    // store, so an eviction that keyed on the wrong tag (or cleared the store) is seen too.

    /// A directory holding `330` and `256` both as a field and as a group — the shape only
    /// [`Ifd::sub_ifds_mut`] can reach, built here without the setters under test.
    fn both_stores() -> Ifd {
        let group = |tag| SubIfd {
            tag,
            ifds: vec![Ifd::new()],
        };
        Ifd {
            fields: vec![
                Field {
                    tag: 256,
                    value: Value::Short(vec![1]),
                },
                Field {
                    tag: 330,
                    value: Value::Long(vec![8]),
                },
            ],
            sub_ifds: vec![group(256), group(330)],
        }
    }

    fn field_tags(ifd: &Ifd) -> Vec<u16> {
        ifd.fields().iter().map(|f| f.tag).collect()
    }

    fn group_tags(ifd: &Ifd) -> Vec<u16> {
        ifd.sub_ifds().iter().map(|s| s.tag).collect()
    }

    #[test]
    fn set_drops_the_group_under_its_tag() {
        let mut ifd = both_stores();
        ifd.set(330, Value::Long(vec![16]));
        assert_eq!(group_tags(&ifd), vec![256]);
        assert_eq!(field_tags(&ifd), vec![256, 330]);
        assert_eq!(ifd.get_u32(330), Some(16));
    }

    #[test]
    fn set_sub_ifd_drops_the_field_under_its_tag() {
        let mut ifd = both_stores();
        ifd.set_sub_ifd(330, vec![Ifd::new(), Ifd::new()]);
        assert_eq!(field_tags(&ifd), vec![256]);
        assert_eq!(group_tags(&ifd), vec![256, 330]);
        assert_eq!(ifd.sub_ifds()[1].ifds.len(), 2);
    }

    #[test]
    fn add_sub_ifd_drops_the_field_under_its_tag() {
        let mut ifd = both_stores();
        ifd.add_sub_ifd(330, Ifd::new());
        assert_eq!(field_tags(&ifd), vec![256]);
        assert_eq!(group_tags(&ifd), vec![256, 330]);
        assert_eq!(ifd.sub_ifds()[1].ifds.len(), 2);
    }

    #[test]
    fn add_sub_ifd_accumulates_per_tag() {
        let mut ifd = Ifd::new();
        // Attach out of tag order: the groups must come back sorted (canonical form, so a
        // reconstructed tree compares equal regardless of attachment order).
        ifd.add_sub_ifd(34665, Ifd::new());
        ifd.add_sub_ifd(330, Ifd::new());
        ifd.add_sub_ifd(330, Ifd::new());
        // One group per distinct tag (not one per call), and tag 330 holds both children.
        assert_eq!(ifd.sub_ifds.len(), 2);
        let order: Vec<u16> = ifd.sub_ifds().iter().map(|s| s.tag).collect();
        assert_eq!(order, vec![330, 34665]);
        assert_eq!(
            ifd.sub_ifds
                .iter()
                .find(|s| s.tag == 330)
                .unwrap()
                .ifds
                .len(),
            2
        );
    }
}
