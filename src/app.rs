//! The window: graph canvas (pan, zoom, drag, click-to-open), tooltip, toast, and the tag
//! filter panel. All view state lives in [`State`], which outlives the window: when the
//! graph is hidden there is no window at all (Wayland cannot hide one), and showing it
//! again picks up graph, layout, camera and filters where they were.

use crate::config::{Config, Theme};
use crate::graph::{Changes, Graph};
use crate::layout::{self, Params, Sim};
use crate::mesh::Batch;
use eframe::egui::emath::GuiRounding as _;
use crate::model::{Filter, Mode, Model};
use crate::proto;
use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, CornerRadius, Event, FontId, Id, Key, Layout, Mesh,
    Modifiers, MouseWheelUnit, Order, Pos2, Rect, Sense, Shadow, Shape, Stroke, StrokeKind,
    TextEdit, Vec2, ViewportCommand,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Messages for the UI thread, from the reload and stdin threads.
pub enum UiEvent {
    Graph(Arc<Graph>),
    LoadError(String),
    Current(String),
    Show,
    Hide,
    /// nvim quit or stdin closed: close the window (if any) and end the process.
    Quit,
}

/// The live window's context, if any: background threads use it to wake the UI.
pub type Waker = Arc<Mutex<Option<egui::Context>>>;

pub fn wake(w: &Waker) {
    if let Some(ctx) = w.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        ctx.request_repaint();
    }
}

const TOAST: Duration = Duration::from_millis(2500);
const TOOLTIP_DELAY: Duration = Duration::from_millis(350);
const PRESTABILIZE_BUDGET: Duration = Duration::from_millis(300);
const ZOOM_MIN: f32 = 0.02;
const ZOOM_MAX: f32 = 8.0;
/// Zoom factor per mouse-wheel notch.
const WHEEL_ZOOM: f32 = 1.18;
/// Pointer travel (points) before a press becomes a drag instead of a click.
const DRAG_SLOP: f32 = 3.0;
/// Label sizes (points) shown; below the minimum labels are hidden.
const LABEL_MIN: f32 = 7.0;
const LABEL_MAX: f32 = 22.0;

/// Up to two laid-out labels per node: (size in physical pixels, layout). Two, so zooming
/// back and forth across one size step does not lay the label out again.
type LabelSlots = [Option<(u16, Arc<egui::Galley>)>; 2];

#[derive(Clone, Copy)]
struct Press {
    origin: Pos2,
    node: Option<usize>,
    moved: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Lock {
    Fit,
    Node(usize, f32),
}

struct Camera {
    center: Vec2,
    zoom: f32,
    /// Keeps tracking a target (fit, or a node) while the layout still moves.
    lock: Option<Lock>,
    /// Jump to the target on the next frame instead of animating.
    snap: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    None,
    Pan,
    Node(usize),
}

pub struct State {
    cfg: Config,
    model: Option<Model>,
    sim: Option<Sim>,
    radius: Vec<f32>,
    filter: Filter,
    current: Option<String>,
    colors_on: bool,
    follow: bool,
    panel_open: bool,
    camera: Camera,
    user_moved: bool,
    // derived (recomputed when dirty)
    dirty: bool,
    visible: Vec<bool>,
    node_accent: Vec<Option<usize>>,
    tag_accent: HashMap<String, usize>,
    // transient
    error: Option<String>,
    toast: Option<(String, Instant)>,
    hover: Option<(usize, Instant)>,
    drag: Drag,
    press: Option<Press>,
    /// Canvas rect of the last frame (for "is the current note on screen?").
    view: Option<Rect>,
    /// Laid-out labels per node. egui drops any layout not used in the previous frame;
    /// this keeps them while the zoom stays within a pixel size. A layout's texture
    /// coordinates point into one window's font atlas, so the cache is cleared whenever a
    /// window opens (each window has its own atlas), the atlas is rebuilt, or the scale
    /// factor changes.
    label_cache: Vec<LabelSlots>,
    label_key: (f32, f32),
    search: String,
    search_idx: usize,
    focus_search: bool,
    pub window_size: Option<Vec2>,
    tick_cost: Duration,
    pub quit: bool,
}

impl State {
    pub fn new(cfg: Config) -> State {
        State {
            colors_on: cfg.tag_colors,
            follow: cfg.follow,
            panel_open: cfg.legend_open,
            cfg,
            model: None,
            sim: None,
            radius: Vec::new(),
            filter: Filter::default(),
            current: None,
            camera: Camera { center: Vec2::ZERO, zoom: 1.0, lock: Some(Lock::Fit), snap: true },
            user_moved: false,
            dirty: true,
            visible: Vec::new(),
            node_accent: Vec::new(),
            tag_accent: HashMap::new(),
            error: None,
            toast: None,
            hover: None,
            drag: Drag::None,
            press: None,
            view: None,
            label_cache: Vec::new(),
            label_key: (0.0, 0.0),
            search: String::new(),
            search_idx: 0,
            focus_search: false,
            window_size: None,
            tick_cost: Duration::ZERO,
            quit: false,
        }
    }

    /// Applies an event. `ctx` is the live window's context (None while hidden).
    pub fn handle(&mut self, ev: UiEvent, ctx: Option<&egui::Context>) {
        match ev {
            UiEvent::Graph(g) => self.apply_graph(g),
            UiEvent::LoadError(e) => self.error = Some(e),
            UiEvent::Current(id) => {
                self.current = (!id.is_empty()).then_some(id);
                self.dirty = true;
                if self.follow && !self.current_on_screen() {
                    self.follow_current(false);
                }
            }
            UiEvent::Show => {
                if let Some(ctx) = ctx {
                    ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(ViewportCommand::Focus);
                }
            }
            UiEvent::Hide | UiEvent::Quit => {
                if let Some(ctx) = ctx {
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
                if matches!(ev, UiEvent::Quit) {
                    self.quit = true;
                }
            }
        }
    }

    fn current_index(&self) -> Option<usize> {
        let id = self.current.as_deref()?;
        self.model.as_ref()?.graph.get(id)
    }

    fn apply_graph(&mut self, g: Arc<Graph>) {
        let first = self.model.is_none();
        let changes = self.model.as_ref().map(|m| Changes::between(&m.graph, &g));

        // radius from backlinks, area-proportional
        let max_b = g.nodes.iter().map(|n| n.backlinks).max().unwrap_or(0).max(1) as f32;
        let radius: Vec<f32> = g
            .nodes
            .iter()
            .map(|n| if n.ghost { 4.0 } else { 6.0 + 14.0 * (n.backlinks as f32 / max_b).sqrt() })
            .collect();

        // keep positions of known nodes; seed new ones next to a positioned neighbour
        let old: HashMap<&str, Vec2> = match (&self.model, &self.sim) {
            (Some(m), Some(s)) => m.graph.nodes.iter().zip(&s.pos).map(|(n, p)| (n.id.as_str(), *p)).collect(),
            _ => HashMap::new(),
        };
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); g.nodes.len()];
        for &(s, t) in &g.edges {
            adj[s as usize].push(t as usize);
            adj[t as usize].push(s as usize);
        }
        let known: Vec<Option<Vec2>> = g.nodes.iter().map(|n| old.get(n.id.as_str()).copied()).collect();
        let pos: Vec<Vec2> = (0..g.nodes.len())
            .map(|i| {
                known[i].unwrap_or_else(|| {
                    let h = g.nodes[i].id.bytes().fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
                    let a = (h % 6283) as f32 / 1000.0;
                    match adj[i].iter().find_map(|&nb| known[nb]) {
                        Some(p) => p + vec2(a.cos(), a.sin()) * 30.0,
                        None if first => layout::phyllotaxis(i),
                        None => layout::phyllotaxis(i) + vec2(a.cos(), a.sin()),
                    }
                })
            })
            .collect();

        let mut sim = Sim::new(pos, &g.edges, &radius, Params::default());
        if first {
            let t0 = Instant::now();
            for _ in 0..self.cfg.stabilize_iterations {
                sim.tick();
                if t0.elapsed() > PRESTABILIZE_BUDGET {
                    break;
                }
            }
            self.camera.lock = Some(Lock::Fit);
            self.camera.snap = true;
        } else {
            let structural = changes
                .as_ref()
                .is_none_or(|c| c.added + c.removed + c.links_added + c.links_removed > 0);
            sim.alpha = if structural { 0.3 } else { 0.05 };
        }

        self.toast = changes.and_then(|c| c.describe()).map(|t| (t, Instant::now()));
        self.model = Some(Model::new(g));
        self.sim = Some(sim);
        self.radius = radius;
        self.error = None;
        self.hover = None;
        self.drag = Drag::None;
        self.press = None;
        self.dirty = true;
        self.label_cache.clear();
        if let Some(m) = &self.model {
            self.filter.sel.retain(|t| m.counts.contains_key(t));
        }
    }

    fn derive(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let Some(m) = &self.model else { return };
        self.visible = m.visible(&self.filter, self.current_index());
        let (n, t) = m.colors(&self.filter, &self.cfg.color_tags, self.cfg.color_max_share);
        self.node_accent = n;
        self.tag_accent = t;
    }

    /// Whether the current note is visible and inside the middle of the last view.
    fn current_on_screen(&mut self) -> bool {
        self.derive();
        let (Some(i), Some(view), Some(sim)) = (self.current_index(), self.view, self.sim.as_ref()) else {
            return false;
        };
        if self.visible.get(i) != Some(&true) {
            return false;
        }
        let p = view.center() + (sim.pos[i] - self.camera.center) * self.camera.zoom;
        view.shrink2(view.size() * 0.12).contains(p)
    }

    fn follow_current(&mut self, snap: bool) {
        self.derive();
        if let Some(i) = self.current_index().filter(|&i| self.visible.get(i) == Some(&true)) {
            let zoom = self.camera.zoom.max(0.6);
            self.camera.lock = Some(Lock::Node(i, zoom));
            self.camera.snap |= snap;
        }
    }

    /// Called when a window opens.
    pub fn on_show(&mut self) {
        self.label_cache.clear(); // new window, new font atlas
        self.label_key = (0.0, 0.0);
        self.drag = Drag::None;
        self.press = None;
        self.hover = None;
        self.derive();
        let following = self.follow
            && self.current_index().is_some_and(|i| self.visible.get(i) == Some(&true));
        if following {
            self.follow_current(true);
        } else if !self.user_moved {
            self.camera.lock = Some(Lock::Fit);
            self.camera.snap = true;
        }
    }

    fn accent(&self, idx: usize) -> Color32 {
        let a = &self.cfg.theme.accents;
        a[idx % a.len()]
    }

    fn open(&mut self, i: usize) {
        let Some(m) = &self.model else { return };
        let n = &m.graph.nodes[i];
        if n.path.is_file() {
            proto::send(&proto::Out::Open { path: n.path.to_string_lossy().into_owned() });
        } else {
            self.toast = Some((format!("No note file for {}", n.id), Instant::now()));
        }
    }
}

// ---- theme ---------------------------------------------------------------------------------

fn apply_theme(ctx: &egui::Context, t: &Theme) {
    let mut v = if t.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    v.panel_fill = t.bg;
    v.window_fill = t.panel;
    v.window_stroke = Stroke::new(1.0, t.border);
    v.extreme_bg_color = t.field;
    v.faint_bg_color = t.field;
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(6);
    v.popup_shadow = Shadow { offset: [0, 4], blur: 12, spread: 0, color: Color32::from_black_alpha(90) };
    v.window_shadow = v.popup_shadow;
    v.selection.bg_fill = t.focus.gamma_multiply(0.45);
    v.selection.stroke = Stroke::new(1.0, t.fg);
    v.text_cursor.stroke = Stroke::new(1.5, t.focus);
    v.hyperlink_color = t.focus;
    for (w, stroke) in [
        (&mut v.widgets.noninteractive, t.border),
        (&mut v.widgets.inactive, t.border),
        (&mut v.widgets.hovered, t.muted),
        (&mut v.widgets.active, t.fg),
        (&mut v.widgets.open, t.muted),
    ] {
        w.bg_fill = t.field;
        w.weak_bg_fill = t.field;
        w.bg_stroke = Stroke::new(1.0, stroke);
        w.fg_stroke = Stroke::new(1.0, t.fg);
        w.corner_radius = CornerRadius::same(6);
        w.expansion = 0.0;
    }
    v.widgets.noninteractive.bg_fill = t.panel;
    v.widgets.noninteractive.weak_bg_fill = t.panel;
    ctx.set_visuals(v);
    ctx.global_style_mut(|s| {
        use egui::TextStyle;
        s.text_styles.insert(TextStyle::Body, FontId::proportional(13.0));
        s.text_styles.insert(TextStyle::Button, FontId::proportional(13.0));
        s.text_styles.insert(TextStyle::Small, FontId::proportional(11.0));
        s.spacing.item_spacing = vec2(6.0, 6.0);
        s.spacing.button_padding = vec2(7.0, 3.0);
        s.interaction.tooltip_delay = 0.35;
    });
}

// ---- app ------------------------------------------------------------------------------------

pub struct App {
    state: Rc<RefCell<State>>,
    rx: Rc<Receiver<UiEvent>>,
    stats: Option<Stats>,
}

/// `ZK_GRAPH_STATS=1`: every 2 s while frames are drawn, logs the frame rate, the worst
/// gap between two frames (stutter), and CPU time per frame (layout, scene, everything egui
/// does incl. tessellation and GPU submission; not GPU execution) to stderr / :ZkGraphLog.
struct Stats {
    since: Instant,
    last: Option<Instant>,
    frames: u32,
    gap: Duration,
    /// Time covered by frames that followed each other closely (excludes idle periods).
    active: Duration,
    sum: f32,
    max: f32,
    sim: Duration,
    draw: Duration,
}

impl Stats {
    fn new() -> Stats {
        Stats {
            since: Instant::now(),
            last: None,
            frames: 0,
            gap: Duration::ZERO,
            active: Duration::ZERO,
            sum: 0.0,
            max: 0.0,
            sim: Duration::ZERO,
            draw: Duration::ZERO,
        }
    }

    fn record(&mut self, cpu: Option<f32>, nodes: usize, running: bool) {
        let now = Instant::now();
        // gaps over 250 ms are idle time (nothing to draw), not stutter
        if let Some(gap) = self.last.map(|l| now - l).filter(|g| *g < Duration::from_millis(250)) {
            self.gap = self.gap.max(gap);
            self.active += gap;
        }
        self.last = Some(now);
        self.frames += 1;
        let cpu = cpu.unwrap_or(0.0);
        self.sum += cpu;
        self.max = self.max.max(cpu);
        let dt = self.since.elapsed();
        if dt >= Duration::from_secs(2) {
            let n = self.frames as f32;
            eprintln!(
                "zk-graph: stats: {:.0} fps, worst gap {:.1}ms | cpu/frame avg {:.2}ms max {:.2}ms (layout {:.2}ms, scene {:.2}ms) | {nodes} nodes, layout {}",
                n / self.active.as_secs_f32().max(1e-3),
                self.gap.as_secs_f32() * 1000.0,
                1000.0 * self.sum / n,
                1000.0 * self.max,
                self.sim.as_secs_f32() * 1000.0 / n,
                self.draw.as_secs_f32() * 1000.0 / n,
                if running { "running" } else { "idle" }
            );
            *self = Stats { last: self.last, ..Stats::new() };
        }
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, state: Rc<RefCell<State>>, rx: Rc<Receiver<UiEvent>>) -> App {
        App::with_context(&cc.egui_ctx, state, rx)
    }

    pub fn with_context(ctx: &egui::Context, state: Rc<RefCell<State>>, rx: Rc<Receiver<UiEvent>>) -> App {
        apply_theme(ctx, &state.borrow().cfg.theme);
        state.borrow_mut().on_show();
        let stats = std::env::var_os("ZK_GRAPH_STATS")
            .filter(|v| v != "0")
            .map(|_| Stats::new());
        App { state, rx, stats }
    }
}

fn hit(st: &State, sim: &Sim, to_screen: impl Fn(Vec2) -> Pos2, p: Pos2, zoom: f32) -> Option<usize> {
    let mut best: Option<(usize, f32)> = None;
    for (i, &w) in sim.pos.iter().enumerate() {
        if st.visible.get(i) != Some(&true) {
            continue;
        }
        let d = (to_screen(w) - p).length();
        let r = (st.radius[i] * zoom).max(4.0) + 3.0;
        if d <= r && best.is_none_or(|(_, bd)| d < bd) {
            best = Some((i, d));
        }
    }
    best.map(|(i, _)| i)
}

/// Bounding box (world) of visible nodes, including their radius.
fn visible_bounds(st: &State, sim: &Sim) -> Option<Rect> {
    let mut r: Option<Rect> = None;
    for (i, &p) in sim.pos.iter().enumerate() {
        if st.visible.get(i) != Some(&true) {
            continue;
        }
        let rad = st.radius[i] + 4.0;
        let nr = Rect::from_center_size(p.to_pos2(), Vec2::splat(rad * 2.0));
        r = Some(r.map_or(nr, |r| r.union(nr)));
    }
    r
}

impl eframe::App for App {
    fn clear_color(&self, _v: &egui::Visuals) -> [f32; 4] {
        self.state.borrow().cfg.theme.bg.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.frame(ui, frame.info().cpu_usage);
    }
}

impl App {
    /// One frame. Separate from `eframe::App::ui` so it can run headless (benchmarks).
    pub fn frame(&mut self, ui: &mut egui::Ui, cpu_usage: Option<f32>) {
        let ctx = ui.ctx().clone();
        let mut guard = self.state.borrow_mut();
        let st: &mut State = &mut guard;
        if let Some(stats) = self.stats.as_mut() {
            let nodes = st.model.as_ref().map_or(0, |m| m.graph.nodes.len());
            stats.record(cpu_usage, nodes, st.sim.as_ref().is_some_and(Sim::running));
        }

        while let Ok(ev) = self.rx.try_recv() {
            st.handle(ev, Some(&ctx));
        }
        if let Some(r) = ctx.input(|i| i.viewport().inner_rect) {
            st.window_size = Some(r.size());
        }
        st.derive();

        // keyboard (layout independent: text events), only when no text field has focus
        if !ctx.egui_wants_keyboard_input() {
            let mut slash = false;
            ctx.input_mut(|i| {
                i.events.retain(|e| match e {
                    Event::Text(t) if t == "/" => {
                        slash = true;
                        false
                    }
                    Event::Text(t) if t == "f" || t == "F" => {
                        st.camera.lock = Some(Lock::Fit);
                        false
                    }
                    Event::Text(t) if t == "c" || t == "C" => {
                        st.follow_current(false);
                        false
                    }
                    _ => true,
                })
            });
            if slash {
                st.panel_open = true;
                st.focus_search = true;
            }
        }

        // simulation: one or two ticks per frame, depending on cost
        let mut animating = false;
        if let Some(sim) = st.sim.as_mut() {
            if sim.running() {
                let ticks = if st.tick_cost < Duration::from_millis(4) { 2 } else { 1 };
                let t0 = Instant::now();
                for _ in 0..ticks {
                    sim.tick();
                }
                st.tick_cost = t0.elapsed() / ticks;
                animating = true;
                if let Some(s) = self.stats.as_mut() {
                    s.sim += t0.elapsed();
                }
            }
        }

        let theme = st.cfg.theme.clone();
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme.bg))
            .show(ui, |ui| {
                let t0 = Instant::now();
                canvas(ui, st, &theme);
                if let Some(s) = self.stats.as_mut() {
                    s.draw += t0.elapsed();
                }
            });

        panel(&ctx, st, &theme);
        overlays(&ctx, st, &theme);

        // repaint scheduling: nothing runs while idle
        let settling = st.sim.as_ref().is_some_and(Sim::running);
        if animating || settling || st.camera.lock.is_some() || st.drag != Drag::None {
            ctx.request_repaint();
        }
        if let Some((_, t)) = &st.toast {
            let left = TOAST.saturating_sub(t.elapsed());
            if left.is_zero() {
                st.toast = None;
            } else {
                ctx.request_repaint_after(left.saturating_sub(Duration::from_millis(400)));
            }
        }
        if let Some((_, t)) = st.hover {
            let left = TOOLTIP_DELAY.saturating_sub(t.elapsed());
            if !left.is_zero() {
                ctx.request_repaint_after(left);
            }
        }
    }
}

// ---- canvas ---------------------------------------------------------------------------------

fn canvas(ui: &mut egui::Ui, st: &mut State, theme: &Theme) {
    let rect = ui.max_rect();
    let resp = ui.interact(rect, Id::new("canvas"), Sense::hover());
    let ctx = ui.ctx().clone();
    let dt = ctx.input(|i| i.stable_dt).clamp(0.001, 0.1);

    if st.model.is_none() || st.sim.is_none() {
        return;
    }
    st.view = Some(rect);

    // --- input ---
    let zoom0 = st.camera.zoom;
    let center0 = st.camera.center;
    let to_screen = |c: Vec2, z: f32| move |w: Vec2| rect.center() + (w - c) * z;
    let to_world = |p: Pos2, c: Vec2, z: f32| (p - rect.center()) / z + c;

    let pointer = resp.hover_pos();
    let hovered_node = {
        let sim = st.sim.as_ref().unwrap();
        pointer.and_then(|p| hit(st, sim, to_screen(center0, zoom0), p, zoom0))
    };
    if st.drag == Drag::None {
        match (hovered_node, st.hover) {
            (Some(i), Some((h, _))) if h == i => {}
            (Some(i), _) => st.hover = Some((i, Instant::now())),
            (None, _) => st.hover = None,
        }
    }

    // Pointer: own press/drag tracking. egui only reports a drag after its click threshold
    // and drops the distance travelled until then; here panning starts within DRAG_SLOP and
    // catches up the slop, so the graph stays glued to the pointer.
    let (pressed, down, delta, latest, origin) = ctx.input(|i| {
        let p = &i.pointer;
        // the press event itself: egui clears press_origin() when the release lands in
        // the same frame (a quick flick)
        let pressed_at = i.events.iter().find_map(|e| match e {
            Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, .. } => Some(*pos),
            _ => None,
        });
        (p.primary_pressed(), p.primary_down(), p.delta(), p.latest_pos(), pressed_at.or(p.press_origin()))
    });
    let over_canvas = |o: Pos2| rect.contains(o) && ctx.layer_id_at(o).is_none_or(|l| l.order == Order::Background);
    if pressed && st.press.is_none() && origin.is_some_and(over_canvas) {
        if let Some(o) = origin {
            let sim = st.sim.as_ref().unwrap();
            let node = hit(st, sim, to_screen(center0, zoom0), o, zoom0);
            st.press = Some(Press { origin: o, node, moved: false });
        }
    }
    if let Some(mut pr) = st.press {
        if down {
            if let Some(p) = latest {
                if !pr.moved && (p - pr.origin).length() > DRAG_SLOP {
                    pr.moved = true;
                    st.drag = pr.node.map_or(Drag::Pan, Drag::Node);
                    st.camera.lock = None;
                    st.user_moved = true;
                    st.hover = None;
                    if st.drag == Drag::Pan {
                        st.camera.center -= (p - pr.origin - delta) / st.camera.zoom;
                    }
                }
                if pr.moved {
                    match st.drag {
                        Drag::Pan => st.camera.center -= delta / st.camera.zoom,
                        Drag::Node(i) => {
                            let w = to_world(p, st.camera.center, st.camera.zoom);
                            let sim = st.sim.as_mut().unwrap();
                            sim.fixed[i] = Some(w);
                            sim.pos[i] = w;
                            sim.alpha_target = 0.3;
                            sim.reheat(0.1);
                        }
                        Drag::None => {}
                    }
                }
            }
            st.press = Some(pr);
        } else {
            let travel = latest.map_or(Vec2::ZERO, |p| p - pr.origin);
            if !pr.moved && travel.length() > DRAG_SLOP && pr.node.is_none() {
                st.camera.center -= travel / st.camera.zoom; // pressed and released within one frame
                st.camera.lock = None;
                st.user_moved = true;
            } else if !pr.moved {
                if let Some(i) = pr.node {
                    st.open(i);
                }
            }
            if let Drag::Node(i) = st.drag {
                let sim = st.sim.as_mut().unwrap();
                sim.fixed[i] = None;
                sim.alpha_target = 0.0;
            }
            st.drag = Drag::None;
            st.press = None;
        }
    }

    // Wheel and gestures, read raw: egui's smoothed scroll delta spreads one wheel notch
    // over several frames, which makes zoom trail behind the wheel.
    // Mouse wheel (line units) zooms; touchpad scrolling (point units) pans, or zooms with
    // Ctrl held; pinch zooms.
    if resp.hovered() {
        let (lines, points, pinch, ctrl) = ctx.input(|i| {
            let (mut l, mut p, mut z) = (Vec2::ZERO, Vec2::ZERO, 1.0f32);
            for e in &i.events {
                match e {
                    Event::MouseWheel { unit: MouseWheelUnit::Line, delta, .. } => l += *delta,
                    Event::MouseWheel { unit: MouseWheelUnit::Page, delta, .. } => l += *delta * 5.0,
                    Event::MouseWheel { unit: MouseWheelUnit::Point, delta, .. } => p += *delta,
                    Event::Zoom(f) => z *= *f,
                    _ => {}
                }
            }
            (l, p, z, i.modifiers.command || i.modifiers.ctrl)
        });
        let mut factor = pinch * WHEEL_ZOOM.powf(lines.y);
        if ctrl {
            factor *= (points.y * 0.006).exp();
        } else if points != Vec2::ZERO {
            st.camera.center -= points / st.camera.zoom;
            st.camera.lock = None;
            st.user_moved = true;
        }
        if lines.x != 0.0 {
            st.camera.center.x -= lines.x * 40.0 / st.camera.zoom;
        }
        if (factor - 1.0).abs() > 1e-4 {
            if let Some(p) = pointer {
                let w = to_world(p, st.camera.center, st.camera.zoom);
                let z = (st.camera.zoom * factor).clamp(ZOOM_MIN, ZOOM_MAX);
                st.camera.zoom = z;
                st.camera.center = w - (p - rect.center()) / z;
            }
            st.camera.lock = None;
            st.user_moved = true;
        }
    }

    // --- camera ---
    if let Some(lock) = st.camera.lock {
        let sim = st.sim.as_ref().unwrap();
        let target = match lock {
            Lock::Fit => visible_bounds(st, sim).map(|b| {
                let avail = rect.shrink(40.0).size().max(vec2(50.0, 50.0));
                let z = (avail.x / b.width()).min(avail.y / b.height()).clamp(ZOOM_MIN, 1.2);
                (b.center().to_vec2(), z)
            }),
            Lock::Node(i, z) => sim.pos.get(i).map(|p| (*p, z)),
        };
        match target {
            Some((c, z)) => {
                if st.camera.snap {
                    st.camera.center = c;
                    st.camera.zoom = z;
                } else {
                    let k = 1.0 - (-dt * 9.0).exp();
                    st.camera.center += (c - st.camera.center) * k;
                    st.camera.zoom *= (z / st.camera.zoom).powf(k);
                }
                let close = (c - st.camera.center).length() * st.camera.zoom < 0.5
                    && (z / st.camera.zoom - 1.0).abs() < 0.002;
                if close && !sim.running() {
                    st.camera.lock = None;
                }
            }
            None => st.camera.lock = None,
        }
        st.camera.snap = false;
    }

    // --- draw ---
    let painter = ui.painter_at(rect);
    let (center, zoom) = (st.camera.center, st.camera.zoom);
    let screen = to_screen(center, zoom);
    let sim = st.sim.as_ref().unwrap();
    let model = st.model.as_ref().unwrap();
    let cull = rect.expand(40.0);
    let cur = st.current_index();
    let hov = st.hover.map(|(i, _)| i).or(match st.drag {
        Drag::Node(i) => Some(i),
        _ => None,
    });
    let pts: Vec<Pos2> = sim.pos.iter().map(|&w| screen(w)).collect();
    let vis = &st.visible;

    let ppp = ctx.pixels_per_point();
    let hi_stroke = Stroke::new(theme.edge_width + 1.0, theme.fg);
    let mut edges = Batch::new(ppp);
    let mut highlighted: Vec<(usize, usize)> = Vec::new();
    for &(s, t) in &model.graph.edges {
        let (s, t) = (s as usize, t as usize);
        if !(vis[s] && vis[t]) {
            continue;
        }
        let (a, b) = (pts[s], pts[t]);
        if !cull.intersects(Rect::from_two_pos(a, b)) {
            continue;
        }
        if Some(s) == hov || Some(t) == hov || Some(s) == cur || Some(t) == cur {
            highlighted.push((s, t));
            continue;
        }
        edges.line(a, b, theme.edge_width, theme.edge);
        if st.cfg.directed {
            arrow(&painter, a, b, st.radius[t] * zoom, theme.edge, zoom);
        }
    }
    painter.add(edges.into_shape());
    for &(s, t) in &highlighted {
        painter.line_segment([pts[s], pts[t]], hi_stroke);
        if st.cfg.directed {
            arrow(&painter, pts[s], pts[t], st.radius[t] * zoom, theme.fg, zoom);
        }
    }

    let mut discs = Batch::new(ppp);
    let mut rings: Vec<(Pos2, f32, Color32, Stroke)> = Vec::new();
    let mut labels: Vec<(usize, f32)> = Vec::new();
    for (i, n) in model.graph.nodes.iter().enumerate() {
        if !vis[i] || !cull.contains(pts[i]) {
            continue;
        }
        let r = (st.radius[i] * zoom).max(1.5);
        let is_cur = Some(i) == cur;
        let is_hov = Some(i) == hov;
        if n.ghost {
            rings.push((pts[i], r, theme.bg, Stroke::new(1.0, theme.muted)));
        } else {
            let fill = st
                .node_accent
                .get(i)
                .copied()
                .flatten()
                .filter(|_| st.colors_on)
                .map_or(theme.muted, |a| st.accent(a));
            discs.disc(pts[i], r, fill);
        }
        if is_cur {
            rings.push((pts[i], r + 2.0, Color32::TRANSPARENT, Stroke::new(2.0, theme.fg)));
        } else if is_hov {
            rings.push((pts[i], r + 1.0, Color32::TRANSPARENT, Stroke::new(1.5, theme.fg)));
        }
        // world-space label size
        let world = 11.0 + 3.0 * ((st.radius[i] - 6.0) / 14.0).clamp(0.0, 1.0);
        let mut size = world * zoom;
        if is_cur || is_hov {
            size = size.max(12.0);
        }
        if size >= LABEL_MIN {
            labels.push((i, size.min(LABEL_MAX)));
        }
    }
    painter.add(discs.into_shape());
    for (c, r, fill, stroke) in rings {
        painter.circle(c, r, fill, stroke);
    }
    let mut text = Mesh::default();
    let fill = ctx.fonts(|f| f.font_atlas_fill_ratio());
    if fill < st.label_key.1 || ppp != st.label_key.0 {
        st.label_cache.clear(); // atlas rebuilt (UVs moved) or new scale factor
    }
    st.label_key = (ppp, fill);
    st.label_cache.resize(model.graph.nodes.len(), Default::default());
    let cache = &mut st.label_cache;
    ctx.fonts_mut(|fonts| {
        let mut placed = Vec::with_capacity(labels.len());
        for (i, size) in labels {
            let n = &model.graph.nodes[i];
            let r = (st.radius[i] * zoom).max(1.5);
            let color = if n.ghost { theme.muted } else { theme.fg };
            // Lay out at the exact pixel size it is drawn at (whole physical pixels), and
            // draw unscaled at a pixel-aligned origin: glyphs map 1:1 to screen pixels.
            let px = (size * ppp).round().max(1.0) as u16;
            let slots = &mut cache[i];
            let galley = match slots.iter().flatten().find(|(p, _)| *p == px) {
                Some((_, g)) => g.clone(),
                None => {
                    let g = fonts.layout_no_wrap(n.title.clone(), FontId::proportional(px as f32 / ppp), Color32::WHITE);
                    slots[1] = slots[0].take();
                    slots[0] = Some((px, g.clone()));
                    g
                }
            };
            let top_left = (pts[i] + vec2(-galley.size().x * 0.5, r + 2.0)).round_to_pixels(ppp);
            placed.push((galley, top_left, color));
        }
        // Galley UVs are in texels; normalize by the atlas size after all layouts (which
        // may have grown the atlas), as epaint's own text tessellation does.
        let [w, h] = fonts.font_image_size();
        let uv_scale = vec2(1.0 / w.max(1) as f32, 1.0 / h.max(1) as f32);
        for (galley, top_left, color) in &placed {
            append_text(&mut text, galley, *top_left, *color, uv_scale);
        }
    });
    painter.add(Shape::mesh(text));
}

/// Appends a laid-out text (white) to `out` at `top_left`, tinted with `color`.
/// `uv_scale` = 1 / font atlas size: galley UVs are in texels.
fn append_text(out: &mut Mesh, galley: &egui::Galley, top_left: Pos2, color: Color32, uv_scale: Vec2) {
    for placed in &galley.rows {
        let m = &placed.row.visuals.mesh;
        let base = out.vertices.len() as u32;
        let off = placed.pos.to_vec2();
        out.vertices.extend(m.vertices.iter().map(|v| egui::epaint::Vertex {
            pos: top_left + v.pos.to_vec2() + off,
            uv: (v.uv.to_vec2() * uv_scale).to_pos2(),
            color: color.gamma_multiply(v.color.a() as f32 / 255.0),
        }));
        out.indices.extend(m.indices.iter().map(|ix| base + ix));
    }
}

fn arrow(p: &egui::Painter, a: Pos2, b: Pos2, r: f32, color: Color32, zoom: f32) {
    let d = b - a;
    let len = d.length();
    if len < r + 4.0 {
        return;
    }
    let u = d / len;
    let tip = b - u * (r + 1.0);
    let s = (7.0 * zoom.sqrt()).clamp(3.0, 9.0);
    let n = u.rot90();
    let base = tip - u * s;
    p.add(egui::Shape::convex_polygon(vec![tip, base + n * s * 0.5, base - n * s * 0.5], color, Stroke::NONE));
}

// ---- overlays --------------------------------------------------------------------------------

fn overlays(ctx: &egui::Context, st: &mut State, theme: &Theme) {
    let screen = ctx.content_rect();
    let painter = ctx.layer_painter(egui::LayerId::new(Order::Background, Id::new("overlay")));

    match &st.model {
        None => {
            painter.text(screen.center(), Align2::CENTER_CENTER, "Loading notes", FontId::proportional(14.0), theme.muted);
        }
        Some(m) if m.graph.nodes.is_empty() => {
            painter.text(screen.center(), Align2::CENTER_CENTER, "No notes found", FontId::proportional(14.0), theme.muted);
        }
        _ => {}
    }

    // error banner (bottom left), else toast
    if let Some(err) = st.error.clone() {
        egui::Area::new(Id::new("banner"))
            .anchor(Align2::LEFT_BOTTOM, vec2(12.0, -12.0))
            .order(Order::Foreground)
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(theme.panel)
                    .stroke(Stroke::new(1.0, theme.danger))
                    .corner_radius(6)
                    .inner_margin(egui::Margin::symmetric(12, 8))
                    .show(ui, |ui| {
                        ui.set_max_width((screen.width() * 0.7).min(640.0));
                        ui.label(egui::RichText::new(err).size(12.0).color(theme.fg));
                    });
            });
    } else if let Some((msg, t)) = &st.toast {
        let left = TOAST.saturating_sub(t.elapsed()).as_secs_f32();
        let alpha = (left / 0.4).clamp(0.0, 1.0);
        painter.text(
            screen.left_bottom() + vec2(12.0, -12.0),
            Align2::LEFT_BOTTOM,
            msg,
            FontId::proportional(12.0),
            theme.muted.gamma_multiply(alpha),
        );
    }

    // tooltip
    if let (Some((i, since)), Drag::None) = (st.hover, st.drag) {
        if since.elapsed() >= TOOLTIP_DELAY {
            if let (Some(m), Some(p)) = (&st.model, ctx.pointer_hover_pos()) {
                let n = &m.graph.nodes[i];
                egui::Area::new(Id::new("tip"))
                    .fixed_pos(p + vec2(14.0, 16.0))
                    .order(Order::Tooltip)
                    .interactable(false)
                    .constrain(true)
                    .show(ctx, |ui| {
                        egui::Frame::new()
                            .fill(theme.panel)
                            .stroke(Stroke::new(1.0, theme.border))
                            .corner_radius(6)
                            .inner_margin(egui::Margin::symmetric(9, 6))
                            .shadow(ctx.global_style().visuals.popup_shadow)
                            .show(ui, |ui| {
                                ui.set_max_width(420.0);
                                ui.spacing_mut().item_spacing.y = 2.0;
                                ui.label(egui::RichText::new(&n.title).strong().color(theme.fg));
                                let meta = |ui: &mut egui::Ui, s: String| {
                                    ui.label(egui::RichText::new(s).size(12.0).color(theme.muted));
                                };
                                meta(ui, n.id.clone());
                                if n.ghost {
                                    meta(ui, "Outside the current zk selection".into());
                                } else {
                                    if !n.tags.is_empty() {
                                        meta(ui, n.tags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" "));
                                    }
                                    let pl = |k: u32, w: &str| format!("{k} {w}{}", if k == 1 { "" } else { "s" });
                                    meta(ui, format!("{}, {}", pl(n.backlinks, "backlink"), pl(n.words, "word")));
                                }
                            });
                    });
            }
        }
    }
}

// ---- filter panel ----------------------------------------------------------------------------

/// A tag chip: optional color dot, name, count (or ×). Inverted when pressed.
fn chip(ui: &mut egui::Ui, theme: &Theme, tag: &str, trailing: &str, dot: Option<Color32>, pressed: bool) -> egui::Response {
    let font = FontId::proportional(12.0);
    let (fg, bg) = if pressed { (theme.panel, theme.fg) } else { (theme.fg, theme.field) };
    let name = ui.painter().layout_no_wrap(tag.to_string(), font.clone(), fg);
    let tail = ui.painter().layout_no_wrap(trailing.to_string(), font, fg.gamma_multiply(0.65));
    let dot_w = if dot.is_some() { 13.0 } else { 0.0 };
    let size = vec2(8.0 + dot_w + name.size().x + 5.0 + tail.size().x + 8.0, 20.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let border = if pressed {
            theme.fg
        } else if resp.hovered() {
            theme.muted
        } else {
            theme.border
        };
        let p = ui.painter();
        p.rect(rect, CornerRadius::same(10), bg, Stroke::new(1.0, border), StrokeKind::Inside);
        let mut x = rect.left() + 8.0;
        if let Some(c) = dot {
            p.circle_filled(pos2(x + 4.0, rect.center().y), 4.0, c);
            x += dot_w;
        }
        let y = rect.center().y;
        p.galley(pos2(x, y - name.size().y / 2.0), name.clone(), fg);
        x += name.size().x + 5.0;
        p.galley(pos2(x, y - tail.size().y / 2.0), tail, fg);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn toggle_button(ui: &mut egui::Ui, theme: &Theme, label: &str, on: bool) -> egui::Response {
    let text = egui::RichText::new(label).color(if on { theme.fg } else { theme.muted });
    let stroke = Stroke::new(1.0, if on { theme.muted } else { theme.border });
    ui.add(egui::Button::new(text).stroke(stroke).fill(theme.field))
}

fn panel(ctx: &egui::Context, st: &mut State, theme: &Theme) {
    let Some(model) = st.model.as_ref() else { return };
    let tag_count = model.order.len();
    let active = st.filter.active();

    let mut changed = false;
    let mut fit = false;
    let mut refollow = false;

    egui::Area::new(Id::new("zkf"))
        .anchor(Align2::RIGHT_TOP, vec2(-12.0, 12.0))
        .order(Order::Foreground)
        .show(ctx, |ui| {
            ui.with_layout(Layout::top_down(Align::Max), |ui| {
                let label = if active > 0 { format!("Filter ({active})") } else { "Filter".to_string() };
                let text = egui::RichText::new(label).color(theme.fg);
                let text = if active > 0 { text.strong() } else { text };
                let border = if active > 0 { theme.fg } else { theme.border };
                if ui
                    .add(egui::Button::new(text).fill(theme.panel).stroke(Stroke::new(1.0, border)).min_size(vec2(0.0, 26.0)))
                    .clicked()
                {
                    st.panel_open = !st.panel_open;
                    st.focus_search = st.panel_open;
                }
                if !st.panel_open {
                    return;
                }
                egui::Frame::new()
                    .fill(theme.panel)
                    .stroke(Stroke::new(1.0, theme.border))
                    .corner_radius(8)
                    .inner_margin(10)
                    .shadow(Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(64) })
                    .show(ui, |ui| {
                        ui.set_width(280.0);
                        ui.with_layout(Layout::top_down(Align::Min), |ui| {
                            ui.spacing_mut().item_spacing = vec2(4.0, 8.0);

                            // search with suggestions
                            let search_id = Id::new("zkf-search");
                            let has_focus = ui.memory(|m| m.has_focus(search_id));
                            let suggestions = model.search(&st.search, &st.filter.sel, 8);
                            if has_focus && !suggestions.is_empty() {
                                ui.input_mut(|i| {
                                    if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                                        st.search_idx = (st.search_idx + 1).min(suggestions.len() - 1);
                                    }
                                    if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                                        st.search_idx = st.search_idx.saturating_sub(1);
                                    }
                                });
                            }
                            st.search_idx = st.search_idx.min(suggestions.len().saturating_sub(1));
                            let te = TextEdit::singleline(&mut st.search)
                                .id(search_id)
                                .hint_text(format!("Search {tag_count} tags"))
                                .desired_width(f32::INFINITY)
                                .margin(vec2(8.0, 5.0));
                            let r = ui.add(te);
                            if st.focus_search {
                                r.request_focus();
                                st.focus_search = false;
                            }
                            if r.changed() {
                                st.search_idx = 0;
                            }
                            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                            let escape = ui.input(|i| i.key_pressed(Key::Escape));
                            if escape && (has_focus || r.lost_focus()) {
                                st.search.clear();
                            }
                            let mut picked: Option<String> = None;
                            if enter {
                                picked = suggestions.get(st.search_idx).cloned();
                                if picked.is_some() {
                                    r.request_focus(); // keep typing the next tag
                                }
                            }
                            if (has_focus || r.has_focus()) && !suggestions.is_empty() && !st.search.is_empty() {
                                egui::Frame::new()
                                    .fill(theme.field)
                                    .stroke(Stroke::new(1.0, theme.border))
                                    .corner_radius(6)
                                    .inner_margin(4)
                                    .show(ui, |ui| {
                                        ui.spacing_mut().item_spacing.y = 0.0;
                                        for (k, tag) in suggestions.iter().enumerate() {
                                            let row = ui.allocate_response(vec2(ui.available_width(), 22.0), Sense::click());
                                            let sel = k == st.search_idx || row.hovered();
                                            if sel {
                                                ui.painter().rect_filled(row.rect, 4, theme.border);
                                            }
                                            let y = row.rect.center().y;
                                            ui.painter().text(pos2(row.rect.left() + 6.0, y), Align2::LEFT_CENTER, tag, FontId::proportional(12.5), theme.fg);
                                            let n = model.count(tag);
                                            ui.painter().text(
                                                pos2(row.rect.right() - 6.0, y),
                                                Align2::RIGHT_CENTER,
                                                format!("{n} note{}", if n == 1 { "" } else { "s" }),
                                                FontId::proportional(11.5),
                                                theme.muted,
                                            );
                                            // pointer press, not click: the text field loses focus first
                                            if row.is_pointer_button_down_on() || row.clicked() {
                                                picked = Some(tag.clone());
                                                ui.memory_mut(|m| m.request_focus(search_id));
                                            }
                                        }
                                    });
                            }
                            if let Some(tag) = picked {
                                st.filter.add(&tag);
                                st.search.clear();
                                st.search_idx = 0;
                                changed = true;
                            }

                            let (colors_on, tag_accent) = (st.colors_on, st.tag_accent.clone());
                            let dot = |tag: &str| -> Option<Color32> {
                                let a = &theme.accents;
                                colors_on.then(|| tag_accent.get(tag).map(|&i| a[i % a.len()])).flatten()
                            };
                            let top: Vec<String> = model.order.iter().take(st.cfg.top).cloned().collect();

                            // selected tags outside the top list (removable)
                            let extra: Vec<String> = st.filter.sel.iter().filter(|t| !top.contains(t)).cloned().collect();
                            if !extra.is_empty() {
                                ui.horizontal_wrapped(|ui| {
                                    ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
                                    for tag in &extra {
                                        if chip(ui, theme, tag, "\u{00d7}", dot(tag), true).clicked() {
                                            st.filter.toggle(tag);
                                            changed = true;
                                        }
                                    }
                                });
                                ui.separator();
                            }

                            // most used tags
                            egui::ScrollArea::vertical().max_height(ctx.content_rect().height() * 0.5).show(ui, |ui| {
                                ui.horizontal_wrapped(|ui| {
                                    ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
                                    for tag in &top {
                                        let on = st.filter.sel.contains(tag);
                                        let resp = chip(ui, theme, tag, &model.count(tag).to_string(), dot(tag), on);
                                        if resp.clicked() {
                                            st.filter.toggle(tag);
                                            changed = true;
                                        }
                                    }
                                });
                            });

                            ui.horizontal(|ui| {
                                let mode = st.filter.mode;
                                egui::ComboBox::from_id_salt("zkf-mode")
                                    .selected_text(if mode == Mode::Any { "Any tag" } else { "All tags" })
                                    .width(84.0)
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(&mut st.filter.mode, Mode::Any, "Any tag");
                                        ui.selectable_value(&mut st.filter.mode, Mode::All, "All tags");
                                    });
                                changed |= st.filter.mode != mode;

                                let scope = st.filter.scope;
                                let names = ["All notes", "1 link away", "2 links away", "3 links away"];
                                egui::ComboBox::from_id_salt("zkf-scope")
                                    .selected_text(names[scope as usize])
                                    .width(100.0)
                                    .show_ui(ui, |ui| {
                                        for (k, name) in names.iter().enumerate() {
                                            ui.selectable_value(&mut st.filter.scope, k as u8, *name);
                                        }
                                    });
                                if st.filter.scope != scope {
                                    changed = true;
                                    fit = true;
                                    if st.filter.scope > 0 && st.current.is_none() {
                                        st.toast = Some(("Open a note in nvim to centre the local view".into(), Instant::now()));
                                    }
                                }
                                if ui.button("Clear").clicked() {
                                    st.filter = Filter::default();
                                    changed = true;
                                    fit = true;
                                }
                            });

                            ui.horizontal(|ui| {
                                if toggle_button(ui, theme, "Colors", st.colors_on).clicked() {
                                    st.colors_on = !st.colors_on;
                                }
                                if toggle_button(ui, theme, "Follow", st.follow).clicked() {
                                    st.follow = !st.follow;
                                    refollow = st.follow;
                                }
                                if ui.button("Fit").clicked() {
                                    fit = true;
                                }
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    let total = model.notes;
                                    let shown = model
                                        .graph
                                        .nodes
                                        .iter()
                                        .zip(&st.visible)
                                        .filter(|(n, v)| !n.ghost && **v)
                                        .count();
                                    let s = |n: usize| format!("{n} note{}", if n == 1 { "" } else { "s" });
                                    let text = if shown == total { s(total) } else { format!("{shown} of {}", s(total)) };
                                    ui.label(egui::RichText::new(text).size(12.0).color(theme.muted));
                                });
                            });
                        });
                    });
            });
        });

    if changed {
        st.dirty = true;
        st.derive();
    }
    if fit {
        st.camera.lock = Some(Lock::Fit);
    }
    if refollow {
        st.follow_current(false);
    }
}

#[cfg(test)]
#[path = "bench.rs"]
mod bench;
