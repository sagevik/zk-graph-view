//! zk-graph: a live, GPU-rendered graph view of a zk notebook, driven by Neovim.
//!
//! Started by the zk-graph.lua plugin, one process per notebook. Talks to nvim over
//! stdin/stdout with newline-delimited JSON (see `proto`); logs go to stderr.
//!
//! Threads: `stdin` (nvim messages), `watch` (debounced file changes), `reload`
//! (`zk graph` + parse), and the main thread, which owns all view state and the window.
//! While hidden there is no window; the main thread just applies incoming events.

mod app;
mod config;
mod graph;
mod layout;
mod mesh;
mod model;
mod proto;
mod watch;
mod zk;

use app::{UiEvent, Waker};
use clap::Parser;
use eframe::egui;
use std::cell::RefCell;
use std::io::BufRead;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(name = "zk-graph", version, about = "Live graph view for zk notebooks, driven by Neovim")]
struct Args {
    /// Notebook root
    #[arg(long)]
    root: PathBuf,
    /// zk executable
    #[arg(long, default_value = "zk")]
    zk: String,
    /// View config as JSON (theme, filter, ...), as produced by the nvim plugin
    #[arg(long, default_value = "{}")]
    config: String,
    /// Start without a window; it opens on a "show" message
    #[arg(long)]
    hidden: bool,
    /// Wayland app_id / X11 WM_CLASS, for compositor window rules
    #[arg(long = "app-id", default_value = "zk-graph")]
    app_id: String,
    /// Window title
    #[arg(long, default_value = "zk-graph")]
    title: String,
    /// Comma-separated note file extensions to watch
    #[arg(long, default_value = "md")]
    ext: String,
    /// Quiet period after file changes before re-reading the graph (e.g. 250ms, 1s)
    #[arg(long, default_value = "250ms", value_parser = parse_duration)]
    debounce: Duration,
    /// Accepted for compatibility with the Go/Wails version; rendering always uses the GPU
    #[arg(long, default_value = "never")]
    gpu: String,
    /// Initial window width
    #[arg(long, default_value_t = 1200.0)]
    width: f32,
    /// Initial window height
    #[arg(long, default_value_t = 850.0)]
    height: f32,
    /// Extra `zk graph` arguments, after `--`
    #[arg(last = true)]
    zk_args: Vec<String>,
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len()));
    let v: f64 = num.trim().parse().map_err(|_| format!("bad duration {s:?}"))?;
    let secs = match unit {
        "ms" | "" => v / 1000.0,
        "s" => v,
        "m" => v * 60.0,
        _ => return Err(format!("bad duration unit in {s:?} (use ms, s or m)")),
    };
    if !(0.0..=3600.0).contains(&secs) {
        return Err(format!("duration out of range: {s:?}"));
    }
    Ok(Duration::from_secs_f64(secs))
}

/// Forwards warnings and errors from eframe/winit/wgpu (the `log` crate) to stderr, so
/// they show in :ZkGraphLog. `ZK_GRAPH_LOG=info|debug|trace` for more.
struct StderrLog;

impl log::Log for StderrLog {
    fn enabled(&self, m: &log::Metadata<'_>) -> bool {
        m.level() <= log::max_level()
    }
    fn log(&self, r: &log::Record<'_>) {
        if self.enabled(r.metadata()) {
            eprintln!("zk-graph: [{} {}] {}", r.level(), r.target(), r.args());
        }
    }
    fn flush(&self) {}
}

fn init_logging() {
    static LOGGER: StderrLog = StderrLog;
    let level = match std::env::var("ZK_GRAPH_LOG").unwrap_or_default().to_lowercase().as_str() {
        "trace" => log::LevelFilter::Trace,
        "debug" => log::LevelFilter::Debug,
        "info" => log::LevelFilter::Info,
        _ => log::LevelFilter::Warn,
    };
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(level);
    }
}

fn log(msg: impl AsRef<str>) {
    eprintln!("zk-graph: {}", msg.as_ref());
}

/// Runs `zk graph` on every trigger (coalescing bursts) and forwards changed graphs.
fn spawn_reloader(args: &Args, root: PathBuf, rx: Receiver<()>, ui: Sender<UiEvent>, waker: Waker) {
    let (bin, extra) = (args.zk.clone(), args.zk_args.clone());
    thread::Builder::new()
        .name("reload".into())
        .spawn(move || {
            let mut last: Option<Arc<graph::Graph>> = None;
            let mut last_err: Option<String> = None;
            while rx.recv().is_ok() {
                while rx.try_recv().is_ok() {} // coalesce
                let t0 = Instant::now();
                let res = zk::graph(&bin, &root, &extra).and_then(|d| graph::Graph::parse(&d, &root));
                match res {
                    Ok(g) => {
                        last_err = None;
                        let changed = last.as_deref() != Some(&g);
                        log(format!(
                            "reload: {} nodes, {} edges, changed={changed}, {}ms",
                            g.nodes.len(),
                            g.edges.len(),
                            t0.elapsed().as_millis()
                        ));
                        if !changed {
                            continue;
                        }
                        let g = Arc::new(g);
                        if last.is_none() {
                            proto::send(&proto::Out::Ready { notes: g.nodes.len() });
                        }
                        last = Some(g.clone());
                        let _ = ui.send(UiEvent::Graph(g));
                    }
                    Err(e) => {
                        log(&e);
                        if last_err.as_ref() != Some(&e) {
                            proto::send(&proto::Out::Error { message: e.clone() });
                        }
                        last_err = Some(e.clone());
                        let _ = ui.send(UiEvent::LoadError(e));
                    }
                }
                app::wake(&waker);
            }
        })
        .expect("spawn reload thread");
}

/// nvim -> app. EOF (nvim exited or crashed) ends the process.
fn spawn_stdin(ui: Sender<UiEvent>, reload: Sender<()>, waker: Waker) {
    thread::Builder::new()
        .name("stdin".into())
        .spawn(move || {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                let ev = match proto::parse(&line) {
                    Ok(proto::In::Show) => UiEvent::Show,
                    Ok(proto::In::Hide) => UiEvent::Hide,
                    Ok(proto::In::Current { id }) => UiEvent::Current(id),
                    Ok(proto::In::Refresh) => {
                        let _ = reload.send(());
                        continue;
                    }
                    Ok(proto::In::Quit) => break,
                    Err(e) => {
                        log(format!("stdin: bad message {line:?}: {e}"));
                        continue;
                    }
                };
                let _ = ui.send(ev);
                app::wake(&waker);
            }
            // Shut down on the main thread (closes the window and GL context in order);
            // force it if that does not happen promptly.
            let _ = ui.send(UiEvent::Quit);
            app::wake(&waker);
            thread::sleep(Duration::from_secs(1));
            log("forced exit");
            std::process::exit(0);
        })
        .expect("spawn stdin thread");
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Backend {
    Wgpu,
    Glow,
}

/// `ZK_GRAPH_RENDERER=glow` (or `gl`) selects OpenGL; anything else wgpu (Vulkan first).
fn backend_from_env() -> Backend {
    match std::env::var("ZK_GRAPH_RENDERER").unwrap_or_default().to_lowercase().as_str() {
        "glow" | "gl" | "opengl" => Backend::Glow,
        _ => Backend::Wgpu,
    }
}

fn native_options(args: &Args, size: egui::Vec2, backend: Backend) -> eframe::NativeOptions {
    let mut wgpu_options = eframe::WgpuConfiguration {
        // one frame in flight: lowest input-to-screen latency, plenty for this workload
        surface: eframe::SurfaceConfig::LOW_LATENCY,
        ..Default::default()
    };
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(c) = &mut wgpu_options.wgpu_setup {
        // integrated GPU unless WGPU_POWER_PREF says otherwise: no reason to wake a dGPU
        if std::env::var_os("WGPU_POWER_PREF").is_none() {
            c.power_preference = eframe::wgpu::PowerPreference::LowPower;
        }
    }
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(&args.title)
            .with_app_id(&args.app_id)
            .with_inner_size(size)
            .with_min_inner_size([400.0, 300.0]),
        run_and_return: true,
        renderer: match backend {
            Backend::Wgpu => eframe::Renderer::Wgpu,
            Backend::Glow => eframe::Renderer::Glow,
        },
        wgpu_options,
        ..Default::default()
    }
}

/// Logs which GPU/driver actually renders (shown in :ZkGraphLog).
fn log_renderer(cc: &eframe::CreationContext<'_>) {
    if let Some(rs) = &cc.wgpu_render_state {
        let i = rs.adapter.get_info();
        log(format!(
            "renderer: wgpu {:?}, {} ({:?}), driver {} {}",
            i.backend, i.name, i.device_type, i.driver, i.driver_info
        ));
    } else if let Some(gl) = &cc.gl {
        use eframe::glow::HasContext as _;
        // SAFETY: plain string queries on the current context during creation
        let (vendor, renderer, version) = unsafe {
            (
                gl.get_parameter_string(eframe::glow::VENDOR),
                gl.get_parameter_string(eframe::glow::RENDERER),
                gl.get_parameter_string(eframe::glow::VERSION),
            )
        };
        log(format!("renderer: OpenGL, {renderer} ({vendor}), {version}"));
        if renderer.to_lowercase().contains("llvmpipe") || renderer.to_lowercase().contains("softpipe") {
            log("warning: software rendering (llvmpipe): the GPU driver is not in use");
        }
    }
}

thread_local! {
    /// Renderer that worked last time (a failed wgpu init falls back to OpenGL for good).
    static BACKEND: std::cell::Cell<Option<Backend>> = const { std::cell::Cell::new(None) };
}

fn run_window(args: &Args, state: Rc<RefCell<app::State>>, rx: Rc<Receiver<UiEvent>>, waker: Waker) -> Result<(), String> {
    let size = state.borrow().window_size.unwrap_or(egui::vec2(args.width, args.height));
    let first = BACKEND.get().unwrap_or_else(backend_from_env);
    let mut res = Ok(());
    for backend in [first, Backend::Glow] {
        let (w, st, r) = (waker.clone(), state.clone(), rx.clone());
        res = eframe::run_native(
            &args.app_id,
            native_options(args, size, backend),
            Box::new(move |cc| {
                log_renderer(cc);
                *w.lock().unwrap_or_else(|e| e.into_inner()) = Some(cc.egui_ctx.clone());
                Ok(Box::new(app::App::new(cc, st, r)))
            }),
        );
        *waker.lock().unwrap_or_else(|e| e.into_inner()) = None;
        match &res {
            Err(e) if backend == Backend::Wgpu => log(format!("wgpu renderer failed ({e}); falling back to OpenGL")),
            _ => {
                BACKEND.set(Some(backend));
                break;
            }
        }
    }
    res.map_err(|e| e.to_string())
}

fn main() {
    let args = Args::parse();
    init_logging();
    let root = match args.root.canonicalize() {
        Ok(r) if r.is_dir() => r,
        _ => {
            log(format!("not a directory: {}", args.root.display()));
            std::process::exit(2);
        }
    };
    let cfg = match config::Config::parse(&args.config) {
        Ok(c) => c,
        Err(e) => {
            log(&e);
            std::process::exit(2);
        }
    };
    if args.gpu != "never" && args.gpu != "ondemand" && args.gpu != "always" {
        log(format!("ignoring unknown --gpu {:?}", args.gpu));
    }

    let waker: Waker = Arc::new(Mutex::new(None));
    let (ui_tx, ui_rx) = mpsc::channel::<UiEvent>();
    let (reload_tx, reload_rx) = mpsc::channel::<()>();

    spawn_reloader(&args, root.clone(), reload_rx, ui_tx.clone(), waker.clone());
    spawn_stdin(ui_tx, reload_tx.clone(), waker.clone());
    let exts = args.ext.split(',').map(|e| e.trim().trim_start_matches('.').to_lowercase()).filter(|e| !e.is_empty()).collect();
    let rt = reload_tx.clone();
    if let Err(e) = watch::spawn(watch::Config { root: root.clone(), exts, debounce: args.debounce }, move || {
        let _ = rt.send(());
    }) {
        log(format!("file watcher disabled: {e}"));
        proto::send(&proto::Out::Error { message: format!("file watcher disabled: {e}") });
    }
    let _ = reload_tx.send(());

    let state = Rc::new(RefCell::new(app::State::new(cfg)));
    let rx = Rc::new(ui_rx);
    let mut show = !args.hidden;
    loop {
        if state.borrow().quit {
            return;
        }
        if show {
            show = false;
            if let Err(e) = run_window(&args, state.clone(), rx.clone(), waker.clone()) {
                log(format!("window: {e}"));
                proto::send(&proto::Out::Error { message: format!("cannot open window: {e}") });
            }
            continue;
        }
        match rx.recv() {
            Ok(UiEvent::Show) => show = true,
            Ok(UiEvent::Hide) => {}
            Ok(UiEvent::Quit) | Err(_) => return,
            Ok(ev) => state.borrow_mut().handle(ev, None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("250ms"), Ok(Duration::from_millis(250)));
        assert_eq!(parse_duration("1s"), Ok(Duration::from_secs(1)));
        assert_eq!(parse_duration("0.5s"), Ok(Duration::from_millis(500)));
        assert_eq!(parse_duration("300"), Ok(Duration::from_millis(300)));
        assert!(parse_duration("5h").is_err());
        assert!(parse_duration("abc").is_err());
    }

    #[test]
    fn accepts_the_plugin_command_line() {
        let a = Args::try_parse_from([
            "zk-graph", "--root", "/nb", "--zk", "zk", "--config", "{}", "--title", "zk-graph",
            "--app-id", "zk-graph", "--ext", "md", "--gpu", "never", "--debounce", "250ms",
            "--width", "1200", "--height", "850", "--hidden", "--", "--exclude", "journal",
        ])
        .unwrap();
        assert!(a.hidden);
        assert_eq!(a.zk_args, ["--exclude", "journal"]);
        assert_eq!(a.debounce, Duration::from_millis(250));
    }
}
