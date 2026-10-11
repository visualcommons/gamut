//! Parsing of the TIFF byte-order header and the IFD chain.
//!
//! The structure is offset-driven — a classic parser-exploit surface — so every access is
//! bounds-checked, the IFD chain is guarded against loops and runaway length, and unknown field
//! types are preserved verbatim ([`Value::Unknown`]) rather than trusted or dropped.

use std::collections::BTreeSet;

use gamut_core::{Error, Result};

use crate::{ByteOrder, Ifd, Value, Variant};

/// A parsed TIFF/IFD stream: its byte order, container variant, and the chain of Image File
/// Directories.
#[derive(Debug, Clone, PartialEq)]
pub struct TiffFile {
    /// The byte order the stream was written in.
    pub order: ByteOrder,
    /// Whether the stream is classic TIFF or BigTIFF (which sizes its offsets/counts).
    pub variant: Variant,
    /// The Image File Directories, in stream order (one per subfile/page).
    pub ifds: Vec<Ifd>,
}

/// An upper bound on the number of IFDs followed, to bound malformed/looping chains.
const MAX_IFDS: usize = 1 << 16;

/// Guards a top-level next-IFD walk against loops and runaway length, shared by the slice
/// reader ([`read`]) and the streaming reader ([`IfdChain`](crate::IfdChain)) so the two paths
/// cannot drift.
///
/// A hash set keeps the loop guard O(1) per link: a hostile chain can be [`MAX_IFDS`] long, and
/// a linear scan per link would make it quadratic.
#[derive(Debug)]
pub(crate) struct ChainGuard {
    seen: std::collections::HashSet<u64>,
    count: usize,
}

impl ChainGuard {
    pub(crate) fn new() -> Self {
        Self {
            seen: std::collections::HashSet::new(),
            count: 0,
        }
    }

    /// Admits the next directory offset into the walk, or rejects a repeated offset (a loop) or
    /// the [`MAX_IFDS`] + 1'th directory (runaway length).
    pub(crate) fn admit(&mut self, offset: u64) -> Result<()> {
        if !self.seen.insert(offset) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "TIFF: IFD chain loops",
            ));
        }
        if self.count >= MAX_IFDS {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "TIFF: too many IFDs",
            ));
        }
        self.count += 1;
        Ok(())
    }
}

/// Reads a 16-bit value at `pos` in `order`, bounds-checked.
pub(crate) fn u16_at(data: &[u8], pos: usize, order: ByteOrder) -> Result<u16> {
    let b = pos
        .checked_add(2)
        .and_then(|end| data.get(pos..end))
        .ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: truncated 16-bit field")
                .with_byte_offset(pos as u64)
        })?;
    Ok(order.u16([b[0], b[1]]))
}

/// Reads a 32-bit value at `pos` in `order`, bounds-checked.
pub(crate) fn u32_at(data: &[u8], pos: usize, order: ByteOrder) -> Result<u32> {
    let b = pos
        .checked_add(4)
        .and_then(|end| data.get(pos..end))
        .ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: truncated 32-bit field")
                .with_byte_offset(pos as u64)
        })?;
    Ok(order.u32([b[0], b[1], b[2], b[3]]))
}

/// Reads a 64-bit value at `pos` in `order`, bounds-checked (BigTIFF offsets/counts).
#[cfg(feature = "bigtiff")]
pub(crate) fn u64_at(data: &[u8], pos: usize, order: ByteOrder) -> Result<u64> {
    let b = pos
        .checked_add(8)
        .and_then(|end| data.get(pos..end))
        .ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: truncated 64-bit field")
                .with_byte_offset(pos as u64)
        })?;
    Ok(order.u64([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
}

/// Reads an offset-sized field at `pos` (a `u32` in classic TIFF, a `u64` in BigTIFF) as `u64`.
///
/// Used for every file offset and for the per-field value count, which share the offset width.
pub(crate) fn offset_at(
    data: &[u8],
    pos: usize,
    order: ByteOrder,
    variant: Variant,
) -> Result<u64> {
    match variant {
        Variant::Classic => Ok(u64::from(u32_at(data, pos, order)?)),
        #[cfg(feature = "bigtiff")]
        Variant::Big => u64_at(data, pos, order),
    }
}

/// Parses the image file header, returning the byte order, the container variant, and the offset
/// of the first IFD. The header is 8 bytes for classic TIFF and 16 bytes for BigTIFF.
///
/// Without the `bigtiff` feature a BigTIFF magic number (`43`) is rejected as an unknown magic.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if the byte-order mark, magic number, or (for BigTIFF) the
/// fixed offset-size / reserved fields are not valid.
pub fn read_header(data: &[u8]) -> Result<(ByteOrder, Variant, u64)> {
    let head = data
        .get(..8)
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: header too short"))?;
    let order = match [head[0], head[1]] {
        [0x49, 0x49] => ByteOrder::LittleEndian,
        [0x4D, 0x4D] => ByteOrder::BigEndian,
        _ => {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "TIFF: bad byte-order mark",
            ));
        }
    };
    match order.u16([head[2], head[3]]) {
        42 => Ok((order, Variant::Classic, u64::from(u32_at(data, 4, order)?))),
        #[cfg(feature = "bigtiff")]
        43 => {
            // BigTIFF: bytes 4-5 are the offset bytesize (always 8), bytes 6-7 are reserved (0),
            // and the first-IFD offset is the 8-byte value at bytes 8-15.
            if order.u16([head[4], head[5]]) != 8 {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "TIFF: BigTIFF offset size must be 8",
                ));
            }
            if order.u16([head[6], head[7]]) != 0 {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "TIFF: BigTIFF reserved field must be 0",
                ));
            }
            Ok((order, Variant::Big, u64_at(data, 8, order)?))
        }
        _ => Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "TIFF: bad magic number",
        )),
    }
}

/// Reads the single IFD located at `offset` in `data`, ignoring its next-IFD pointer.
///
/// This is how a codec follows a **sub-IFD pointer** (see [`SubIfd`](crate::SubIfd)): the generic
/// [`read`] cannot know which `LONG` tags are offsets, so it leaves a pointer tag as a plain
/// integer value; the codec reads that offset and calls this to parse the child directory (e.g. a
/// DNG raw sub-IFD via `SubIFDs`, or an `ExifIFD`). `order` and `variant` come from the enclosing
/// file's header (via [`read_header`]).
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if the directory at `offset` is out of bounds or a field value
/// is truncated.
pub fn read_ifd_at(data: &[u8], offset: u64, order: ByteOrder, variant: Variant) -> Result<Ifd> {
    let mut reader = crate::IfdReader::with_layout(data, order, variant);
    let raw = reader.read_ifd(offset)?;
    reader.decode_ifd(&raw)
}

/// Parses a TIFF/IFD stream: the header followed by the whole IFD chain.
///
/// This is a thin wrapper over the streaming engine ([`IfdReader`](crate::IfdReader)) — there is
/// exactly **one** parser, so the slice and streaming paths cannot drift.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if the header is invalid, an offset is out of bounds, the IFD
/// chain loops, or a field value is truncated.
pub fn read(data: &[u8]) -> Result<TiffFile> {
    crate::IfdReader::open(data)?.read_file()
}

/// Which sub-IFD pointer tags a subtree walk ([`resolve_subtree`], [`read_tree`]) follows at each
/// level, and how many levels below the starting directory it may descend.
///
/// The walk starts at a **root** directory (depth 0) and follows `root_tags` there, and
/// `nested_tags` at every directory it reaches below it. Two lists rather than one because the
/// two levels can answer opposite questions: a codec reading one page's metadata may want to
/// follow only `ExifIFD` at the page itself (a `SubIFDs` target it throws away must not be able to
/// fail it) yet every standard pointer inside the Exif directory (a pointer left unresolved there
/// is a stale absolute offset in a directory it hands back). [`read_tree`] uses one list for both.
///
/// A directory nested deeper than [`max_depth`](Self::max_depth) is refused, never walked: the
/// bound is what stops a few-kilobyte hostile file from nesting enough directories to exhaust the
/// stack. It defaults to, and can only be tightened below, [`MAX_DEPTH`](Self::MAX_DEPTH).
///
/// ```
/// use gamut_ifd::SubtreeScope;
/// use gamut_ifd::tags::{EXIF_IFD, STANDARD_POINTER_TAGS};
///
/// let scope = SubtreeScope::new(&[EXIF_IFD], STANDARD_POINTER_TAGS).with_max_depth(2);
/// assert_eq!(scope.tags_at(0), &[EXIF_IFD]);
/// assert_eq!(scope.tags_at(1), STANDARD_POINTER_TAGS);
/// assert_eq!(scope.max_depth(), 2);
/// // The cap is a ceiling a caller can lower, never raise.
/// assert_eq!(scope.with_max_depth(100).max_depth(), 2);
/// assert_eq!(SubtreeScope::new(&[], &[]).with_max_depth(100).max_depth(), SubtreeScope::MAX_DEPTH);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubtreeScope<'a> {
    root_tags: &'a [u16],
    nested_tags: &'a [u16],
    max_depth: usize,
}

impl<'a> SubtreeScope<'a> {
    /// The deepest nesting any walk follows: fifteen directories below the root, sixteen levels in
    /// all. The deepest legitimate trees (a DNG raw sub-IFD, or EXIF's IFD 0 → Exif → Interop
    /// chain) are three levels; a generic walk is handed arbitrary tags, so it allows headroom.
    pub const MAX_DEPTH: usize = 15;

    /// A scope following `root_tags` at the root directory and `nested_tags` at every directory
    /// below it, to the full [`MAX_DEPTH`](Self::MAX_DEPTH).
    ///
    /// A named tag whose value is not an offset array (`LONG`, the typed `IFD`, or BigTIFF
    /// `LONG8`/`IFD8`) is left in place as a regular field.
    #[must_use]
    pub const fn new(root_tags: &'a [u16], nested_tags: &'a [u16]) -> Self {
        Self {
            root_tags,
            nested_tags,
            max_depth: Self::MAX_DEPTH,
        }
    }

    /// Lowers the depth bound to `max_depth` levels below the root; a value above the current
    /// bound leaves it unchanged, so no scope ever walks deeper than
    /// [`MAX_DEPTH`](Self::MAX_DEPTH). At `0` the walk reaches no child at all, so any followed
    /// pointer in the root is refused.
    #[must_use]
    pub fn with_max_depth(self, max_depth: usize) -> Self {
        Self {
            max_depth: max_depth.min(self.max_depth),
            ..self
        }
    }

    /// The pointer tags followed in a directory at `depth` below the root: the root tags at depth
    /// 0, the nested tags everywhere else.
    #[must_use]
    pub const fn tags_at(&self, depth: usize) -> &'a [u16] {
        if depth == 0 {
            self.root_tags
        } else {
            self.nested_tags
        }
    }

    /// The deepest level below the root a directory may sit at.
    #[must_use]
    pub const fn max_depth(&self) -> usize {
        self.max_depth
    }
}

/// The file offsets a sub-IFD pointer value carries: a `LONG` array (TIFF 6.0 §2), the typed
/// `IFD` (13) form of TIFF Technical Note 1, or the 64-bit `LONG8`/`IFD8` forms BigTIFF writers
/// use. Any other type is not a pointer.
pub(crate) fn pointer_offsets(value: &Value) -> Option<Vec<u64>> {
    match value {
        Value::Long(v) | Value::Ifd(v) => Some(v.iter().map(|&x| u64::from(x)).collect()),
        #[cfg(feature = "bigtiff")]
        Value::Long8(v) | Value::Ifd8(v) => Some(v.clone()),
        _ => None,
    }
}

/// Like [`read`], but additionally follows the given sub-IFD **pointer tags**, reconstructing the
/// directory tree that [`write`](crate::write) flattens: each pointer field is replaced by a
/// [`sub_ifds`](Ifd::sub_ifds) group holding its parsed children, recursively.
///
/// The caller names the pointer tags because the structure alone cannot: which `LONG` tags hold
/// directory offsets is tag *semantics* (e.g. `SubIFDs` 330, `ExifIFD` 34665, `GPSInfo` 34853,
/// `Interoperability` 40965), which lives in the consuming codec. A named tag whose value is not
/// an offset array (`LONG`, or BigTIFF `LONG8`/`IFD8`) is left in place as a regular field.
///
/// This is [`write`](crate::write)'s inverse: `read_tree(&write(&file)?, tags)? == file` for any
/// tree whose pointer tags are all named in `tags`. For per-pointer control (e.g. tolerating a
/// malformed child), follow pointers manually with [`read_ifd_at`].
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] under the same conditions as [`read`], or if the pointer graph
/// is not a tree: a repeated or self-referential child offset, or nesting deeper than 16 levels
/// ([`SubtreeScope::MAX_DEPTH`] below each page).
pub fn read_tree(data: &[u8], pointer_tags: &[u16]) -> Result<TiffFile> {
    crate::IfdReader::open(data)?.read_tree(pointer_tags)
}

/// Resolves the sub-IFD pointers of **one directory's subtree**: each pointer field `scope` names
/// in `ifd`, and recursively in the children it reaches, is replaced by a
/// [`sub_ifds`](Ifd::sub_ifds) group holding the parsed children — what [`read_tree`] does for
/// every page of a file, applied to a single directory a caller already holds.
///
/// That scoping is the point. [`read_tree`] resolves its list at every node of every page, so a
/// dangling pointer on a page the caller discards fails the whole read; this walks only the
/// subtree below `ifd`, with a per-level tag list and depth bound ([`SubtreeScope`]).
///
/// `fetch` parses the directory at a file offset, so the walk serves any source: a closure over
/// [`read_ifd_at`] for a slice, or over [`IfdReader`](crate::IfdReader) for a stream. The guards
/// are the ones [`read_tree`] uses — they exist once — and apply to this call's walk: a cycle, two
/// pointers naming one directory, or nesting past the scope's depth bound fails rather than loops.
///
/// ```
/// use gamut_ifd::{ByteOrder, Ifd, SubtreeScope, TiffFile, Value, Variant};
/// use gamut_ifd::{read, read_ifd_at, resolve_subtree, write};
/// use gamut_ifd::tags::{EXIF_IFD, SUB_IFDS};
///
/// let mut exif = Ifd::new();
/// exif.set(33434, Value::Rational(vec![(1, 200)])); // ExposureTime
/// let mut page = Ifd::new();
/// page.set_sub_ifd(EXIF_IFD, vec![exif.clone()]);
/// page.set(SUB_IFDS, Value::Long(vec![0xFFFF_FFF0])); // dangling: points past the end
/// let bytes = write(&TiffFile {
///     order: ByteOrder::LittleEndian,
///     variant: Variant::Classic,
///     ifds: vec![page],
/// })
/// .unwrap();
///
/// // Follow only `ExifIFD` at the page, so the dangling `SubIFDs` cannot fail the walk.
/// let mut ifd0 = read(&bytes).unwrap().ifds.swap_remove(0);
/// let scope = SubtreeScope::new(&[EXIF_IFD], &[]);
/// resolve_subtree(&mut ifd0, scope, |offset| {
///     read_ifd_at(&bytes, offset, ByteOrder::LittleEndian, Variant::Classic)
/// })
/// .unwrap();
/// assert_eq!(ifd0.sub_ifds()[0].ifds, [exif]);
/// assert_eq!(ifd0.get(SUB_IFDS), Some(&Value::Long(vec![0xFFFF_FFF0])));
/// ```
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if two pointers in the subtree name one directory (including a
/// cycle back to a visited one), or a directory sits deeper than
/// [`max_depth`](SubtreeScope::max_depth); and propagates any error `fetch` returns for a
/// pointer target unchanged. On error `ifd` may be partially resolved.
pub fn resolve_subtree(
    ifd: &mut Ifd,
    scope: SubtreeScope<'_>,
    mut fetch: impl FnMut(u64) -> Result<Ifd>,
) -> Result<()> {
    resolve_pointers_with(&mut fetch, ifd, scope, &mut BTreeSet::new(), 0)
}

/// Follows each of `scope`'s tags in `ifd` (and, recursively, in the children), replacing the
/// pointer field with a parsed sub-IFD group. `ifd` sits `depth` levels below the walk's root.
/// `visited` spans the whole walk so a hostile pointer graph (a cycle, or two pointers claiming
/// one directory) fails instead of looping.
///
/// `fetch` parses the directory at an offset — a closure over [`read_ifd_at`] for the slice
/// path, or over [`IfdReader::read_ifd`](crate::IfdReader::read_ifd) for the streaming path —
/// so the depth and cycle guards exist exactly once.
///
/// `visited` is a **set**, not a list, and that is hardening rather than style: nothing bounds
/// how many offsets one pointer array holds, so a linear membership scan makes the walk quadratic
/// in a number a hostile file chooses. Measured in release on hand-built files whose single
/// `ExifIFD` array names N distinct empty directories (in `gamut-tiff`, before this walk was
/// hoisted here), a scanned list took 0.24 s at 0.6 MB and 4.2 s at 2.3 MB; a set answered the
/// same files in 5 ms and 27 ms. What still has no bound is the *breadth* of one pointer array;
/// that is issue #579.
pub(crate) fn resolve_pointers_with(
    fetch: &mut dyn FnMut(u64) -> Result<Ifd>,
    ifd: &mut Ifd,
    scope: SubtreeScope<'_>,
    visited: &mut BTreeSet<u64>,
    depth: usize,
) -> Result<()> {
    if depth > scope.max_depth() {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "TIFF: sub-IFD tree too deep",
        ));
    }
    for &tag in scope.tags_at(depth) {
        let Some(offsets) = ifd.get(tag).and_then(pointer_offsets) else {
            continue;
        };
        let mut children = Vec::with_capacity(offsets.len());
        for off in offsets {
            if !visited.insert(off) {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "TIFF: sub-IFD pointer loop",
                ));
            }
            let mut child = fetch(off)?;
            resolve_pointers_with(fetch, &mut child, scope, visited, depth + 1)?;
            children.push(child);
        }
        ifd.remove(tag);
        ifd.set_sub_ifd(tag, children);
    }
    Ok(())
}

/// Like [`read`], but returns a dual-ledger-checked [`SegmentReport`](crate::SegmentReport)
/// alongside the parse: the whole file is read through a [`Tracked`](crate::Tracked) source,
/// the parser's typed claims are collected into a [`SegmentMap`](crate::SegmentMap), and the
/// two are cross-checked — so the report *proves* what the parse touched.
///
/// This audits the header and the **top-level chain** only; sub-IFDs a codec follows, its
/// strip/tile data extents, and padding classification are the codec-level audit's job (each
/// sub-IFD via [`IfdReader::read_ifd_at_audited`](crate::IfdReader::read_ifd_at_audited) over
/// the same map).
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] under the same conditions as [`read`].
pub fn read_audited(data: &[u8]) -> Result<(TiffFile, crate::SegmentReport)> {
    let mut tracked = crate::Tracked::new(data);
    let mut map = crate::SegmentMap::new(data.len() as u64);
    let file = crate::IfdReader::open(&mut tracked)?.read_file_audited(&mut map)?;
    Ok((file, map.finish(Some(tracked.ledger()))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_header() {
        assert!(read_header(b"\x49\x49").is_err()); // too short
        assert!(read_header(b"XX\x2a\x00\x08\x00\x00\x00").is_err()); // bad BOM
        assert!(read_header(b"II\x00\x00\x08\x00\x00\x00").is_err()); // bad magic
        let (order, variant, first) = read_header(b"II\x2a\x00\x08\x00\x00\x00").expect("ok");
        assert_eq!(order, ByteOrder::LittleEndian);
        assert_eq!(variant, Variant::Classic);
        assert_eq!(first, 8);
    }

    #[cfg(feature = "bigtiff")]
    #[test]
    fn parses_bigtiff_header() {
        // II, magic 43, offset-size 8, reserved 0, then an 8-byte first-IFD offset of 16.
        let head = b"II\x2b\x00\x08\x00\x00\x00\x10\x00\x00\x00\x00\x00\x00\x00";
        let (order, variant, first) = read_header(head).expect("ok");
        assert_eq!(order, ByteOrder::LittleEndian);
        assert_eq!(variant, Variant::Big);
        assert_eq!(first, 16);
        // The fixed BigTIFF offset-size (8) and reserved (0) fields are validated.
        assert!(
            read_header(b"II\x2b\x00\x04\x00\x00\x00\x10\x00\x00\x00\x00\x00\x00\x00").is_err()
        );
        assert!(
            read_header(b"II\x2b\x00\x08\x00\x01\x00\x10\x00\x00\x00\x00\x00\x00\x00").is_err()
        );
        // A BigTIFF magic with a truncated (classic-length) header is rejected, not read OOB.
        assert!(read_header(b"II\x2b\x00\x08\x00\x00\x00").is_err());
    }

    /// Without the feature, a BigTIFF magic is an unknown magic, not a mis-parse.
    #[cfg(not(feature = "bigtiff"))]
    #[test]
    fn rejects_bigtiff_without_feature() {
        let head = b"II\x2b\x00\x08\x00\x00\x00\x10\x00\x00\x00\x00\x00\x00\x00";
        assert!(read_header(head).is_err());
    }

    #[test]
    fn empty_input_errors() {
        assert!(read(&[]).is_err());
    }

    #[test]
    fn rejects_truncated_ifd() {
        // Classic header with the first IFD at offset 8. The IFD declares one SHORT entry, but the
        // file ends right after the entry's count field — no room for the entry's value/offset word
        // or the next-IFD pointer. The `next_pos + offset_size > data.len()` guard must reject this;
        // without it the unchecked inline-value slice would index past the end.
        let data = [
            b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, // header: classic, first IFD @ 8
            0x01, 0x00, // entry count = 1
            0x00, 0x01, // tag 256
            0x03, 0x00, // type 3 (SHORT)
            0x01, 0x00, 0x00, 0x00, // value count = 1
        ];
        assert_eq!(data.len(), 18);
        assert!(read(&data).is_err());
    }

    /// An IFD whose out-of-line values are all even-length, so [`crate::write`] emits no alignment
    /// padding and a deconstruct can account every byte.
    fn even_value_ifd() -> Ifd {
        let mut ifd = Ifd::new();
        ifd.set(256, Value::Short(vec![640])); // inline (2 bytes)
        ifd.set(258, Value::Short(vec![8, 8, 8])); // 6 bytes -> out of line
        ifd.set(282, Value::Rational(vec![(300, 1)])); // 8 bytes -> out of line
        ifd
    }

    #[test]
    fn read_audited_classifies_a_written_file() {
        let file = TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![even_value_ifd()],
        };
        let bytes = crate::write(&file).expect("write");
        let (parsed, report) = read_audited(&bytes).expect("read");
        // The audited reader returns exactly what the plain reader would.
        assert_eq!(parsed, file);
        assert!(report.is_fully_classified(), "report: {report:?}");
        // The typed view: one header, one directory body, two out-of-line values.
        use crate::SpanKind;
        assert_eq!(report.segments[0].kind, SpanKind::Header);
        assert_eq!(report.segments[1].kind, SpanKind::IfdBody { ifd: 8 });
        assert_eq!(
            report
                .segments
                .iter()
                .filter(|s| matches!(s.kind, SpanKind::Value { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn unknown_field_type_is_preserved_verbatim() {
        // One IFD entry with an unrecognised field-type code (0xF0). The readers preserve the
        // entry verbatim as a `Value::Unknown` (nothing is dropped on a rewrite); the 12-byte
        // entry is part of the IFD body, so the file stays fully classified.
        let data = [
            b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, // header, first IFD @ 8
            0x01, 0x00, // entry count = 1
            0x99, 0x99, // tag 0x9999
            0xf0, 0x00, // type 0xF0 (unknown)
            0x01, 0x00, 0x00, 0x00, // value count = 1
            0xde, 0xad, 0xbe, 0xef, // value/offset word (opaque)
            0x00, 0x00, 0x00, 0x00, // next IFD = 0
        ];
        let (parsed, report) = read_audited(&data).expect("read");
        // The entry is preserved, not skipped: the raw record survives in the Ifd.
        assert_eq!(parsed.ifds[0].fields().len(), 1);
        let Some(Value::Unknown(u)) = parsed.ifds[0].get(0x9999) else {
            panic!("expected a preserved Value::Unknown");
        };
        assert_eq!(u.type_code(), 0xF0);
        assert_eq!(u.count(), 1);
        assert_eq!(u.word(), &[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(u.order(), ByteOrder::LittleEndian);
        assert_eq!(u.variant(), Variant::Classic);
        // The plain reader agrees (preservation is not an audit-path special case).
        assert_eq!(read(&data).expect("plain read"), parsed);
        assert!(report.is_fully_classified(), "report: {report:?}");
    }

    /// The keystone symmetry: `read_tree` is `write`'s inverse over a nested sub-IFD tree — the
    /// pointer fields disappear, the children come back in place, and the whole `TiffFile`
    /// compares equal.
    fn read_tree_inverts_write(order: ByteOrder, variant: Variant) {
        let mut grandchild = Ifd::new();
        grandchild.set(33434, Value::Rational(vec![(1, 200)])); // ExposureTime
        let mut raw_a = Ifd::new();
        raw_a.set(256, Value::Short(vec![16]));
        raw_a.set_sub_ifd(34665, vec![grandchild]);
        let mut raw_b = Ifd::new();
        raw_b.set(256, Value::Short(vec![8]));
        let mut root = Ifd::new();
        root.set(256, Value::Short(vec![640]));
        root.set(258, Value::Short(vec![8, 8, 8])); // out of line
        root.set_sub_ifd(330, vec![raw_a, raw_b]);
        let file = TiffFile {
            order,
            variant,
            ifds: vec![root],
        };
        let bytes = crate::write(&file).expect("write");
        let tree = read_tree(&bytes, &[330, 34665]).expect("read_tree");
        assert_eq!(tree, file);
        // The pointer tags were consumed into sub_ifds, not left as stale offset fields.
        assert_eq!(tree.ifds[0].get(330), None);
    }

    #[test]
    fn read_tree_inverts_write_classic_both_orders() {
        read_tree_inverts_write(ByteOrder::LittleEndian, Variant::Classic);
        read_tree_inverts_write(ByteOrder::BigEndian, Variant::Classic);
    }

    #[cfg(feature = "bigtiff")]
    #[test]
    fn read_tree_inverts_write_bigtiff() {
        read_tree_inverts_write(ByteOrder::LittleEndian, Variant::Big);
    }

    /// A named pointer tag whose value is not an offset array is left alone as a regular field
    /// (the type check TIFF 6.0 §2 asks of readers), and unnamed `LONG` tags are never followed.
    #[test]
    fn read_tree_leaves_non_pointer_values_in_place() {
        let mut root = Ifd::new();
        root.set(330, Value::Ascii("not a pointer".to_owned()));
        root.set(700, Value::Long(vec![8])); // a plausible offset, but not a named pointer tag
        let file = TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![root.clone()],
        };
        let bytes = crate::write(&file).expect("write");
        let tree = read_tree(&bytes, &[330]).expect("read_tree");
        assert_eq!(tree.ifds[0], root);
    }

    /// A pointer stored with the typed `IFD` (13) field type — TIFF Technical Note 1's form —
    /// is followed by `read_tree` exactly like a `LONG` pointer. Hand-built, since `write`
    /// synthesises `LONG` pointers.
    #[test]
    fn typed_ifd_pointer_is_followed_by_read_tree() {
        let data = [
            b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, // header, first IFD @ 8
            0x01, 0x00, // IFD0: entry count = 1
            0x4a, 0x01, // tag 330 (SubIFDs)
            0x0d, 0x00, // type 13 (IFD)
            0x01, 0x00, 0x00, 0x00, // count = 1
            0x1a, 0x00, 0x00, 0x00, // offset 26 (the child directory)
            0x00, 0x00, 0x00, 0x00, // next IFD = 0
            0x01, 0x00, // child @ 26: entry count = 1
            0x00, 0x01, // tag 256
            0x03, 0x00, // type 3 (SHORT)
            0x01, 0x00, 0x00, 0x00, // count = 1
            0x07, 0x00, 0x00, 0x00, // value 7 (inline)
            0x00, 0x00, 0x00, 0x00, // next IFD = 0
        ];
        // The flat reader decodes the pointer as a typed value, not a followed directory.
        let flat = read(&data).expect("read");
        assert_eq!(flat.ifds[0].get(330), Some(&Value::Ifd(vec![26])));
        // read_tree follows it like a LONG pointer.
        let tree = read_tree(&data, &[330]).expect("read_tree");
        assert_eq!(
            tree.ifds[0].get(330),
            None,
            "pointer consumed into the tree"
        );
        let children = &tree.ifds[0].sub_ifds()[0];
        assert_eq!(children.tag, 330);
        assert_eq!(children.ifds[0].get_u32(256), Some(7));
    }

    /// Adversarial: a sub-IFD pointer aimed at its own directory must terminate with a typed
    /// error (the visited set), not recurse forever.
    #[test]
    fn read_tree_rejects_self_pointing_sub_ifd() {
        // Root IFD at 8 with one LONG field, tag 330, whose value is a child offset — pointing
        // at offset 26, which is a directory whose own tag-330 pointer points back at 26.
        let data = [
            b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, // header, first IFD @ 8
            0x01, 0x00, // IFD0: entry count = 1
            0x4a, 0x01, // tag 330
            0x04, 0x00, // type 4 (LONG)
            0x01, 0x00, 0x00, 0x00, // count = 1
            0x1a, 0x00, 0x00, 0x00, // offset 26 (the child directory)
            0x00, 0x00, 0x00, 0x00, // next IFD = 0
            0x01, 0x00, // child @ 26: entry count = 1
            0x4a, 0x01, // tag 330
            0x04, 0x00, // type 4 (LONG)
            0x01, 0x00, 0x00, 0x00, // count = 1
            0x1a, 0x00, 0x00, 0x00, // offset 26 — itself
            0x00, 0x00, 0x00, 0x00, // next IFD = 0
        ];
        assert!(read(&data).is_ok()); // the flat reader is untouched by the cycle
        assert!(read_tree(&data, &[330]).is_err());
    }

    /// A tree at exactly the depth cap parses — pinning the guard's boundary against the
    /// depth-bomb rejection below.
    #[test]
    fn read_tree_allows_exactly_max_depth() {
        let mut ifd = Ifd::new();
        ifd.set(256, Value::Short(vec![1]));
        // 15 nestings put the deepest directory at `SubtreeScope::MAX_DEPTH` below the root.
        for _ in 0..15 {
            let mut parent = Ifd::new();
            parent.set_sub_ifd(330, vec![ifd]);
            ifd = parent;
        }
        let bytes = crate::write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![ifd],
        })
        .expect("write");
        assert!(read_tree(&bytes, &[330]).is_ok());
    }

    /// Adversarial: nesting past the depth cap is a typed error, not a stack overflow.
    #[test]
    fn read_tree_rejects_depth_bomb() {
        let mut ifd = Ifd::new();
        ifd.set(256, Value::Short(vec![1]));
        for _ in 0..17 {
            let mut parent = Ifd::new();
            parent.set_sub_ifd(330, vec![ifd]);
            ifd = parent;
        }
        let bytes = crate::write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![ifd],
        })
        .expect("write");
        assert!(read_tree(&bytes, &[330]).is_err());
    }

    /// Writes `root` as the single page of a little-endian classic file and returns the bytes
    /// with the page read back flat — its pointers still integer fields.
    fn written_root(root: Ifd) -> (Vec<u8>, Ifd) {
        let bytes = crate::write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![root],
        })
        .expect("write");
        let flat = read(&bytes).expect("read").ifds.swap_remove(0);
        (bytes, flat)
    }

    /// Runs [`resolve_subtree`] over `ifd` with the slice fetch.
    fn resolve(bytes: &[u8], ifd: &mut Ifd, scope: SubtreeScope<'_>) -> Result<()> {
        resolve_subtree(ifd, scope, |off| {
            read_ifd_at(bytes, off, ByteOrder::LittleEndian, Variant::Classic)
        })
    }

    /// A one-entry directory distinguishable by `marker`.
    fn leaf(marker: u16) -> Ifd {
        let mut ifd = Ifd::new();
        ifd.set(256, Value::Short(vec![marker]));
        ifd
    }

    #[test]
    fn resolve_subtree_follows_root_tags_at_the_root_and_nested_tags_below_it() {
        // Root: an Exif pointer (root tag) and a GPS pointer (nested-only tag). Inside the Exif
        // directory: an Interop pointer (nested tag) and an Exif pointer (root-only tag). Only
        // the level's own list is followed, so each directory keeps the other list's pointer.
        let mut exif = leaf(1);
        exif.set_sub_ifd(40965, vec![leaf(2)]);
        exif.set_sub_ifd(34665, vec![leaf(3)]);
        let mut root = leaf(0);
        root.set_sub_ifd(34665, vec![exif]);
        root.set_sub_ifd(34853, vec![leaf(4)]);
        let (bytes, mut ifd) = written_root(root);

        resolve(&bytes, &mut ifd, SubtreeScope::new(&[34665], &[40965])).expect("resolve");
        assert!(
            ifd.get(34853).is_some(),
            "a nested-only tag stays a field at the root"
        );
        let [group] = ifd.sub_ifds() else {
            panic!("exactly the root tag's group: {:?}", ifd.sub_ifds());
        };
        assert_eq!(group.tag, 34665);
        let exif = &group.ifds[0];
        assert!(
            exif.get(34665).is_some(),
            "a root-only tag stays a field below the root"
        );
        assert_eq!(exif.sub_ifds().len(), 1);
        assert_eq!(exif.sub_ifds()[0].tag, 40965);
        assert_eq!(exif.sub_ifds()[0].ifds, [leaf(2)]);
    }

    /// A chain of `levels` directories below a root, each linked by tag 330.
    fn nested(levels: usize) -> Ifd {
        let mut ifd = leaf(9);
        for _ in 0..levels {
            let mut parent = Ifd::new();
            parent.set_sub_ifd(330, vec![ifd]);
            ifd = parent;
        }
        ifd
    }

    #[test]
    fn resolve_subtree_admits_exactly_the_scoped_depth() {
        let scope = SubtreeScope::new(&[330], &[330]).with_max_depth(2);
        let (bytes, mut ifd) = written_root(nested(2));
        resolve(&bytes, &mut ifd, scope).expect("two levels below the root is the bound");
        let (bytes, mut ifd) = written_root(nested(3));
        let err = resolve(&bytes, &mut ifd, scope).expect_err("three is past it");
        assert!(err.to_string().contains("sub-IFD tree too deep"), "{err}");
    }

    #[test]
    fn with_max_depth_never_raises_the_bound() {
        // Asking for more than the ceiling keeps the ceiling, so the walk still refuses the
        // sixteenth level below the root.
        let scope = SubtreeScope::new(&[330], &[330]).with_max_depth(100);
        assert_eq!(scope.max_depth(), SubtreeScope::MAX_DEPTH);
        let (bytes, mut ifd) = written_root(nested(SubtreeScope::MAX_DEPTH + 1));
        assert!(resolve(&bytes, &mut ifd, scope).is_err());
    }

    #[test]
    fn resolve_subtree_rejects_two_pointers_naming_one_directory() {
        // The root's tag-330 array names the directory at 26 twice; the fetch stands in for a
        // file, so only the walk's own guard can refuse it.
        let mut root = Ifd::new();
        root.set(330, Value::Long(vec![26, 26]));
        let mut fetched = Vec::new();
        let err = resolve_subtree(&mut root, SubtreeScope::new(&[330], &[]), |off| {
            fetched.push(off);
            Ok(Ifd::new())
        })
        .expect_err("one directory claimed twice");
        assert!(err.to_string().contains("sub-IFD pointer loop"), "{err}");
        assert_eq!(fetched, [26], "the repeat is refused before it is fetched");
    }

    #[test]
    fn resolve_subtree_propagates_the_fetch_error_unchanged() {
        let mut root = Ifd::new();
        root.set(330, Value::Long(vec![26]));
        let err = resolve_subtree(&mut root, SubtreeScope::new(&[330], &[]), |_| {
            Err(Error::invalid_input("probe", "the fetch's own refusal"))
        })
        .expect_err("fetch failed");
        assert!(err.to_string().contains("the fetch's own refusal"), "{err}");
    }

    #[test]
    fn read_ifd_at_audited_accounts_a_subifd() {
        // Root + one sub-IFD, each with an even-length out-of-line value: claiming the root
        // chain and then the child via the pointer must leave no byte unclassified.
        let mut child = Ifd::new();
        child.set(256, Value::Short(vec![16]));
        child.set(258, Value::Short(vec![8, 8, 8])); // 6 bytes -> out of line
        let mut root = Ifd::new();
        root.set(256, Value::Short(vec![640]));
        root.set(258, Value::Short(vec![8, 8, 8])); // 6 bytes -> out of line
        root.set_sub_ifd(330, vec![child]);
        let bytes = crate::write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![root],
        })
        .expect("write");

        let mut map = crate::SegmentMap::new(bytes.len() as u64);
        let mut reader = crate::IfdReader::open(&bytes[..]).expect("open");
        let parsed = reader.read_file_audited(&mut map).expect("root");
        let child_off = parsed.ifds[0].get_u32(330).expect("SubIFDs pointer");
        let (_child, next) = reader
            .read_ifd_at_audited(child_off.into(), &mut map)
            .expect("child");
        assert_eq!(next, 0);
        let report = map.finish(None);
        assert!(report.is_fully_classified(), "report: {report:?}");
    }
}
