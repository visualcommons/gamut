//! IPTC codec and reconciliation throughput benchmarks (issue #149).
//!
//! Intentionally tight: the IIM dataset codec and the Photoshop `8BIM` carrier over a realistic
//! block spanning every mapped Core field (UTF-8 with the 1:90 escape, repeated keywords and
//! creators, a long caption), plus the keystone merge of that block against a conflicting XMP
//! view. Counters report serialized bytes per second. Run with `cargo bench -p gamut-iptc`.
//!
//! No competitive baseline, deliberately: there is no comparable pure-Rust IPTC IIM+XMP library,
//! and benchmarking exiv2 through the oracle FFI would measure C++ marshalling, not the codec.

use divan::counter::BytesCount;
use divan::{Bencher, black_box};
use gamut_iptc::{IimBlock, IptcReader, IptcWriter, PhotoMetadata, PhotoshopIrb, Sources};

fn main() {
    divan::main();
}

/// A fully-populated Core view: every typed accessor set, with multi-valued keywords/creators and
/// a caption long enough to exercise the length paths.
fn sample_view(city: &str) -> PhotoMetadata {
    let mut pm = PhotoMetadata::new();
    pm.set_headline("Benchmark headline: a fairly typical length for a wire photo");
    pm.set_city(city);
    pm.set_country("France");
    pm.set_country_code("FRA");
    pm.set_caption(&"A descriptive caption sentence, repeated to a realistic length. ".repeat(8));
    pm.set_copyright_notice("© 2026 Agence gamut — tous droits réservés");
    pm.set_title("Sunset over the Seine");
    pm.set_usage_terms("Editorial use only");
    pm.set_keywords(&["sky", "sea", "river", "sunset", "Paris", "évènement"]);
    pm.set_creators(&["Ansel Adams", "Dorothea Lange"]);
    pm.set_intellectual_genre("Documentary");
    pm.set_instructions("Embargoed until Friday");
    pm.set_date_created("2026-06-15T12:00:00+02:00");
    pm.set_authors_position("Staff Photographer");
    pm.set_sublocation("Rive Gauche");
    pm.set_state("Île-de-France");
    pm.set_transmission_reference("JOB-42");
    pm.set_credit("Agence gamut");
    pm.set_source("gamut wire");
    pm.set_caption_writer("Ed");
    pm.set_subject_codes(&["15054000", "15073031"]);
    pm.set_scene_codes(&["011900"]);
    pm.set_alt_text_accessibility("A sunset over the Seine");
    pm.set_extended_description_accessibility("A long red sunset over the Seine, from a bank");
    pm
}

/// The sample view projected to IIM (UTF-8, so the 1:90 escape and the multi-byte text paths are
/// exercised).
fn sample_block() -> IimBlock {
    IptcWriter::new()
        .to_iim_block(&sample_view("Paris"))
        .expect("sample view projects to IIM")
}

#[divan::bench]
fn iim_encode(bencher: Bencher) {
    let block = sample_block();
    bencher
        .counter(BytesCount::new(block.encode().unwrap().len()))
        .bench_local(|| black_box(&block).encode().unwrap());
}

#[divan::bench]
fn iim_parse(bencher: Bencher) {
    let bytes = sample_block().encode().unwrap();
    bencher
        .counter(BytesCount::new(bytes.len()))
        .bench_local(|| IimBlock::parse(black_box(&bytes)).unwrap());
}

#[divan::bench]
fn irb_encode(bencher: Bencher) {
    let view = sample_view("Paris");
    let writer = IptcWriter::new();
    let len = writer.write_irb(&view).unwrap().unwrap().len();
    bencher
        .counter(BytesCount::new(len))
        .bench_local(|| writer.write_irb(black_box(&view)).unwrap().unwrap());
}

#[divan::bench]
fn irb_parse(bencher: Bencher) {
    let bytes = IptcWriter::new()
        .write_irb(&sample_view("Paris"))
        .unwrap()
        .unwrap();
    bencher
        .counter(BytesCount::new(bytes.len()))
        .bench_local(|| PhotoshopIrb::parse(black_box(&bytes)).unwrap());
}

#[divan::bench]
fn reconcile_merge(bencher: Bencher) {
    // The keystone path: charset detection plus the 20-row map walk, with every field present in
    // both carriers and disagreeing, so the policy comparison runs for each row.
    let iim = sample_block();
    let xmp = sample_view("Lyon").to_xmp();
    let reader = IptcReader::new();
    bencher
        .counter(BytesCount::new(iim.encode().unwrap().len()))
        .bench_local(|| {
            reader
                .read(Sources {
                    iim: Some(black_box(&iim)),
                    xmp: Some(black_box(&xmp)),
                })
                .unwrap()
        });
}
