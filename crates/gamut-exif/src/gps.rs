//! A typed view over the GPS sub-IFD (Exif 3.0 §4.6.6).
//!
//! [`GpsInfo`] is a convenience projection of the positioning tags — latitude, longitude, and
//! altitude — not a lossless model of the whole GPS directory. The complete directory is always
//! available as the raw [`gamut_ifd::Ifd`] from [`Exif::gps_ifd`](crate::Exif::gps_ifd); this view
//! lifts the common fields into typed coordinates and decimal-degree accessors.

use gamut_ifd::{Ifd, Value};

use crate::tag::ExifTag;
use crate::value::{Rational, as_text};

/// Errors converting EXIF GPS fields into `geocoordinates` types.
#[cfg(feature = "geocoordinates")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum GpsConversionError {
    /// Latitude is required for a complete position.
    #[error("GPS latitude is missing")]
    MissingLatitude,
    /// Longitude is required for a complete position.
    #[error("GPS longitude is missing")]
    MissingLongitude,
    /// A rational component had a zero denominator.
    #[error("GPS {0} has a zero denominator")]
    ZeroDenominator(&'static str),
    /// A DMS coordinate used minutes or seconds outside `[0, 60)`.
    #[error("GPS {0} has invalid degrees/minutes/seconds components")]
    InvalidDms(&'static str),
    /// The resulting latitude or longitude is outside the WGS-84 angular domain.
    #[error("GPS position is outside the valid latitude/longitude range")]
    OutOfRange,
}

/// The positioning data from the GPS sub-IFD, as typed latitude/longitude/altitude.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpsInfo {
    /// Latitude with its N/S reference, if both are present and readable.
    pub latitude: Option<GpsLatitude>,
    /// Longitude with its E/W reference, if both are present and readable.
    pub longitude: Option<GpsLongitude>,
    /// Altitude relative to sea level, if present.
    pub altitude: Option<GpsAltitude>,
}

/// A latitude: degrees/minutes/seconds as rationals (`GPSLatitude`), plus its north/south
/// reference (`GPSLatitudeRef`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpsLatitude {
    /// Degrees.
    pub degrees: Rational,
    /// Minutes.
    pub minutes: Rational,
    /// Seconds.
    pub seconds: Rational,
    /// The hemisphere: `N` or `S`.
    pub reference: LatitudeRef,
}

/// A longitude: degrees/minutes/seconds as rationals (`GPSLongitude`), plus its east/west
/// reference (`GPSLongitudeRef`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpsLongitude {
    /// Degrees.
    pub degrees: Rational,
    /// Minutes.
    pub minutes: Rational,
    /// Seconds.
    pub seconds: Rational,
    /// The hemisphere: `E` or `W`.
    pub reference: LongitudeRef,
}

/// An altitude: metres from sea level, with the sign carried by `below_sea_level`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpsAltitude {
    /// The magnitude in metres.
    pub meters: Rational,
    /// Whether the altitude is below sea level (`GPSAltitudeRef` = 1).
    pub below_sea_level: bool,
}

/// The hemisphere of a latitude (`GPSLatitudeRef`, Exif 3.0 §4.6.7.1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatitudeRef {
    /// `N` — northern latitude.
    North,
    /// `S` — southern latitude.
    South,
}

/// The hemisphere of a longitude (`GPSLongitudeRef`, Exif 3.0 §4.6.7.1.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LongitudeRef {
    /// `E` — eastern longitude.
    East,
    /// `W` — western longitude.
    West,
}

impl LatitudeRef {
    /// Parses the single-letter reference string (`"N"`/`"S"`, either case). Any other letter —
    /// an `E`/`W` written into `GPSLatitudeRef` included — is no latitude reference.
    fn parse(s: &str) -> Option<Self> {
        match s.trim_end_matches('\0').chars().next()? {
            'N' | 'n' => Some(LatitudeRef::North),
            'S' | 's' => Some(LatitudeRef::South),
            _ => None,
        }
    }

    /// The single-letter reference string.
    fn as_str(self) -> &'static str {
        match self {
            LatitudeRef::North => "N",
            LatitudeRef::South => "S",
        }
    }

    /// `-1.0` for the southern hemisphere, `+1.0` for the northern.
    fn sign(self) -> f64 {
        match self {
            LatitudeRef::North => 1.0,
            LatitudeRef::South => -1.0,
        }
    }
}

impl LongitudeRef {
    /// Parses the single-letter reference string (`"E"`/`"W"`, either case). Any other letter —
    /// an `N`/`S` written into `GPSLongitudeRef` included — is no longitude reference.
    fn parse(s: &str) -> Option<Self> {
        match s.trim_end_matches('\0').chars().next()? {
            'E' | 'e' => Some(LongitudeRef::East),
            'W' | 'w' => Some(LongitudeRef::West),
            _ => None,
        }
    }

    /// The single-letter reference string.
    fn as_str(self) -> &'static str {
        match self {
            LongitudeRef::East => "E",
            LongitudeRef::West => "W",
        }
    }

    /// `-1.0` for the western hemisphere, `+1.0` for the eastern.
    fn sign(self) -> f64 {
        match self {
            LongitudeRef::East => 1.0,
            LongitudeRef::West => -1.0,
        }
    }
}

impl GpsLatitude {
    /// The latitude as signed decimal degrees (negative south), or `None` if any rational has a
    /// zero denominator.
    #[must_use]
    pub fn to_degrees(self) -> Option<f64> {
        Some(self.reference.sign() * unsigned_degrees(self.degrees, self.minutes, self.seconds)?)
    }
}

impl GpsLongitude {
    /// The longitude as signed decimal degrees (negative west), or `None` if any rational has a
    /// zero denominator.
    #[must_use]
    pub fn to_degrees(self) -> Option<f64> {
        Some(self.reference.sign() * unsigned_degrees(self.degrees, self.minutes, self.seconds)?)
    }
}

/// `d + m/60 + s/3600`, or `None` if any rational has a zero denominator.
fn unsigned_degrees(degrees: Rational, minutes: Rational, seconds: Rational) -> Option<f64> {
    Some(degrees.to_f64()? + minutes.to_f64()? / 60.0 + seconds.to_f64()? / 3600.0)
}

impl GpsInfo {
    /// Lifts the positioning tags out of a GPS sub-IFD, or `None` if none are present.
    #[must_use]
    pub fn from_ifd(ifd: &Ifd) -> Option<GpsInfo> {
        let latitude = latitude(ifd);
        let longitude = longitude(ifd);
        let altitude = altitude(ifd);
        if latitude.is_none() && longitude.is_none() && altitude.is_none() {
            None
        } else {
            Some(GpsInfo {
                latitude,
                longitude,
                altitude,
            })
        }
    }

    /// Builds a minimal, spec-valid GPS sub-IFD from this view.
    ///
    /// This carries only the latitude/longitude/altitude tags plus the mandatory `GPSVersionID`
    /// (2.3.0.0); it is not the inverse of [`from_ifd`](Self::from_ifd) for a directory that held
    /// other GPS tags (use the raw [`Exif::gps_ifd`](crate::Exif::gps_ifd) to preserve those).
    #[must_use]
    pub fn to_ifd(&self) -> Ifd {
        let mut ifd = Ifd::new();
        ifd.set(
            ExifTag::GpsVersionId.tag_id(),
            Value::Byte(vec![2, 3, 0, 0]),
        );
        if let Some(lat) = self.latitude {
            set_coordinate(
                &mut ifd,
                [lat.degrees, lat.minutes, lat.seconds],
                ExifTag::GpsLatitude,
                lat.reference.as_str(),
                ExifTag::GpsLatitudeRef,
            );
        }
        if let Some(lon) = self.longitude {
            set_coordinate(
                &mut ifd,
                [lon.degrees, lon.minutes, lon.seconds],
                ExifTag::GpsLongitude,
                lon.reference.as_str(),
                ExifTag::GpsLongitudeRef,
            );
        }
        if let Some(alt) = self.altitude {
            ifd.set(
                ExifTag::GpsAltitudeRef.tag_id(),
                Value::Byte(vec![u8::from(alt.below_sea_level)]),
            );
            ifd.set(
                ExifTag::GpsAltitude.tag_id(),
                Value::Rational(vec![alt.meters.into()]),
            );
        }
        ifd
    }

    /// Latitude in signed decimal degrees (negative for the southern hemisphere).
    #[must_use]
    pub fn latitude_deg(&self) -> Option<f64> {
        self.latitude.and_then(GpsLatitude::to_degrees)
    }

    /// Longitude in signed decimal degrees (negative for the western hemisphere).
    #[must_use]
    pub fn longitude_deg(&self) -> Option<f64> {
        self.longitude.and_then(GpsLongitude::to_degrees)
    }

    /// Altitude in signed metres (negative below sea level).
    #[must_use]
    pub fn altitude_m(&self) -> Option<f64> {
        self.altitude.and_then(|a| {
            a.meters
                .to_f64()
                .map(|m| if a.below_sea_level { -m } else { m })
        })
    }
}

#[cfg(feature = "geocoordinates")]
impl TryFrom<GpsInfo> for geocoordinates::Wgs84 {
    type Error = GpsConversionError;

    /// Converts a complete, valid EXIF GPS position to decimal-degree WGS-84.
    ///
    /// EXIF altitude is intentionally omitted because [`geocoordinates::Wgs84`] is a 2D datum
    /// newtype. Convert to [`geocoordinates::Coordinate`] to preserve it.
    fn try_from(value: GpsInfo) -> Result<Self, Self::Error> {
        let coordinate = coordinate_without_height(&value)?;
        Ok(geocoordinates::Wgs84::new(coordinate.lat, coordinate.lon))
    }
}

#[cfg(feature = "geocoordinates")]
impl TryFrom<GpsInfo> for geocoordinates::Coordinate {
    type Error = GpsConversionError;

    /// Converts a complete, valid EXIF GPS position to a WGS-84 coordinate, preserving EXIF
    /// sea-level altitude as an orthometric height when present.
    fn try_from(value: GpsInfo) -> Result<Self, Self::Error> {
        let mut coordinate = coordinate_without_height(&value)?;
        if let Some(altitude) = value.altitude {
            let meters = altitude
                .meters
                .to_f64()
                .ok_or(GpsConversionError::ZeroDenominator("altitude"))?;
            let signed = if altitude.below_sea_level {
                -meters
            } else {
                meters
            };
            coordinate = coordinate.with_height(geocoordinates::Height::Orthometric(signed));
        }
        coordinate
            .validate()
            .map_err(|_| GpsConversionError::OutOfRange)?;
        Ok(coordinate)
    }
}

#[cfg(feature = "geocoordinates")]
fn coordinate_without_height(
    value: &GpsInfo,
) -> Result<geocoordinates::Coordinate, GpsConversionError> {
    let latitude = value.latitude.ok_or(GpsConversionError::MissingLatitude)?;
    let longitude = value
        .longitude
        .ok_or(GpsConversionError::MissingLongitude)?;
    let lat = coordinate_degrees(
        [latitude.degrees, latitude.minutes, latitude.seconds],
        latitude.reference.sign(),
        "latitude",
    )?;
    let lon = coordinate_degrees(
        [longitude.degrees, longitude.minutes, longitude.seconds],
        longitude.reference.sign(),
        "longitude",
    )?;
    let coordinate = geocoordinates::Coordinate::wgs84(lat, lon);
    coordinate
        .validate()
        .map_err(|_| GpsConversionError::OutOfRange)?;
    Ok(coordinate)
}

/// Signed decimal degrees of one DMS triple, refusing a zero denominator or a minutes/seconds
/// component outside `[0, 60)`. `axis` names the coordinate in the error.
#[cfg(feature = "geocoordinates")]
fn coordinate_degrees(
    [degrees, minutes, seconds]: [Rational; 3],
    sign: f64,
    axis: &'static str,
) -> Result<f64, GpsConversionError> {
    let degrees = degrees
        .to_f64()
        .ok_or(GpsConversionError::ZeroDenominator(axis))?;
    let minutes = minutes
        .to_f64()
        .ok_or(GpsConversionError::ZeroDenominator(axis))?;
    let seconds = seconds
        .to_f64()
        .ok_or(GpsConversionError::ZeroDenominator(axis))?;
    if minutes >= 60.0 || seconds >= 60.0 {
        return Err(GpsConversionError::InvalidDms(axis));
    }
    Ok(sign * (degrees + minutes / 60.0 + seconds / 3600.0))
}

/// Reads the latitude and its N/S reference from `ifd`.
fn latitude(ifd: &Ifd) -> Option<GpsLatitude> {
    let [degrees, minutes, seconds] = dms(ifd, ExifTag::GpsLatitude)?;
    let reference = LatitudeRef::parse(as_text(ifd.get(ExifTag::GpsLatitudeRef.tag_id())?)?)?;
    Some(GpsLatitude {
        degrees,
        minutes,
        seconds,
        reference,
    })
}

/// Reads the longitude and its E/W reference from `ifd`.
fn longitude(ifd: &Ifd) -> Option<GpsLongitude> {
    let [degrees, minutes, seconds] = dms(ifd, ExifTag::GpsLongitude)?;
    let reference = LongitudeRef::parse(as_text(ifd.get(ExifTag::GpsLongitudeRef.tag_id())?)?)?;
    Some(GpsLongitude {
        degrees,
        minutes,
        seconds,
        reference,
    })
}

/// Reads a degrees/minutes/seconds triple of RATIONALs from `tag`; `None` unless it holds at
/// least three.
fn dms(ifd: &Ifd, tag: ExifTag) -> Option<[Rational; 3]> {
    match ifd.get(tag.tag_id())? {
        Value::Rational(r) if r.len() >= 3 => Some([r[0].into(), r[1].into(), r[2].into()]),
        _ => None,
    }
}

/// Writes a coordinate's reference letter and its three DMS rationals into `ifd`.
fn set_coordinate(
    ifd: &mut Ifd,
    [degrees, minutes, seconds]: [Rational; 3],
    value_tag: ExifTag,
    reference: &str,
    ref_tag: ExifTag,
) {
    ifd.set(ref_tag.tag_id(), Value::Ascii(reference.to_owned()));
    ifd.set(
        value_tag.tag_id(),
        Value::Rational(vec![degrees.into(), minutes.into(), seconds.into()]),
    );
}

/// Reads the altitude and its above/below-sea-level reference from `ifd`.
fn altitude(ifd: &Ifd) -> Option<GpsAltitude> {
    let meters = match ifd.get(ExifTag::GpsAltitude.tag_id())? {
        Value::Rational(r) => (*r.first()?).into(),
        _ => return None,
    };
    // GPSAltitudeRef: 0 = above sea level (the default when absent), 1 = below.
    let below_sea_level = ifd.get_u32(ExifTag::GpsAltitudeRef.tag_id()).unwrap_or(0) == 1;
    Some(GpsAltitude {
        meters,
        below_sea_level,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_ifd() -> Ifd {
        let mut ifd = Ifd::new();
        // 48°51'29.6" N, 2°21'02.4" E, 35 m above sea level (approx. Paris).
        ifd.set(ExifTag::GpsLatitudeRef.tag_id(), Value::Ascii("N".into()));
        ifd.set(
            ExifTag::GpsLatitude.tag_id(),
            Value::Rational(vec![(48, 1), (51, 1), (296, 10)]),
        );
        ifd.set(ExifTag::GpsLongitudeRef.tag_id(), Value::Ascii("E".into()));
        ifd.set(
            ExifTag::GpsLongitude.tag_id(),
            Value::Rational(vec![(2, 1), (21, 1), (24, 10)]),
        );
        ifd.set(ExifTag::GpsAltitudeRef.tag_id(), Value::Byte(vec![0]));
        ifd.set(
            ExifTag::GpsAltitude.tag_id(),
            Value::Rational(vec![(35, 1)]),
        );
        ifd
    }

    #[test]
    fn parses_coordinates_and_altitude() {
        let gps = GpsInfo::from_ifd(&sample_ifd()).expect("gps present");
        assert_eq!(gps.latitude.unwrap().reference, LatitudeRef::North);
        assert_eq!(gps.longitude.unwrap().reference, LongitudeRef::East);
        assert!((gps.latitude_deg().unwrap() - 48.858_222).abs() < 1e-5);
        assert!((gps.longitude_deg().unwrap() - 2.350_666).abs() < 1e-5);
        assert_eq!(gps.altitude_m(), Some(35.0));
    }

    #[test]
    fn southern_and_below_sea_level_are_negative() {
        let mut ifd = sample_ifd();
        ifd.set(ExifTag::GpsLatitudeRef.tag_id(), Value::Ascii("S".into()));
        ifd.set(ExifTag::GpsAltitudeRef.tag_id(), Value::Byte(vec![1]));
        let gps = GpsInfo::from_ifd(&ifd).expect("gps");
        assert!(gps.latitude_deg().unwrap() < 0.0);
        assert_eq!(gps.altitude_m(), Some(-35.0));
    }

    #[test]
    fn western_longitude_is_negative() {
        // The sibling test above covers South, so the *latitude* axis was pinned; longitude is
        // East in every fixture, so `'W' | 'w' => West` in `LongitudeRef::parse` had no test at
        // all -- deleting that arm entirely left the suite green (#110). Without it a western
        // longitude parses as `None` and the coordinate silently disappears, which is every
        // photograph taken in the Americas.
        let mut ifd = sample_ifd();
        ifd.set(ExifTag::GpsLongitudeRef.tag_id(), Value::Ascii("W".into()));
        let gps = GpsInfo::from_ifd(&ifd).expect("gps");

        assert_eq!(gps.longitude.unwrap().reference, LongitudeRef::West);
        assert!(
            gps.longitude_deg().unwrap() < 0.0,
            "a western longitude is negative"
        );
        // The magnitude is unchanged -- only the sign flips.
        assert!((gps.longitude_deg().unwrap() + 2.350_666).abs() < 1e-5);
    }

    #[test]
    fn lowercase_references_parse_too() {
        // `parse` accepts either case on all four axes. Only the uppercase arms were exercised.
        let mut ifd = sample_ifd();
        ifd.set(ExifTag::GpsLatitudeRef.tag_id(), Value::Ascii("s".into()));
        ifd.set(ExifTag::GpsLongitudeRef.tag_id(), Value::Ascii("w".into()));
        let gps = GpsInfo::from_ifd(&ifd).expect("gps");

        assert_eq!(gps.latitude.unwrap().reference, LatitudeRef::South);
        assert_eq!(gps.longitude.unwrap().reference, LongitudeRef::West);
    }

    #[test]
    fn a_reference_from_the_other_axis_is_no_reference() {
        // `GPSLatitudeRef` is `N` or `S` and `GPSLongitudeRef` is `E` or `W` (Exif 3.0
        // §4.6.7.1.2, §4.6.7.1.4). A letter from the other axis names no hemisphere of this one,
        // so the coordinate is dropped exactly as an unknown letter is, and the other axis
        // survives.
        for crossed in ["E", "e", "W", "w"] {
            let mut ifd = sample_ifd();
            ifd.set(
                ExifTag::GpsLatitudeRef.tag_id(),
                Value::Ascii(crossed.into()),
            );
            let gps = GpsInfo::from_ifd(&ifd).expect("longitude and altitude survive");
            assert_eq!(gps.latitude, None, "{crossed} is not a latitude reference");
            assert!(gps.longitude.is_some());
        }
        for crossed in ["N", "n", "S", "s"] {
            let mut ifd = sample_ifd();
            ifd.set(
                ExifTag::GpsLongitudeRef.tag_id(),
                Value::Ascii(crossed.into()),
            );
            let gps = GpsInfo::from_ifd(&ifd).expect("latitude and altitude survive");
            assert_eq!(
                gps.longitude, None,
                "{crossed} is not a longitude reference"
            );
            assert!(gps.latitude.is_some());
        }
    }

    #[test]
    fn references_write_back_as_their_letters() {
        // `to_ifd` writes each reference through `as_str`; every hemisphere round-trips.
        for (lat, lon, lat_text, lon_text) in [
            (LatitudeRef::North, LongitudeRef::East, "N", "E"),
            (LatitudeRef::South, LongitudeRef::West, "S", "W"),
        ] {
            let mut gps = GpsInfo::from_ifd(&sample_ifd()).expect("gps");
            gps.latitude.as_mut().unwrap().reference = lat;
            gps.longitude.as_mut().unwrap().reference = lon;
            let ifd = gps.to_ifd();
            assert_eq!(
                ifd.get(ExifTag::GpsLatitudeRef.tag_id()),
                Some(&Value::Ascii(lat_text.into()))
            );
            assert_eq!(
                ifd.get(ExifTag::GpsLongitudeRef.tag_id()),
                Some(&Value::Ascii(lon_text.into()))
            );
        }
    }

    #[test]
    fn a_latitude_alone_is_still_gps_info() {
        // `from_ifd` returns None only when *all three* of latitude, longitude and altitude are
        // absent. Every fixture had either all three or none, so the conjunction was never
        // evaluated with a mixture and `&&` could be `||` unnoticed -- which would discard a
        // partial fix rather than reporting what it has.
        let mut ifd = Ifd::new();
        ifd.set(ExifTag::GpsLatitudeRef.tag_id(), Value::Ascii("N".into()));
        ifd.set(
            ExifTag::GpsLatitude.tag_id(),
            Value::Rational(vec![(48, 1), (51, 1), (296, 10)]),
        );

        let gps = GpsInfo::from_ifd(&ifd).expect("a latitude alone is a GPS fix worth reporting");
        assert!(gps.latitude_deg().is_some());
        assert!(gps.longitude_deg().is_none());
        assert_eq!(gps.altitude_m(), None);
    }

    #[test]
    fn an_altitude_alone_is_still_gps_info() {
        // The other end of the same conjunction: altitude present, both coordinates absent.
        let mut ifd = Ifd::new();
        ifd.set(ExifTag::GpsAltitudeRef.tag_id(), Value::Byte(vec![0]));
        ifd.set(
            ExifTag::GpsAltitude.tag_id(),
            Value::Rational(vec![(35, 1)]),
        );

        let gps = GpsInfo::from_ifd(&ifd).expect("an altitude alone is still a reading");
        assert_eq!(gps.altitude_m(), Some(35.0));
        assert!(gps.latitude_deg().is_none());
        assert!(gps.longitude_deg().is_none());
    }

    #[test]
    fn a_truncated_dms_array_is_refused() {
        // `coordinate` guards on `r.len() >= 3` before indexing `dms[0..=2]`. Nothing tested a
        // shorter array, so the guard could be `true` unnoticed (#110) -- and then the very next
        // lines index out of bounds and PANIC on a malformed file. This is a decode path fed
        // untrusted input, so refusing is the contract and panicking is not.
        for short in [vec![(48, 1)], vec![(48, 1), (51, 1)], vec![]] {
            let mut ifd = Ifd::new();
            ifd.set(ExifTag::GpsLatitudeRef.tag_id(), Value::Ascii("N".into()));
            ifd.set(
                ExifTag::GpsLatitude.tag_id(),
                Value::Rational(short.clone()),
            );

            assert_eq!(
                GpsInfo::from_ifd(&ifd),
                None,
                "a {}-element DMS array is not a coordinate",
                short.len()
            );
        }

        // And exactly three is accepted, so the guard is not simply refusing everything.
        let mut ifd = Ifd::new();
        ifd.set(ExifTag::GpsLatitudeRef.tag_id(), Value::Ascii("N".into()));
        ifd.set(
            ExifTag::GpsLatitude.tag_id(),
            Value::Rational(vec![(48, 1), (51, 1), (296, 10)]),
        );
        assert!(GpsInfo::from_ifd(&ifd).is_some());
    }

    #[test]
    fn empty_ifd_has_no_gps_info() {
        assert_eq!(GpsInfo::from_ifd(&Ifd::new()), None);
    }

    #[test]
    fn to_ifd_round_trips_the_typed_view() {
        let original = GpsInfo::from_ifd(&sample_ifd()).expect("gps");
        let rebuilt = GpsInfo::from_ifd(&original.to_ifd()).expect("rebuilt gps");
        assert_eq!(rebuilt, original);
        // The rebuilt directory carries the mandatory version tag.
        assert_eq!(
            original.to_ifd().get(ExifTag::GpsVersionId.tag_id()),
            Some(&Value::Byte(vec![2, 3, 0, 0]))
        );
    }

    #[test]
    fn zero_denominator_yields_no_degrees() {
        let zero = Rational { num: 1, den: 0 };
        let one = Rational { num: 0, den: 1 };
        // A zero denominator in any of the three components leaves no degrees, on either axis.
        for [degrees, minutes, seconds] in [[zero, one, one], [one, zero, one], [one, one, zero]] {
            let latitude = GpsLatitude {
                degrees,
                minutes,
                seconds,
                reference: LatitudeRef::North,
            };
            assert_eq!(latitude.to_degrees(), None);
            let longitude = GpsLongitude {
                degrees,
                minutes,
                seconds,
                reference: LongitudeRef::East,
            };
            assert_eq!(longitude.to_degrees(), None);
        }
    }

    #[cfg(feature = "geocoordinates")]
    mod geocoordinates_conversion {
        use geocoordinates::{Coordinate, Crs, Height, Wgs84};

        use super::*;

        fn whole(num: u32) -> Rational {
            Rational { num, den: 1 }
        }

        fn lat(degrees: u32, minutes: u32, seconds: u32, reference: LatitudeRef) -> GpsLatitude {
            GpsLatitude {
                degrees: whole(degrees),
                minutes: whole(minutes),
                seconds: whole(seconds),
                reference,
            }
        }

        fn lon(degrees: u32, minutes: u32, seconds: u32, reference: LongitudeRef) -> GpsLongitude {
            GpsLongitude {
                degrees: whole(degrees),
                minutes: whole(minutes),
                seconds: whole(seconds),
                reference,
            }
        }

        fn position() -> GpsInfo {
            GpsInfo {
                latitude: Some(lat(48, 51, 30, LatitudeRef::North)),
                longitude: Some(lon(2, 21, 2, LongitudeRef::East)),
                altitude: Some(GpsAltitude {
                    meters: Rational { num: 35, den: 1 },
                    below_sea_level: false,
                }),
            }
        }

        #[test]
        fn wgs84_conversion_uses_axis_signs_and_drops_altitude() {
            let north_east = Wgs84::try_from(position()).unwrap();
            assert!((north_east.lat - 48.858_333_333_333_334).abs() < 1e-12);
            assert!((north_east.lon - 2.350_555_555_555_555_4).abs() < 1e-12);

            let mut south_west = position();
            south_west.latitude.as_mut().unwrap().reference = LatitudeRef::South;
            south_west.longitude.as_mut().unwrap().reference = LongitudeRef::West;
            let converted = Wgs84::try_from(south_west).unwrap();
            assert_eq!(converted.lat, -north_east.lat);
            assert_eq!(converted.lon, -north_east.lon);
        }

        #[test]
        fn coordinate_conversion_preserves_orthometric_altitude() {
            let above = Coordinate::try_from(position()).unwrap();
            assert_eq!(above.crs, Crs::Wgs84);
            assert_eq!(above.height, Some(Height::Orthometric(35.0)));
            above.validate().unwrap();

            let mut below = position();
            below.altitude.as_mut().unwrap().below_sea_level = true;
            assert_eq!(
                Coordinate::try_from(below).unwrap().height,
                Some(Height::Orthometric(-35.0))
            );

            let mut no_altitude = position();
            no_altitude.altitude = None;
            assert_eq!(Coordinate::try_from(no_altitude).unwrap().height, None);
        }

        #[test]
        fn complete_coordinates_are_required() {
            // A reference from the wrong axis is no longer representable (`LatitudeRef` has no
            // East, `LongitudeRef` no North), so only a missing coordinate is left to refuse.
            let mut missing_latitude = position();
            missing_latitude.latitude = None;
            assert_eq!(
                Wgs84::try_from(missing_latitude),
                Err(GpsConversionError::MissingLatitude)
            );

            let mut missing_longitude = position();
            missing_longitude.longitude = None;
            assert_eq!(
                Wgs84::try_from(missing_longitude),
                Err(GpsConversionError::MissingLongitude)
            );
        }

        #[test]
        fn malformed_rationals_and_dms_are_rejected() {
            let mut zero_denominator = position();
            zero_denominator.latitude.as_mut().unwrap().seconds.den = 0;
            assert_eq!(
                Wgs84::try_from(zero_denominator),
                Err(GpsConversionError::ZeroDenominator("latitude"))
            );

            let mut invalid_dms = position();
            invalid_dms.longitude.as_mut().unwrap().minutes.num = 60;
            assert_eq!(
                Wgs84::try_from(invalid_dms),
                Err(GpsConversionError::InvalidDms("longitude"))
            );

            let mut invalid_altitude = position();
            invalid_altitude.altitude.as_mut().unwrap().meters.den = 0;
            assert_eq!(
                Coordinate::try_from(invalid_altitude),
                Err(GpsConversionError::ZeroDenominator("altitude"))
            );
            assert!(
                Wgs84::try_from(invalid_altitude).is_ok(),
                "the 2D conversion deliberately ignores altitude"
            );
        }

        #[test]
        fn angular_boundaries_are_valid_and_excess_is_rejected() {
            let boundary = GpsInfo {
                latitude: Some(lat(90, 0, 0, LatitudeRef::South)),
                longitude: Some(lon(180, 0, 0, LongitudeRef::West)),
                altitude: None,
            };
            assert_eq!(Wgs84::try_from(boundary), Ok(Wgs84::new(-90.0, -180.0)));

            let mut beyond_pole = boundary;
            beyond_pole.latitude = Some(lat(90, 0, 1, LatitudeRef::North));
            assert_eq!(
                Coordinate::try_from(beyond_pole),
                Err(GpsConversionError::OutOfRange)
            );
        }
    }
}
