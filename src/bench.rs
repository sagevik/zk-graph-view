//! Headless frame benchmark: drives the real UI code through egui with synthetic input and
//! measures what the app costs per frame on the CPU, independent of the GPU/driver.
//!
//! `cargo test --release bench_frames -- --ignored --nocapture`
//! (`ZKG_BENCH_N=400,3000` to pick graph sizes.)

use super::*;
use eframe::egui::{MouseWheelUnit, PointerButton, RawInput, TouchPhase};
use std::path::Path;
use std::sync::mpsc;

/// A notebook-like graph: n notes, 97 tags (Zipf-ish use), 1-4 tags per note, ~3 links per
/// note with preferential attachment (a few hubs), realistic title lengths.
pub fn synth(n: usize) -> Arc<Graph> {
    let words = [
        "arch", "linux", "hardware", "floating", "window", "setup", "neovim", "lua", "config",
        "go", "python", "rust", "audio", "synth", "midi", "notes", "zk", "graph", "server",
        "backup", "network", "kernel", "driver", "plugin", "workflow", "ideas", "reading",
    ];
    let mut s = 0x9e3779b9u32;
    let mut rnd = move || {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        s
    };
    let mut notes = Vec::new();
    let mut links = Vec::new();
    let mut targets: Vec<usize> = Vec::new();
    for i in 0..n {
        let len = 2 + (rnd() % 5) as usize;
        let title: Vec<&str> = (0..len).map(|_| words[(rnd() as usize) % words.len()]).collect();
        let ntags = 1 + (rnd() % 4) as usize;
        let tags: Vec<String> = (0..ntags)
            .map(|_| {
                let r = (rnd() % 10_000) as f64 / 10_000.0;
                format!("tag{}", (97.0 * r * r) as usize) // skewed towards low numbers
            })
            .collect();
        notes.push(serde_json::json!({
            "path": format!("n{i}.md"), "absPath": format!("/nb/n{i}.md"),
            "title": title.join(" "), "tags": tags, "wordCount": 100
        }));
        for _ in 0..3 {
            if i == 0 {
                break;
            }
            let t = if !targets.is_empty() && rnd() % 2 == 0 {
                targets[(rnd() as usize) % targets.len()]
            } else {
                (rnd() as usize) % i
            };
            links.push(serde_json::json!({"sourcePath": format!("n{i}.md"), "targetPath": format!("n{t}.md")}));
            targets.push(t);
        }
    }
    let json = serde_json::json!({"notes": notes, "links": links}).to_string();
    Arc::new(Graph::parse(json.as_bytes(), Path::new("/nb")).unwrap())
}

#[derive(Default, Clone, Copy)]
struct Sample {
    ui: f32,
    tess: f32,
    tex_kb: f32,
    verts: usize,
}

struct Harness {
    ctx: egui::Context,
    app: App,
    state: Rc<RefCell<State>>,
    t: f64,
    screen: Rect,
    _tx: mpsc::Sender<UiEvent>,
    /// CPU copy of the font texture, kept up to date from each frame's texture deltas.
    atlas: Atlas,
    /// Tessellated output of the last frame.
    prims: Vec<egui::ClippedPrimitive>,
}

/// Mirror of the font atlas texture (TextureId::default()).
#[derive(Default)]
struct Atlas {
    size: [usize; 2],
    px: Vec<Color32>,
}

impl Atlas {
    fn apply(&mut self, d: &egui::TexturesDelta) {
        for delta in d.set.get(&egui::TextureId::default()).into_iter().flatten() {
            let egui::ImageData::Color(img) = &delta.image;
            match delta.pos {
                None => {
                    self.size = img.size;
                    self.px = img.pixels.clone();
                }
                Some([x0, y0]) => {
                    for y in 0..img.size[1] {
                        for x in 0..img.size[0] {
                            self.px[(y0 + y) * self.size[0] + x0 + x] = img.pixels[y * img.size[0] + x];
                        }
                    }
                }
            }
        }
    }

    /// Highest alpha in the texel rectangle spanned by `uv_min..uv_max` (normalized UVs).
    fn max_alpha(&self, uv_min: Pos2, uv_max: Pos2) -> u8 {
        let [w, h] = self.size;
        let tx = |u: f32| ((u * w as f32) as usize).min(w.saturating_sub(1));
        let ty = |v: f32| ((v * h as f32) as usize).min(h.saturating_sub(1));
        let mut a = 0;
        for y in ty(uv_min.y)..=ty(uv_max.y) {
            for x in tx(uv_min.x)..=tx(uv_max.x) {
                a = a.max(self.px[y * w + x].a());
            }
        }
        a
    }
}

impl Harness {
    fn new(g: Arc<Graph>, cfg: Config) -> Harness {
        let ctx = egui::Context::default();
        let state = Rc::new(RefCell::new(State::new(cfg)));
        state.borrow_mut().handle(UiEvent::Graph(g), None);
        let (tx, rx) = mpsc::channel();
        let app = App::with_context(&ctx, state.clone(), Rc::new(rx));
        Harness {
            ctx,
            app,
            state,
            t: 0.0,
            screen: Rect::from_min_size(Pos2::ZERO, vec2(1400.0, 950.0)),
            _tx: tx,
            atlas: Atlas::default(),
            prims: Vec::new(),
        }
    }

    /// Simulates hide + show: a new window has a new egui context and font atlas, while
    /// the app state (including any caches in it) survives.
    fn reopen(&mut self, pixels_per_point: f32) {
        // As in a real session: the old window's atlas had grown, so its fill ratio was
        // low, and a fresh small atlas does not look "rebuilt" by fill ratio alone.
        self.state.borrow_mut().label_key.1 = 0.0;
        self.ctx = egui::Context::default();
        self.ctx.set_pixels_per_point(pixels_per_point);
        let (tx, rx) = mpsc::channel();
        self.app = App::with_context(&self.ctx, self.state.clone(), Rc::new(rx));
        self._tx = tx;
        self.atlas = Atlas::default();
    }

    /// Finish the layout off-screen so scenarios measure interaction, not settling.
    fn settle(&mut self) {
        {
            let mut st = self.state.borrow_mut();
            let sim = st.sim.as_mut().unwrap();
            let mut n = 0;
            while sim.running() && n < 3000 {
                sim.tick();
                n += 1;
            }
            st.camera.lock = Some(Lock::Fit);
            st.camera.snap = true;
        }
        for _ in 0..5 {
            self.frame(vec![]);
        }
    }

    fn frame(&mut self, events: Vec<Event>) -> Sample {
        let raw = RawInput {
            screen_rect: Some(self.screen),
            time: Some(self.t),
            predicted_dt: 1.0 / 60.0,
            focused: true,
            events,
            ..Default::default()
        };
        self.t += 1.0 / 60.0;
        let t0 = Instant::now();
        let app = &mut self.app;
        let out = self.ctx.run_ui(raw, |ui| app.frame(ui, None));
        let ui = t0.elapsed().as_secs_f32() * 1000.0;
        let t1 = Instant::now();
        let prims = self.ctx.tessellate(out.shapes, out.pixels_per_point);
        let tess = t1.elapsed().as_secs_f32() * 1000.0;
        self.atlas.apply(&out.textures_delta);
        let verts = prims
            .iter()
            .map(|p| match &p.primitive {
                egui::epaint::Primitive::Mesh(m) => m.vertices.len(),
                _ => 0,
            })
            .sum();
        let tex_kb = out
            .textures_delta
            .set
            .values()
            .flatten()
            .map(|d| (d.image.width() * d.image.height() * 4) as f32 / 1024.0)
            .sum();
        self.prims = prims;
        Sample { ui, tess, tex_kb, verts }
    }

    fn report(name: &str, s: &[Sample]) {
        let mut tot: Vec<f32> = s.iter().map(|x| x.ui + x.tess).collect();
        tot.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let avg = |f: fn(&Sample) -> f32| s.iter().map(f).sum::<f32>() / s.len() as f32;
        println!(
            "  {name:<22} cpu avg {:6.2}ms p95 {:6.2}ms max {:6.2}ms | ui {:5.2} tess {:5.2} | tex upload {:7.1} KB/frame | {:6} verts",
            avg(|x| x.ui + x.tess),
            tot[(tot.len() * 95 / 100).min(tot.len() - 1)],
            tot[tot.len() - 1],
            avg(|x| x.ui),
            avg(|x| x.tess),
            avg(|x| x.tex_kb),
            s.iter().map(|x| x.verts).max().unwrap_or(0),
        );
    }

    fn wheel(&mut self, at: Pos2, lines: f32) -> Sample {
        self.frame(vec![
            Event::PointerMoved(at),
            Event::MouseWheel {
                unit: MouseWheelUnit::Line,
                delta: vec2(0.0, lines),
                phase: TouchPhase::Move,
                modifiers: Modifiers::NONE,
            },
        ])
    }

    fn drag(&mut self, from: Pos2, step: Vec2, frames: usize) -> Vec<Sample> {
        let mut out = Vec::new();
        let press = |pos, pressed| Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        self.frame(vec![Event::PointerMoved(from)]);
        self.frame(vec![press(from, true)]);
        let mut p = from;
        for _ in 0..frames {
            p += step;
            out.push(self.frame(vec![Event::PointerMoved(p)]));
        }
        self.frame(vec![press(p, false)]);
        out
    }
}

fn bench_cfg() -> Config {
    let json = r##"{"theme":{"bg":"#000000","panel":"#1c1c24","field":"#141415","fg":"#cdcdcd",
        "muted":"#606079","border":"#252530","edge":"#878787","edge_opacity":0.8,"edge_width":1,
        "focus":"#7e98e8","accents":["#7e98e8","#e8b589","#7fa563","#d8647e","#bb9dbd","#b4d4cf",
        "#c48282","#6e94b2","#e0a363","#aeaed1","#9bb4bc","#f3be7c"]},"filter":{"top":12},
        "legend_open":true,"tag_colors":true,"follow":true,"stabilize_iterations":200}"##;
    Config::parse(json).unwrap()
}

#[test]
#[ignore]
fn bench_frames() {
    let sizes: Vec<usize> = std::env::var("ZKG_BENCH_N")
        .ok()
        .map(|v| v.split(',').filter_map(|x| x.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![400, 3000]);
    for n in sizes {
        let g = synth(n);
        println!("{} nodes, {} edges", g.nodes.len(), g.edges.len());
        let mut h = Harness::new(g, bench_cfg());
        h.settle();
        let c = h.screen.center();

        let idle: Vec<Sample> = (0..30).map(|_| h.frame(vec![])).collect();
        Harness::report("idle (forced repaint)", &idle);

        // wheel zoom in and out over the centre: one notch per frame
        let mut zoom = Vec::new();
        for k in 0..60 {
            zoom.push(h.wheel(c + vec2(37.0, -21.0), if k < 30 { 1.0 } else { -1.0 }));
        }
        Harness::report("wheel zoom in/out", &zoom);

        // pan with the mouse at the fitted zoom
        let pan = h.drag(pos2(30.0, 920.0), vec2(9.0, -5.0), 60);
        Harness::report("drag pan (overview)", &pan);

        // zoom far in (labels everywhere), then pan
        for _ in 0..12 {
            h.wheel(c, 1.0);
        }
        for _ in 0..30 {
            h.frame(vec![]);
        }
        let zp = h.drag(pos2(30.0, 920.0), vec2(9.0, -5.0), 60);
        Harness::report("drag pan (zoomed in)", &zp);
        let mut zz = Vec::new();
        for k in 0..40 {
            zz.push(h.wheel(c, if k % 20 < 10 { 1.0 } else { -1.0 }));
        }
        Harness::report("wheel zoom (zoomed in)", &zz);
        let st = h.state.borrow();
        println!("  final zoom {:.2}", st.camera.zoom);
    }
}

/// Every glyph quad drawn in the last frame: (min uv, max uv, positions).
fn glyph_quads(prims: &[egui::ClippedPrimitive]) -> Vec<(Pos2, Pos2, [Pos2; 4], Color32)> {
    let mut out = Vec::new();
    for p in prims {
        if let egui::epaint::Primitive::Mesh(m) = &p.primitive {
            // epaint text meshes: 4 vertices per glyph quad, uv != WHITE_UV
            for q in m.vertices.chunks_exact(4) {
                if q.iter().all(|v| v.uv != egui::epaint::WHITE_UV) && q[0].uv != q[2].uv {
                    let (mut lo, mut hi) = (q[0].uv, q[0].uv);
                    for v in q {
                        lo = lo.min(v.uv);
                        hi = hi.max(v.uv);
                    }
                    out.push((lo, hi, [q[0].pos, q[1].pos, q[2].pos, q[3].pos], q[0].color));
                }
            }
        }
    }
    out
}

/// Regression for invisible/garbled labels: every label glyph must sample glyph pixels in
/// the *current* window's font atlas (normalized UVs, no layouts cached from a previous
/// window's atlas), be pixel aligned (crisp), and use the theme's text color.
#[test]
fn label_glyphs_hit_the_current_atlas() {
    let mut h = Harness::new(synth(60), bench_cfg());
    h.state.borrow_mut().panel_open = false; // only label text on screen
    h.settle();
    let fg = h.state.borrow().cfg.theme.fg;
    let c = h.screen.center();

    let check = |h: &mut Harness, what: &str| {
        let ppp = h.ctx.pixels_per_point();
        // a sweep of zoom levels, each a different label pixel size
        for step in 0..6 {
            h.wheel(c, if step == 0 { 0.0 } else { 1.0 });
            h.frame(vec![]);
            let quads = glyph_quads(&h.prims);
            assert!(quads.len() > 50, "{what} step {step}: only {} glyphs drawn", quads.len());
            let mut hit = 0;
            for (lo, hi, pos, color) in &quads {
                assert!((0.0..=1.0).contains(&lo.x) && hi.x <= 1.0 && hi.y <= 1.0, "{what}: uv out of range {lo:?}..{hi:?}");
                if h.atlas.max_alpha(*lo, *hi) > 0 {
                    hit += 1;
                }
                for p in pos {
                    let (fx, fy) = (p.x * ppp, p.y * ppp);
                    assert!((fx - fx.round()).abs() < 0.02 && (fy - fy.round()).abs() < 0.02, "{what}: glyph not pixel aligned: {p:?} at ppp {ppp}");
                }
                let [r, g, b, _] = color.to_array();
                let [fr, fg_, fb, _] = fg.gamma_multiply(color.a() as f32 / 255.0).to_array();
                assert!((r, g, b) == (fr, fg_, fb), "{what}: label color {color:?} is not the theme text color");
            }
            assert_eq!(hit, quads.len(), "{what} step {step}: {} of {} glyphs sample empty atlas space", quads.len() - hit, quads.len());
            for _ in 0..3 {
                h.wheel(c, -1.0); // back out, so the next step reuses cached sizes too
                h.wheel(c, 1.0);
            }
        }
    };

    check(&mut h, "first window");
    h.reopen(1.0);
    for _ in 0..3 {
        h.frame(vec![]);
    }
    check(&mut h, "reopened window");
    h.reopen(1.5);
    for _ in 0..3 {
        h.frame(vec![]);
    }
    check(&mut h, "reopened at 1.5x");
}
