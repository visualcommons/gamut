//! Serialisation of the TIFF byte-order header and the IFD tree.
//!
//! The writer lays the stream out in two passes: it first computes where each IFD and each
//! out-of-line value lands (header → IFDs → value pool, every block on an even/word boundary),
//! then emits the bytes with the absolute offsets patched in. Values that fit in the entry's
//! value/offset field — four bytes in classic TIFF, eight in BigTIFF — are packed inline,
//! left-justified (TIFF 6.0 §2); the [`Variant`] selects every structural field width.
//!
//! This two-pass offset layout is the crate's **keystone**: out-of-line values and following IFDs
//! need absolute offsets that are only known once sizes are fixed, so the layout is planned then
//! the offset words are back-patched. A read → write → read round-trip reproduces the directory
//! exactly.
//!
//! ## Sub-IFD trees
//!
//! Beyond the top-level next-IFD chain ([`TiffFile::ifds`]), an [`Ifd`] may carry
//! [`sub_ifds`](Ifd::sub_ifds): child directories referenced by a pointer *tag* rather than the
//! chain (e.g. a DNG's raw sub-IFD via `SubIFDs`, or an `ExifIFD`). The layout generalises to the
//! whole **tree** — every directory (top-level and descendant) is placed first, then a single
//! value pool — and each pointer tag's value is synthesised as a `LONG`/`LONG8` array of its
//! children's offsets. With no sub-IFDs this reduces to the flat chain, byte-for-byte.

use gamut_core::{Error, Result};

use crate::segment::{Claim, SegmentMap, SpanKind};
use crate::{ByteOrder, Ifd, TiffFile, Value, Variant};

/// A directive to land one field's out-of-line value at an exact absolute offset.
///
/// This is the **maker-note preservation primitive**: vendor blobs often encode internal
/// offsets relative to the TIFF header, so relocating them on a rewrite makes those offsets
/// stale. Pinning the value at its original offset keeps them valid. The tag must appear
/// exactly once in the whole IFD tree and its value must be out-of-line (larger than the
/// inline threshold).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinnedSpan {
    /// The tag whose value is pinned.
    pub tag: u16,
    /// The absolute file offset the value must start at.
    pub offset: u64,
}

/// Options for [`write_with`].
///
/// `#[non_exhaustive]`: construct via [`Default`] and set fields, so options can grow without
/// a breaking change.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct WriteOptions {
    /// Values to land at exact absolute offsets (see [`PinnedSpan`]). The normal value pool
    /// flows around the pinned ranges; the resulting zero filler is declared as
    /// [`SpanKind::Padding`] in the returned map.
    pub pinned: Vec<PinnedSpan>,
    /// Bytes to emit immediately after the file header, before the first directory — a **vendor
    /// preamble**.
    ///
    /// This is the position-preservation counterpart to [`PinnedSpan`]. Pinning keeps a *value*
    /// where a vendor blob's internal offsets expect it; a preamble is not a value at all, so it
    /// has no tag to pin by — it is identified only by sitting between the header and the first
    /// structure. Real writers put signatures there: Apple's ProRAW files carry the ASCII
    /// `APPLEDNG` immediately after the 8-byte TIFF header, and a rewrite that relocates those
    /// bytes leaves a vendor tool looking at the wrong place.
    ///
    /// The directory region starts after the preamble (word-aligned), and the run is declared as
    /// [`SpanKind::Preamble`] in the returned map. Empty — the default — reproduces the previous
    /// layout byte for byte.
    ///
    /// An odd-length preamble is followed by one zero byte of word alignment, and the declared
    /// span covers it: the declared preamble is always the whole gap between the header and the
    /// first directory, `even(len)` bytes, because that is exactly what an independent audit of
    /// the emitted bytes can see there.
    pub preamble: Vec<u8>,
}

impl WriteOptions {
    /// Adds a pinned span, builder-style: `WriteOptions::default().pin(37500, 0x1000)`.
    #[must_use]
    pub fn pin(mut self, tag: u16, offset: u64) -> Self {
        self.pinned.push(PinnedSpan { tag, offset });
        self
    }

    /// Sets the vendor preamble emitted between the header and the first directory,
    /// builder-style: `WriteOptions::default().with_preamble(*b"APPLEDNG\0\0")`.
    #[must_use]
    pub fn with_preamble(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.preamble = bytes.into();
        self
    }
}

/// Rounds `n` up to the next even (word) boundary — where TIFF 6.0 §2 requires values (and this
/// crate places every structure) to start.
///
/// This is the alignment [`write()`]'s layout contract guarantees, exported so a codec appending
/// data after the stream can place it on the same boundary rather than re-deriving the rule.
/// Saturates at `u64::MAX` instead of wrapping.
#[must_use]
pub const fn align_word(n: u64) -> u64 {
    n.saturating_add(n & 1)
}

/// Rounds `n` up to the next even (word) boundary, as required for value offsets.
fn even(n: usize) -> usize {
    align_word(n as u64) as usize
}

/// A field value to emit: either borrowed from the directory, or a sub-IFD pointer-offset array the
/// writer synthesises once the child directories have been placed.
enum FieldRef<'a> {
    /// A real field value, borrowed from the source [`Ifd`].
    Real(&'a Value),
    /// A synthesised `SubIFDs`/`ExifIFD`-style pointer: the offsets of the child directories.
    Synth(Value),
}

impl FieldRef<'_> {
    fn value(&self) -> &Value {
        match self {
            FieldRef::Real(v) => v,
            FieldRef::Synth(v) => v,
        }
    }
}

/// One IFD flattened out of the tree, with its placement bookkeeping.
struct Node<'a> {
    /// The source directory (for its real fields).
    ifd: &'a Ifd,
    /// Each sub-IFD pointer as `(tag, child node indices)`, in `sub_ifds` order.
    pointers: Vec<(u16, Vec<usize>)>,
    /// The next directory in the top-level chain (`None` for descendants and the last page).
    next: Option<usize>,
    /// Number of on-disk entries: real fields plus one synthesised entry per pointer.
    n_entries: usize,
    /// The directory's absolute file offset (assigned in pass 1).
    offset: u64,
}

/// Appends `ifd` and its descendants to `nodes` (parent before children), returning `ifd`'s index.
fn push_node<'a>(ifd: &'a Ifd, nodes: &mut Vec<Node<'a>>) -> usize {
    let idx = nodes.len();
    nodes.push(Node {
        ifd,
        pointers: Vec::new(),
        next: None,
        n_entries: 0,
        offset: 0,
    });
    let mut pointers = Vec::with_capacity(ifd.sub_ifds().len());
    for sub in ifd.sub_ifds() {
        let children: Vec<usize> = sub.ifds.iter().map(|c| push_node(c, nodes)).collect();
        pointers.push((sub.tag, children));
    }
    nodes[idx].n_entries = ifd.fields().len() + pointers.len();
    nodes[idx].pointers = pointers;
    idx
}

/// Whether a layout of `len` bytes has outgrown `variant`'s offset width — possible only for
/// classic TIFF's 32-bit offsets (an in-memory BigTIFF stream cannot reach 2^64).
///
/// A pure predicate so the 4 GiB boundary is unit-testable with plain integers; exercising it
/// through [`write()`] would need a >4 GiB allocation.
fn layout_overflows(variant: Variant, len: u64) -> bool {
    variant == Variant::Classic && len > u64::from(u32::MAX)
}

/// Writes an offset-sized integer (`u32` classic / `u64` BigTIFF) at `pos`, used for every file
/// offset and the per-field value count, which share the offset width.
fn put_offset(out: &mut [u8], pos: usize, v: u64, order: ByteOrder, variant: Variant) {
    match variant {
        Variant::Classic => out[pos..pos + 4].copy_from_slice(&order.pack_u32(v as u32)),
        #[cfg(feature = "bigtiff")]
        Variant::Big => out[pos..pos + 8].copy_from_slice(&order.pack_u64(v)),
    }
}

/// Serialises a TIFF/IFD stream (header + IFD tree + out-of-line value pool) to bytes.
///
/// Top-level IFDs ([`TiffFile::ifds`]) are written in order and linked through their next-IFD
/// pointers; any [`sub_ifds`](Ifd::sub_ifds) are laid out as additional directories and referenced
/// by a synthesised pointer field. Out-of-line values are appended in a value pool after the
/// directories. Image/pixel data is not handled here — a codec composes that around this primitive
/// (see `gamut-tiff`'s strip/tile writers).
///
/// # Layout contract
///
/// Codecs that append their own data (strips, tiles, an embedded JPEG) after the returned stream
/// and back-patch offset fields may rely on two properties of the layout:
///
/// - **Alignment** — the header, every directory, and every out-of-line value are placed on an
///   even (word) boundary, as TIFF 6.0 §2 requires of value offsets. (The stream's final byte may
///   land on an odd length; round an append position up with [`align_word`].)
/// - **Structural determinism** — the position of every structure, and the total length, is a
///   pure function of the *structure*: the variant, the sub-IFD tree shape, the tag order, and
///   each value's byte length. Value contents never move the layout, so writing with
///   correctly-sized placeholder values, measuring, then re-writing with the real values (e.g.
///   patched strip offsets) yields a byte-identical layout.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if the stream is not representable in the classic-TIFF widths:
/// a directory with more than `u16::MAX` entries, or a total layout past the 4 GiB offset limit.
/// (BigTIFF widths cannot overflow in practice.)
pub fn write(file: &TiffFile) -> Result<Vec<u8>> {
    write_with(file, &WriteOptions::default()).map(|(bytes, _map)| bytes)
}

/// Like [`write()`], but takes [`WriteOptions`] (value pinning) and returns, alongside the
/// bytes, a [`SegmentMap`] in which the writer **declares every byte it emitted** — header,
/// directory bodies, value spans, and all zero-fill padding. `map.finish(None)` is therefore
/// fully classified by construction, closing the audit loop: an independent audit of the
/// returned stream must reproduce this map segment for segment.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] under the same conditions as [`write()`], or if a
/// [`PinnedSpan`] is unsatisfiable: its tag absent or duplicated, its value inline or
/// unsizable, its offset colliding with the directory region or another pin.
pub fn write_with(file: &TiffFile, opts: &WriteOptions) -> Result<(Vec<u8>, SegmentMap)> {
    let order = file.order;
    let variant = file.variant;
    let entry_size = variant.entry_size();
    let offset_size = variant.offset_size();
    let inline = variant.inline_threshold();

    // Flatten the tree, then link the top-level directories through the next-IFD chain.
    let mut nodes: Vec<Node> = Vec::new();
    let top: Vec<usize> = file
        .ifds
        .iter()
        .map(|ifd| push_node(ifd, &mut nodes))
        .collect();
    for pair in top.windows(2) {
        nodes[pair[0]].next = Some(pair[1]);
    }

    // An unknown-type value's word is opaque bytes in its captured byte order and offset width;
    // it cannot be transcoded, so a stream that changes either is refused up front rather than
    // silently emitting a word whose (unknowable) meaning would be corrupted.
    for node in &nodes {
        for field in node.ifd.fields() {
            if let Value::Unknown(u) = &field.value
                && (u.order() != order || u.variant() != variant)
            {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "TIFF: unknown-type field cannot be transcoded across byte order or variant",
                ));
            }
        }
    }

    // Pass 1a: place every directory block (top-level and descendant), each on a word boundary.
    // A vendor preamble sits between the header and the first directory, so the region starts
    // after it; with no preamble this is exactly the header size, as before.
    let preamble_end = variant
        .header_size()
        .checked_add(opts.preamble.len())
        .ok_or_else(|| {
            Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "TIFF: preamble overflows the layout",
            )
        })?;
    let mut cursor = even(preamble_end);
    for node in &mut nodes {
        // The entry count must fit its on-disk width (2 bytes in classic TIFF); a silent `as u16`
        // truncation would drop entries.
        if variant == Variant::Classic && node.n_entries > usize::from(u16::MAX) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "TIFF: too many IFD entries for classic TIFF",
            ));
        }
        node.offset = cursor as u64;
        cursor = even(cursor + variant.count_size() + node.n_entries * entry_size + offset_size);
    }

    // Resolve the pinned spans against the placed directories: each pin's tag must name
    // exactly one out-of-line value in the whole tree, and the pinned ranges must lie beyond
    // the directory region without overlapping one another.
    let dir_end = cursor;
    struct Pin {
        tag: u16,
        offset: u64,
        len: u64,
    }
    let mut pins: Vec<Pin> = Vec::with_capacity(opts.pinned.len());
    for p in &opts.pinned {
        let mut fields = nodes
            .iter()
            .flat_map(|n| n.ifd.fields())
            .filter(|f| f.tag == p.tag);
        let Some(field) = fields.next() else {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "TIFF: pinned tag is not present",
            ));
        };
        if fields.next().is_some()
            || nodes
                .iter()
                .any(|n| n.pointers.iter().any(|(t, _)| *t == p.tag))
        {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "TIFF: pinned tag must appear exactly once",
            ));
        }
        let Some(len) = field.value.byte_len() else {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "TIFF: an unknown-type value cannot be pinned",
            ));
        };
        if len <= inline as u64 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "TIFF: pinned value packs inline, there is no span to pin",
            ));
        }
        if p.offset < dir_end as u64 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "TIFF: pinned offset collides with the directory layout",
            ));
        }
        if p.offset.checked_add(len).is_none() {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "TIFF: pinned span overflows",
            ));
        }
        pins.push(Pin {
            tag: p.tag,
            offset: p.offset,
            len,
        });
    }
    pins.sort_by_key(|p| p.offset);
    if pins
        .windows(2)
        .any(|w| w[1].offset < w[0].offset + w[0].len)
    {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "TIFF: pinned spans overlap",
        ));
    }

    // With every directory offset known, synthesise each pointer field (a child-offset array) and
    // build each directory's tag-sorted entry list (real fields interleaved with pointers).
    let mut entries_per_node: Vec<Vec<(u16, FieldRef)>> = Vec::with_capacity(nodes.len());
    for node in &nodes {
        let mut entries: Vec<(u16, FieldRef)> = Vec::with_capacity(node.n_entries);
        for field in node.ifd.fields() {
            entries.push((field.tag, FieldRef::Real(&field.value)));
        }
        for (tag, children) in &node.pointers {
            let offsets: Vec<u64> = children.iter().map(|&ci| nodes[ci].offset).collect();
            entries.push((
                *tag,
                FieldRef::Synth(Value::offset_array(variant, &offsets)?),
            ));
        }
        entries.sort_by_key(|(tag, _)| *tag);
        entries_per_node.push(entries);
    }

    // Pass 1b: place the out-of-line value pool after the directories, flowing around the
    // pinned reservations; a pinned value lands exactly where asked.
    let mut value_offsets: Vec<Vec<u64>> = Vec::with_capacity(nodes.len());
    let mut pool: Vec<(usize, Vec<u8>)> = Vec::new();
    // Every placed value as `(directory offset, tag, start, len)` — the writer's own claims.
    let mut value_spans: Vec<(u64, u16, u64, u64)> = Vec::new();
    for (idx, entries) in entries_per_node.iter().enumerate() {
        let node_offset = nodes[idx].offset;
        let mut offs = Vec::with_capacity(entries.len());
        for (tag, field) in entries {
            // An unknown-type value has no sizable extent (`byte_len` is `None`); its verbatim
            // word always re-emits inline, so it never claims pool space.
            match field.value().byte_len() {
                Some(n) if n > inline as u64 => {
                    let pinned = pins.iter().find(|p| p.tag == *tag);
                    let start = match pinned {
                        Some(pin) => pin.offset,
                        None => {
                            // Advance to the next word boundary, jumping over any pinned range
                            // the value would intersect. `pins` is sorted by offset and validated
                            // disjoint above, so a jump can never re-expose a pin already behind
                            // the cursor: one forward pass over the slice settles the placement,
                            // and the slice is what bounds it. A `while let` driven by `find`
                            // terminates only while the jump arithmetic really moves the cursor
                            // forward, so a mutation that stops it moving could be reported only
                            // as a mutation-testing timeout, never as a wrong answer (issue #110).
                            // Taking the later of the two positions is what makes a pin the
                            // value already clears a no-op, so one comparison decides the jump
                            // where two used to -- and that one is the abutment boundary the
                            // flow-around tests pin from both sides.
                            let mut c = align_word(cursor as u64);
                            for p in &pins {
                                if p.offset < c.saturating_add(n) {
                                    c = c.max(align_word(p.offset + p.len));
                                }
                            }
                            c
                        }
                    };
                    let start_usize = usize::try_from(start).map_err(|_| {
                        Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: layout overflows")
                    })?;
                    offs.push(start);
                    pool.push((start_usize, field.value().encode(order)));
                    value_spans.push((node_offset, *tag, start, n));
                    if pinned.is_none() {
                        // The encoded length equals `byte_len` for every sizable type.
                        cursor = start_usize + n as usize;
                    }
                }
                _ => offs.push(0),
            }
        }
        value_offsets.push(offs);
    }
    // The stream ends at the later of the flowing pool and the farthest pinned span.
    let total_len = cursor.max(
        pins.iter()
            .map(|p| usize::try_from(p.offset + p.len).unwrap_or(usize::MAX))
            .max()
            .unwrap_or(0),
    );

    // The layout is final; every offset word is a position below `total_len`, so one
    // total-length check proves every classic 32-bit offset (and value count, which is at most
    // a byte length) fits without truncation.
    if layout_overflows(variant, total_len as u64) {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "TIFF: layout exceeds the 4 GiB classic-TIFF offset limit",
        ));
    }

    // Pass 2: emit.
    let mut out = vec![0u8; total_len];
    out[0..2].copy_from_slice(match order {
        ByteOrder::LittleEndian => b"II",
        ByteOrder::BigEndian => b"MM",
    });
    out[2..4].copy_from_slice(&order.pack_u16(variant.magic()));
    let first = nodes.first().map_or(0, |n| n.offset);
    match variant {
        Variant::Classic => out[4..8].copy_from_slice(&order.pack_u32(first as u32)),
        #[cfg(feature = "bigtiff")]
        Variant::Big => {
            // Bytes 4-5 are the offset bytesize (8), 6-7 are reserved (0, already zeroed), and
            // the first-IFD offset is the 8-byte value at bytes 8-15.
            out[4..6].copy_from_slice(&order.pack_u16(8));
            out[8..16].copy_from_slice(&order.pack_u64(first));
        }
    }

    for (idx, entries) in entries_per_node.iter().enumerate() {
        let node = &nodes[idx];
        let mut pos = node.offset as usize;
        let n = entries.len();
        match variant {
            Variant::Classic => out[pos..pos + 2].copy_from_slice(&order.pack_u16(n as u16)),
            #[cfg(feature = "bigtiff")]
            Variant::Big => out[pos..pos + 8].copy_from_slice(&order.pack_u64(n as u64)),
        }
        pos += variant.count_size();
        for ((tag, field), &voff) in entries.iter().zip(&value_offsets[idx]) {
            let value = field.value();
            let bytes = value.encode(order);
            out[pos..pos + 2].copy_from_slice(&order.pack_u16(*tag));
            out[pos + 2..pos + 4].copy_from_slice(&order.pack_u16(value.type_code()));
            put_offset(&mut out, pos + 4, value.count(), order, variant);
            let value_pos = pos + 4 + offset_size;
            if bytes.len() <= inline {
                // Inline, left-justified: low bytes hold the value, remainder is zero.
                out[value_pos..value_pos + bytes.len()].copy_from_slice(&bytes);
            } else {
                put_offset(&mut out, value_pos, voff, order, variant);
            }
            pos += entry_size;
        }
        let next = node.next.map_or(0, |ni| nodes[ni].offset);
        put_offset(&mut out, pos, next, order, variant);
    }

    out[variant.header_size()..preamble_end].copy_from_slice(&opts.preamble);

    for (offset, bytes) in pool {
        out[offset..offset + bytes.len()].copy_from_slice(&bytes);
    }

    // The writer declares every byte it emitted — header, directory bodies, value spans, and
    // (as the complement) the zero-fill padding — so the returned map classifies the stream
    // fully by construction, and an independent audit must reproduce it.
    let mut map = SegmentMap::new(total_len as u64);
    let mut placed: Vec<(u64, u64)> = Vec::new();
    let header_len = variant.header_size() as u64;
    map.claim(0, header_len, SpanKind::Header, Claim::Parsed);
    placed.push((0, header_len));
    if !opts.preamble.is_empty() {
        // The declared span runs to the first directory, so it swallows the word-alignment
        // filler byte an odd-length preamble needs: nothing on disk separates that byte from the
        // vendor bytes, so an audit of these bytes sees one run from the header's end to the
        // first directory and must be able to name exactly what was declared here.
        let len = even(opts.preamble.len()) as u64;
        map.claim(header_len, len, SpanKind::Preamble, Claim::Parsed);
        placed.push((header_len, len));
    }
    for (idx, entries) in entries_per_node.iter().enumerate() {
        let body = (variant.count_size() + entries.len() * entry_size + offset_size) as u64;
        let ifd = nodes[idx].offset;
        map.claim(ifd, body, SpanKind::IfdBody { ifd }, Claim::Parsed);
        placed.push((ifd, body));
    }
    for &(ifd, tag, start, len) in &value_spans {
        map.claim(start, len, SpanKind::Value { ifd, tag }, Claim::Parsed);
        placed.push((start, len));
    }
    placed.sort_unstable();
    // Declare the gaps between placed spans, and any tail, as padding.
    //
    // Neither claim is guarded by a comparison, because `SegmentMap::claim` already returns
    // early on a zero length: `if start > at { claim(at, start - at, ..) }` and
    // `claim(at, start.saturating_sub(at), ..)` are the same function, and the second has no
    // operator to get wrong. That is six surviving mutants removed structurally rather than
    // excluded (#110) -- cargo-mutants could flip `>` to `>=` in either guard without any test
    // noticing, precisely because the guard never decided anything.
    //
    // The tail claim is additionally unreachable as the writer stands: `total_len` is
    // `max(cursor, farthest pinned end)`, every accepted pin is rejected unless it has a real
    // out-of-line span (an absent, duplicated, unknown-type, inline, directory-colliding or
    // overflowing pin all error out), and every such span is in `placed` -- so `at` always
    // reaches `total_len`. It is kept rather than deleted because `saturating_sub` makes it free,
    // and because that argument is about the *callers*, which a later feature could change.
    let mut at = 0u64;
    for &(start, len) in &placed {
        map.claim(
            at,
            start.saturating_sub(at),
            SpanKind::Padding,
            Claim::Parsed,
        );
        at = at.max(start + len);
    }
    map.claim(
        at,
        (total_len as u64).saturating_sub(at),
        SpanKind::Padding,
        Claim::Parsed,
    );
    Ok((out, map))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::{Range, Segment};
    use crate::{read, read_header, read_ifd_at};

    // Tag numbers are used literally: tag semantics live in the consuming codec, not this
    // structural core. 256/257 = ImageWidth/ImageLength, 258 = BitsPerSample, 282 = XResolution.
    fn sample_ifd() -> Ifd {
        let mut ifd = Ifd::new();
        ifd.set(256, Value::Short(vec![640]));
        ifd.set(257, Value::Long(vec![480]));
        ifd.set(258, Value::Short(vec![8, 8, 8])); // 6 bytes -> out of line (classic)
        ifd.set(282, Value::Rational(vec![(300, 1)])); // 8 bytes -> out of line (classic)
        ifd.set(72, Value::Ascii("gamut-tiff".to_owned())); // out of line
        ifd
    }

    fn roundtrip(order: ByteOrder, variant: Variant) {
        let file = TiffFile {
            order,
            variant,
            ifds: vec![sample_ifd()],
        };
        let bytes = write(&file).expect("write");
        let parsed = read(&bytes).expect("read back");
        assert_eq!(parsed, file);
    }

    fn multi_ifd_roundtrip(variant: Variant) {
        let mut second = Ifd::new();
        second.set(256, Value::Short(vec![1]));
        second.set(257, Value::Short(vec![1]));
        let file = TiffFile {
            order: ByteOrder::LittleEndian,
            variant,
            ifds: vec![sample_ifd(), second],
        };
        let bytes = write(&file).expect("write");
        let parsed = read(&bytes).expect("read back");
        assert_eq!(parsed.ifds.len(), 2);
        assert_eq!(parsed, file);
    }

    #[test]
    fn classic_single_ifd_roundtrips_both_orders() {
        roundtrip(ByteOrder::LittleEndian, Variant::Classic);
        roundtrip(ByteOrder::BigEndian, Variant::Classic);
    }

    #[test]
    fn classic_multi_ifd_chain_roundtrips() {
        multi_ifd_roundtrip(Variant::Classic);
    }

    #[test]
    fn write_layout_is_tight() {
        // The exact stream length pins the two-pass cursor math — `ifd_size`, the per-IFD advance,
        // and the value-pool append — that a read->write->read round-trip can't see: the reader
        // follows stored offsets, so a too-large IFD size or a wrong cursor step only inserts gaps
        // it still parses back correctly.
        let one = write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![sample_ifd()],
        })
        .expect("write");
        assert_eq!(one.len(), 100);
        let mut second = Ifd::new();
        second.set(256, Value::Short(vec![1]));
        second.set(257, Value::Short(vec![1]));
        let two = write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![sample_ifd(), second],
        })
        .expect("write");
        assert_eq!(two.len(), 130);
    }

    fn subifd_tree_roundtrips(order: ByteOrder, variant: Variant) {
        // IFD0 carries a raw sub-IFD (tag 330) with two children and an EXIF sub-IFD (tag 34665).
        let mut raw_a = Ifd::new();
        raw_a.set(256, Value::Short(vec![16]));
        raw_a.set(257, Value::Short(vec![16]));
        raw_a.set(254, Value::Long(vec![0])); // NewSubFileType = full-resolution
        let mut raw_b = Ifd::new();
        raw_b.set(256, Value::Short(vec![8]));
        raw_b.set(257, Value::Short(vec![8]));
        let mut exif = Ifd::new();
        exif.set(33434, Value::Rational(vec![(1, 100)])); // ExposureTime

        let mut root = sample_ifd();
        root.set_sub_ifd(330, vec![raw_a.clone(), raw_b.clone()]);
        root.set_sub_ifd(34665, vec![exif.clone()]);

        let file = TiffFile {
            order,
            variant,
            ifds: vec![root],
        };
        let bytes = write(&file).expect("write");

        // The generic reader returns just the top-level chain (the children are not chained).
        let parsed = read(&bytes).expect("read back");
        assert_eq!(parsed.ifds.len(), 1);
        let root_ifd = &parsed.ifds[0];
        // Its real fields survive...
        assert_eq!(root_ifd.get(256), Some(&Value::Short(vec![640])));
        // ...and the synthesised pointer tags are present as offset arrays.
        let sub_offsets = root_ifd.get_u32_vec(330).expect("SubIFDs pointer");
        assert_eq!(sub_offsets.len(), 2);
        let exif_offset = root_ifd.get_u32(34665).expect("ExifIFD pointer");

        // Following the pointers re-parses the children exactly.
        assert_eq!(
            read_ifd_at(&bytes, sub_offsets[0].into(), order, variant).unwrap(),
            raw_a
        );
        assert_eq!(
            read_ifd_at(&bytes, sub_offsets[1].into(), order, variant).unwrap(),
            raw_b
        );
        assert_eq!(
            read_ifd_at(&bytes, exif_offset.into(), order, variant).unwrap(),
            exif
        );
    }

    #[test]
    fn classic_subifd_tree_roundtrips_both_orders() {
        subifd_tree_roundtrips(ByteOrder::LittleEndian, Variant::Classic);
        subifd_tree_roundtrips(ByteOrder::BigEndian, Variant::Classic);
    }

    #[test]
    fn nested_subifd_tree_roundtrips() {
        // A grandchild: IFD0 -> SubIFD -> its own (e.g. EXIF) sub-IFD, exercising recursion.
        let mut grandchild = Ifd::new();
        grandchild.set(33434, Value::Rational(vec![(1, 200)]));
        let mut child = Ifd::new();
        child.set(256, Value::Short(vec![32]));
        child.set_sub_ifd(34665, vec![grandchild.clone()]);
        let mut root = Ifd::new();
        root.set(256, Value::Short(vec![64]));
        root.set_sub_ifd(330, vec![child]);

        let file = TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![root],
        };
        let bytes = write(&file).expect("write");
        let parsed = read(&bytes).expect("read");
        let child_off = parsed.ifds[0].get_u32(330).expect("SubIFDs");
        let child_ifd = read_ifd_at(
            &bytes,
            child_off.into(),
            ByteOrder::LittleEndian,
            Variant::Classic,
        )
        .expect("child");
        assert_eq!(child_ifd.get(256), Some(&Value::Short(vec![32])));
        let gc_off = child_ifd.get_u32(34665).expect("nested ExifIFD");
        let gc = read_ifd_at(
            &bytes,
            gc_off.into(),
            ByteOrder::LittleEndian,
            Variant::Classic,
        )
        .expect("grandchild");
        assert_eq!(gc, grandchild);
    }

    #[cfg(feature = "bigtiff")]
    #[test]
    fn bigtiff_roundtrips_and_inline_threshold() {
        roundtrip(ByteOrder::LittleEndian, Variant::Big);
        roundtrip(ByteOrder::BigEndian, Variant::Big);
        multi_ifd_roundtrip(Variant::Big);
        subifd_tree_roundtrips(ByteOrder::LittleEndian, Variant::Big);

        // The 8-byte XResolution rational is out of line in classic TIFF (>4 B) but packs inline
        // in BigTIFF (<=8 B); both must round-trip identically.
        let bytes = write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Big,
            ifds: vec![sample_ifd()],
        })
        .expect("write");
        assert_eq!(&bytes[0..2], b"II");
        let (order, variant, first) = read_header(&bytes).expect("header");
        assert_eq!(order, ByteOrder::LittleEndian);
        assert_eq!(variant, Variant::Big);
        assert_eq!(bytes[2], 0x2b); // magic 43
        assert_eq!(first, 16); // 16-byte header
        let parsed = read(&bytes).expect("read back");
        assert_eq!(parsed.variant, Variant::Big);
        assert_eq!(
            parsed.ifds[0].get(282),
            Some(&Value::Rational(vec![(300, 1)]))
        );
    }

    #[test]
    fn subifd_children_are_placed_consecutively_after_the_root() {
        // Pass-1a places each directory right after the previous one, word-aligned. With a single
        // inline field each, the sizes are exact, so the children land at fixed offsets — pinning the
        // per-directory cursor arithmetic (which round-trips can't, since wrong-but-consistent
        // offsets still parse).
        let mut child_a = Ifd::new();
        child_a.set(256, Value::Short(vec![1]));
        let mut child_b = Ifd::new();
        child_b.set(256, Value::Short(vec![1]));
        let mut root = Ifd::new();
        root.set(256, Value::Short(vec![1]));
        root.set_sub_ifd(330, vec![child_a, child_b]);

        let bytes = write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![root],
        })
        .expect("write");
        let offs = read(&bytes).expect("read").ifds[0]
            .get_u32_vec(330)
            .expect("SubIFDs pointer");
        // header(8) + root dir(count 2 + 2 entries*12 + next 4 = 30) -> child A at 38;
        // child A dir(2 + 1*12 + 4 = 18) -> child B at 56.
        assert_eq!(offs, vec![38, 56]);
    }

    #[test]
    fn out_of_line_value_pool_is_tightly_packed() {
        // sample_ifd carries several out-of-line values; each advances the cursor by its own length.
        // A mutated advance (e.g. `*=` for `+=`) would balloon the file far past this bound.
        let bytes = write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![sample_ifd()],
        })
        .expect("write");
        assert!(
            bytes.len() < 256,
            "value pool not tightly packed: {} bytes",
            bytes.len()
        );
    }

    /// An unknown-type entry survives read → write **byte-exactly**: for an already-canonical
    /// fixture the rewrite reproduces the input bit-for-bit, opaque value/offset word included —
    /// the issue #263 preservation requirement.
    #[test]
    fn unknown_type_entries_write_back_verbatim() {
        let data = [
            b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, // header, first IFD @ 8
            0x01, 0x00, // entry count = 1
            0x99, 0x99, // tag 0x9999
            0xf0, 0x00, // type 0xF0 (unknown)
            0x01, 0x00, 0x00, 0x00, // value count = 1
            0xde, 0xad, 0xbe, 0xef, // opaque value/offset word
            0x00, 0x00, 0x00, 0x00, // next IFD = 0
        ];
        let file = read(&data).expect("read");
        assert_eq!(write(&file).expect("write"), data);

        // Big-endian: the word is emitted verbatim and the record pins byte-for-byte.
        let u = crate::UnknownValue::new(
            0xF0,
            3,
            &[1, 2, 3, 4],
            ByteOrder::BigEndian,
            Variant::Classic,
        )
        .expect("capture");
        let mut ifd = Ifd::new();
        ifd.set(0x9999, Value::Unknown(u));
        let file = TiffFile {
            order: ByteOrder::BigEndian,
            variant: Variant::Classic,
            ifds: vec![ifd],
        };
        let bytes = write(&file).expect("write");
        assert_eq!(read(&bytes).expect("read back"), file);
        // Entry record at 10: tag, type code, count (all big-endian), then the verbatim word.
        assert_eq!(
            &bytes[10..22],
            &[0x99, 0x99, 0x00, 0xF0, 0, 0, 0, 3, 1, 2, 3, 4]
        );
    }

    /// A BigTIFF unknown-type entry keeps its 8-byte word verbatim through read → write.
    #[cfg(feature = "bigtiff")]
    #[test]
    fn bigtiff_unknown_type_entries_write_back_verbatim() {
        let u = crate::UnknownValue::new(
            0xF0,
            2,
            &[8, 7, 6, 5, 4, 3, 2, 1],
            ByteOrder::LittleEndian,
            Variant::Big,
        )
        .expect("capture");
        let mut ifd = Ifd::new();
        ifd.set(0x9999, Value::Unknown(u));
        let file = TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Big,
            ifds: vec![ifd],
        };
        let bytes = write(&file).expect("write");
        assert_eq!(read(&bytes).expect("read back"), file);
        assert_eq!(write(&read(&bytes).expect("read")).expect("rewrite"), bytes);
    }

    /// The opaque word cannot be transcoded: writing an unknown-type value into a stream of a
    /// different byte order (or variant) is a typed error, not a silent corruption.
    #[test]
    fn unknown_type_write_refuses_transcode() {
        let u = crate::UnknownValue::new(
            0xF0,
            1,
            &[1, 2, 3, 4],
            ByteOrder::LittleEndian,
            Variant::Classic,
        )
        .expect("capture");
        let mut ifd = Ifd::new();
        ifd.set(0x9999, Value::Unknown(u));
        // Same order and variant: fine.
        assert!(
            write(&TiffFile {
                order: ByteOrder::LittleEndian,
                variant: Variant::Classic,
                ifds: vec![ifd.clone()],
            })
            .is_ok()
        );
        // Flipped byte order: refused.
        assert!(
            write(&TiffFile {
                order: ByteOrder::BigEndian,
                variant: Variant::Classic,
                ifds: vec![ifd.clone()],
            })
            .is_err()
        );
        // Changed variant: refused (the word width itself no longer matches).
        #[cfg(feature = "bigtiff")]
        assert!(
            write(&TiffFile {
                order: ByteOrder::LittleEndian,
                variant: Variant::Big,
                ifds: vec![ifd],
            })
            .is_err()
        );
    }

    /// The audit closed loop: the map `write_with` declares is fully classified by
    /// construction, and an independent audited read of the emitted bytes reproduces it
    /// **segment for segment** (after padding classification) — the writer cannot claim a
    /// layout it did not emit, and the reader cannot see a layout the writer did not declare.
    #[test]
    fn write_with_map_matches_an_independent_audit() {
        // The sample carries inline and out-of-line values including an odd-length ASCII, so
        // real padding participates.
        let file = TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![sample_ifd()],
        };
        let (bytes, map) = write_with(&file, &WriteOptions::default()).expect("write");
        let writer_report = map.finish(None);
        assert!(
            writer_report.is_fully_classified(),
            "writer must declare every byte: {writer_report:?}"
        );

        let (parsed, mut audit_report) = crate::read_audited(&bytes).expect("audit");
        assert_eq!(parsed, file);
        audit_report
            .classify_padding(&mut (&bytes[..]))
            .expect("classify");
        assert!(audit_report.is_fully_classified());
        assert_eq!(writer_report.segments, audit_report.segments);
    }

    /// A vendor preamble lands immediately after the header, the directory region starts after
    /// it, and the run is declared — so an audited read of the result sees the same preamble the
    /// writer put there. This is what lets a rewrite keep Apple ProRAW's `APPLEDNG` signature at
    /// the offset a vendor tool looks for.
    #[test]
    fn a_preamble_lands_between_the_header_and_the_first_directory() {
        let file = TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![sample_ifd()],
        };
        let preamble = b"APPLEDNG\0\0";
        let opts = WriteOptions::default().with_preamble(*preamble);
        let (bytes, map) = write_with(&file, &opts).expect("write");

        let header_len = Variant::Classic.header_size();
        assert_eq!(&bytes[header_len..header_len + preamble.len()], preamble);
        // The first directory starts after the preamble, word-aligned — not at the header's end.
        let (_, _, first) = read_header(&bytes).expect("header");
        assert_eq!(first, align_word((header_len + preamble.len()) as u64));

        // Declared, not merely emitted: the writer's map is complete and an independent audit
        // reproduces it, preamble segment included.
        let writer_report = map.finish(None);
        assert!(
            writer_report.is_fully_classified(),
            "writer must declare the preamble: {writer_report:?}"
        );
        assert!(
            writer_report
                .segments
                .iter()
                .any(|s| s.kind == SpanKind::Preamble
                    && s.range.start == header_len as u64
                    && s.range.len == preamble.len() as u64),
            "the preamble must be declared as one: {writer_report:?}"
        );
        let (parsed, mut audit_report) = crate::read_audited(&bytes).expect("audit");
        assert_eq!(parsed, file, "the directory survives a preamble");
        audit_report
            .classify_padding(&mut (&bytes[..]))
            .expect("classify");
        audit_report.classify_unclaimed();
        assert_eq!(writer_report.segments, audit_report.segments);
    }

    /// An empty preamble is the default and must change nothing: byte-for-byte the same stream.
    #[test]
    fn an_empty_preamble_reproduces_the_plain_layout() {
        let file = TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![sample_ifd()],
        };
        let plain = write_with(&file, &WriteOptions::default())
            .expect("write")
            .0;
        let empty = write_with(&file, &WriteOptions::default().with_preamble(Vec::new()))
            .expect("write")
            .0;
        assert_eq!(plain, empty);
    }

    /// An odd-length preamble still leaves the directory word-aligned, with the one filler byte
    /// declared as part of the preamble rather than as a separate padding span — nothing on disk
    /// separates it from the vendor bytes, so an independent audit reproduces the map only if the
    /// writer declares the whole gap.
    #[test]
    fn an_odd_length_preamble_is_padded_to_the_word_boundary() {
        let file = TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![sample_ifd()],
        };
        let opts = WriteOptions::default().with_preamble(vec![0xABu8; 5]);
        let (bytes, map) = write_with(&file, &opts).expect("write");
        let header_len = Variant::Classic.header_size() as u64;
        let (_, _, first) = read_header(&bytes).expect("header");
        assert_eq!(first, header_len + 6, "5 bytes of preamble, padded to 6");
        assert_eq!(bytes[13], 0, "the filler byte is zero");

        let writer_report = map.finish(None);
        assert!(writer_report.is_fully_classified());
        assert!(
            writer_report.segments.contains(&Segment {
                range: Range {
                    start: header_len,
                    len: 6
                },
                kind: SpanKind::Preamble,
            }),
            "the whole header/directory gap is the preamble: {writer_report:?}"
        );
        let (parsed, mut audit_report) = crate::read_audited(&bytes).expect("audit");
        assert_eq!(parsed, file);
        audit_report
            .classify_padding(&mut (&bytes[..]))
            .expect("classify");
        audit_report.classify_unclaimed();
        assert_eq!(writer_report.segments, audit_report.segments);
    }

    /// An all-zero preamble is a preamble, not padding: the audit's padding pass must leave the
    /// header/first-directory gap alone even when its bytes are zero, or the two views of the
    /// same stream disagree on what those bytes are.
    #[test]
    fn an_all_zero_preamble_stays_a_preamble() {
        let file = TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![sample_ifd()],
        };
        let opts = WriteOptions::default().with_preamble(vec![0u8; 4]);
        let (bytes, map) = write_with(&file, &opts).expect("write");
        let header_len = Variant::Classic.header_size() as u64;
        let (_, _, first) = read_header(&bytes).expect("header");
        assert_eq!(first, header_len + 4);

        let writer_report = map.finish(None);
        assert!(writer_report.is_fully_classified());
        let (_, mut audit_report) = crate::read_audited(&bytes).expect("audit");
        audit_report
            .classify_padding(&mut (&bytes[..]))
            .expect("classify");
        audit_report.classify_unclaimed();
        assert!(
            audit_report.is_fully_classified(),
            "audit: {audit_report:?}"
        );
        assert_eq!(
            audit_report.unclaimed_spans(),
            vec![Segment {
                range: Range {
                    start: header_len,
                    len: 4
                },
                kind: SpanKind::Preamble,
            }],
            "zeros after the header are the preamble, not padding"
        );
        assert_eq!(writer_report.segments, audit_report.segments);
    }

    /// Pinning lands a value at its exact absolute offset, the pool flows around it, the
    /// filler is declared padding, and the pointer word points at the pin.
    #[test]
    fn pinned_value_lands_at_its_offset_with_declared_padding() {
        let mut ifd = Ifd::new();
        ifd.set(256, Value::Short(vec![640]));
        // A maker-note-like opaque blob, pinned far beyond the natural layout.
        let note = vec![0xC5u8; 10];
        ifd.set(37500, Value::Undefined(note.clone()));
        ifd.set(258, Value::Short(vec![8, 8, 8])); // 6 bytes: flows around the pin
        let file = TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![ifd],
        };
        let pin_at = 100u64;
        let opts = WriteOptions::default().pin(37500, pin_at);
        let (bytes, map) = write_with(&file, &opts).expect("write");
        // The blob sits exactly at the pin.
        assert_eq!(&bytes[pin_at as usize..pin_at as usize + note.len()], &note);
        // The entry's value offset points at the pin.
        let parsed = read(&bytes).expect("read");
        assert_eq!(parsed.ifds[0].get(37500), Some(&Value::Undefined(note)));
        // Every byte is declared, including the filler up to the pin.
        let report = map.finish(None);
        assert!(report.is_fully_classified(), "report: {report:?}");
        assert!(
            report
                .segments
                .iter()
                .any(|s| s.kind == crate::SpanKind::Padding && s.range.end() == pin_at),
            "filler before the pin is declared padding: {report:?}"
        );
        // The stream ends at the pin's end (nothing after it).
        assert_eq!(bytes.len() as u64, pin_at + 10);
    }

    #[test]
    fn overlapping_pins_are_rejected() {
        // Two pinned spans that intersect cannot both be honoured, and silently dropping one
        // would relocate a vendor blob whose internal offsets the pin exists to keep valid.
        let mut ifd = Ifd::new();
        ifd.set(37500, Value::Undefined(vec![0xC5; 10]));
        ifd.set(37501, Value::Undefined(vec![0xD6; 10]));
        let file = TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![ifd],
        };

        // 100..110 and 105..115 overlap by five bytes.
        let opts = WriteOptions::default().pin(37500, 100).pin(37501, 105);
        let error = write_with(&file, &opts).expect_err("overlapping pins must be rejected");

        assert_eq!(error.static_message(), Some("TIFF: pinned spans overlap"));
    }

    #[test]
    fn pins_that_merely_abut_are_accepted() {
        // The boundary the overlap check decides: a span ending exactly where the next begins is
        // not an overlap. A `<` mutated to `<=` would reject this.
        let mut ifd = Ifd::new();
        ifd.set(37500, Value::Undefined(vec![0xC5; 10]));
        ifd.set(37501, Value::Undefined(vec![0xD6; 10]));
        let file = TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![ifd],
        };

        // 100..110 then 110..120 -- adjacent, not overlapping.
        let opts = WriteOptions::default().pin(37500, 100).pin(37501, 110);
        let (bytes, _) = write_with(&file, &opts).expect("abutting pins are legal");

        assert_eq!(&bytes[100..110], &[0xC5; 10]);
        assert_eq!(&bytes[110..120], &[0xD6; 10]);
    }

    /// A pool value that ends exactly where a pin begins does **not** jump the pin.
    ///
    /// The sibling of `a_pool_value_is_pushed_past_a_pin_it_would_have_landed_in`, on the other
    /// side of the boundary. The jump test is `p.offset < c + n`: the pin starts before the
    /// value would end, i.e. they overlap. Merely abutting is not overlapping, and a value that
    /// jumped anyway would leave a hole and a longer file for no reason.
    ///
    /// Nothing distinguished `<` from `<=` there before this (#110). Every other pin test either
    /// overlaps by a wide margin or does not come near, so the one input where the two operators
    /// disagree -- exact abutment -- was never written.
    #[test]
    fn a_pool_value_that_merely_abuts_a_pin_stays_where_it_is() {
        let mut ifd = Ifd::new();
        let pinned = vec![0xC5u8; 10];
        let pooled = vec![0x77u8; 20];
        ifd.set(37500, Value::Undefined(pinned.clone()));
        ifd.set(700, Value::Undefined(pooled.clone()));
        ifd.set(256, Value::Short(vec![640]));
        let file = TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![ifd],
        };

        // Three entries put the directory's end -- and so the pool -- at 50, and the 20-byte
        // pooled value therefore occupies 50..70. Pinning at exactly 70 abuts it.
        let pool_start = 50u64;
        let pin_at = pool_start + pooled.len() as u64;
        let opts = WriteOptions::default().pin(37500, pin_at);
        let (bytes, map) = write_with(&file, &opts).expect("write");

        let pooled_at = bytes
            .windows(pooled.len())
            .position(|w| w == pooled.as_slice())
            .expect("the pooled value is in the file") as u64;
        assert_eq!(
            pooled_at, pool_start,
            "abutting is not overlapping: the value must not have jumped the pin"
        );
        assert_eq!(
            &bytes[pin_at as usize..pin_at as usize + pinned.len()],
            pinned.as_slice(),
            "and the pin is still intact"
        );

        // Both values read back, and every byte is still accounted for.
        let parsed = read(&bytes).expect("read");
        assert_eq!(parsed.ifds[0].get(700), Some(&Value::Undefined(pooled)));
        assert_eq!(parsed.ifds[0].get(37500), Some(&Value::Undefined(pinned)));
        assert!(map.finish(None).is_fully_classified());
    }

    #[test]
    fn a_pool_value_is_pushed_past_a_pin_it_would_have_landed_in() {
        // The pool flows *around* pinned ranges. A pool value whose natural placement intersects
        // a pin must jump to the far side of it rather than overwrite it.
        let mut ifd = Ifd::new();
        let pinned = vec![0xC5u8; 10];
        let pooled = vec![0x77u8; 20];
        ifd.set(37500, Value::Undefined(pinned.clone()));
        ifd.set(700, Value::Undefined(pooled.clone()));
        ifd.set(256, Value::Short(vec![640]));
        let file = TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![ifd],
        };

        // The directory ends at 50 for three entries, so an unpinned 20-byte value would sit at
        // 50..70 -- straight through this pin.
        let pin_at = 60u64;
        let opts = WriteOptions::default().pin(37500, pin_at);
        let (bytes, map) = write_with(&file, &opts).expect("write");

        // The pin is intact: the pool did not write over it.
        assert_eq!(
            &bytes[pin_at as usize..pin_at as usize + pinned.len()],
            pinned.as_slice()
        );

        // Both values still read back, so the pooled one was relocated rather than truncated.
        let parsed = read(&bytes).expect("read");
        assert_eq!(
            parsed.ifds[0].get(37500),
            Some(&Value::Undefined(pinned.clone()))
        );
        assert_eq!(
            parsed.ifds[0].get(700),
            Some(&Value::Undefined(pooled.clone()))
        );

        // Where it landed, not merely that it moved. The jump goes to the word-aligned end of the
        // pin, so an off-by-one in that alignment relocates the value correctly and still puts it
        // in the wrong place -- which every assertion above would accept.
        let pooled_at = bytes
            .windows(pooled.len())
            .position(|w| w == pooled.as_slice())
            .expect("the pooled value is in the file");
        assert_eq!(
            pooled_at as u64,
            align_word(pin_at + pinned.len() as u64),
            "the pool resumes at the word-aligned end of the pin"
        );

        // And the jump left no unaccounted bytes behind it.
        let report = map.finish(None);
        assert!(report.is_fully_classified(), "report: {report:?}");
    }

    #[test]
    fn pin_may_start_exactly_at_directory_end() {
        let note = vec![0xA5; 10];
        let mut ifd = Ifd::new();
        ifd.set(37500, Value::Undefined(note.clone()));
        let file = TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![ifd],
        };

        // Classic header (8) + one-entry directory (18) ends exactly at byte 26.
        let (bytes, _) = write_with(&file, &WriteOptions::default().pin(37500, 26))
            .expect("pin at directory boundary");
        assert_eq!(&bytes[26..36], note.as_slice());
    }

    /// Unsatisfiable pins are typed errors: absent tag, duplicated tag, inline value,
    /// unknown-type value, directory collision, and overlapping pins.
    #[test]
    fn unsatisfiable_pins_are_refused() {
        let pin = |tag, offset| WriteOptions::default().pin(tag, offset);
        let le = |ifds| TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds,
        };
        let mut ifd = Ifd::new();
        ifd.set(37500, Value::Undefined(vec![1; 10]));
        ifd.set(256, Value::Short(vec![1])); // inline
        // Absent tag.
        assert!(write_with(&le(vec![ifd.clone()]), &pin(999, 100)).is_err());
        // Inline value: no span to pin.
        assert!(write_with(&le(vec![ifd.clone()]), &pin(256, 100)).is_err());
        // Offset inside the directory region.
        assert!(write_with(&le(vec![ifd.clone()]), &pin(37500, 8)).is_err());
        // Duplicated tag (present in two directories of the chain).
        assert!(write_with(&le(vec![ifd.clone(), ifd.clone()]), &pin(37500, 100)).is_err());
        // Overlapping pins.
        let mut two = ifd.clone();
        two.set(700, Value::Undefined(vec![2; 10]));
        let opts = WriteOptions::default().pin(37500, 100).pin(700, 105);
        assert!(write_with(&le(vec![two]), &opts).is_err());
        // A valid pin on the same fixture still succeeds (the guards are not blanket).
        assert!(write_with(&le(vec![ifd]), &pin(37500, 100)).is_ok());
    }

    /// `align_word` is the exported form of the writer's alignment rule: identity on even,
    /// round-up on odd, saturating at the top of `u64`.
    #[test]
    fn align_word_rounds_up_to_even() {
        assert_eq!(align_word(0), 0);
        assert_eq!(align_word(7), 8);
        assert_eq!(align_word(8), 8);
        assert_eq!(align_word(u64::MAX), u64::MAX);
    }

    /// The 4 GiB guard's boundary, on the pure predicate (a >4 GiB allocation is not testable):
    /// the largest classic offset is representable; one past it is not; BigTIFF never overflows.
    #[test]
    fn layout_overflow_boundary() {
        assert!(!layout_overflows(Variant::Classic, 100));
        assert!(!layout_overflows(Variant::Classic, u64::from(u32::MAX)));
        assert!(layout_overflows(Variant::Classic, u64::from(u32::MAX) + 1));
        #[cfg(feature = "bigtiff")]
        assert!(!layout_overflows(Variant::Big, u64::from(u32::MAX) + 1));
    }

    /// Exactly `u16::MAX` entries — the largest classic-representable directory — must write
    /// (pinning the entry-count guard's boundary, with the rejection test just below).
    #[test]
    fn classic_write_accepts_a_full_directory() {
        let mut ifd = Ifd::new();
        for tag in 0..u16::MAX {
            ifd.set(tag, Value::Short(vec![0]));
        }
        let bytes = write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![ifd],
        })
        .expect("write");
        assert_eq!(
            read(&bytes).expect("read").ifds[0].fields().len(),
            usize::from(u16::MAX)
        );
    }

    /// A directory of `u16::MAX + 1` entries, one past the classic 2-byte entry count.
    fn oversized_ifd() -> Ifd {
        let mut ifd = Ifd::new();
        for tag in 0..=u16::MAX {
            ifd.set(tag, Value::Short(vec![0]));
        }
        ifd
    }

    /// Classic TIFF stores the entry count in 2 bytes; a directory that cannot fit must be a
    /// typed error, not a silently truncated `as u16` count.
    #[test]
    fn classic_write_rejects_oversized_directory() {
        let file = TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![oversized_ifd()],
        };
        assert!(write(&file).is_err());
    }

    /// The same directory is representable in BigTIFF's 8-byte entry count — the guard is
    /// classic-only, not a blanket cap.
    #[cfg(feature = "bigtiff")]
    #[test]
    fn bigtiff_write_accepts_oversized_directory() {
        let file = TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Big,
            ifds: vec![oversized_ifd()],
        };
        let bytes = write(&file).expect("write");
        assert_eq!(read(&bytes).expect("read").ifds[0].fields().len(), 65536);
    }
}
