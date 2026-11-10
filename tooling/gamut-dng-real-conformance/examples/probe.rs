//! Reports what `gamut-dng` currently makes of each DNG passed on the command line.
//!
//! This is the tool used to derive a corpus file's `MANIFEST.toml` expectations from measured
//! behaviour rather than guesswork, and to triage a real file that fails. It asserts nothing.
//!
//! ```sh
//! cargo run --manifest-path tooling/gamut-dng-real-conformance/Cargo.toml \
//!     --example probe -- third_party/gamut-dng-samples/apple/iphone-12-pro/IMG_1361.DNG
//! ```

use gamut_dng::{
    ColorProfileInfo, DngDecoder, DngRewrite, HsvTable, NoiseProfile, SubImageData, deconstruct,
};

fn main() {
    for path in std::env::args().skip(1) {
        println!("{}", "=".repeat(78));
        println!("{path}");
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) => {
                println!("  read: {e}");
                continue;
            }
        };
        println!("  {} bytes", data.len());
        report_structure(&data);
        report_decode(&data);
        report_rewrite(&data);
    }
}

/// Byte-classification and unknown-field inventory.
fn report_structure(data: &[u8]) {
    match deconstruct(data) {
        Ok(report) => {
            println!(
                "  deconstruct   : classified={} unclassified={}B/{} ranges unclaimed={} unread={} conflicts={} oob={}",
                report.segments.is_fully_classified(),
                report.segments.unclassified_bytes(),
                report.segments.unclassified.len(),
                report.segments.unclaimed_reads.len(),
                report.segments.unread_claims.len(),
                report.segments.conflicts.len(),
                report.segments.out_of_bounds.len(),
            );
            println!(
                "  unknown       : tags={} field_types={} anomalies={}",
                report.unknown_tags.len(),
                report.unknown_fields.len(),
                report.anomalies.len(),
            );
            for a in report.anomalies.iter().take(8) {
                println!("      anomaly: {a:?}");
            }
            for u in report.unknown_fields.iter().take(8) {
                println!("      unknown field type: {u:?}");
            }
        }
        Err(e) => println!("  deconstruct   : ERROR {:?} {e}", e.kind()),
    }
}

/// Full decode, or the typed error that stops it.
fn report_decode(data: &[u8]) {
    let decoded = match DngDecoder::new().decode(data) {
        Ok(d) => d,
        Err(e) => {
            println!(
                "  decode        : ERROR kind={:?} msg={:?} origin={:?} detail={:?}",
                e.kind(),
                e.static_message(),
                e.origin(),
                e.detail()
            );
            return;
        }
    };
    let raw = &decoded.raw;
    println!(
        "  decode        : OK {}x{} bits={} spp={} photometry={:?}",
        raw.dimensions().width(),
        raw.dimensions().height(),
        raw.bits_per_sample(),
        raw.samples_per_pixel(),
        raw.photometry(),
    );
    println!(
        "  version       : {:?} backward={:?} digest_tag={}",
        decoded.dng_version,
        decoded.backward_version,
        decoded.new_raw_image_digest.is_some(),
    );
    match &decoded.profile {
        Some(profile) => println!(
            "  profile       : model={:?} as_shot_neutral={:?}",
            profile.unique_camera_model(),
            profile.as_shot_neutral(),
        ),
        None => println!("  profile       : none (no colour calibration in the file)"),
    }
    report_color_profile(decoded.color_profile.as_ref());
    report_noise_profile(decoded.noise_profile.as_ref());
    // `exif` counts the EXIF sub-IFD's own entries, not an extras list: the whole directory is
    // carried inside `DngMetadata::exif`, so there is no unmodelled EXIF residue to count.
    println!(
        "  extras        : ifd0={} raw={} exif={} gain_map={} gain_map2={}",
        decoded.ifd0_extra.len(),
        decoded.raw_extra.len(),
        decoded
            .metadata
            .exif
            .as_ref()
            .map_or(0, |ifd| ifd.fields().len()),
        decoded.gain_table_map.is_some(),
        decoded.gain_table_map2.is_some(),
    );
    let raw_tags: Vec<u16> = decoded.raw_extra.iter().map(|t| t.tag).collect();
    println!("      raw_extra tags: {raw_tags:?}");
    println!("  sub_images    : {}", decoded.sub_images.len());
    for s in &decoded.sub_images {
        let payload = match &s.data {
            SubImageData::Undecoded {
                compression,
                chunks,
            } => format!(
                "Undecoded(compression={compression}, {} chunks)",
                chunks.len()
            ),
            SubImageData::Decoded(v) => format!("Decoded({} samples)", v.len()),
            _ => "Other".to_string(),
        };
        println!(
            "      kind={:?} {}x{} photometric={} bits={} spp={} -> {payload}",
            s.kind,
            s.dimensions.width(),
            s.dimensions.height(),
            s.photometric,
            s.bits_per_sample,
            s.samples_per_pixel,
        );
    }
    match raw.to_linear() {
        Ok(l) => println!(
            "  to_linear     : OK {}x{} spp={}",
            l.width, l.height, l.samples_per_pixel
        ),
        Err(e) => println!(
            "  to_linear     : ERROR {:?} {:?}",
            e.kind(),
            e.static_message()
        ),
    }
}

/// The camera-profile colour tags `CameraProfile` does not model: the rendering tables and curve,
/// the profile exposure offset, the DNG 1.6 third calibration set and the reduction matrices.
///
/// These reach a caller typed rather than as `ifd0_extra` residue, so a file's colour rendering is
/// visible here without reading raw tag numbers back by hand.
fn report_color_profile(info: Option<&ColorProfileInfo>) {
    let Some(info) = info else {
        println!("  color_profile : none (no rendering tables, curve or third calibration set)");
        return;
    };
    println!(
        "  color_profile : hue_sat=[{}, {}, {}] look={} tone_curve={} exposure_offset={:?}",
        hsv_table(info.hue_sat_map1.as_ref()),
        hsv_table(info.hue_sat_map2.as_ref()),
        hsv_table(info.hue_sat_map3.as_ref()),
        hsv_table(info.look_table.as_ref()),
        info.tone_curve
            .as_ref()
            .map_or_else(|| "-".to_string(), |c| format!("{} pts", c.len())),
        info.baseline_exposure_offset,
    );
    println!(
        "      third calibration set: matrix={} illuminant={:?} calibration={} forward={}",
        info.color_matrix3.is_some(),
        info.calibration_illuminant3,
        info.camera_calibration3.is_some(),
        info.forward_matrix3.is_some(),
    );
    println!(
        "      reduction matrices: 1={} 2={} 3={}",
        matrix_terms(info.reduction_matrix1.as_deref()),
        matrix_terms(info.reduction_matrix2.as_deref()),
        matrix_terms(info.reduction_matrix3.as_deref()),
    );
}

/// One hue/saturation/value table's divisions and encoding, or `-` when the file carries none.
fn hsv_table(table: Option<&HsvTable>) -> String {
    table.map_or_else(
        || "-".to_string(),
        |t| {
            format!(
                "{}x{}x{} {:?}",
                t.hue_divisions, t.saturation_divisions, t.value_divisions, t.encoding
            )
        },
    )
}

/// A reduction matrix's term count, or `-` when absent. Its shape is `3 x ColorPlanes`, so the
/// count is three times the number of colour planes it reduces from.
fn matrix_terms(matrix: Option<&[f64]>) -> String {
    matrix.map_or_else(|| "-".to_string(), |m| format!("{} terms", m.len()))
}

/// The sensor's noise model, read from the raw IFD (the spec's home for it) with an IFD 0 fallback.
fn report_noise_profile(profile: Option<&NoiseProfile>) {
    let Some(profile) = profile else {
        println!("  noise_profile : none");
        return;
    };
    let models: Vec<String> = profile
        .planes
        .iter()
        .map(|m| format!("scale={:.3e} offset={:.3e}", m.scale, m.offset))
        .collect();
    println!(
        "  noise_profile : planes={} [{}]",
        profile.planes.len(),
        models.join(", "),
    );
}

/// The preserving rewrite round-trip.
fn report_rewrite(data: &[u8]) {
    let opened = match DngRewrite::open(data) {
        Ok(r) => r,
        Err(e) => {
            println!(
                "  rewrite open  : ERROR {:?} {:?}",
                e.kind(),
                e.static_message()
            );
            return;
        }
    };
    match opened.write() {
        Ok(out) => println!(
            "  rewrite       : OK {} bytes, maker_note={:?}, classified={:?}",
            out.bytes.len(),
            out.maker_note,
            deconstruct(&out.bytes).map(|r| r.segments.is_fully_classified()),
        ),
        Err(e) => println!(
            "  rewrite write : ERROR {:?} {:?}",
            e.kind(),
            e.static_message()
        ),
    }
}
