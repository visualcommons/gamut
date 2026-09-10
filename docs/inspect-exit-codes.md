# `gamut inspect` — the exit-code contract

Normative for **what `gamut inspect` exits with, per format**: the gate each format is judged by,
PNG's third outcome, the budgets the walk observes, and every reason the PNG filter scan can
decline to read. What each walk *finds* is the format crate's own contract; this document is only
about the verdict the command turns those findings into.

Source: `crates/gamut-cli/src/commands/inspect.rs`, `crates/gamut-cli/src/main.rs`,
`crates/gamut-png/src/deconstruct.rs`, `crates/gamut-tiff/src/deconstruct.rs`,
`crates/gamut-dng/src/deconstruct.rs`, `crates/gamut-heic/src/c2pa.rs`.

## There are two exit codes

`main` maps a command's `Ok(())` to `ExitCode::SUCCESS` and **every** `Err` to
`ExitCode::FAILURE`, printing `error: {e}` to stderr. So `gamut inspect` exits `0` or `1`, and
nothing else; the distinctions below are carried by the stderr message, not by the code.

| Exit | Meaning |
| --- | --- |
| `0` | The file passed its format's gate, or its format has no gate (HEIC). The report is on stdout. |
| `1` | Either the file failed its gate (a report is still printed to stdout first, and the summary goes to stderr), or the walk could not run at all — the file was unreadable, the container could not be opened, or it was not a container this command reads. |

A gate failure and an unreadable file are **not** distinguished by exit code. A caller that needs
to tell them apart reads the message: a gate failure is `<path>: not fully accounted — …`,
`<path>: not verified — …` or `<path>: not a complete, undamaged PNG datastream — …`; anything
else — including `<path>: unsupported container brand '…' — …` — is the walk itself never running.

## The gate, per format

The format is sniffed unless `--format` forces it: a PNG signature, else an `ftyp` box (an ISOBMFF
file) routes to the HEIC arm, else a readable TIFF whose IFD 0 carries `DNGVersion` (50706) is a
DNG, else TIFF.

The `ftyp` test is a **route, not a verdict**. It says the file is ISOBMFF, not that it is the HEVC
still image the HEIC arm reports on, so that arm parses the container and then *confirms* it with
`gamut-heic`'s own `HeifImage::is_hevc_still` (`references/heif` §7) before printing anything; a
file that fails the confirmation exits `1` with `<path>: unsupported container brand '<brand>' — …`
and prints no report. No brand list lives in the command. One would have to be exhaustive over the
still-image brands — `mif2`, `avci` and `avcs` beside `heic`/`heix`/`heim`/`heis`/`mif1` — and would
still be wrong about `mif1`, which is the generic MIAF structural brand an AVIF may carry as its
*major* brand, not merely among its compatible ones. `gamut-heic` settles that case on the primary
item carrying an `hvcC`.

`--format heic` skips the sniff **and** the confirmation: a forced format is the caller's own
assertion about the file, and overriding detection is what `--format` is for. `gamut inspect
--format heic some.avif` therefore prints a full HEIC C2PA section at exit `0`.

The report's opening label says which of the two happened. A detected container is
`<path>: HEIF/HEIC`; a forced one is `<path>: HEIF/HEIC (asserted by --format, not detected)`. On
the forced path the command tested nothing about the container, so the plain label would be this
command vouching for a file it never examined — echoing the caller's own assertion back as a
finding.

- **TIFF / DNG** — `DeconstructReport::is_fully_accounted()`: every byte classified into exactly
  one typed segment, **and** no unknown field type, no unknown tag, no anomaly. Identical in both
  crates.
- **PNG** — `PngReport::is_verified()`: `is_intact()` **and** `FilterScan::is_counted()`, i.e.
  every byte classified, every chunk CRC valid, IEND present, no trailing bytes after it, no
  truncated tail, nothing the filter scan found damaging — *and the filter scan actually ran*.
- **HEIC** — **no gate**; see the section below. Exit `0` whenever `HeifContainer::parse` succeeds
  and the container is the one this arm reads.

PNG's `is_fully_classified()` is printed but is **not** the gate: it is true by construction for
every file `deconstruct` accepts (a truncated tail and a trailer each get a segment of their own),
so gating on it would exit `0` on a truncated PNG. It exists so that a walk *bug* makes the
predicate false.

## HEIC has no gate, because it answers a different question

The HEIC arm is not a deconstruct. It parses the container and reports one thing — **does this file
carry a C2PA manifest store, and where?** — from `gamut-heic`'s `HeifContainer::c2pa_summary()`:
per store a `box_purpose`, a size and a half-open byte range, and the non-validation disclaimer
inline. It classifies no bytes and looks for no unknowns, so it has nothing to hold against the
file:

| HEIC state | Exit | stdout |
| --- | --- | --- |
| a store is present | `0` | the store(s), each with its purpose, size and range |
| a C2PA box is present but no store could be read from it | `0` | that the box is present, its range, and why no store was read — explicitly *not* an absence |
| no C2PA box is present | `0` | that none was found in the top-level boxes of the primary stream |
| a top-level `uuid` box carries some other extended type | `0` | a count of them, beside whichever of the three rows above applies |
| the container is not the HEVC still image this arm reads (e.g. an AVIF) | `1` | nothing; `unsupported container brand …` on stderr |
| the container cannot be parsed | `1` | nothing; `error: …` on stderr |

**The exit code says whether the inspection succeeded, not what it found.** Presence never changes
it, absence never changes it, and a box gamut could not read through never changes it either: all
three are ordinary properties of an ordinary file, and none is an accounting anomaly. That
asymmetry against TIFF/DNG/PNG is deliberate — those three exit `1` on a *finding*, because their
walk has a claim to make about the file; this one has none.

**Absence of a store line is not absence of provenance.** A file can carry a genuine
`ContentProvenanceBox` — right extended type, real JUMBF store — that this reader declines: its
`FullBox` version is not zero (§A.5.1.2), its `box_purpose` is the auxiliary `merkle` or a value
this revision does not know (§A.5.3), it is truncated, or no valid JUMBF `LBox` bounds a store
where its purpose puts one. Such a box gets its own line naming the reason, precisely so a reader
cannot infer "no provenance" from bytes gamut merely could not read. A caller gating on stdout must
treat the `unread C2PA box` line as *unknown*, never as absence, and reach for a validator.

**A near miss on the extended type is reported as a count, and as nothing more.** §A.5.1.1 makes
the sixteen-byte extended type the box's whole identity, and the specification has no notion of an
approximate one, so a top-level `uuid` box that is a single byte off is **not** damaged C2PA
framing and never earns an `unread C2PA box` line. But it is not passed over either: the report
carries `other top-level uuid boxes: N (extended type is not the C2PA one; a uuid box is not
provenance framing)`. Without it, a file whose only `uuid` box is one byte off — what a signed file
corrupted in transit looks like — printed byte-for-byte what a file carrying no such box prints,
and the reader most in need of looking closer was the one told the least. The line states a fact
about bytes and claims nothing: an ordinary file carries vendor `uuid` boxes.

**A box's position is reported when it sits past the file's media data**, as `; its box begins
after the first mdat box` on that box's own line. §A.5.3 places a manifest-store box "before the
first 'mdat' box in the file and before any 'moov' box in the file", and an appended box is
precisely the adversarial shape — but the clause is positional, not a verdict: §A.5.3 itself
requires the `update` box of a mid-update file to be the last box of the file. (`moov` is the other
boundary §A.5.3 names and is unreachable here: `gamut-heic` refuses a file carrying a top-level
movie box, image sequences being out of the workspace's scope.)

**The per-box lines are truncated at twenty**, like every other list this command prints, with the
same `… and N more` tail. §A.5.3 permits any number of these boxes, so their count is chosen by the
input and needs no malformity to grow: a legal 2.7 MB file carrying fifty thousand of them printed
fifty thousand lines and put the headline — non-validation disclaimer and all — at line 2 of them.
The cap is this command's presentation decision; `gamut-heic` keeps returning every entry, and
`C2paSummary::detail_line_count()` is the true total the tail is computed from. The headline and
the `uuid`-box count are never truncated: there are at most two such lines whatever the file holds.

**A machine consumer has no surface here yet.** Every distinction above is carried by stdout prose
and none by the exit code, which is deliberate — two exit codes are the command's contract, and a
third would not fit the outcomes anyway. Structured output is the answer and is tracked in
issue #596; until it lands, a caller matching on wording is matching on wording this document is
free to improve.

Nothing in that report is a verdict. gamut locates a manifest store and never validates one — no
signature, no hash binding, no trust list (C2PA 2.4 §15.12; `references/c2pa/README.md`) — and the
line that reports a store says so, because "C2PA: present" printed beside EXIF and ICC otherwise
reads as *verified*. The wording lives in `gamut-heic` (`C2PA_NOT_VALIDATED`) so that every host
renders the same words and none can reword the disclaimer away; that crate's tests pin the wording,
and `gamut-cli`'s `tests/inspect_c2pa.rs` pins that this command carries it to the terminal
unabridged. The store's bytes are never printed at any verbosity: they are opaque to gamut,
routinely hundreds of kilobytes, and a byte range is what a caller hands to `c2pa-rs`.

The other containers gamut can locate a store in gain the same section as each one's locator slice
lands; only HEIC's is on `master` today.

## PNG alone has a third outcome

A TIFF or DNG walk reads directories and tags and never touches pixel data, so there is no step in
it the reader can decline: `is_fully_accounted()` never depends on a budget. A PNG's verification
step **is** an inflation of the IDAT stream, and an inflation can be declined. So PNG has three
outcomes where the other two formats have two:

| PNG state | Exit | stderr |
| --- | --- | --- |
| `is_verified()` | `0` | — |
| `is_intact()` but not verified — nothing is known against the file, but its IDAT was never read | `1` | `<path>: not verified — <why the scan did not run>` |
| not `is_intact()` — something is known against the file | `1` | `<path>: not a complete, undamaged PNG datastream — N finding(s)` |

The middle row is why `is_intact()` is not the gate. A file whose filter scan was skipped for
budget is not *damaged* — `intact: yes` is printed truthfully — but a corrupt zlib payload under a
valid CRC is damage **only** the scan can see, so exiting `0` on an unread file would report this
reader's budget as a property of the file. Gating PNG on `is_intact()` instead would leave the two
formats symmetric in wording and asymmetric in strength: a TIFF's exit `0` means the walk read
everything, and a PNG's would not.

## The budgets the walk observes

`gamut inspect` walks with `DeconstructLimits::default().with_max_image_bytes(1 << 30)` — one
gibibyte — against the PNG decoder's default of `64 << 20`, 64 MiB (a 4096×4096 RGBA8 image).
`max_chunks` is left at its default, `DEFAULT_MAX_CHUNKS = 1 << 20`.

They differ because they answer different questions:

- The **decoder's** 64 MiB bounds what a decode of hostile input may allocate. A file past it is
  refused; refusing is the safe outcome, because nothing downstream needs the pixels.
- **Inspection's** whole job is to read the file, and a file it declines to inflate is a file it
  cannot vouch for. At 64 MiB every PNG past 4096×4096 RGBA8 — an ordinary photograph — would be
  reported as intact but not verified. A gibibyte is past any real image and short of unbounded.

The gibibyte bounds the **image the header declares**, not what a small file may inflate to.
Past the decoder's default budget the walk additionally refuses, before inflating, any stream that
would grow to more than sixty-four times its own length (`INFLATION_RATIO`), so a raised image
budget cannot be spent by a zlib bomb: a megabyte declaring a 16384×16384 header over a zlib
stream of zeros is refused unread. Inside the decoder's default budget the ratio does not apply —
a flat image really does compress thousands-fold, and the walk is never a cheaper target than a
decode of the same header, which allocates the same bytes.

What a hostile file can still cost, therefore: an inflation of up to twice the decoder's default
budget (native bytes plus one filter byte per scanline — up to 128 MiB for a degenerate
one-pixel-wide greyscale column) for free, and anything above that only by paying one input byte
for every sixty-four bytes inflated.

Exceeding `max_chunks` is **not** a finding: the walk returns an error and the command exits `1`
with that error, having printed no report. A PNG at the ceiling carries at least 12 MiB of pure
chunk framing.

## Why the PNG filter scan declines, and which reasons are damage

`FilterScan::Skipped(SkippedFilterScan)` names the reason. `SkippedFilterScan::is_damage()` is the
single source of truth for whether the reason describes the **file** or this **reader**; the
command raises a finding for the first kind and reports "not verified" for the second.

| Reason | Damage? | What it means | PNG outcome |
| --- | --- | --- | --- |
| `OverBudget` | no | The image the header declares is larger than `max_image_bytes`. | intact, not verified → exit `1` |
| `ImplausibleInflation` | no | The image fits this reader's budget but is past the decoder's default one, and the IDAT stream is more than sixty-four times too short to plausibly inflate to it — the shape of a zlib bomb under a permissive budget. | intact, not verified → exit `1` |
| `CorruptStream` | yes | The IDAT stream is not a valid zlib stream, is truncated, or inflates past the length the header implies. | finding → exit `1` |
| `LengthMismatch` | yes | The stream inflated, but not to the length the header implies, so the scanline boundaries are not where the filter bytes are. | finding → exit `1` |
| `UndefinedFilterCode` | yes | A scanline's leading byte is not one of the five filter codes §9.1 defines. | finding → exit `1` |

The two non-damage reasons are the two ways this reader can refuse to *read*, and they are
distinct because they blame different things: `OverBudget` is the image exceeding a limit,
`ImplausibleInflation` is a file whose declared image the limit admits — a valid flat 16384×16384
RGBA8 PNG is exactly the gibibyte `gamut inspect` allows, so telling it that it is larger than the
budget would name a limit it does not cross.

`SkippedFilterScan` is `#[non_exhaustive]` and its `#[repr(u8)]` discriminants are permanent and
append-only. A future reason is **damage until it says otherwise**, so it raises a finding and the
command renders it generically rather than passing a file it does not understand.
