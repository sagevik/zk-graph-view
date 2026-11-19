//! View configuration, passed by the nvim plugin as `--config <json>`. Parsed leniently:
//! anything missing or malformed falls back to the default.

use eframe::egui::Color32;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub dark: bool,
    pub bg: Color32,
    pub panel: Color32,
    pub field: Color32,
    pub fg: Color32,
    pub muted: Color32,
    pub border: Color32,
    /// Edge color with `edge_opacity` already applied.
    pub edge: Color32,
    pub edge_width: f32,
    pub focus: Color32,
    pub danger: Color32,
    pub accents: Vec<Color32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub theme: Theme,
    pub top: usize,
    pub legend_open: bool,
    pub tag_colors: bool,
    pub follow: bool,
    pub directed: bool,
    pub stabilize_iterations: usize,
    pub color_tags: Vec<String>,
    pub color_max_share: f32,
}

pub fn hex(s: &str) -> Option<Color32> {
    let s = s.trim().strip_prefix('#')?;
    let v = |i: usize, n: usize| u8::from_str_radix(s.get(i..i + n)?, 16).ok();
    match s.len() {
        3 => Some(Color32::from_rgb(v(0, 1)? * 17, v(1, 1)? * 17, v(2, 1)? * 17)),
        6 => Some(Color32::from_rgb(v(0, 2)?, v(2, 2)?, v(4, 2)?)),
        8 => Some(Color32::from_rgba_unmultiplied(v(0, 2)?, v(2, 2)?, v(4, 2)?, v(6, 2)?)),
        _ => None,
    }
}

fn with_opacity(c: Color32, a: f32) -> Color32 {
    let [r, g, b, _] = c.to_srgba_unmultiplied();
    Color32::from_rgba_unmultiplied(r, g, b, (a.clamp(0.0, 1.0) * 255.0).round() as u8)
}

impl Theme {
    /// The plain look used with `theme = { enabled = false }` (no theme in the config).
    pub fn light() -> Theme {
        let c = |s| hex(s).unwrap();
        Theme {
            dark: false,
            bg: c("#ffffff"),
            panel: c("#ffffff"),
            field: c("#f6f8fa"),
            fg: c("#24292f"),
            muted: c("#8c959f"),
            border: c("#d0d7de"),
            edge: with_opacity(c("#8c959f"), 0.6),
            edge_width: 1.0,
            focus: c("#0969da"),
            danger: c("#cf222e"),
            accents: [
                "#0969da", "#bf8700", "#1a7f37", "#cf222e", "#8250df", "#1b7c83", "#bc4c00",
                "#4d2d8f", "#953800", "#116329", "#a40e26", "#0550ae",
            ]
            .iter()
            .map(|s| c(s))
            .collect(),
        }
    }

    fn from_json(v: &Value) -> Theme {
        let mut t = Theme::light();
        t.dark = true; // the plugin only sends a theme when theming is on (dark palette)
        let col = |k: &str, d: Color32| v.get(k).and_then(Value::as_str).and_then(hex).unwrap_or(d);
        t.bg = col("bg", Color32::BLACK);
        t.panel = col("panel", Color32::from_rgb(0x1c, 0x1c, 0x24));
        t.field = col("field", Color32::from_rgb(0x14, 0x14, 0x15));
        t.fg = col("fg", Color32::from_rgb(0xcd, 0xcd, 0xcd));
        t.muted = col("muted", Color32::from_rgb(0x60, 0x60, 0x79));
        t.border = col("border", Color32::from_rgb(0x25, 0x25, 0x30));
        t.focus = col("focus", Color32::from_rgb(0x7e, 0x98, 0xe8));
        t.danger = col("danger", Color32::from_rgb(0xd8, 0x64, 0x7e));
        let opacity = v.get("edge_opacity").and_then(Value::as_f64).unwrap_or(0.8) as f32;
        t.edge = with_opacity(col("edge", Color32::from_rgb(0x87, 0x87, 0x87)), opacity);
        t.edge_width = v.get("edge_width").and_then(Value::as_f64).unwrap_or(1.0).clamp(0.25, 8.0) as f32;
        let accents: Vec<Color32> = v
            .get("accents")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).filter_map(hex).collect())
            .unwrap_or_default();
        if !accents.is_empty() {
            t.accents = accents;
        }
        t
    }
}

impl Default for Config {
    fn default() -> Config {
        Config {
            theme: Theme::light(),
            top: 12,
            legend_open: true,
            tag_colors: true,
            follow: true,
            directed: false,
            stabilize_iterations: 200,
            color_tags: Vec::new(),
            color_max_share: 0.5,
        }
    }
}

impl Config {
    pub fn parse(json: &str) -> Result<Config, String> {
        let v: Value = serde_json::from_str(json).map_err(|e| format!("--config: {e}"))?;
        let mut c = Config::default();
        let flag = |k: &str, d: bool| v.get(k).and_then(Value::as_bool).unwrap_or(d);
        if let Some(t) = v.get("theme").filter(|t| t.is_object()) {
            c.theme = Theme::from_json(t);
        }
        if let Some(top) = v.pointer("/filter/top").and_then(Value::as_u64) {
            c.top = top.min(200) as usize;
        }
        c.legend_open = flag("legend_open", c.legend_open);
        c.tag_colors = flag("tag_colors", c.tag_colors);
        c.follow = flag("follow", c.follow);
        c.directed = flag("directed", c.directed);
        if let Some(n) = v.get("stabilize_iterations").and_then(Value::as_u64) {
            c.stabilize_iterations = n.min(5000) as usize;
        }
        // vim.json encodes an empty Lua table as [] (or {}): both mean "none"
        if let Some(a) = v.get("color_tags").and_then(Value::as_array) {
            c.color_tags = a.iter().filter_map(Value::as_str).map(str::to_string).collect();
        }
        if let Some(s) = v.get("color_max_share").and_then(Value::as_f64) {
            c.color_max_share = s.clamp(0.0, 1.0) as f32;
        }
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex() {
        assert_eq!(hex("#7e98e8"), Some(Color32::from_rgb(0x7e, 0x98, 0xe8)));
        assert_eq!(hex("#fff"), Some(Color32::WHITE));
        assert_eq!(hex("7e98e8"), None);
        assert_eq!(hex("#zzzzzz"), None);
    }

    #[test]
    fn parses_plugin_config() {
        let c = Config::parse(
            r##"{"theme":{"bg":"#000000","fg":"#cdcdcd","edge":"#878787","edge_opacity":0.5,
                "accents":["#7e98e8","#e8b589"]},"filter":{"top":8},"legend_open":false,
                "tag_colors":true,"follow":false,"directed":true,"stabilize_iterations":50,
                "color_tags":{},"color_max_share":0.4}"##,
        )
        .unwrap();
        assert!(c.theme.dark);
        assert_eq!(c.theme.bg, Color32::BLACK);
        assert_eq!(c.theme.edge.a(), 128);
        assert_eq!(c.theme.accents.len(), 2);
        assert_eq!((c.top, c.legend_open, c.follow, c.directed), (8, false, false, true));
        assert_eq!(c.stabilize_iterations, 50);
        assert!(c.color_tags.is_empty(), "an empty Lua table may arrive as {{}}");
        assert!((c.color_max_share - 0.4).abs() < 1e-6);
    }

    #[test]
    fn missing_theme_means_light() {
        let c = Config::parse("{}").unwrap();
        assert!(!c.theme.dark);
        assert!(Config::parse("nope").is_err());
    }
}
