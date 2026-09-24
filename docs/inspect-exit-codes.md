# `gamut inspect` — the exit-code contract

Normative for **what `gamut inspect` exits with, per format**: the gate each format is judged by,
PNG's third outcome, the budgets the walk observes, and every reason the PNG filter scan can
decline to read. What each walk *finds* is the format crate's own contract; this document is only
about the verdict the command turns those findings into.

Source: `crates/gamut-cli/src/commands/inspect.rs`, `crates/gamut-cli/src/main.rs`,
`crates/gamut-png/src/deconstruct.rs`, `crates/gamut-tiff/src/deconstruct.rs`,
`crates/gamut-dng/src/deconstruct.rs`.

## There are two exit codes

`main` maps a command's `Ok(())` to `ExitCode::SUCCESS` and **every** `Err` to
`ExitCode::FAILURE`, printing `error: {e}` to stderr. So `gamut inspect` exits `0` or `1`, and
nothing else; the distinctions below are carried by the stderr message, not by the code.

| Exit | Meaning |
| --- | --- |
| `0` | The file passed its format's gate. The report is on stdout. |
| `1` | Either the file failed its gate (a report is still printed to stdout first, and the summary goes to stderr), or the walk could not run at all — the file was unreadable, or the container could not be opened. |

A gate failure and an unreadable file are **not** distinguished by exit code. A caller that needs
to tell them apart reads the message: a gate failure is `<path>: not fully accounted — …`,
`<path>: not verified — …` or `<path>: not a complete, undamaged PNG datastream — …`; anything
else is the walk itself failing.

## The gate, per format

The format is sniffed unless `--format` forces it: a PNG signature, else a readable TIFF whose
IFD 0 carries `DNGVersion` (50706) is a DNG, else TIFF.

- **TIFF / DNG** — `DeconstructReport::is_fully_accounted()`: every byte classified into exactly
  one typed segment, **and** no unknown field type, no unknown tag, no anomaly. Identical in both
  crates.
- **PNG** — `PngReport::is_verified()`: `is_intact()` **and** `FilterScan::is_counted()`, i.e.
  every byte classified, every chunk CRC valid, IEND present, no trailing bytes after it, no
  truncated tail, nothing the filter scan found damaging — *and the filter scan actually ran*.

PNG's `is_fully_classified()` is printed but is **not** the gate: it is true by construction for
every file `deconstruct` accepts (a truncated tail and a trailer each get a segment of their own),
so gating on it would exit `0` on a truncated PNG. It exists so that a walk *bug* makes the
predicate false.

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
