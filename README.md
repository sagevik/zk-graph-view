# zk-graph

A live graph view for [zk](https://github.com/zk-org/zk) notebooks, driven from Neovim.
Inspired by [zk-graph-view](https://github.com/cyberSapoPerro/zk-graph-view)

## Build and install

Requirements: Rust >= 1.95 (`rustup update`, or Arch's `rust` package).

```sh
cargo build --release
install -Dm755 target/release/zk-graph ~/.local/bin/zk-graph   # replaces the Go binary
```

Check that `zk-graph` are on `$PATH`with `which -a zk-graph`, or point
the plugin at the binary: `require("zk-graph").setup({ bin = "/path/to/zk-graph" })`.

Runtime: a Vulkan driver (Arch: `vulkan-intel`, `vulkan-radeon` or `nvidia-utils`) or,
as fallback, OpenGL via EGL/GLX; libxkbcommon. All standard on a desktop Arch install.

### Renderer

By default the window renders with wgpu on Vulkan, on the integrated GPU (a hybrid laptop's
discrete GPU is left asleep). If wgpu cannot start, it falls back to OpenGL for the rest of
the session. The first lines of `:ZkGraphLog` say what is actually used, for example:

```
zk-graph: renderer: wgpu Vulkan, Intel(R) Graphics (ARL) (IntegratedGpu), driver Intel open-source Mesa driver ...
```

`llvmpipe` there means software rendering (no GPU driver in use). Override via the
plugin's `env` option:

| variable                    | effect                                             |
|-----------------------------|----------------------------------------------------|
| `ZK_GRAPH_RENDERER=glow`    | OpenGL instead of wgpu                             |
| `WGPU_BACKEND=gl`           | wgpu on OpenGL instead of Vulkan                   |
| `WGPU_POWER_PREF=high`      | prefer the discrete GPU                            |
| `ZK_GRAPH_STATS=1`          | frame rate, stutter and CPU time every 2 s         |
| `ZK_GRAPH_LOG=info`         | more eframe/winit/wgpu logging (default: warnings) |

## Neovim

```lua
require("zk-graph").setup({ theme = { bg = "#000000" } })
vim.keymap.set("n", "<leader>zg", "<cmd>ZkGraph<cr>")
```

`:ZkGraph`, `:ZkGraphHide`, `:ZkGraphRefresh`, `:ZkGraphStop`, `:ZkGraphLog` behave as
before. All view options (theme, `filter.top`, `legend_open`, `tag_colors`, `follow`,
`directed`, `stabilize_iterations`, `color_tags`, `color_max_share`) are honored. The
plugin's `gpu` option is accepted and ignored (rendering always uses the GPU); its `env`
option is passed to the process (see the table above), e.g.
`env = { ZK_GRAPH_STATS = "1" }`.

## Using the window

| Input                          | Action                                           |
|--------------------------------|--------------------------------------------------|
| mouse wheel                    | zoom around the pointer, 18% per notch           |
| drag background                | pan                                              |
| touchpad two-finger scroll     | pan                                              |
| pinch, or Ctrl + touchpad scroll | zoom                                           |
| drag node                      | move it (the layout follows, then lets go)       |
| click node                     | open the note in the Neovim that owns the window |
| hover                          | tooltip: path, tags, backlinks, words            |
| `/`                            | search tags (Up/Down, Enter adds, Esc clears)    |
| `f` / `c`                      | fit all / centre on the current note             |

Follow mode only moves the view when the note you switch to in Neovim is off screen or
near the edge, so clicking around in the graph does not make it jump.

The filter panel shows the most used tags as toggles (color dot = node color), a search
over all tags, Any/All matching, a local scope (1 to 3 links around the note in the current
buffer), and toggles for colors and follow mode.

## Window rules (mango, Hyprland, ...)

app_id (Wayland) and WM_CLASS (X11) are `zk-graph`, stable across launches:

```
windowrule=isfloating:1,width:1100,height:850,appid:^zk-graph$
```

## Protocol

```
nvim -> app   {"type":"show"} {"type":"hide"} {"type":"refresh"} {"type":"quit"}
              {"type":"current","id":"notebook/relative/path.md"}
app -> nvim   {"type":"ready","notes":N} {"type":"open","path":"/abs/path.md"}
              {"type":"error","message":"..."}
```

stdin EOF (Neovim exits or crashes) ends the process. Logs go to stderr (`:ZkGraphLog`).
Standalone: `zk-graph --root ~/notes` (`--help` for all flags).

## Troubleshooting

- **Force X11 under Wayland** (e.g. to compare): set `env = { WAYLAND_DISPLAY = "" }`.
- **Window does not come to the front on `:ZkGraph`**: Wayland does not let clients raise
  themselves; use a compositor rule or keybinding.
- **No live updates**: check `:ZkGraphLog` for watcher errors; very large trees can hit
  `fs.inotify.max_user_watches`.

## Development

```sh
cargo test                       # unit tests (graph, layout, filters, protocol, watcher)
cargo test --release bench_frames -- --ignored --nocapture   # per-frame CPU cost, headless
cargo test --release bench_tessellation -- --ignored --nocapture
cargo clippy --all-targets
```

Layout: `src/main.rs` (CLI, threads, window lifecycle), `src/app.rs` (UI), `src/layout.rs`
(simulation), `src/model.rs` (filter and color rules), `src/graph.rs` (zk JSON),
`src/watch.rs`, `src/proto.rs`, `src/config.rs`, `src/mesh.rs` (batched drawing),
`src/bench.rs` (headless frame benchmark driving the real UI with synthetic input).
