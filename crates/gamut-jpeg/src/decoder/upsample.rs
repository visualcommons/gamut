//! Chroma upsampling (T.81 §A.2 NOTE leaves the filter open): mapping each decoded component onto
//! the output grid, either through libjpeg-turbo's interpolating kernels or by sample replication.
//! See the parent module's [Upsampling](super#upsampling) section for the policy.

use gamut_color::ChromaUpsampling;
use gamut_dsp::upsample::{libjpeg, taps};

use super::DecodedImage;

impl DecodedImage {
    /// The row of component `ci` under output row `py`, via nearest-neighbour (sample-replication)
    /// upsampling -- [`ColumnMap`]'s vertical twin -- as the slice of the plane that holds it.
    fn sample_row(&self, ci: usize, py: usize) -> &[u8] {
        let c = &self.comps[ci];
        self.plane_row(ci, (py * usize::from(c.v) / self.vmax).min(c.comp_h - 1))
    }

    /// Row `cy` of component `ci`'s plane, `stride` samples wide.
    fn plane_row(&self, ci: usize, cy: usize) -> &[u8] {
        let c = &self.comps[ci];
        &c.data[cy * c.stride..(cy + 1) * c.stride]
    }
}

/// How one component is upsampled to the output grid: libjpeg-turbo's choice of method
/// (`jinit_upsampler` in `jdsample.c`) under [`ChromaUpsampling::Interpolated`], and replication
/// for every component under [`ChromaUpsampling::Nearest`].
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Filter {
    /// Sample replication through the component's [`ColumnMap`] and [`DecodedImage::sample_row`].
    Replicate(ColumnMap),
    /// Half horizontal resolution, full vertical: [`libjpeg::h2v1`].
    H2v1,
    /// Full horizontal resolution, half vertical: [`libjpeg::h1v2`].
    H1v2,
    /// Half resolution both ways: [`libjpeg::h2v2`].
    H2v2,
}

impl Filter {
    /// The filter for a component sampled `h` of `hmax` horizontally and `v` of `vmax`
    /// vertically, under an image `width` columns wide. Like libjpeg-turbo, the interpolating
    /// kernels are picked by the ratio alone, so a component sampled 2 of 4 takes the same one as
    /// a component sampled 1 of 2.
    pub(super) fn new(
        (h, hmax): (usize, usize),
        (v, vmax): (usize, usize),
        width: usize,
        upsampling: ChromaUpsampling,
    ) -> Self {
        if upsampling == ChromaUpsampling::Interpolated {
            match (h * 2 == hmax, v * 2 == vmax) {
                (true, true) => return Self::H2v2,
                (true, false) if v == vmax => return Self::H2v1,
                (false, true) if h == hmax => return Self::H1v2,
                _ => {}
            }
        }
        Self::Replicate(ColumnMap::new(h, hmax, width))
    }

    /// Component `ci`'s samples under every column of output row `py`: the plane row itself when
    /// [`ColumnMap::Direct`] replicates it in place, otherwise `scratch` (one output row wide)
    /// filled with them.
    pub(super) fn row<'a>(
        &self,
        img: &'a DecodedImage,
        ci: usize,
        py: usize,
        scratch: &'a mut [u8],
    ) -> &'a [u8] {
        let rows = || {
            let (near, far) = taps(py, img.comps[ci].comp_h);
            (img.plane_row(ci, near), img.plane_row(ci, far))
        };
        match self {
            Self::Replicate(map) => return map.upsample(img.sample_row(ci, py), scratch),
            Self::H2v1 => libjpeg::h2v1(img.sample_row(ci, py), scratch),
            Self::H1v2 => {
                let (near, far) = rows();
                libjpeg::h1v2(near, far, !py.is_multiple_of(2), scratch);
            }
            Self::H2v2 => {
                let (near, far) = rows();
                libjpeg::h2v2(near, far, scratch);
            }
        }
        scratch
    }
}

/// How one component's samples spread across the output columns under nearest-neighbour
/// (sample-replication) upsampling: output column `px` takes component column `px * h / hmax`.
///
/// That column never needs clamping to the component's width: the driver's `assemble` sizes a component
/// `comp_w = ceil(width * h / hmax)` samples wide, and for `px < width`,
/// `px * h / hmax <= (width - 1) * h / hmax < comp_w`. (The same argument holds for the vertical
/// twin, [`DecodedImage::sample_row`], whose clamp this change leaves as it was.)
///
/// The two ratios real streams use get a path of their own, so the common rows need no per-sample
/// index: full horizontal resolution reads the plane row in place, and half resolution (4:2:0 and
/// 4:2:2 chroma) doubles each sample. Every other ratio (the `3`s §A.1.1 permits) goes through a
/// column map computed once per image. All three compute the same function, so no decoded image
/// can tell which one a component took; `column_map_picks_the_path_for_each_ratio` pins the
/// choice itself.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ColumnMap {
    /// `h == hmax`: component column `px` under output column `px`, read in place.
    Direct,
    /// `2 * h == hmax`: component column `px / 2` under output column `px`.
    Double,
    /// Any other ratio: the component column under every output column.
    Indexed(Vec<usize>),
}

impl ColumnMap {
    /// The map for a component sampled `h` of `hmax` horizontally, under an image `width` columns
    /// wide.
    fn new(h: usize, hmax: usize, width: usize) -> Self {
        if h == hmax {
            Self::Direct
        } else if 2 * h == hmax {
            Self::Double
        } else {
            Self::Indexed((0..width).map(|px| px * h / hmax).collect())
        }
    }

    /// The component's samples under every output column of the plane row `src`: `src` itself for
    /// [`Self::Direct`], otherwise `scratch` (one output row wide) filled with them.
    fn upsample<'a>(&self, src: &'a [u8], scratch: &'a mut [u8]) -> &'a [u8] {
        match self {
            Self::Direct => &src[..scratch.len()],
            Self::Double => {
                let (pairs, last) = scratch.as_chunks_mut::<2>();
                let paired = pairs.len();
                for (pair, &s) in pairs.iter_mut().zip(src) {
                    *pair = [s, s];
                }
                // An odd width's last column is the first half of a pair the row has no room for.
                for (dst, &s) in last.iter_mut().zip(&src[paired..]) {
                    *dst = s;
                }
                scratch
            }
            Self::Indexed(map) => {
                for (dst, &cx) in scratch.iter_mut().zip(map) {
                    *dst = src[cx];
                }
                scratch
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_map_picks_the_path_for_each_ratio() {
        // The three paths agree on every output (next test), so only the choice itself can show
        // which ratio takes which path.
        assert_eq!(ColumnMap::new(1, 1, 5), ColumnMap::Direct);
        assert_eq!(ColumnMap::new(2, 2, 5), ColumnMap::Direct);
        assert_eq!(ColumnMap::new(1, 2, 5), ColumnMap::Double);
        assert_eq!(ColumnMap::new(2, 4, 5), ColumnMap::Double);
        assert_eq!(
            ColumnMap::new(1, 4, 5),
            ColumnMap::Indexed(vec![0, 0, 0, 0, 1])
        );
        assert_eq!(
            ColumnMap::new(2, 3, 5),
            ColumnMap::Indexed(vec![0, 0, 1, 2, 2])
        );
    }

    #[test]
    fn filter_picks_libjpeg_turbos_method_for_each_ratio() {
        // jinit_upsampler's ladder: by ratio, not absolute factor, and only the three 2:1 shapes
        // interpolate. Each case pins one arm or guard; a 2:1 axis paired with any other ratio on
        // the other axis falls back to replication.
        let fancy = ChromaUpsampling::Interpolated;
        let f = |h, v| Filter::new(h, v, 5, fancy);
        assert_eq!(f((1, 2), (1, 2)), Filter::H2v2);
        assert_eq!(f((2, 4), (2, 4)), Filter::H2v2);
        assert_eq!(f((1, 2), (1, 1)), Filter::H2v1);
        assert_eq!(f((2, 4), (2, 2)), Filter::H2v1);
        assert_eq!(f((1, 1), (1, 2)), Filter::H1v2);
        assert_eq!(f((3, 3), (2, 4)), Filter::H1v2);
        assert_eq!(f((1, 1), (1, 1)), Filter::Replicate(ColumnMap::Direct));
        assert_eq!(f((1, 2), (1, 4)), Filter::Replicate(ColumnMap::Double));
        assert_eq!(
            f((1, 4), (1, 2)),
            Filter::Replicate(ColumnMap::Indexed(vec![0, 0, 0, 0, 1]))
        );
        let nearest = |h, v| Filter::new(h, v, 5, ChromaUpsampling::Nearest);
        assert_eq!(
            nearest((1, 2), (1, 2)),
            Filter::Replicate(ColumnMap::Double)
        );
        assert_eq!(
            nearest((1, 1), (1, 2)),
            Filter::Replicate(ColumnMap::Direct)
        );
    }

    #[test]
    fn column_map_paths_replicate_like_the_index_formula() {
        // Every path against `px * h / hmax` itself, at odd and even widths, from a plane row
        // padded past the component's width (as a block-padded plane is) with samples that differ
        // at every column, so reading one column off or past the valid region moves an output.
        for (h, hmax) in [(1, 1), (2, 2), (1, 2), (2, 4), (1, 4), (1, 3), (2, 3)] {
            for width in 1usize..=19 {
                let comp_w = (width * h).div_ceil(hmax);
                let src: Vec<u8> = (0..comp_w + 8).map(|i| (i * 37 + 11) as u8).collect();
                let want: Vec<u8> = (0..width).map(|px| src[px * h / hmax]).collect();
                let mut scratch = vec![0u8; width];
                let got = ColumnMap::new(h, hmax, width).upsample(&src, &mut scratch);
                assert_eq!(got, want, "h {h} of {hmax}, width {width}");
            }
        }
    }
}
