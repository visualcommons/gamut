//! The egui front end: one tab per scenario or primitive panel, every one of them a thin view
//! over a [`gamut_icc_sandbox::panels`] function.
//!
//! Rendered images are cached as textures keyed on everything that produced them, so a frame
//! that changes nothing recomputes nothing.

use std::collections::HashMap;

use eframe::egui::{self, Color32, ComboBox, TextureHandle, TextureOptions};
use egui_plot::{Legend, Line, Plot, Points, Polygon};
use gamut_cmm::{ClutInterpolation, PipelineOptimization};
use gamut_icc::RenderingIntent;
use gamut_icc_sandbox::formats::{self, Container};
use gamut_icc_sandbox::panels::{self, Display, INTENTS, Settings};
use gamut_icc_sandbox::profiles::{Battery, ProfileKind};
use gamut_icc_sandbox::scene;

/// The tabs, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Formats,
    Camera,
    Print,
    Inspector,
    Curves,
    Clut,
    Hull,
    Malformed,
    Conformance,
}

impl Tab {
    const ALL: [(Tab, &'static str); 9] = [
        (Tab::Formats, "Formats"),
        (Tab::Camera, "Camera"),
        (Tab::Print, "Print"),
        (Tab::Inspector, "Tag inspector"),
        (Tab::Curves, "TRC curves"),
        (Tab::Clut, "CLUT viewer"),
        (Tab::Hull, "Gamut hull"),
        (Tab::Malformed, "Malformed profile"),
        (Tab::Conformance, "Conformance"),
    ];
}

/// The optimization levels offered, with display names.
const OPTIMIZATIONS: [(PipelineOptimization, &str); 5] = [
    (PipelineOptimization::None, "None (stage by stage)"),
    (PipelineOptimization::Collapse, "Collapse"),
    (PipelineOptimization::Precalculate, "Precalculate"),
    (PipelineOptimization::PrecalculateF32, "Precalculate (f32)"),
    (
        PipelineOptimization::PrecalculateFixed16,
        "Precalculate (16-bit fixed)",
    ),
];

/// The conformance cross-check's state.
enum Badge {
    /// Built without the `oracle` feature (always the case on wasm).
    #[cfg_attr(feature = "oracle", allow(dead_code))]
    Unavailable,
    /// The report, or why it could not run.
    #[cfg(feature = "oracle")]
    Ran(Result<gamut_icc_sandbox::conformance::Report, String>),
}

/// The application state.
pub struct SandboxApp {
    battery: Battery,
    display: Display,
    tab: Tab,
    settings: Settings,
    proofing_intent: RenderingIntent,
    print_source: ProfileKind,
    inspect_kind: ProfileKind,
    curve_kind: ProfileKind,
    clut_kind: ProfileKind,
    clut_tag: usize,
    clut_interpolation: ClutInterpolation,
    clut_axes: [usize; 2],
    clut_fixed: [f64; 4],
    hull_kinds: Vec<(ProfileKind, bool)>,
    textures: HashMap<String, Result<TextureHandle, String>>,
    badge: Badge,
}

impl SandboxApp {
    /// Builds the battery, the display link and (with `oracle`) the conformance report.
    ///
    /// # Errors
    ///
    /// The battery or display link failing to build — a defect in the sandbox, reported rather
    /// than panicked on.
    pub fn new() -> Result<SandboxApp, String> {
        let battery = Battery::build().map_err(|e| e.to_string())?;
        let display = Display::new().map_err(|e| e.to_string())?;
        #[cfg(feature = "oracle")]
        let badge = Badge::Ran(gamut_icc_sandbox::conformance::run(&battery));
        #[cfg(not(feature = "oracle"))]
        let badge = Badge::Unavailable;
        Ok(SandboxApp {
            battery,
            display,
            tab: Tab::Formats,
            settings: Settings::default(),
            proofing_intent: RenderingIntent::MediaRelativeColorimetric,
            print_source: ProfileKind::DisplayP3,
            inspect_kind: ProfileKind::SrgbV2,
            curve_kind: ProfileKind::SrgbV2,
            clut_kind: ProfileKind::Printer,
            clut_tag: 0,
            clut_interpolation: ClutInterpolation::Tetrahedral,
            clut_axes: [0, 1],
            clut_fixed: [0.0; 4],
            hull_kinds: vec![
                (ProfileKind::SrgbV4, true),
                (ProfileKind::DisplayP3, true),
                (ProfileKind::CameraA, false),
                (ProfileKind::CameraB, false),
                (ProfileKind::Printer, true),
            ],
            textures: HashMap::new(),
            badge,
        })
    }

    /// The top bar: tabs, the shared rendering knobs, and the conformance badge.
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            for (tab, name) in Tab::ALL {
                ui.selectable_value(&mut self.tab, tab, name);
            }
        });
        ui.horizontal_wrapped(|ui| {
            intent_combo(ui, "intent", "Rendering intent", &mut self.settings.intent);
            ui.checkbox(&mut self.settings.bpc, "Black-point compensation");
            ComboBox::from_id_salt("optimization")
                .selected_text(
                    OPTIMIZATIONS
                        .iter()
                        .find(|(o, _)| *o == self.settings.optimization)
                        .map_or("", |(_, n)| n),
                )
                .show_ui(ui, |ui| {
                    for (o, name) in OPTIMIZATIONS {
                        ui.selectable_value(&mut self.settings.optimization, o, name);
                    }
                });
            ui.separator();
            self.badge_label(ui);
        });
    }

    /// The max-ΔE₀₀ badge.
    fn badge_label(&self, ui: &mut egui::Ui) {
        match &self.badge {
            Badge::Unavailable => {
                ui.label("lcms2 conformance: native `oracle` builds only");
            }
            #[cfg(feature = "oracle")]
            Badge::Ran(Ok(report)) => {
                let bound = gamut_icc_sandbox::conformance::BOUND;
                let (color, verdict) = if report.passes() {
                    (Color32::from_rgb(40, 160, 60), "pass")
                } else {
                    (Color32::from_rgb(200, 50, 50), "FAIL")
                };
                ui.colored_label(
                    color,
                    format!(
                        "vs lcms2: max ΔE₀₀ {:.2e} (bound {bound:.0e}) — {verdict}",
                        report.max_de00()
                    ),
                );
            }
            #[cfg(feature = "oracle")]
            Badge::Ran(Err(e)) => {
                ui.colored_label(Color32::from_rgb(200, 50, 50), format!("conformance: {e}"));
            }
        }
    }

    fn formats_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Same pixels, different embedded profiles");
        ui.label(
            "The chart is encoded with each profile attached, decoded back by the same crate, \
             the profile extracted from the container and parsed, and the pixels converted to \
             the display's sRGB by gamut-cmm.",
        );
        let settings = self.settings;
        for container in Container::ALL {
            ui.separator();
            ui.strong(container.name());
            ui.horizontal(|ui| {
                for kind in [ProfileKind::SrgbV2, ProfileKind::DisplayP3] {
                    let icc = &self.battery.get(kind).bytes;
                    let display = &self.display;
                    show_image(
                        &mut self.textures,
                        ui,
                        &format!("tagged {} ({} profile bytes)", kind.name(), icc.len()),
                        format!("formats {container:?} {kind:?} {settings:?}"),
                        || {
                            let pixels = scene::chart_rgb8();
                            let file = formats::encode(
                                container,
                                &pixels,
                                scene::WIDTH,
                                scene::HEIGHT,
                                icc,
                            )
                            .map_err(|e| e.to_string())?;
                            let decoded =
                                formats::decode(container, &file).map_err(|e| e.to_string())?;
                            let embedded = decoded.icc.ok_or("no embedded profile")?;
                            if embedded != *icc {
                                return Err("the extracted profile differs from the embedded one"
                                    .to_owned());
                            }
                            let profile = gamut_icc::IccProfile::parse(&embedded)
                                .map_err(|e| e.to_string())?;
                            let out =
                                panels::to_display(&profile, display, &decoded.rgb8, settings)
                                    .map_err(|e| e.to_string())?;
                            Ok((out, size(decoded.width, decoded.height)))
                        },
                    );
                }
            });
        }
    }

    fn camera_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("One raw capture, two camera profiles");
        ui.label(
            "The 16-bit linear raw values camera A's sensor records for the chart, interpreted \
             through camera A's own matrix/TRC profile and through camera B's CLUT profile.",
        );
        let settings = self.settings;
        ui.horizontal(|ui| {
            for kind in [ProfileKind::CameraA, ProfileKind::CameraB] {
                let display = &self.display;
                let profile = self.battery.profile(kind);
                show_image(
                    &mut self.textures,
                    ui,
                    kind.name(),
                    format!("camera {kind:?} {settings:?}"),
                    || {
                        let profile = profile.ok_or("profile did not parse")?;
                        panels::raw_to_display(profile, display, &scene::raw_rgb16(), settings)
                            .map(|out| (out, size(scene::WIDTH, scene::HEIGHT)))
                            .map_err(|e| e.to_string())
                    },
                );
            }
        });
        ui.separator();
        ui.strong("ΔE₀₀ to the scene, per patch (exposure undone, media-relative)");
        let errors: Vec<_> = [ProfileKind::CameraA, ProfileKind::CameraB]
            .iter()
            .map(|&k| {
                self.battery
                    .profile(k)
                    .map(|p| panels::camera_patch_errors(p).map_err(|e| e.to_string()))
            })
            .collect();
        egui::Grid::new("camera errors")
            .striped(true)
            .show(ui, |ui| {
                ui.label("patch");
                ui.label("camera A");
                ui.label("camera B");
                ui.end_row();
                for i in 0..scene::PATCHES.len() {
                    ui.label(format!("{}", i + 1));
                    for e in &errors {
                        match e {
                            Some(Ok(v)) => ui.label(format!("{:.2}", v[i])),
                            Some(Err(err)) => ui.label(err.as_str()),
                            None => ui.label("—"),
                        };
                    }
                    ui.end_row();
                }
            });
    }

    fn print_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("CMYK print: separation, soft proof, gamut warning");
        ui.horizontal(|ui| {
            ComboBox::from_id_salt("print source")
                .selected_text(self.print_source.name())
                .show_ui(ui, |ui| {
                    for kind in [ProfileKind::SrgbV2, ProfileKind::DisplayP3] {
                        ui.selectable_value(&mut self.print_source, kind, kind.name());
                    }
                });
            intent_combo(
                ui,
                "proofing intent",
                "Proof → display intent",
                &mut self.proofing_intent,
            );
        });
        let chart = size(scene::WIDTH, scene::HEIGHT);
        let settings = self.settings;
        let proofing = self.proofing_intent;
        let source = self.print_source;
        let (Some(src), Some(printer)) = (
            self.battery.profile(source),
            self.battery.profile(ProfileKind::Printer),
        ) else {
            ui.label("battery profile did not parse");
            return;
        };
        let display = &self.display;
        let textures = &mut self.textures;
        let key = format!("{source:?} {settings:?} {proofing:?}");
        ui.horizontal(|ui| {
            show_image(
                textures,
                ui,
                "Source on the display",
                format!("print src {key}"),
                || {
                    panels::to_display(src, display, &scene::chart_rgb8(), settings)
                        .map(|o| (o, chart))
                        .map_err(|e| e.to_string())
                },
            );
            show_image(
                textures,
                ui,
                "Soft proof",
                format!("print proof {key}"),
                || {
                    let pixels = scene::chart_rgb8();
                    panels::proof_to_display(src, printer, display, &pixels, settings, proofing)
                        .map(|o| (o, chart))
                        .map_err(|e| e.to_string())
                },
            );
            show_image(
                textures,
                ui,
                "Out of printer gamut (magenta)",
                format!("print gamut {key}"),
                || {
                    let pixels = scene::chart_rgb8();
                    let mask = panels::gamut_mask(src, printer, &pixels, settings)
                        .map_err(|e| e.to_string())?;
                    let out = pixels
                        .as_chunks::<3>()
                        .0
                        .iter()
                        .zip(mask)
                        .flat_map(|(&p, out)| if out { [255, 0, 255] } else { p })
                        .collect();
                    Ok((out, chart))
                },
            );
        });
        ui.strong("Ink plates");
        ui.horizontal(|ui| {
            for (index, name) in ["Cyan", "Magenta", "Yellow", "Black"]
                .into_iter()
                .enumerate()
            {
                show_image(
                    textures,
                    ui,
                    name,
                    format!("print plate {index} {key}"),
                    || {
                        let cmyk = panels::separate(src, printer, &scene::chart_rgb8(), settings)
                            .map_err(|e| e.to_string())?;
                        let out = cmyk
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .flat_map(|p| [panels::to_u8(1.0 - p[index]); 3])
                            .collect();
                        Ok((out, chart))
                    },
                );
            }
        });
    }

    fn inspector_tab(&mut self, ui: &mut egui::Ui) {
        profile_combo(ui, "inspect", &mut self.inspect_kind, &ProfileKind::ALL);
        let entry = self.battery.get(self.inspect_kind);
        ui.label(format!("{} bytes", entry.bytes.len()));
        let Some(profile) = entry.profile() else {
            ui.label("This profile does not parse: see the Malformed profile tab.");
            return;
        };
        let view = panels::inspect(profile);
        egui::Grid::new("header").striped(true).show(ui, |ui| {
            for (field, value) in &view.header {
                ui.label(*field);
                ui.monospace(value);
                ui.end_row();
            }
        });
        if view.conformance.is_empty() {
            ui.label("validate(): every tag ICC.1:2022 §8 requires for this class is present.");
        }
        for issue in &view.conformance {
            ui.colored_label(Color32::from_rgb(200, 120, 0), issue);
        }
        if let Some(m) = view.colorants {
            ui.strong("Colorant matrix (rXYZ gXYZ bXYZ as columns, PCS D50)");
            matrix_grid(ui, "colorants", &m);
        }
        if let Some(w) = view.white {
            ui.monospace(format!("wtpt  {:.4} {:.4} {:.4}", w[0], w[1], w[2]));
        }
        if let Some(m) = view.chad {
            ui.strong("chad (Bradford adaptation to D50)");
            matrix_grid(ui, "chad", &m);
        }
        ui.strong("Tags");
        egui::Grid::new("tags").striped(true).show(ui, |ui| {
            for row in &view.tags {
                ui.monospace(&row.signature);
                ui.label(row.element);
                ui.label(&row.summary);
                ui.end_row();
            }
        });
    }

    fn curves_tab(&mut self, ui: &mut egui::Ui) {
        profile_combo(
            ui,
            "curves",
            &mut self.curve_kind,
            &[
                ProfileKind::SrgbV2,
                ProfileKind::SrgbV4,
                ProfileKind::DisplayP3,
                ProfileKind::CameraA,
            ],
        );
        let Some(profile) = self.battery.profile(self.curve_kind) else {
            return;
        };
        let curves = match panels::trc_curves(profile) {
            Ok(c) => c,
            Err(e) => {
                ui.label(e.to_string());
                return;
            }
        };
        for c in &curves {
            ui.monospace(format!(
                "{}  {} — monotonic: {}, max |f⁻¹(f(x)) − x|: {}",
                c.tag,
                c.encoding,
                c.monotonic,
                c.round_trip_error
                    .map_or("—".to_owned(), |e| format!("{e:.2e}"))
            ));
        }
        Plot::new("trc")
            .legend(Legend::default())
            .data_aspect(1.0)
            .height(420.0)
            .show(ui, |plot| {
                for c in &curves {
                    plot.line(Line::new(format!("{} f(x)", c.tag), c.forward.clone()));
                    if let Some(inv) = &c.inverse {
                        plot.line(Line::new(format!("{} f⁻¹(y)", c.tag), inv.clone()));
                    }
                }
            });
    }

    fn clut_tab(&mut self, ui: &mut egui::Ui) {
        profile_combo(
            ui,
            "clut",
            &mut self.clut_kind,
            &[ProfileKind::CameraB, ProfileKind::Printer],
        );
        let Some(profile) = self.battery.profile(self.clut_kind).cloned() else {
            return;
        };
        let tags = panels::clut_tags(&profile);
        if tags.is_empty() {
            ui.label("No CLUT in this profile.");
            return;
        }
        self.clut_tag = self.clut_tag.min(tags.len() - 1);
        ui.horizontal(|ui| {
            for (i, (name, _)) in tags.iter().enumerate() {
                ui.selectable_value(&mut self.clut_tag, i, name.as_str());
            }
        });
        let (name, clut) = &tags[self.clut_tag];
        let inputs = clut.grid_points.len();
        ui.horizontal(|ui| {
            ui.radio_value(
                &mut self.clut_interpolation,
                ClutInterpolation::Tetrahedral,
                "Tetrahedral",
            );
            ui.radio_value(
                &mut self.clut_interpolation,
                ClutInterpolation::Multilinear,
                "Multilinear",
            );
        });
        for (slot, label) in self.clut_axes.iter_mut().zip(["x axis", "y axis"]) {
            *slot = (*slot).min(inputs - 1);
            ui.horizontal(|ui| {
                ui.label(label);
                for axis in 0..inputs {
                    ui.radio_value(slot, axis, format!("input {axis}"));
                }
            });
        }
        for axis in 0..inputs {
            if !self.clut_axes.contains(&axis) {
                ui.add(
                    egui::Slider::new(&mut self.clut_fixed[axis], 0.0..=1.0)
                        .text(format!("input {axis}")),
                );
            }
        }
        if self.clut_axes[0] == self.clut_axes[1] {
            ui.label("Pick two different axes.");
            return;
        }
        const SIZE: usize = 128;
        let caption = format!(
            "{name}: grid {:?}, {} outputs, {SIZE}×{SIZE} interpolated",
            clut.grid_points, clut.output_channels
        );
        let key = format!(
            "clut {:?} {} {:?} {:?} {:?}",
            self.clut_kind, self.clut_tag, self.clut_interpolation, self.clut_axes, self.clut_fixed
        );
        let display = &self.display;
        let (interpolation, axes, fixed) =
            (self.clut_interpolation, self.clut_axes, self.clut_fixed);
        show_image(&mut self.textures, ui, &caption, key, || {
            panels::clut_slice(clut, interpolation, axes[0], axes[1], &fixed, SIZE)
                .map(|s| (panels::slice_rgb8(&s, display), [SIZE, SIZE]))
                .map_err(|e| e.to_string())
        });
    }

    fn hull_tab(&mut self, ui: &mut egui::Ui) {
        ui.label(
            "Device-surface samples converted to PCS in bulk, projected onto CIE 1931 xy; \
             round trips go PCS → device → PCS through the same profile.",
        );
        ui.horizontal_wrapped(|ui| {
            for (kind, on) in &mut self.hull_kinds {
                ui.checkbox(on, kind.name());
            }
        });
        let intent = self.settings.intent;
        let hulls: Vec<_> = self
            .hull_kinds
            .iter()
            .filter(|(_, on)| *on)
            .filter_map(|&(kind, _)| {
                let profile = self.battery.profile(kind)?;
                Some((kind, panels::gamut_hull(profile, intent)))
            })
            .collect();
        egui::Grid::new("hull stats").striped(true).show(ui, |ui| {
            ui.label("profile");
            ui.label("round trip max ΔE₀₀");
            ui.label("mean");
            ui.end_row();
            for (kind, hull) in &hulls {
                ui.label(kind.name());
                match hull {
                    Ok(h) => {
                        ui.label(
                            h.round_trip_max
                                .map_or("no PCS → device".to_owned(), |v| format!("{v:.2e}")),
                        );
                        ui.label(
                            h.round_trip_mean
                                .map_or(String::new(), |v| format!("{v:.2e}")),
                        );
                    }
                    Err(e) => {
                        ui.label(e.to_string());
                    }
                }
                ui.end_row();
            }
        });
        Plot::new("hull")
            .legend(Legend::default())
            .data_aspect(1.0)
            .height(480.0)
            .show(ui, |plot| {
                for (kind, hull) in &hulls {
                    if let Ok(h) = hull {
                        plot.polygon(Polygon::new(kind.name(), h.hull.clone()));
                        plot.points(
                            Points::new(format!("{} samples", kind.name()), h.points.clone())
                                .radius(1.0),
                        );
                    }
                }
            });
    }

    fn malformed_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("A truncated profile");
        let bytes = self.battery.get(ProfileKind::Truncated).bytes.clone();
        match panels::diagnose(&bytes) {
            Some(d) => {
                ui.colored_label(
                    Color32::from_rgb(200, 50, 50),
                    format!("Parse error: {}", d.message),
                );
                ui.label(format!(
                    "Header-declared size: {}; bytes present: {}",
                    d.declared_size.map_or("—".to_owned(), |s| s.to_string()),
                    d.actual_size
                ));
                ui.label(d.advice);
            }
            None => {
                ui.label("Unexpectedly parsed.");
            }
        }
        ui.separator();
        ui.label(
            "Embedded in a PNG, the bad profile does not stop the image: the container still \
             decodes, the profile is refused, and the pixels are shown untagged (as sRGB).",
        );
        let settings = self.settings;
        let display = &self.display;
        show_image(
            &mut self.textures,
            ui,
            "PNG with the truncated profile",
            format!("malformed {settings:?}"),
            || {
                let pixels = scene::chart_rgb8();
                let file =
                    formats::encode(Container::Png, &pixels, scene::WIDTH, scene::HEIGHT, &bytes)
                        .map_err(|e| e.to_string())?;
                let decoded = formats::decode(Container::Png, &file).map_err(|e| e.to_string())?;
                let profile = decoded
                    .icc
                    .and_then(|icc| gamut_icc::IccProfile::parse(&icc).ok())
                    .unwrap_or_else(|| display.profile.clone());
                panels::to_display(&profile, display, &decoded.rgb8, settings)
                    .map(|o| (o, size(decoded.width, decoded.height)))
                    .map_err(|e| e.to_string())
            },
        );
    }

    fn conformance_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Cross-check against Little-CMS");
        self.badge_label(ui);
        #[cfg(feature = "oracle")]
        if let Badge::Ran(Ok(report)) = &self.badge {
            ui.label(
                "gamut-cmm (f64, stage by stage) vs lcms2's full-precision path \
                 (TYPE_*_DBL, NOOPTIMIZE | NOCACHE), measured as ΔE₀₀ through the destination's \
                 lcms2 Lab lens.",
            );
            egui::ScrollArea::vertical().show(ui, |ui| {
                egui::Grid::new("conformance").striped(true).show(ui, |ui| {
                    for cell in &report.cells {
                        ui.label(format!("{} → {}", cell.src.name(), cell.dst.name()));
                        ui.label(format!("{:?}", cell.intent));
                        ui.label(if cell.bpc { "BPC" } else { "" });
                        ui.monospace(format!("{:.2e}", cell.max_de00));
                        ui.end_row();
                    }
                });
            });
        }
    }
}

/// Rendered textures, keyed on everything that produced them; an entry is the texture or the
/// error that stopped it.
type Textures = HashMap<String, Result<TextureHandle, String>>;

/// An image size as egui wants it.
fn size(width: u32, height: u32) -> [usize; 2] {
    [width as usize, height as usize]
}

/// Draws the cached image for `key`, doubled, under `caption` — rendering it with `make` (RGB8
/// pixels and their size) only on a cache miss. An error is drawn as text instead.
fn show_image(
    textures: &mut Textures,
    ui: &mut egui::Ui,
    caption: &str,
    key: String,
    make: impl FnOnce() -> Result<(Vec<u8>, [usize; 2]), String>,
) {
    let ctx = ui.ctx().clone();
    let texture = textures
        .entry(key.clone())
        .or_insert_with(|| {
            make().map(|(rgb, size)| {
                ctx.load_texture(
                    key,
                    egui::ColorImage::from_rgb(size, &rgb),
                    TextureOptions::NEAREST,
                )
            })
        })
        .clone();
    ui.vertical(|ui| {
        ui.label(caption);
        match texture {
            Ok(t) => {
                let size = t.size_vec2() * 2.0;
                ui.add(egui::Image::from_texture(&t).fit_to_exact_size(size));
            }
            Err(e) => {
                ui.colored_label(Color32::from_rgb(200, 50, 50), e);
            }
        }
    });
}

/// A rendering-intent selector.
fn intent_combo(ui: &mut egui::Ui, id: &str, label: &str, intent: &mut RenderingIntent) {
    ui.label(label);
    ComboBox::from_id_salt(id)
        .selected_text(
            INTENTS
                .iter()
                .find(|(i, _)| i == intent)
                .map_or("", |(_, n)| n),
        )
        .show_ui(ui, |ui| {
            for (i, name) in INTENTS {
                ui.selectable_value(intent, i, name);
            }
        });
}

/// A battery-profile selector over `kinds`.
fn profile_combo(ui: &mut egui::Ui, id: &str, kind: &mut ProfileKind, kinds: &[ProfileKind]) {
    ComboBox::from_id_salt(id)
        .selected_text(kind.name())
        .show_ui(ui, |ui| {
            for &k in kinds {
                ui.selectable_value(kind, k, k.name());
            }
        });
}

/// A 3×3 matrix as a grid.
fn matrix_grid(ui: &mut egui::Ui, id: &str, m: &[[f64; 3]; 3]) {
    egui::Grid::new(id).show(ui, |ui| {
        for row in m {
            for v in row {
                ui.monospace(format!("{v:9.5}"));
            }
            ui.end_row();
        }
    });
}

impl eframe::App for SandboxApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }
}

impl SandboxApp {
    /// One frame of the whole window — the [`eframe::App::ui`] body, separate so a test can
    /// drive it without an `eframe::Frame`.
    fn draw(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            self.top_bar(ui);
            ui.separator();
            egui::ScrollArea::both().show(ui, |ui| match self.tab {
                Tab::Formats => self.formats_tab(ui),
                Tab::Camera => self.camera_tab(ui),
                Tab::Print => self.print_tab(ui),
                Tab::Inspector => self.inspector_tab(ui),
                Tab::Curves => self.curves_tab(ui),
                Tab::Clut => self.clut_tab(ui),
                Tab::Hull => self.hull_tab(ui),
                Tab::Malformed => self.malformed_tab(ui),
                Tab::Conformance => self.conformance_tab(ui),
            });
        });
    }
}

/// The window size the native build opens at.
#[cfg(not(target_arch = "wasm32"))]
pub const WINDOW: egui::Vec2 = egui::Vec2::new(1100.0, 800.0);

#[cfg(test)]
mod tests {
    use super::*;

    /// Every tab draws headlessly, and every image it asks for renders: no tab's pipeline
    /// fails at its default settings, nor at each intent with BPC on — the acceptance criterion
    /// that no scenario runs with a code path stubbed.
    #[test]
    fn every_tab_renders_every_image() {
        let ctx = egui::Context::default();
        let mut app = SandboxApp::new().expect("the app builds");
        let mut frames = Vec::new();
        for (intent, _) in INTENTS {
            frames.push(Settings {
                intent,
                bpc: true,
                ..Settings::default()
            });
        }
        frames.push(Settings::default());
        for settings in frames {
            app.settings = settings;
            for (tab, _) in Tab::ALL {
                app.tab = tab;
                let mut output = ctx.run_ui(egui::RawInput::default(), |ui| app.draw(ui));
                // No renderer consumes the uploads here.
                output.textures_delta.clear();
            }
        }
        let failures: Vec<_> = app
            .textures
            .iter()
            .filter_map(|(key, t)| t.as_ref().err().map(|e| format!("{key}: {e}")))
            .collect();
        assert!(failures.is_empty(), "{failures:#?}");
        // Formats 4, camera 2, print 7 and malformed 1 per settings; the CLUT slice does not
        // depend on them, so it renders once.
        assert_eq!(app.textures.len(), 14 * 5 + 1);
    }
}
