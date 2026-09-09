//! Prints what c2pa-rs actually does with a gamut-written AVIF, so the assertions in `tests/` are
//! written against observed behaviour rather than against the documentation.
//!
//! Run it with `cargo run --manifest-path tooling/c2pa-oracle/Cargo.toml --example probe`. It is a
//! developer aid, not a check: nothing here fails.

use c2pa::{Builder, BuilderIntent};
use c2pa_oracle::{
    AVIF_MIME, HEIC_MIME, OracleError, Result, declared_store_len, embed, jumbf_superbox_span,
    manifest_builder, read, reserve_then_fill, signing_context, split_composed_box,
};
use gamut_avif::{AvifContainer, AvifEncoder};
use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};

const W: u32 = 34;
const H: u32 = 18;

fn source_rgb() -> Vec<u8> {
    let mut rgb = vec![0u8; (W * H * 3) as usize];
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 3) as usize;
            rgb[i] = ((x * 7 + y * 3) & 0xff) as u8;
            rgb[i + 1] = ((x * x + y) & 0xff) as u8;
            rgb[i + 2] = ((x ^ (y * 5)) & 0xff) as u8;
        }
    }
    rgb
}

fn plain_avif() -> Vec<u8> {
    let rgb = source_rgb();
    AvifEncoder::new()
        .encode_to_vec(
            ImageRef::<Rgb8>::new(
                &rgb,
                Dimensions {
                    width: W,
                    height: H,
                },
            )
            .expect("buffer matches dimensions"),
        )
        .expect("encode")
}

fn main() -> Result<()> {
    let plain = plain_avif();
    println!("plain gamut AVIF: {} bytes", plain.len());

    // --- composed framing ------------------------------------------------------------------
    let mut builder = manifest_builder()?;
    println!("hash_type(avif) = {:?}", builder.hash_type(AVIF_MIME));
    println!(
        "needs_placeholder(avif) = {}",
        builder.needs_placeholder(AVIF_MIME)
    );
    let ph = split_composed_box(builder.placeholder(AVIF_MIME)?)?;
    println!(
        "placeholder: composed {} bytes, store at +{}, store {} bytes, LBox {:?}",
        ph.composed.len(),
        ph.store_offset,
        ph.store().len(),
        declared_store_len(ph.store())
    );
    println!("framing: {:02x?}", &ph.composed[..ph.store_offset]);

    // --- direction 1 -----------------------------------------------------------------------
    let rgb = source_rgb();
    let filled = reserve_then_fill(AVIF_MIME, |len| {
        let (bytes, report) = AvifEncoder::new()
            .with_c2pa_reserved(len)
            .encode_with_report(
                ImageRef::<Rgb8>::new(
                    &rgb,
                    Dimensions {
                        width: W,
                        height: H,
                    },
                )
                .expect("buffer matches dimensions"),
            )
            .map_err(|e| OracleError::Asset(e.to_string()))?;
        let range = report
            .c2pa
            .ok_or_else(|| OracleError::Asset("no c2pa range reported".into()))?;
        Ok((bytes, range))
    })?;
    println!(
        "direction 1: asset {} bytes, slot {:?}, store {} bytes (placeholder asked {}), LBox {:?}",
        filled.asset.len(),
        filled.slot,
        filled.store.len(),
        filled.placeholder_store_len,
        declared_store_len(&filled.store),
    );
    println!(
        "direction 1 validation: {:?}",
        read(AVIF_MIME, &filled.asset)
    );

    // --- direction 2 -----------------------------------------------------------------------
    let signed = embed(AVIF_MIME, &plain)?;
    println!("direction 2: signed {} bytes", signed.len());
    println!("direction 2 validation: {:?}", read(AVIF_MIME, &signed));
    let span = jumbf_superbox_span(&signed)?;
    println!("direction 2: independent JUMBF span {span:?}");

    match AvifContainer::parse(&signed) {
        Ok(container) => match container.c2pa() {
            Some(slot) => println!(
                "gamut-avif slot: range {:?}, {} bytes, purpose {:?}, LBox {:?}, tail zeros {}",
                slot.range,
                slot.slot_bytes.len(),
                slot.purpose,
                declared_store_len(slot.slot_bytes),
                slot.slot_bytes[span.len().min(slot.slot_bytes.len())..]
                    .iter()
                    .all(|b| *b == 0),
            ),
            None => println!("gamut-avif slot: none"),
        },
        Err(error) => println!("gamut-avif parse failed: {error}"),
    }

    match gamut_heic::HeifContainer::parse(&signed) {
        Ok(container) => match container.c2pa() {
            Some(store) => println!(
                "gamut-heic store: range {:?}, {} bytes, purpose {:?}",
                store.range,
                store.bytes.len(),
                store.purpose
            ),
            None => println!("gamut-heic store: none"),
        },
        Err(error) => println!("gamut-heic parse failed: {error}"),
    }
    println!(
        "heic-mime read of the same bytes: {:?}",
        read(HEIC_MIME, &signed)
    );

    // --- unsigned derivative ----------------------------------------------------------------
    println!("plain asset read: {:?}", read(AVIF_MIME, &plain));

    // --- update manifest (decision 4) -------------------------------------------------------
    let mut update = Builder::from_context(signing_context()?);
    update.set_intent(BuilderIntent::Update);
    let mut source = std::io::Cursor::new(signed.clone());
    let mut dest = std::io::Cursor::new(Vec::new());
    match update.save_to_stream(AVIF_MIME, &mut source, &mut dest) {
        Ok(_) => {
            let updated = dest.into_inner();
            println!("update: {} bytes", updated.len());
            match AvifContainer::parse(&updated) {
                Ok(container) => {
                    for slot in container.c2pa_manifest_stores() {
                        println!(
                            "  update slot: range {:?}, purpose {:?}, LBox {:?}",
                            slot.range,
                            slot.purpose,
                            declared_store_len(slot.slot_bytes)
                        );
                    }
                }
                Err(error) => println!("  parse failed: {error}"),
            }
            println!("  validation: {:?}", read(AVIF_MIME, &updated));
        }
        Err(error) => println!("update refused: {error}"),
    }

    Ok(())
}
