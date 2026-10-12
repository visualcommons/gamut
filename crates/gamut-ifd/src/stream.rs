//! A lazy, streaming IFD reader over any [`ReadAt`] source.
//!
//! Where [`read`](crate::read) eagerly materialises every value from a full in-memory slice,
//! [`IfdReader`] fetches only what is asked for: [`read_ifd`](IfdReader::read_ifd) reads a
//! directory *body* (count, entries, next pointer — one small contiguous span), leaving each
//! entry raw; a value's bytes are fetched and decoded on demand by [`value`](IfdReader::value).
//! Over a multi-hundred-MB camera file the parse therefore touches kilobytes — the header, the
//! directories on the path a decoder actually follows, and the values it actually reads.
//!
//! This module is **the** parser: the slice functions ([`read`](crate::read),
//! [`read_ifd_at`](crate::read_ifd_at), [`read_tree`](crate::read_tree)) are thin wrappers over
//! an `IfdReader<&[u8]>`, so there is exactly one directory-body walk and one set of
//! hostile-input guards (the chain loop/length guard, the sub-IFD depth/cycle guards) —
//! byte-accounting or robustness rules cannot drift between two copies.
//! `tests/robustness.rs` still drives both entry points over the hostile corpus as a
//! regression gate on the wrappers.

use gamut_core::{Error, Result};

use crate::audit::AuditFinding;
use crate::reader::{ChainGuard, offset_at, read_header, resolve_pointers_with, u16_at};
use crate::segment::{Claim, SegmentMap, SpanKind};
use crate::value::UnknownValue;
use crate::{ByteOrder, FieldType, Ifd, ReadAt, TiffFile, Value, Variant};

/// A lazy, streaming IFD reader: walks TIFF structure through positioned reads on a [`ReadAt`]
/// source instead of requiring the whole file as a slice.
///
/// Construct with [`open`](Self::open) for a headered TIFF/IFD stream, or
/// [`with_layout`](Self::with_layout) for a bare directory whose byte order and variant are
/// known from an enclosing container (a maker note, an embedded TIFF block).
///
/// The laziness contract: [`read_ifd`](Self::read_ifd) touches only the directory body,
/// [`value`](Self::value) only that value's span — over a multi-hundred-MB camera file the
/// whole parse reads kilobytes.
#[derive(Debug)]
pub struct IfdReader<S> {
    source: S,
    order: ByteOrder,
    variant: Variant,
    first: u64,
}

/// One directory read raw: entries undecoded, values unfetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawIfd {
    /// The absolute offset this directory was read from.
    pub offset: u64,
    /// The entries in on-disk order. TIFF 6.0 §2 requires ascending tags, but hostile files may
    /// not comply, so this preserves what the file said rather than sorting.
    pub entries: Vec<RawEntry>,
    /// The next-IFD offset (`0` if this is the last directory of its chain).
    pub next: u64,
}

impl RawIfd {
    /// Returns the first entry with `tag`, or `None` if absent.
    ///
    /// A linear scan: the on-disk order is untrusted, so no binary search is possible. (The
    /// eager [`Ifd`] keeps the *last* duplicate; a raw directory exposes all of them.)
    #[must_use]
    pub fn entry(&self, tag: u16) -> Option<&RawEntry> {
        self.entries.iter().find(|e| e.tag == tag)
    }
}

/// One undecoded IFD entry: the on-disk 12-byte (classic) or 20-byte (BigTIFF) record, with the
/// value/offset word kept as raw bytes in the file's byte order.
///
/// Keeping the word raw (rather than pre-decoding it as an integer) is what makes the later
/// [`IfdReader::value`] call byte-for-byte identical to the eager path: an inline value is
/// decoded from these bytes exactly as it would be from the entry record in the file — a
/// 3-byte inline `BYTE[3]` value has no integer meaning at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEntry {
    /// The 16-bit tag identifying the field.
    pub tag: u16,
    /// The raw on-disk field-type code — kept raw so unknown codes are representable; see
    /// [`field_type`](Self::field_type).
    pub type_code: u16,
    /// The declared value count (untrusted until sized against the source by
    /// [`IfdReader::value`]).
    pub count: u64,
    /// The absolute offset of this entry record, for diagnostics.
    pub offset: u64,
    /// The raw value/offset field in the file's byte order; classic TIFF fills `word[..4]`.
    word: [u8; 8],
}

impl RawEntry {
    /// The recognised field type, or `None` for an unknown code.
    ///
    /// An unknown-type entry cannot be decoded, but it is never dropped:
    /// [`IfdReader::decode_ifd`], [`IfdReader::read_file`], and [`IfdReader::value`] all
    /// preserve it verbatim as a [`Value::Unknown`], mirroring [`read`](crate::read).
    #[must_use]
    pub fn field_type(&self) -> Option<FieldType> {
        FieldType::from_code(self.type_code)
    }
}

impl<S: ReadAt> IfdReader<S> {
    /// Opens a TIFF/IFD stream: parses and validates the header (byte-order mark, magic, and —
    /// for BigTIFF — the fixed offset-size/reserved fields), like [`read_header`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if the header is not valid, or [`Error::Io`] if the
    /// source fails.
    pub fn open(mut source: S) -> Result<Self> {
        // Read exactly the 8-byte classic header first; only a BigTIFF magic (43, in either
        // byte order) needs the rest of its 16-byte header. Fetching precisely what the parse
        // consumes keeps a tracked read ledger byte-exact — a blanket 16-byte probe would
        // "read" 8 bytes of a classic file's first directory that no structure claims.
        let len = source.len()?;
        let mut head = [0u8; 16];
        let first8 = len.min(8) as usize;
        source.read_exact_at(0, &mut head[..first8])?;
        let big = first8 == 8 && (head[2..4] == [0x2b, 0x00] || head[2..4] == [0x00, 0x2b]);
        let head_len = if big {
            let rest = (len.min(16) as usize) - 8;
            source.read_exact_at(8, &mut head[8..8 + rest])?;
            8 + rest
        } else {
            first8
        };
        let (order, variant, first) = read_header(&head[..head_len])?;
        Ok(Self {
            source,
            order,
            variant,
            first,
        })
    }

    /// Wraps a source that carries **no header** — a maker-note mini-IFD, or any directory
    /// whose byte order and variant are known from an enclosing container. The first-IFD offset
    /// is `0` (meaning "none"); drive the reader with [`read_ifd`](Self::read_ifd) at explicit
    /// offsets, typically over a [`Rebased`](crate::Rebased) source so the directory's internal
    /// offsets resolve.
    #[must_use]
    pub fn with_layout(source: S, order: ByteOrder, variant: Variant) -> Self {
        Self {
            source,
            order,
            variant,
            first: 0,
        }
    }

    /// The byte order the stream was written in.
    #[must_use]
    pub fn order(&self) -> ByteOrder {
        self.order
    }

    /// Whether the stream is classic TIFF or BigTIFF.
    #[must_use]
    pub fn variant(&self) -> Variant {
        self.variant
    }

    /// The header's first-IFD offset (`0` when constructed with
    /// [`with_layout`](Self::with_layout)).
    #[must_use]
    pub fn first_ifd_offset(&self) -> u64 {
        self.first
    }

    /// Reads the directory at `offset`: its raw entries and next-IFD offset, in two small
    /// positioned reads (the count field, then the body). Values are neither fetched nor
    /// decoded.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if the directory is truncated, extends past the end of
    /// the source, or its entry count overflows; [`Error::Io`] if the source fails.
    pub fn read_ifd(&mut self, offset: u64) -> Result<RawIfd> {
        let entry_size = self.variant.entry_size() as u64;
        let count_size = self.variant.count_size() as u64;
        let offset_size = self.variant.offset_size() as u64;
        let len = self.source.len()?;

        let mut count_buf = [0u8; 8];
        self.source
            .read_exact_at(offset, &mut count_buf[..count_size as usize])?;
        let count = match self.variant {
            Variant::Classic => u64::from(self.order.u16([count_buf[0], count_buf[1]])),
            #[cfg(feature = "bigtiff")]
            Variant::Big => self.order.u64(count_buf),
        };

        // Checked in u64 end to end (no `usize` truncation): a hostile 8-byte BigTIFF count can
        // overflow the multiply, and any of the sums can wrap near u64::MAX.
        let entries_start = offset.checked_add(count_size).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: IFD entry count overflow")
        })?;
        let body_size = count
            .checked_mul(entry_size)
            .and_then(|n| n.checked_add(offset_size))
            .ok_or_else(|| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: IFD entry count overflow")
            })?;
        let body_end = entries_start.checked_add(body_size).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: IFD entry count overflow")
        })?;
        // Bound the directory to the source *before* allocating, so a corrupt count fails fast;
        // this also bounds the body buffer and entry vector by the source length.
        if body_end > len {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "TIFF: IFD extends past end of file",
            ));
        }
        let body_len = usize::try_from(body_size).map_err(|_| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: IFD entry count overflow")
        })?;
        let mut body = vec![0u8; body_len];
        self.source.read_exact_at(entries_start, &mut body)?;

        let entry_size = self.variant.entry_size();
        let offset_size = self.variant.offset_size();
        let mut entries = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            let pos = i * entry_size;
            let tag = u16_at(&body, pos, self.order)?;
            let type_code = u16_at(&body, pos + 2, self.order)?;
            let value_count = offset_at(&body, pos + 4, self.order, self.variant)?;
            let mut word = [0u8; 8];
            let value_pos = pos + 4 + offset_size;
            word[..offset_size].copy_from_slice(&body[value_pos..value_pos + offset_size]);
            entries.push(RawEntry {
                tag,
                type_code,
                count: value_count,
                offset: entries_start + (i as u64) * (entry_size as u64),
                word,
            });
        }
        let next = offset_at(&body, count as usize * entry_size, self.order, self.variant)?;
        Ok(RawIfd {
            offset,
            entries,
            next,
        })
    }

    /// Fetches (if out of line) and decodes `entry`'s value.
    ///
    /// An entry whose field-type code is unrecognised yields a verbatim [`Value::Unknown`]
    /// (nothing is fetched — its payload cannot be sized), matching the eager readers.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] for a count overflow or an out-of-bounds value offset,
    /// or [`Error::Io`] if the source fails.
    pub fn value(&mut self, entry: &RawEntry) -> Result<Value> {
        let Some(ty) = entry.field_type() else {
            return Ok(Value::Unknown(self.unknown_value(entry)?));
        };
        self.fetch_value(entry, ty, 0, None)
    }

    /// Captures an unknown-type entry's record verbatim (see [`Value::Unknown`]).
    fn unknown_value(&self, entry: &RawEntry) -> Result<UnknownValue> {
        let width = self.variant.offset_size();
        UnknownValue::new(
            entry.type_code,
            entry.count,
            &entry.word[..width],
            self.order,
            self.variant,
        )
    }

    /// The absolute offset of `entry`'s out-of-line value, or `None` if the value packs inline.
    ///
    /// `None` also for an unrecognised field type, whose byte length cannot be sized. The
    /// offset is *declared*, not validated — it may lie outside the source.
    #[must_use]
    pub fn value_offset(&self, entry: &RawEntry) -> Option<u64> {
        let ty = entry.field_type()?;
        // u128 so a hostile 64-bit count cannot wrap the multiply into "inline".
        let byte_len = u128::from(entry.count) * ty.size() as u128;
        if byte_len <= self.variant.inline_threshold() as u128 {
            return None;
        }
        Some(self.word_offset(entry))
    }

    /// Iterates the top-level IFD chain from the first-IFD offset, yielding each directory raw.
    ///
    /// The walk carries the same guards as [`read`](crate::read) — a repeated offset (loop) or
    /// a runaway chain is a typed error — and the iterator fuses after the first error.
    pub fn ifds(&mut self) -> IfdChain<'_, S> {
        IfdChain {
            next: self.first,
            reader: self,
            guard: ChainGuard::new(),
            done: false,
        }
    }

    /// Decodes every entry of `raw` into an [`Ifd`] — unknown field types are preserved
    /// verbatim as [`Value::Unknown`] and a duplicate tag keeps the last occurrence, the same
    /// semantics as the eager readers. The earlier occurrences are dropped without a signal here;
    /// [`decode_ifd_reporting`](Self::decode_ifd_reporting) returns the same directory and names
    /// them.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if a value is truncated or its offset is out of bounds,
    /// or [`Error::Io`] if the source fails.
    pub fn decode_ifd(&mut self, raw: &RawIfd) -> Result<Ifd> {
        self.decode_ifd_inner(raw, None)
    }

    /// [`decode_ifd`](Self::decode_ifd), plus what it discarded: one
    /// [`AuditFinding::ShadowedEntry`] per entry the decoded [`Ifd`] does not hold because a later
    /// entry repeats its tag, in on-disk order. A directory with one entry per tag — every
    /// conforming one (TIFF 6.0 §2) — reports nothing.
    ///
    /// The [`Ifd`] is exactly the one `decode_ifd` returns; only the signal is added. It holds one
    /// value per tag, so writing it back emits the survivor alone and the findings are the only
    /// record that the shadowed entries existed (see [`AuditFinding::ShadowedEntry`]).
    ///
    /// # Errors
    ///
    /// As [`decode_ifd`](Self::decode_ifd): a shadowed entry's value is still decoded, so a
    /// malformed one fails the directory just as it does there.
    pub fn decode_ifd_reporting(&mut self, raw: &RawIfd) -> Result<(Ifd, Vec<AuditFinding>)> {
        let ifd = self.decode_ifd_inner(raw, None)?;
        let mut findings = Vec::new();
        report_shadowed(raw, 0, &mut findings);
        Ok((ifd, findings))
    }

    /// Eagerly parses the whole top-level chain — [`read`](crate::read)'s streaming equivalent:
    /// `IfdReader::open(data)?.read_file()? == read(data)?` for any input, `Ok` or `Err`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] under the same conditions as [`read`](crate::read)
    /// (including "no IFD" for an empty chain — which a
    /// [`with_layout`](Self::with_layout) reader always has), or [`Error::Io`] if the source
    /// fails.
    pub fn read_file(&mut self) -> Result<TiffFile> {
        self.read_chain(None, None)
    }

    /// [`read_tree`](crate::read_tree)'s streaming equivalent: parses the top-level chain, then
    /// follows the named sub-IFD pointer tags with the same depth and cycle guards, reading
    /// only directory bodies and the values they reference.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] under the same conditions as
    /// [`read_tree`](crate::read_tree), or [`Error::Io`] if the source fails.
    pub fn read_tree(&mut self, pointer_tags: &[u16]) -> Result<TiffFile> {
        let mut file = self.read_file()?;
        // One visited set across every page, so two pages claiming one directory fail too.
        let mut visited = std::collections::BTreeSet::new();
        let scope = crate::SubtreeScope::new(pointer_tags, pointer_tags);
        let mut fetch = |off: u64| {
            let raw = self.read_ifd(off)?;
            self.decode_ifd(&raw)
        };
        for ifd in &mut file.ifds {
            resolve_pointers_with(&mut fetch, ifd, scope, &mut visited, 0)?;
        }
        Ok(file)
    }

    /// Like [`read_file`](Self::read_file) but records **typed** byte-range claims into `map`:
    /// the header, every directory body, and every out-of-line value span, each tagged with a
    /// [`SpanKind`] and [`Claim::Parsed`].
    ///
    /// Drive it over a [`Tracked`](crate::Tracked) source and pass the ledger to
    /// [`SegmentMap::finish`] to machine-check the claims against the bytes physically read.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] under the same conditions as [`read_file`](Self::read_file).
    pub fn read_file_audited(&mut self, map: &mut SegmentMap) -> Result<TiffFile> {
        self.read_chain(Some(map), None)
    }

    /// Like `read_ifd` + `decode_ifd` with typed byte-range claims into `map`, mirroring
    /// [`read_file_audited`](Self::read_file_audited) for a sub-IFD reached via a pointer tag;
    /// returns the next-IFD offset alongside the decoded directory.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if the directory or a value is out of bounds or
    /// truncated, or [`Error::Io`] if the source fails.
    pub fn read_ifd_at_audited(&mut self, offset: u64, map: &mut SegmentMap) -> Result<(Ifd, u64)> {
        let raw = self.read_ifd(offset)?;
        let ifd = self.decode_ifd_inner(&raw, Some(map))?;
        Ok((ifd, raw.next))
    }

    /// [`read_ifd_at_audited`](Self::read_ifd_at_audited) for the [`audit`](crate::audit) walk,
    /// which also records the directory's shadowed entries into `findings` — at `base` plus their
    /// offset, so a chain read through a [`Rebased`](crate::Rebased) view reports physical
    /// positions. Nothing is recorded for a directory that fails to decode.
    pub(crate) fn read_ifd_at_reporting(
        &mut self,
        offset: u64,
        map: &mut SegmentMap,
        findings: &mut Vec<AuditFinding>,
        base: u64,
    ) -> Result<(Ifd, u64)> {
        let raw = self.read_ifd(offset)?;
        let ifd = self.decode_ifd_inner(&raw, Some(map))?;
        report_shadowed(&raw, base, findings);
        Ok((ifd, raw.next))
    }

    /// The underlying source — how a codec reads its strip/tile/blob bytes (which this crate
    /// never interprets) through the same handle it parses structure with.
    pub fn source_mut(&mut self) -> &mut S {
        &mut self.source
    }

    /// Unwraps the reader, returning the source.
    #[must_use]
    pub fn into_inner(self) -> S {
        self.source
    }

    /// The eager top-level walk behind [`read_file`](Self::read_file), its audited variant
    /// (header, per-directory body, and value-span claims), and the [`audit`](crate::audit) walk,
    /// which also asks for each directory's shadowed entries.
    pub(crate) fn read_chain(
        &mut self,
        mut map: Option<&mut SegmentMap>,
        mut findings: Option<&mut Vec<AuditFinding>>,
    ) -> Result<TiffFile> {
        if let Some(m) = map.as_deref_mut() {
            m.claim(
                0,
                self.variant.header_size() as u64,
                SpanKind::Header,
                Claim::Parsed,
            );
        }
        let mut ifds = Vec::new();
        let mut guard = ChainGuard::new();
        let mut offset = self.first;
        while offset != 0 {
            guard.admit(offset)?;
            let raw = self.read_ifd(offset)?;
            let ifd = self.decode_ifd_inner(&raw, map.as_deref_mut())?;
            if let Some(sink) = findings.as_deref_mut() {
                report_shadowed(&raw, 0, sink);
            }
            ifds.push(ifd);
            offset = raw.next;
        }
        if ifds.is_empty() {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "TIFF: file has no IFD",
            ));
        }
        Ok(TiffFile {
            order: self.order,
            variant: self.variant,
            ifds,
        })
    }

    /// Decodes `raw`'s entries with optional byte-range claims: unknown types preserved
    /// verbatim (their record sits inside the body claim; their unsizable payload deliberately
    /// stays unclaimed), last duplicate wins, out-of-line spans claimed only after a successful
    /// decode.
    fn decode_ifd_inner(&mut self, raw: &RawIfd, mut map: Option<&mut SegmentMap>) -> Result<Ifd> {
        if let Some(m) = map.as_deref_mut() {
            // The directory body — count field, entry records (inline values included), and the
            // next-IFD pointer — is one contiguous span, exactly as `read_ifd` fetched it.
            let body = self.variant.count_size() as u64
                + raw.entries.len() as u64 * self.variant.entry_size() as u64
                + self.variant.offset_size() as u64;
            m.claim(
                raw.offset,
                body,
                SpanKind::IfdBody { ifd: raw.offset },
                Claim::Parsed,
            );
        }
        let mut ifd = Ifd::new();
        for entry in &raw.entries {
            let Some(ty) = entry.field_type() else {
                ifd.set(entry.tag, Value::Unknown(self.unknown_value(entry)?));
                continue;
            };
            let value = self.fetch_value(entry, ty, raw.offset, map.as_deref_mut())?;
            ifd.set(entry.tag, value);
        }
        Ok(ifd)
    }

    /// Fetches and decodes one entry's value: inline straight from the raw word, out of line
    /// via a bounds-checked positioned read. With a map, the out-of-line on-disk span
    /// (`count * type size`, padding included) is claimed after a successful decode.
    fn fetch_value(
        &mut self,
        entry: &RawEntry,
        ty: FieldType,
        ifd_offset: u64,
        map: Option<&mut SegmentMap>,
    ) -> Result<Value> {
        let count = usize::try_from(entry.count).map_err(|_| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: field length overflow")
        })?;
        let byte_len = count.checked_mul(ty.size()).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: field length overflow")
        })?;
        let inline = self.variant.inline_threshold();
        if byte_len <= inline {
            // Byte-for-byte the eager path's inline decode: the word kept the file's byte
            // order, and the window is the full inline width, exactly like the entry record.
            return Value::decode(ty, count, &entry.word[..inline], self.order);
        }
        let voff = self.word_offset(entry);
        let len = self.source.len()?;
        // The two-error distinction the slice path makes: an offset past the end is one thing,
        // a value that starts in bounds but runs past the end is another. Checking the span
        // *before* allocating also keeps a hostile count from becoming an allocation bomb.
        if voff > len {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "TIFF: value offset out of bounds",
            ));
        }
        if voff
            .checked_add(byte_len as u64)
            .is_none_or(|end| end > len)
        {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "TIFF: field value out of bounds",
            ));
        }
        let mut bytes = vec![0u8; byte_len];
        self.source.read_exact_at(voff, &mut bytes)?;
        let value = Value::decode(ty, count, &bytes, self.order)?;
        if let Some(m) = map {
            m.claim(
                voff,
                byte_len as u64,
                SpanKind::Value {
                    ifd: ifd_offset,
                    tag: entry.tag,
                },
                Claim::Parsed,
            );
        }
        Ok(value)
    }

    /// Decodes the entry's raw word as a value offset (`u32` classic, `u64` BigTIFF).
    fn word_offset(&self, entry: &RawEntry) -> u64 {
        match self.variant {
            Variant::Classic => u64::from(self.order.u32([
                entry.word[0],
                entry.word[1],
                entry.word[2],
                entry.word[3],
            ])),
            #[cfg(feature = "bigtiff")]
            Variant::Big => self.order.u64(entry.word),
        }
    }
}

/// Appends an [`AuditFinding::ShadowedEntry`] for every entry of `raw` that a later entry repeats
/// the tag of — the ones [`Ifd::set`]'s last-wins leaves out of the decoded directory — in
/// on-disk order, with both offsets shifted by `base`.
///
/// One backwards pass over a set of the tags already seen, so a hostile 65 535-entry directory
/// costs a linear scan rather than a quadratic one.
fn report_shadowed(raw: &RawIfd, base: u64, findings: &mut Vec<AuditFinding>) {
    let mut later = std::collections::BTreeSet::new();
    let start = findings.len();
    for entry in raw.entries.iter().rev() {
        if !later.insert(entry.tag) {
            findings.push(AuditFinding::ShadowedEntry {
                ifd: base + raw.offset,
                tag: entry.tag,
                offset: base + entry.offset,
            });
        }
    }
    findings[start..].reverse();
}

/// Iterator over a top-level IFD chain, from [`IfdReader::ifds`]. Yields each directory raw;
/// fuses (returns `None` forever) after the first error.
#[derive(Debug)]
pub struct IfdChain<'a, S> {
    reader: &'a mut IfdReader<S>,
    next: u64,
    guard: ChainGuard,
    done: bool,
}

impl<S: ReadAt> Iterator for IfdChain<'_, S> {
    type Item = Result<RawIfd>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.next == 0 {
            return None;
        }
        if let Err(e) = self.guard.admit(self.next) {
            self.done = true;
            return Some(Err(e));
        }
        match self.reader.read_ifd(self.next) {
            Ok(raw) => {
                self.next = raw.next;
                Some(Ok(raw))
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{read, write};

    fn classic_le(ifds: Vec<Ifd>) -> Vec<u8> {
        write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds,
        })
        .expect("write")
    }

    #[test]
    fn open_parses_headers_like_the_slice_reader() {
        let r = IfdReader::open(&b"II\x2a\x00\x08\x00\x00\x00"[..]).expect("classic header");
        assert_eq!(r.order(), ByteOrder::LittleEndian);
        assert_eq!(r.variant(), Variant::Classic);
        assert_eq!(r.first_ifd_offset(), 8);
        assert!(IfdReader::open(&b"II\x2a"[..]).is_err()); // too short
        assert!(IfdReader::open(&b"XX\x2a\x00\x08\x00\x00\x00"[..]).is_err()); // bad BOM
    }

    #[cfg(feature = "bigtiff")]
    #[test]
    fn open_parses_bigtiff_headers() {
        let head = b"II\x2b\x00\x08\x00\x00\x00\x10\x00\x00\x00\x00\x00\x00\x00";
        let r = IfdReader::open(&head[..]).expect("bigtiff header");
        assert_eq!(r.variant(), Variant::Big);
        assert_eq!(r.first_ifd_offset(), 16);
        // A BigTIFF magic with a truncated header is rejected exactly like the slice path.
        assert!(IfdReader::open(&b"II\x2b\x00\x08\x00\x00\x00\x10\x00\x00\x00"[..]).is_err());
    }

    #[test]
    fn with_layout_has_no_first_ifd() {
        let data: &[u8] = &[];
        let mut r = IfdReader::with_layout(data, ByteOrder::BigEndian, Variant::Classic);
        assert_eq!(r.order(), ByteOrder::BigEndian);
        assert_eq!(r.variant(), Variant::Classic);
        assert_eq!(r.first_ifd_offset(), 0);
        // An empty chain is the same typed error the slice reader gives a first-IFD offset of 0.
        assert!(r.read_file().is_err());
        assert!(r.ifds().next().is_none());
    }

    /// The mutant-killer for the raw `word`: an inline value must decode with the file's byte
    /// order from the file's bytes, in both orders.
    #[test]
    fn inline_values_decode_in_both_byte_orders() {
        for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
            let mut ifd = Ifd::new();
            ifd.set(256, Value::Short(vec![0x0102]));
            let bytes = write(&TiffFile {
                order,
                variant: Variant::Classic,
                ifds: vec![ifd],
            })
            .expect("write");
            let mut r = IfdReader::open(&bytes[..]).expect("open");
            let raw = r.read_ifd(r.first_ifd_offset()).expect("read_ifd");
            let entry = raw.entry(256).expect("entry").clone();
            assert_eq!(entry.count, 1);
            assert_eq!(r.value_offset(&entry), None, "2 bytes pack inline");
            assert_eq!(r.value(&entry).expect("value"), Value::Short(vec![0x0102]));
        }
    }

    /// The inline threshold is the variant's offset width: an exactly-4-byte value packs inline
    /// in classic, a 6-byte value does not.
    #[test]
    fn classic_inline_threshold_boundary() {
        let mut ifd = Ifd::new();
        ifd.set(256, Value::Long(vec![0x0A0B_0C0D])); // 4 bytes: inline
        ifd.set(258, Value::Short(vec![8, 8, 8])); // 6 bytes: out of line
        let bytes = classic_le(vec![ifd]);
        let mut r = IfdReader::open(&bytes[..]).expect("open");
        let raw = r.read_ifd(8).expect("read_ifd");
        // Entry records are 12 bytes apart, from entries_start = 8 (IFD offset) + 2 (count).
        assert_eq!(raw.entries[0].offset, 10);
        assert_eq!(raw.entries[1].offset, 22);
        let at_threshold = raw.entry(256).expect("256").clone();
        let past_threshold = raw.entry(258).expect("258").clone();
        assert_eq!(r.value_offset(&at_threshold), None);
        let voff = r.value_offset(&past_threshold).expect("out of line");
        assert!(voff >= 8, "value pool follows the directory");
        assert_eq!(
            r.value(&at_threshold).expect("value"),
            Value::Long(vec![0x0A0B_0C0D])
        );
        assert_eq!(
            r.value(&past_threshold).expect("value"),
            Value::Short(vec![8, 8, 8])
        );
    }

    /// The same 8-byte value sits out of line in classic TIFF and inline in BigTIFF — pinning
    /// the 4-vs-8 threshold from both sides with one fixture value.
    #[cfg(feature = "bigtiff")]
    #[test]
    fn bigtiff_inline_threshold_is_eight_bytes() {
        for (variant, inline) in [(Variant::Classic, false), (Variant::Big, true)] {
            let mut ifd = Ifd::new();
            ifd.set(282, Value::Rational(vec![(300, 1)])); // 8 bytes
            let bytes = write(&TiffFile {
                order: ByteOrder::LittleEndian,
                variant,
                ifds: vec![ifd],
            })
            .expect("write");
            let mut r = IfdReader::open(&bytes[..]).expect("open");
            let raw = r.read_ifd(r.first_ifd_offset()).expect("read_ifd");
            let entry = raw.entry(282).expect("entry").clone();
            assert_eq!(r.value_offset(&entry).is_none(), inline, "{variant:?}");
            assert_eq!(
                r.value(&entry).expect("value"),
                Value::Rational(vec![(300, 1)])
            );
        }
    }

    /// Raw-entry bookkeeping pinned on a hand-written file (the reader.rs unknown-field
    /// fixture): entry offsets, the unknown type code surfacing raw, and decode-time verbatim
    /// preservation.
    #[test]
    fn raw_entries_expose_unknown_types_and_offsets() {
        let data: &[u8] = &[
            b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, // header, first IFD @ 8
            0x01, 0x00, // entry count = 1
            0x99, 0x99, // tag 0x9999
            0xf0, 0x00, // type 0xF0 (unknown)
            0x02, 0x00, 0x00, 0x00, // value count = 2
            0x2a, 0x00, 0x00, 0x00, // value/offset word
            0x00, 0x00, 0x00, 0x00, // next IFD = 0
        ];
        let mut r = IfdReader::open(data).expect("open");
        let raw = r.read_ifd(8).expect("read_ifd");
        assert_eq!(raw.offset, 8);
        assert_eq!(raw.next, 0);
        assert_eq!(raw.entries.len(), 1);
        let entry = raw.entries[0].clone();
        assert_eq!(entry.tag, 0x9999);
        assert_eq!(entry.type_code, 0xF0);
        assert_eq!(entry.field_type(), None);
        assert_eq!(entry.count, 2);
        assert_eq!(entry.offset, 10);
        assert_eq!(r.value_offset(&entry), None);
        // `value` yields the preserved record — nothing to fetch, nothing dropped.
        let Ok(Value::Unknown(u)) = r.value(&entry) else {
            panic!("expected a preserved Value::Unknown");
        };
        assert_eq!(u.type_code(), 0xF0);
        assert_eq!(u.count(), 2);
        assert_eq!(u.word(), &[0x2a, 0x00, 0x00, 0x00]);
        // Decode preserves the unknown entry too, and the audited read stays fully
        // classified: the entry record sits inside the directory-body claim.
        assert_eq!(r.decode_ifd(&raw).expect("decode").fields().len(), 1);
        let mut map = SegmentMap::new(data.len() as u64);
        let file = r.read_file_audited(&mut map).expect("read_file");
        assert_eq!(file.ifds[0].get(0x9999), Some(&Value::Unknown(u)));
        let report = map.finish(None);
        assert!(report.is_fully_classified(), "report: {report:?}");
    }

    /// A duplicate tag: the raw directory preserves both occurrences ([`RawIfd::entry`] finds
    /// the first), while decode keeps the last — the eager readers' semantics.
    #[test]
    fn duplicate_tags_raw_first_decoded_last() {
        let data: &[u8] = &[
            b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, // header, first IFD @ 8
            0x02, 0x00, // entry count = 2
            0x00, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
            0x00, // tag 256 SHORT = 1
            0x00, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00,
            0x00, // tag 256 SHORT = 2
            0x00, 0x00, 0x00, 0x00, // next IFD = 0
        ];
        let mut r = IfdReader::open(data).expect("open");
        let raw = r.read_ifd(8).expect("read_ifd");
        assert_eq!(raw.entries.len(), 2);
        let first = raw.entry(256).expect("first").clone();
        assert_eq!(r.value(&first).expect("value"), Value::Short(vec![1]));
        let ifd = r.decode_ifd(&raw).expect("decode");
        assert_eq!(ifd.get(256), Some(&Value::Short(vec![2])));
        // The slice reader agrees on last-wins.
        assert_eq!(read(data).expect("read").ifds[0].get_u32(256), Some(2));
    }

    /// `decode_ifd_reporting` returns `decode_ifd`'s directory and names every entry it shadowed,
    /// in on-disk order: a tag written three times around another tag shadows its first two.
    #[test]
    fn decode_ifd_reporting_names_each_shadowed_entry_in_disk_order() {
        let data: &[u8] = &[
            b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, // header, first IFD @ 8
            0x04, 0x00, // entry count = 4
            0x00, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
            0x00, // @10: tag 256 SHORT = 1
            0x01, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00,
            0x00, // @22: tag 257 SHORT = 5
            0x00, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00,
            0x00, // @34: tag 256 SHORT = 2
            0x00, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00,
            0x00, // @46: tag 256 SHORT = 3
            0x00, 0x00, 0x00, 0x00, // next IFD = 0
        ];
        let mut r = IfdReader::open(data).expect("open");
        let raw = r.read_ifd(8).expect("read_ifd");
        let (ifd, findings) = r.decode_ifd_reporting(&raw).expect("decode");
        assert_eq!(ifd, r.decode_ifd(&raw).expect("decode"));
        assert_eq!(
            findings,
            vec![
                AuditFinding::ShadowedEntry {
                    ifd: 8,
                    tag: 256,
                    offset: 10,
                },
                AuditFinding::ShadowedEntry {
                    ifd: 8,
                    tag: 256,
                    offset: 34,
                },
            ]
        );
    }

    /// One entry per tag — every conforming directory — reports nothing.
    #[test]
    fn decode_ifd_reporting_is_silent_on_a_conforming_directory() {
        let mut ifd = Ifd::new();
        ifd.set(256, Value::Short(vec![1]));
        ifd.set(258, Value::Short(vec![8, 8, 8]));
        let bytes = classic_le(vec![ifd.clone()]);
        let mut r = IfdReader::open(&bytes[..]).expect("open");
        let raw = r.read_ifd(8).expect("read_ifd");
        assert_eq!(
            r.decode_ifd_reporting(&raw).expect("decode"),
            (ifd, Vec::new())
        );
    }

    #[test]
    fn chain_iterates_and_fuses_on_a_loop() {
        let mut a = Ifd::new();
        a.set(256, Value::Short(vec![1]));
        let mut b = Ifd::new();
        b.set(256, Value::Short(vec![2]));
        let bytes = classic_le(vec![a, b]);
        let mut r = IfdReader::open(&bytes[..]).expect("open");
        let raws: Vec<_> = r.ifds().collect();
        assert_eq!(raws.len(), 2);
        assert!(raws.iter().all(Result::is_ok));

        // A next pointer aimed at its own directory: one raw directory, then the loop error,
        // then the iterator is fused.
        let looped: &[u8] = &[
            b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, // header, first IFD @ 8
            0x00, 0x00, // entry count = 0
            0x08, 0x00, 0x00, 0x00, // next IFD = 8 (itself)
        ];
        let mut r = IfdReader::open(looped).expect("open");
        let mut chain = r.ifds();
        assert!(chain.next().expect("first directory").is_ok());
        assert!(chain.next().expect("loop error").is_err());
        assert!(chain.next().is_none(), "fused after the error");
        // The eager equivalents surface the same loop as a typed error.
        assert!(r.read_file().is_err());
        assert!(read(looped).is_err());
    }

    /// A hostile declared count fails typed — before any value-sized allocation.
    #[test]
    fn hostile_value_count_is_bounded() {
        let data: &[u8] = &[
            b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, // header, first IFD @ 8
            0x01, 0x00, // entry count = 1
            0x00, 0x01, // tag 256
            0x03, 0x00, // type 3 (SHORT)
            0xff, 0xff, 0xff, 0xff, // value count = u32::MAX
            0x1a, 0x00, 0x00, 0x00, // value offset
            0x00, 0x00, 0x00, 0x00, // next IFD = 0
        ];
        let mut r = IfdReader::open(data).expect("open");
        let raw = r.read_ifd(8).expect("directory itself is fine");
        let entry = raw.entries[0].clone();
        assert_eq!(entry.count, u64::from(u32::MAX));
        assert_eq!(r.value_offset(&entry), Some(26), "declared, not validated");
        assert!(matches!(
            r.value(&entry),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
        // A count whose directory would run past the source also fails before allocating.
        let mut huge = data.to_vec();
        huge[8] = 0xff;
        huge[9] = 0xff; // entry count = 65535 in a 26-byte file
        let mut r = IfdReader::open(&huge[..]).expect("open");
        assert!(matches!(
            r.read_ifd(8),
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput
        ));
    }

    /// The out-of-line two-error distinction the slice path makes, reproduced through a source.
    #[test]
    fn out_of_line_bounds_errors_match_the_slice_path() {
        let mut ifd = Ifd::new();
        ifd.set(258, Value::Short(vec![8, 8, 8])); // 6 bytes -> out of line
        let bytes = classic_le(vec![ifd]);
        let mut r = IfdReader::open(&bytes[..]).expect("open");
        let raw = r.read_ifd(8).expect("read_ifd");
        let entry = raw.entries[0].clone();
        let voff = r.value_offset(&entry).expect("out of line");

        // Truncate so the value starts in bounds but runs past the end.
        let short = &bytes[..voff as usize + 2];
        let mut r_short = IfdReader::open(short).expect("open");
        match r_short.value(&entry) {
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput => {
                assert_eq!(
                    error.static_message(),
                    Some("TIFF: field value out of bounds")
                )
            }
            other => panic!("expected truncated-value error, got {other:?}"),
        }
        // Truncate so the offset itself is past the end.
        let shorter = &bytes[..voff as usize - 2];
        let mut r_off = IfdReader::open(shorter).expect("open");
        match r_off.value(&entry) {
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput => {
                assert_eq!(
                    error.static_message(),
                    Some("TIFF: value offset out of bounds")
                )
            }
            other => panic!("expected offset error, got {other:?}"),
        }
        // The boundary between the two: an offset exactly at EOF is an *empty* value span, not
        // an out-of-bounds offset — the slice path's `data.get(voff..)` semantics.
        let at_end = &bytes[..voff as usize];
        let mut r_end = IfdReader::open(at_end).expect("open");
        match r_end.value(&entry) {
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput => {
                assert_eq!(
                    error.static_message(),
                    Some("TIFF: field value out of bounds")
                )
            }
            other => panic!("expected truncated-value error, got {other:?}"),
        }
    }

    /// `read_tree`'s streaming equivalent inverts `write` exactly like the slice version.
    #[test]
    fn streaming_read_tree_inverts_write() {
        let mut grandchild = Ifd::new();
        grandchild.set(33434, Value::Rational(vec![(1, 200)]));
        let mut raw_a = Ifd::new();
        raw_a.set(256, Value::Short(vec![16]));
        raw_a.set_sub_ifd(34665, vec![grandchild]);
        let mut raw_b = Ifd::new();
        raw_b.set(256, Value::Short(vec![8]));
        let mut root = Ifd::new();
        root.set(256, Value::Short(vec![640]));
        root.set_sub_ifd(330, vec![raw_a, raw_b]);
        let file = TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![root],
        };
        let bytes = write(&file).expect("write");
        let mut r = IfdReader::open(&bytes[..]).expect("open");
        assert_eq!(r.read_tree(&[330, 34665]).expect("read_tree"), file);
        // And the sub-IFD cycle guard is the shared one: the reader.rs self-pointer fixture.
        let cyclic: &[u8] = &[
            b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, //
            0x01, 0x00, 0x4a, 0x01, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x1a, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, //
            0x01, 0x00, 0x4a, 0x01, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x1a, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        let mut r = IfdReader::open(cyclic).expect("open");
        assert!(r.read_file().is_ok());
        assert!(r.read_tree(&[330]).is_err());
    }

    /// The audited streaming walk classifies a whole written tree (root chain + a followed
    /// sub-IFD) with no unclassified bytes.
    #[test]
    fn audited_walk_fully_classifies_a_tree() {
        let mut child = Ifd::new();
        child.set(256, Value::Short(vec![16]));
        child.set(258, Value::Short(vec![8, 8, 8]));
        let mut root = Ifd::new();
        root.set(256, Value::Short(vec![640]));
        root.set(258, Value::Short(vec![8, 8, 8]));
        root.set_sub_ifd(330, vec![child]);
        let bytes = classic_le(vec![root]);

        let mut r = IfdReader::open(&bytes[..]).expect("open");
        let mut map = SegmentMap::new(bytes.len() as u64);
        let file = r.read_file_audited(&mut map).expect("read_file");
        let child_off = u64::from(file.ifds[0].get_u32(330).expect("pointer"));
        let (_, next) = r
            .read_ifd_at_audited(child_off, &mut map)
            .expect("stream child");
        assert_eq!(next, 0);
        let report = map.finish(None);
        assert!(report.is_fully_classified(), "report: {report:?}");
    }

    #[test]
    fn source_accessors_round_trip() {
        let bytes = classic_le(vec![{
            let mut ifd = Ifd::new();
            ifd.set(256, Value::Short(vec![1]));
            ifd
        }]);
        let mut r = IfdReader::open(&bytes[..]).expect("open");
        // Read a byte through the reader's own source handle (how a codec fetches strips).
        let mut head = [0u8; 2];
        r.source_mut()
            .read_exact_at(0, &mut head)
            .expect("source read");
        assert_eq!(&head, b"II");
        assert_eq!(r.into_inner(), &bytes[..]);
    }
}
