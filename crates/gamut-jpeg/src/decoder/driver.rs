//! The marker-loop driver (SOI → tables → frame → scans → EOI): routes each segment to its
//! parser in [`crate::syntax`] and each scan to the sequential or progressive entropy decoder in
//! [`crate::scan`], then assembles the decoded component planes into a [`DecodedImage`].

use gamut_core::{Error, Result};

use super::{DecComp, DecodedImage, JpegDecoder, expect_soi, read_marker, read_segment};
use crate::marker::code;
use crate::scan::{Plane, ProgComp, decode_progressive_scan, decode_scan};
use crate::syntax::{
    ColorInfo, Frame, Tables, parse_app0, parse_app14, parse_dht, parse_dnl, parse_dqt, parse_dri,
    parse_sof, parse_sos,
};

impl JpegDecoder {
    /// The shared marker-loop driver: decodes `data` to an internal [`DecodedImage`].
    pub(super) fn decode_internal(&self, data: &[u8]) -> Result<DecodedImage> {
        expect_soi(data)?;
        let mut pos = 2;
        let mut tables = Tables::default();
        let mut color = ColorInfo::default();
        let mut frame: Option<Frame> = None;
        let mut planes: Vec<Option<Plane>> = Vec::new();
        // Progressive (SOF2) state: one coefficient accumulator per component, filled across scans
        // and reconstructed once at EOI. Empty for a sequential frame.
        let mut progressive = false;
        let mut prog: Vec<ProgComp> = Vec::new();

        loop {
            let (marker, after) = read_marker(data, pos)?;
            match marker {
                code::EOI_CODE => break,
                code::SOF0 | code::SOF1 => {
                    if frame.is_some() {
                        return Err(Error::invalid_input(
                            env!("CARGO_PKG_NAME"),
                            "JPEG: duplicate frame header",
                        ));
                    }
                    let (payload, next) = read_segment(data, after)?;
                    let f = parse_sof(payload)?;
                    self.check_limits(f.x.into(), f.y.into(), f.components.len())?;
                    planes = (0..f.components.len()).map(|_| None).collect();
                    frame = Some(f);
                    pos = next;
                }
                code::SOF2 => {
                    if frame.is_some() {
                        return Err(Error::invalid_input(
                            env!("CARGO_PKG_NAME"),
                            "JPEG: duplicate frame header",
                        ));
                    }
                    let (payload, next) = read_segment(data, after)?;
                    let f = parse_sof(payload)?;
                    self.check_limits(f.x.into(), f.y.into(), f.components.len())?;
                    if f.y == 0 {
                        // A Y=0 (DNL-deferred height) progressive frame is rejected: the
                        // coefficient buffers must be sized to the full block grid before the first
                        // scan, and libjpeg-turbo never emits one. See STATUS.md.
                        return Err(Error::unsupported(
                            env!("CARGO_PKG_NAME"),
                            "JPEG: progressive frame with deferred height (Y=0/DNL) not supported",
                        ));
                    }
                    prog = build_prog_comps(&f);
                    progressive = true;
                    frame = Some(f);
                    pos = next;
                }
                code::SOF3 => {
                    return Err(Error::unsupported(
                        env!("CARGO_PKG_NAME"),
                        "JPEG: lossless process (SOF3) not supported",
                    ));
                }
                0xC5..=0xC7 => {
                    return Err(Error::unsupported(
                        env!("CARGO_PKG_NAME"),
                        "JPEG: hierarchical process (SOF5-7) not supported",
                    ));
                }
                0xC9..=0xCB | 0xCD..=0xCF | code::DAC => {
                    return Err(Error::unsupported(
                        env!("CARGO_PKG_NAME"),
                        "JPEG: arithmetic coding not supported",
                    ));
                }
                code::DHT => {
                    let (payload, next) = read_segment(data, after)?;
                    parse_dht(payload, &mut tables)?;
                    pos = next;
                }
                code::DQT => {
                    let (payload, next) = read_segment(data, after)?;
                    parse_dqt(payload, &mut tables)?;
                    pos = next;
                }
                code::DRI => {
                    let (payload, next) = read_segment(data, after)?;
                    tables.restart_interval = parse_dri(payload)?;
                    pos = next;
                }
                code::SOS => {
                    let f = frame.as_ref().ok_or_else(|| {
                        Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: SOS before SOF")
                    })?;
                    let (payload, next) = read_segment(data, after)?;
                    let scan = parse_sos(payload, f, progressive)?;
                    if progressive {
                        pos = decode_progressive_scan(data, next, f, &scan, &tables, &mut prog)?;
                    } else {
                        for sc in &scan.components {
                            if planes[sc.frame_index].is_some() {
                                return Err(Error::invalid_input(
                                    env!("CARGO_PKG_NAME"),
                                    "JPEG: component coded by more than one scan",
                                ));
                            }
                        }
                        let row_limit = if f.y == 0 {
                            Some(self.deferred_row_limit(f, &scan)?)
                        } else {
                            None
                        };
                        let result = decode_scan(data, next, f, &scan, &tables, row_limit)?;
                        for (ci, plane) in result.planes {
                            planes[ci] = Some(plane);
                        }
                        pos = result.marker_offset;
                    }
                }
                code::DNL => {
                    let (payload, next) = read_segment(data, after)?;
                    let nl = parse_dnl(payload)?;
                    // §B.2.5: DNL defines the height only when Y was 0.
                    if let Some(f) = frame.as_mut()
                        && f.y == 0
                    {
                        f.y = nl;
                        self.check_limits(f.x.into(), f.y.into(), f.components.len())?;
                    }
                    pos = next;
                }
                code::APP0 => {
                    let (payload, next) = read_segment(data, after)?;
                    parse_app0(payload, &mut color);
                    pos = next;
                }
                code::APP14 => {
                    let (payload, next) = read_segment(data, after)?;
                    parse_app14(payload, &mut color);
                    pos = next;
                }
                code::SOI | code::TEM | code::RST0..=code::RST7 => {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "JPEG: unexpected standalone marker outside a scan",
                    ));
                }
                _ => {
                    // Any other segment-bearing marker (other APPn, COM, reserved): skip by length.
                    let (_, next) = read_segment(data, after)?;
                    pos = next;
                }
            }
        }

        if progressive {
            assemble_progressive(frame, prog, color)
        } else {
            assemble(frame, planes, color)
        }
    }
}

/// Allocates one progressive coefficient accumulator per frame component, sized to the component's
/// own block grid `ceil(comp_w/8) × ceil(comp_h/8)` (§A.2.2). The height is known (Y ≠ 0 is enforced
/// for progressive frames), so the grid is fixed before the first scan.
fn build_prog_comps(frame: &Frame) -> Vec<ProgComp> {
    let hmax = usize::from(frame.hmax());
    let vmax = usize::from(frame.vmax());
    let x = usize::from(frame.x);
    let y = usize::from(frame.y);
    frame
        .components
        .iter()
        .map(|c| {
            let comp_w = (x * usize::from(c.h)).div_ceil(hmax);
            let comp_h = (y * usize::from(c.v)).div_ceil(vmax);
            ProgComp::new(c.h, c.v, comp_w.div_ceil(8), comp_h.div_ceil(8))
        })
        .collect()
}

/// Finalizes a progressive decode: enforces the partial-render policy (every component must have
/// received its DC first pass, §G.1.1.1.1), reconstructs each component's sample plane once
/// (dequantize + IDCT + level shift), and reuses [`assemble`] to crop and package the image.
fn assemble_progressive(
    frame: Option<Frame>,
    comps: Vec<ProgComp>,
    color: ColorInfo,
) -> Result<DecodedImage> {
    let frame = frame
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: no frame header"))?;
    for c in &comps {
        // Partial-render policy: an incomplete stream renders whatever bands arrived, but every
        // component must at least have its DC scan or the image has no baseline to show.
        if !c.has_dc() {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: progressive frame missing a component's DC scan",
            ));
        }
    }
    let planes = comps.into_iter().map(|c| Some(c.into_plane())).collect();
    assemble(Some(frame), planes, color)
}

/// Finalizes a decode: validates the frame is complete, crops each component to its valid region,
/// and packages the [`DecodedImage`].
fn assemble(
    frame: Option<Frame>,
    planes: Vec<Option<Plane>>,
    color: ColorInfo,
) -> Result<DecodedImage> {
    let frame = frame
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: no frame header"))?;
    if frame.y == 0 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JPEG: frame height Y is 0 and no DNL supplied it",
        ));
    }
    let hmax = usize::from(frame.hmax());
    let vmax = usize::from(frame.vmax());
    let y = usize::from(frame.y);

    let mut comps = Vec::with_capacity(frame.components.len());
    let mut ids = Vec::with_capacity(frame.components.len());
    for (fc, plane) in frame.components.iter().zip(planes) {
        let plane = plane.ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: a component was never coded")
        })?;
        let comp_h = (y * usize::from(fc.v)).div_ceil(vmax);
        // The decoded plane must cover the valid region's height (a DNL claiming more lines than
        // were coded would fail here). Its width needs no check: every plane is as wide as its
        // scan's block grid, `ceil(ceil(X·h/Hmax) / 8)` blocks or more (`decode_scan`'s `mcus_x`,
        // `build_prog_comps`), so the valid width `ceil(X·h/Hmax)` never exceeds the stride. The
        // width test this replaces could not fire; it went because its `X · h` was a mutant (`*` →
        // `/`) no input distinguishes, and it took that line's two killable siblings with it (#924).
        let padded_rows = plane.data.len().checked_div(plane.stride).unwrap_or(0);
        if comp_h > padded_rows {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: decoded plane smaller than the frame declares",
            ));
        }
        comps.push(DecComp {
            h: plane.h,
            v: plane.v,
            comp_h,
            stride: plane.stride,
            data: plane.data,
        });
        ids.push(fc.id);
    }

    Ok(DecodedImage {
        width: frame.x.into(),
        height: frame.y.into(),
        hmax,
        vmax,
        comps,
        ids,
        color,
    })
}
