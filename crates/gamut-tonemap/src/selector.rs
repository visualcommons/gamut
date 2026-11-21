//! A plain-data operator selection: the fieldless [`ToneOperator`] selector, the `Copy`
//! [`ToneParams`] that carries every operator's parameters and the [`CurveToneMapper`] settings,
//! and [`BuiltinCurve`], the one curve type a selection builds.
//!
//! The built-in operators are separate types behind [`ToneCurve`], which a Rust caller composes
//! directly but a C caller cannot name (#1103). This module is the runtime-selected route to the
//! same operators: a selector and a flat parameter record go in, and [`ToneParams::mapper`] builds
//! the mapper through each operator's own validated constructor and [`CurveToneMapper`]'s own
//! builders, so no rule is restated here and a selection maps exactly as the direct construction
//! does.

use gamut_core::Result;
use gamut_core::luminance::BT709_LUMA_WEIGHTS;

use crate::{
    Aces, ApplyMode, Clamp, CurveToneMapper, DEFAULT_DESAT, DEFAULT_DRAGO_BIAS, Drago, Exposure,
    Hable, Linear, Reinhard, ReinhardExtended, SDR_WHITE_NORMALIZED, ToneCurve,
};

/// Which built-in tone curve a [`ToneParams`] selects.
///
/// Discriminants are explicit and permanent — they are C ABI values (#1103); new operators append.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[repr(u8)]
pub enum ToneOperator {
    /// [`Linear`]; the [`ToneParams`] parameter is unused.
    Linear = 0,
    /// [`Clamp`]; the parameter is its `max`.
    Clamp = 1,
    /// [`Exposure`]; the parameter is its linear gain ([`Exposure::new`]'s `scale`).
    Exposure = 2,
    /// [`Reinhard`]; the parameter is unused.
    Reinhard = 3,
    /// [`ReinhardExtended`]; the parameter is its `white`.
    ReinhardExtended = 4,
    /// [`Aces`]; the parameter is unused.
    Aces = 5,
    /// [`Hable`]; the parameter is its `white`.
    Hable = 6,
    /// [`Drago`]; the parameter is its `world_max`, and [`ToneParams::bias`] its bias.
    Drago = 7,
}

/// One of the built-in tone curves, chosen at run time — what [`ToneParams::curve`] builds.
///
/// Each variant holds the operator exactly as its own constructor validated it, and
/// [`map`](ToneCurve::map) forwards to it, so a `BuiltinCurve` maps bit-for-bit as the operator it
/// wraps.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum BuiltinCurve {
    /// [`Linear`].
    Linear(Linear),
    /// [`Clamp`].
    Clamp(Clamp),
    /// [`Exposure`].
    Exposure(Exposure),
    /// [`Reinhard`].
    Reinhard(Reinhard),
    /// [`ReinhardExtended`].
    ReinhardExtended(ReinhardExtended),
    /// [`Aces`].
    Aces(Aces),
    /// [`Hable`].
    Hable(Hable),
    /// [`Drago`].
    Drago(Drago),
}

impl BuiltinCurve {
    /// The selector naming this curve's operator.
    #[must_use]
    pub fn operator(self) -> ToneOperator {
        match self {
            Self::Linear(_) => ToneOperator::Linear,
            Self::Clamp(_) => ToneOperator::Clamp,
            Self::Exposure(_) => ToneOperator::Exposure,
            Self::Reinhard(_) => ToneOperator::Reinhard,
            Self::ReinhardExtended(_) => ToneOperator::ReinhardExtended,
            Self::Aces(_) => ToneOperator::Aces,
            Self::Hable(_) => ToneOperator::Hable,
            Self::Drago(_) => ToneOperator::Drago,
        }
    }
}

impl ToneCurve for BuiltinCurve {
    #[inline]
    fn map(&self, x: f32) -> f32 {
        match self {
            Self::Linear(curve) => curve.map(x),
            Self::Clamp(curve) => curve.map(x),
            Self::Exposure(curve) => curve.map(x),
            Self::Reinhard(curve) => curve.map(x),
            Self::ReinhardExtended(curve) => curve.map(x),
            Self::Aces(curve) => curve.map(x),
            Self::Hable(curve) => curve.map(x),
            Self::Drago(curve) => curve.map(x),
        }
    }
}

/// A plain-data tone-mapping configuration: a [`ToneOperator`], its parameters, and the
/// [`CurveToneMapper`] settings, built into a mapper by [`ToneParams::mapper`].
///
/// Every field is plain data with a by-value accessor, so the record crosses a C boundary as a
/// flat struct. The setters store what they are given; validation happens once, in
/// [`curve`](Self::curve) and [`mapper`](Self::mapper), by the operator's own constructor and
/// [`CurveToneMapper`]'s own builders, so a `ToneParams` accepts exactly the configurations the
/// direct construction does and rejects the rest with the same error.
///
/// The one curve parameter means what [`ToneOperator`] says per operator: `Clamp`'s `max`,
/// `Exposure`'s gain, `ReinhardExtended`'s and `Hable`'s `white`, `Drago`'s `world_max`. It is
/// unused, and not validated, for `Linear`, `Reinhard` and `Aces`; likewise [`bias`](Self::bias)
/// is used only by `Drago`.
///
/// ```
/// use gamut_tonemap::{ApplyMode, ToneMapper, ToneOperator, ToneParams};
///
/// // Hable with its Uncharted 2 white, a 1000-nit master (1.0 = 100 cd/m²) into SDR.
/// let mapper = ToneParams::new(ToneOperator::Hable, 11.2, 10.0)
///     .with_mode(ApplyMode::Luminance)
///     .mapper()?;
/// let sdr = mapper.map_rgb([4.0, 2.0, 0.5]);
/// assert!(sdr.iter().all(|c| (0.0..=1.0).contains(c)));
/// # Ok::<(), gamut_core::Error>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToneParams {
    operator: ToneOperator,
    parameter: f32,
    bias: f32,
    mode: ApplyMode,
    desat: f32,
    source_peak: f32,
    target_peak: f32,
    luma_weights: [u32; 3],
}

impl ToneParams {
    /// Select `operator` with its curve `parameter` (see [`ToneOperator`] for what it means per
    /// operator), mapping `source_peak` onto the target peak, with [`CurveToneMapper::new`]'s
    /// defaults for the rest: target peak [`SDR_WHITE_NORMALIZED`], [`ApplyMode::MaxRgb`],
    /// [`DEFAULT_DESAT`], BT.709 luma weights, and Drago bias [`DEFAULT_DRAGO_BIAS`].
    ///
    /// Nothing is validated here; [`mapper`](Self::mapper) does it.
    #[must_use]
    pub fn new(operator: ToneOperator, parameter: f32, source_peak: f32) -> Self {
        Self {
            operator,
            parameter,
            bias: DEFAULT_DRAGO_BIAS,
            mode: ApplyMode::default(),
            desat: DEFAULT_DESAT,
            source_peak,
            target_peak: SDR_WHITE_NORMALIZED,
            luma_weights: BT709_LUMA_WEIGHTS,
        }
    }

    /// Set Drago's bias ([`Drago::with_bias`]); unused by every other operator.
    #[must_use]
    pub fn with_bias(mut self, bias: f32) -> Self {
        self.bias = bias;
        self
    }

    /// Set the peak the source peak maps to ([`CurveToneMapper::with_target_peak`]).
    #[must_use]
    pub fn with_target_peak(mut self, target_peak: f32) -> Self {
        self.target_peak = target_peak;
        self
    }

    /// Set which signal drives the curve ([`CurveToneMapper::with_mode`]).
    #[must_use]
    pub fn with_mode(mut self, mode: ApplyMode) -> Self {
        self.mode = mode;
        self
    }

    /// Set the desaturation threshold, `0.0` to disable it ([`CurveToneMapper::with_desat`]).
    #[must_use]
    pub fn with_desat(mut self, desat: f32) -> Self {
        self.desat = desat;
        self
    }

    /// Set the fixed-point luma weights ([`CurveToneMapper::with_luma_weights`]).
    #[must_use]
    pub fn with_luma_weights(mut self, luma_weights: [u32; 3]) -> Self {
        self.luma_weights = luma_weights;
        self
    }

    /// The selected operator.
    #[must_use]
    pub fn operator(self) -> ToneOperator {
        self.operator
    }

    /// The curve parameter, as given.
    #[must_use]
    pub fn parameter(self) -> f32 {
        self.parameter
    }

    /// Drago's bias, as given.
    #[must_use]
    pub fn bias(self) -> f32 {
        self.bias
    }

    /// The signal that drives the curve.
    #[must_use]
    pub fn mode(self) -> ApplyMode {
        self.mode
    }

    /// The desaturation threshold.
    #[must_use]
    pub fn desat(self) -> f32 {
        self.desat
    }

    /// The input value that maps to [`target_peak`](Self::target_peak).
    #[must_use]
    pub fn source_peak(self) -> f32 {
        self.source_peak
    }

    /// The output value [`source_peak`](Self::source_peak) maps to.
    #[must_use]
    pub fn target_peak(self) -> f32 {
        self.target_peak
    }

    /// The fixed-point luma weights.
    #[must_use]
    pub fn luma_weights(self) -> [u32; 3] {
        self.luma_weights
    }

    /// Build the selected curve through its operator's own constructor.
    ///
    /// # Errors
    ///
    /// Returns the operator constructor's
    /// [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error when the parameter
    /// (or, for `Drago`, the bias) is out of its range.
    pub fn curve(self) -> Result<BuiltinCurve> {
        let parameter = self.parameter;
        Ok(match self.operator {
            ToneOperator::Linear => BuiltinCurve::Linear(Linear),
            ToneOperator::Clamp => BuiltinCurve::Clamp(Clamp::new(parameter)?),
            ToneOperator::Exposure => BuiltinCurve::Exposure(Exposure::new(parameter)?),
            ToneOperator::Reinhard => BuiltinCurve::Reinhard(Reinhard),
            ToneOperator::ReinhardExtended => {
                BuiltinCurve::ReinhardExtended(ReinhardExtended::new(parameter)?)
            }
            ToneOperator::Aces => BuiltinCurve::Aces(Aces),
            ToneOperator::Hable => BuiltinCurve::Hable(Hable::new(parameter)?),
            ToneOperator::Drago => {
                BuiltinCurve::Drago(Drago::new(parameter)?.with_bias(self.bias)?)
            }
        })
    }

    /// Build the configured mapper: [`curve`](Self::curve), then [`CurveToneMapper::new`] and its
    /// builders with this record's settings.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error from
    /// [`curve`](Self::curve), or from whichever [`CurveToneMapper`] builder rejects a setting —
    /// the same error the direct construction returns.
    pub fn mapper(self) -> Result<CurveToneMapper<BuiltinCurve>> {
        Ok(CurveToneMapper::new(self.curve()?, self.source_peak)?
            .with_target_peak(self.target_peak)?
            .with_desat(self.desat)?
            .with_luma_weights(self.luma_weights)?
            .with_mode(self.mode))
    }
}

#[cfg(test)]
mod tests {
    use gamut_core::ErrorKind;
    use gamut_core::luminance::BT2020_LUMA_WEIGHTS;

    use super::*;
    use crate::ToneMapper;

    /// Every operator, with a parameter in its range and a direct construction of the same curve.
    fn every_operator() -> [(ToneOperator, f32, BuiltinCurve); 8] {
        let ok = "valid parameter";
        [
            (ToneOperator::Linear, 0.0, BuiltinCurve::Linear(Linear)),
            (
                ToneOperator::Clamp,
                3.0,
                BuiltinCurve::Clamp(Clamp::new(3.0).expect(ok)),
            ),
            (
                ToneOperator::Exposure,
                0.6,
                BuiltinCurve::Exposure(Exposure::new(0.6).expect(ok)),
            ),
            (
                ToneOperator::Reinhard,
                0.0,
                BuiltinCurve::Reinhard(Reinhard),
            ),
            (
                ToneOperator::ReinhardExtended,
                4.0,
                BuiltinCurve::ReinhardExtended(ReinhardExtended::new(4.0).expect(ok)),
            ),
            (ToneOperator::Aces, 0.0, BuiltinCurve::Aces(Aces)),
            (
                ToneOperator::Hable,
                11.2,
                BuiltinCurve::Hable(Hable::new(11.2).expect(ok)),
            ),
            (
                ToneOperator::Drago,
                100.0,
                BuiltinCurve::Drago(Drago::new(100.0).expect(ok)),
            ),
        ]
    }

    /// Inputs spanning each curve's toe, shoulder and saturation.
    const INPUTS: [f32; 9] = [0.0, 1e-3, 0.18, 0.5, 1.0, 2.5, 10.0, 100.0, 1e6];

    fn assert_rejected<T: core::fmt::Debug>(result: Result<T>) {
        assert_eq!(
            result.expect_err("must be rejected").kind(),
            ErrorKind::InvalidInput
        );
    }

    #[test]
    fn tone_operator_discriminants_are_permanent() {
        assert_eq!(ToneOperator::Linear as u8, 0);
        assert_eq!(ToneOperator::Clamp as u8, 1);
        assert_eq!(ToneOperator::Exposure as u8, 2);
        assert_eq!(ToneOperator::Reinhard as u8, 3);
        assert_eq!(ToneOperator::ReinhardExtended as u8, 4);
        assert_eq!(ToneOperator::Aces as u8, 5);
        assert_eq!(ToneOperator::Hable as u8, 6);
        assert_eq!(ToneOperator::Drago as u8, 7);
    }

    #[test]
    fn every_operator_builds_its_direct_construction() {
        for (operator, parameter, direct) in every_operator() {
            let curve = ToneParams::new(operator, parameter, 10.0)
                .curve()
                .expect("valid parameter");
            assert_eq!(curve, direct, "{operator:?}");
            assert_eq!(curve.operator(), operator);
        }
    }

    #[test]
    fn builtin_curve_maps_as_the_operator_it_wraps() {
        // Each operator is compared with its own unwrapped type, so a forwarding arm that reached
        // the wrong operator (or none) is seen; the eight curves differ on these inputs.
        let ok = "valid parameter";
        let direct: [(BuiltinCurve, &dyn ToneCurve); 8] = [
            (BuiltinCurve::Linear(Linear), &Linear),
            (
                BuiltinCurve::Clamp(Clamp::new(3.0).expect(ok)),
                &Clamp::new(3.0).expect(ok),
            ),
            (
                BuiltinCurve::Exposure(Exposure::new(0.6).expect(ok)),
                &Exposure::new(0.6).expect(ok),
            ),
            (BuiltinCurve::Reinhard(Reinhard), &Reinhard),
            (
                BuiltinCurve::ReinhardExtended(ReinhardExtended::new(4.0).expect(ok)),
                &ReinhardExtended::new(4.0).expect(ok),
            ),
            (BuiltinCurve::Aces(Aces), &Aces),
            (
                BuiltinCurve::Hable(Hable::new(11.2).expect(ok)),
                &Hable::new(11.2).expect(ok),
            ),
            (
                BuiltinCurve::Drago(Drago::new(100.0).expect(ok)),
                &Drago::new(100.0).expect(ok),
            ),
        ];
        for (wrapped, curve) in direct {
            for x in INPUTS {
                assert_eq!(
                    wrapped.map(x).to_bits(),
                    curve.map(x).to_bits(),
                    "{wrapped:?} at {x}"
                );
            }
        }
    }

    #[test]
    fn drago_takes_the_bias() {
        let curve = ToneParams::new(ToneOperator::Drago, 100.0, 10.0)
            .with_bias(0.7)
            .curve()
            .expect("valid parameters");
        let direct = Drago::new(100.0)
            .and_then(|d| d.with_bias(0.7))
            .expect("valid parameters");
        assert_eq!(curve, BuiltinCurve::Drago(direct));
    }

    #[test]
    fn every_operator_maps_as_its_direct_mapper() {
        // Every setter away from its default, so a setting the build dropped shows in the output.
        let pixels = [
            [0.0, 0.0, 0.0],
            [0.5, 1.0, 2.0],
            [8.0, 3.0, 0.25],
            [40.0, 0.0, 5.0],
        ];
        for mode in [
            ApplyMode::PerChannel,
            ApplyMode::Luminance,
            ApplyMode::MaxRgb,
        ] {
            for (operator, parameter, direct) in every_operator() {
                let params = ToneParams::new(operator, parameter, 6.0)
                    .with_target_peak(0.8)
                    .with_desat(1.5)
                    .with_luma_weights(BT2020_LUMA_WEIGHTS)
                    .with_mode(mode);
                let selected = params.mapper().expect("valid parameters");
                let expected = CurveToneMapper::new(direct, 6.0)
                    .and_then(|m| m.with_target_peak(0.8))
                    .and_then(|m| m.with_desat(1.5))
                    .and_then(|m| m.with_luma_weights(BT2020_LUMA_WEIGHTS))
                    .expect("valid parameters")
                    .with_mode(mode);
                assert_eq!(selected, expected, "{operator:?} {mode:?}");
                for px in pixels {
                    assert_eq!(
                        selected.map_rgb(px).map(f32::to_bits),
                        expected.map_rgb(px).map(f32::to_bits),
                        "{operator:?} {mode:?} {px:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn defaults_are_curve_tone_mappers() {
        let params = ToneParams::new(ToneOperator::Reinhard, 0.0, 10.0);
        let direct = CurveToneMapper::new(Reinhard, 10.0).expect("valid");
        assert_eq!(params.mode(), direct.mode());
        assert_eq!(params.desat(), direct.desat());
        assert_eq!(params.target_peak(), direct.target_peak());
        assert_eq!(params.luma_weights(), direct.luma_weights());
        assert_eq!(params.bias(), DEFAULT_DRAGO_BIAS);
    }

    #[test]
    fn setters_set_what_the_accessors_report() {
        let params = ToneParams::new(ToneOperator::Hable, 5.0, 3.0)
            .with_bias(0.6)
            .with_target_peak(0.5)
            .with_mode(ApplyMode::Luminance)
            .with_desat(7.5)
            .with_luma_weights(BT2020_LUMA_WEIGHTS);
        assert_eq!(params.operator(), ToneOperator::Hable);
        assert_eq!(params.parameter(), 5.0);
        assert_eq!(params.bias(), 0.6);
        assert_eq!(params.source_peak(), 3.0);
        assert_eq!(params.target_peak(), 0.5);
        assert_eq!(params.mode(), ApplyMode::Luminance);
        assert_eq!(params.desat(), 7.5);
        assert_eq!(params.luma_weights(), BT2020_LUMA_WEIGHTS);
    }

    #[test]
    fn a_parameterless_operator_ignores_the_parameter_and_bias() {
        for operator in [
            ToneOperator::Linear,
            ToneOperator::Reinhard,
            ToneOperator::Aces,
        ] {
            let params = ToneParams::new(operator, f32::NAN, 10.0).with_bias(f32::NAN);
            assert_eq!(params.curve().expect("unused").operator(), operator);
        }
    }

    #[test]
    fn an_out_of_range_parameter_is_the_operators_error() {
        for operator in [
            ToneOperator::Clamp,
            ToneOperator::Exposure,
            ToneOperator::ReinhardExtended,
            ToneOperator::Hable,
            ToneOperator::Drago,
        ] {
            assert_rejected(ToneParams::new(operator, -1.0, 10.0).curve());
        }
        assert_rejected(
            ToneParams::new(ToneOperator::Drago, 100.0, 10.0)
                .with_bias(1.0)
                .curve(),
        );
    }

    #[test]
    fn an_out_of_range_setting_is_the_mappers_error() {
        let params = ToneParams::new(ToneOperator::Linear, 0.0, 1.0);
        assert!(params.mapper().is_ok());
        assert_rejected(ToneParams::new(ToneOperator::Linear, 0.0, 0.0).mapper());
        assert_rejected(params.with_target_peak(0.0).mapper());
        assert_rejected(params.with_desat(-1.0).mapper());
        assert_rejected(params.with_luma_weights([0, 0, 0]).mapper());
        assert_rejected(ToneParams::new(ToneOperator::Clamp, 0.0, 1.0).mapper());
    }
}
