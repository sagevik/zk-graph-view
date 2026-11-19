//! Derived graph data and the filter/coloring rules (no UI here, so it is unit-testable).

use crate::graph::Graph;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Any,
    All,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    /// Selected tags, in selection order (the order decides colors while filtering).
    pub sel: Vec<String>,
    pub mode: Mode,
    /// 0 = all notes, n = notes within n links of the current note.
    pub scope: u8,
}

impl Default for Filter {
    fn default() -> Filter {
        Filter { sel: Vec::new(), mode: Mode::Any, scope: 0 }
    }
}

impl Filter {
    pub fn toggle(&mut self, tag: &str) {
        if let Some(i) = self.sel.iter().position(|t| t == tag) {
            self.sel.remove(i);
        } else {
            self.sel.push(tag.to_string());
        }
    }
    pub fn add(&mut self, tag: &str) {
        if !self.sel.iter().any(|t| t == tag) {
            self.sel.push(tag.to_string());
        }
    }
    pub fn active(&self) -> usize {
        self.sel.len() + usize::from(self.scope > 0)
    }
}

pub struct Model {
    pub graph: Arc<Graph>,
    /// Undirected neighbours.
    pub adj: Vec<Vec<u32>>,
    /// tag -> number of notes carrying it
    pub counts: HashMap<String, usize>,
    /// Tags by count (desc), then name.
    pub order: Vec<String>,
    pub notes: usize,
}

impl Model {
    pub fn new(graph: Arc<Graph>) -> Model {
        let mut adj = vec![Vec::new(); graph.nodes.len()];
        for &(s, t) in &graph.edges {
            adj[s as usize].push(t);
            adj[t as usize].push(s);
        }
        let mut counts: HashMap<String, usize> = HashMap::new();
        for n in &graph.nodes {
            for t in &n.tags {
                *counts.entry(t.clone()).or_default() += 1;
            }
        }
        let mut order: Vec<String> = counts.keys().cloned().collect();
        order.sort_by(|a, b| counts[b].cmp(&counts[a]).then_with(|| a.cmp(b)));
        let notes = graph.note_count();
        Model { graph, adj, counts, order, notes }
    }

    pub fn count(&self, tag: &str) -> usize {
        self.counts.get(tag).copied().unwrap_or(0)
    }

    fn neighbourhood(&self, start: usize, depth: u8) -> Vec<bool> {
        let mut seen = vec![false; self.adj.len()];
        seen[start] = true;
        let mut q = VecDeque::from([(start, 0u8)]);
        while let Some((x, d)) = q.pop_front() {
            if d == depth {
                continue;
            }
            for &nb in &self.adj[x] {
                let nb = nb as usize;
                if !seen[nb] {
                    seen[nb] = true;
                    q.push_back((nb, d + 1));
                }
            }
        }
        seen
    }

    /// Which nodes are shown. Ghosts are shown when linked to a shown note; with a local
    /// scope, the centre note always stays visible even if the tag filter excludes it.
    pub fn visible(&self, f: &Filter, current: Option<usize>) -> Vec<bool> {
        let nodes = &self.graph.nodes;
        let local = match current {
            Some(c) if f.scope > 0 && c < nodes.len() => Some(self.neighbourhood(c, f.scope)),
            _ => None,
        };
        let in_scope = |i: usize| local.as_ref().is_none_or(|l| l[i]);
        let mut vis = vec![false; nodes.len()];
        for (i, n) in nodes.iter().enumerate() {
            if n.ghost || !in_scope(i) {
                continue;
            }
            let centre = local.is_some() && Some(i) == current;
            let ok = f.sel.is_empty()
                || centre
                || match f.mode {
                    Mode::All => f.sel.iter().all(|t| n.tags.contains(t)),
                    Mode::Any => f.sel.iter().any(|t| n.tags.contains(t)),
                };
            vis[i] = ok;
        }
        for (i, n) in nodes.iter().enumerate() {
            if n.ghost && in_scope(i) {
                vis[i] = self.adj[i].iter().any(|&nb| vis[nb as usize]);
            }
        }
        vis
    }

    /// The tag that decides a node's color: the first selected tag it has while filtering,
    /// else the first of `color_tags` it has, else its most used tag that is on at most
    /// `max_share` of all notes (catch-all tags say little), else its most used tag.
    pub fn color_key(&self, i: usize, f: &Filter, color_tags: &[String], max_share: f32) -> Option<String> {
        let n = &self.graph.nodes[i];
        if n.ghost || n.tags.is_empty() {
            return None;
        }
        if !f.sel.is_empty() {
            return f.sel.iter().find(|t| n.tags.contains(t)).cloned();
        }
        if let Some(t) = color_tags.iter().find(|t| n.tags.contains(t)) {
            return Some(t.clone());
        }
        let cap = max_share * self.notes as f32;
        let better = |a: &String, b: Option<&String>| match b {
            None => true,
            Some(b) => {
                let (ca, cb) = (self.count(a), self.count(b));
                ca > cb || (ca == cb && a < b)
            }
        };
        let (mut best, mut fallback): (Option<&String>, Option<&String>) = (None, None);
        for t in &n.tags {
            if better(t, fallback) {
                fallback = Some(t);
            }
            if self.count(t) as f32 <= cap && better(t, best) {
                best = Some(t);
            }
        }
        best.or(fallback).cloned()
    }

    /// Accent index per node (None = uncolored) and per tag. Accents go to the color keys
    /// in use, most important first, so the biggest groups get the most distinct hues.
    pub fn colors(
        &self,
        f: &Filter,
        color_tags: &[String],
        max_share: f32,
    ) -> (Vec<Option<usize>>, HashMap<String, usize>) {
        let keys: Vec<Option<String>> = (0..self.graph.nodes.len())
            .map(|i| self.color_key(i, f, color_tags, max_share))
            .collect();
        let used: HashSet<&str> = keys.iter().flatten().map(String::as_str).collect();
        let ranked: Vec<&String> = if !f.sel.is_empty() {
            f.sel.iter().filter(|t| used.contains(t.as_str())).collect()
        } else {
            let pri: Vec<&String> = color_tags.iter().filter(|t| used.contains(t.as_str())).collect();
            let rest = self
                .order
                .iter()
                .filter(|t| used.contains(t.as_str()) && !color_tags.contains(t));
            pri.into_iter().chain(rest).collect()
        };
        let tag_accent: HashMap<String, usize> =
            ranked.iter().enumerate().map(|(i, t)| ((*t).clone(), i)).collect();
        let node_accent = keys.iter().map(|k| k.as_ref().and_then(|k| tag_accent.get(k).copied())).collect();
        (node_accent, tag_accent)
    }

    /// Tag search: exact, then prefix, then substring matches (case-insensitive), most
    /// used first within each group, excluding `skip`.
    pub fn search(&self, query: &str, skip: &[String], limit: usize) -> Vec<String> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        let mut groups: [Vec<&String>; 3] = Default::default();
        for t in &self.order {
            if skip.contains(t) {
                continue;
            }
            let l = t.to_lowercase();
            if l == q {
                groups[0].push(t);
            } else if l.starts_with(&q) {
                groups[1].push(t);
            } else if l.contains(&q) {
                groups[2].push(t);
            }
        }
        groups.iter().flatten().take(limit).map(|t| (*t).clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn model() -> Model {
        // a(work,x) -> b(work) ; c(home) -> a ; d(x) isolated ; a -> ghost g
        let json = r#"{"notes":[
            {"path":"a.md","tags":["work","x"]},
            {"path":"b.md","tags":["work"]},
            {"path":"c.md","tags":["home"]},
            {"path":"d.md","tags":["x","all"]},
            {"path":"e.md","tags":["all","work"]}
          ],"links":[
            {"sourcePath":"a.md","targetPath":"b.md"},
            {"sourcePath":"c.md","targetPath":"a.md"},
            {"sourcePath":"a.md","targetPath":"g.md"}
          ]}"#;
        Model::new(Arc::new(Graph::parse(json.as_bytes(), Path::new("/nb")).unwrap()))
    }

    fn ids(m: &Model, vis: &[bool]) -> Vec<String> {
        m.graph.nodes.iter().zip(vis).filter(|(_, v)| **v).map(|(n, _)| n.id.clone()).collect()
    }

    #[test]
    fn tag_order_by_count_then_name() {
        assert_eq!(model().order, ["work", "all", "x", "home"]);
    }

    #[test]
    fn unfiltered_shows_notes_and_linked_ghosts() {
        let m = model();
        let v = m.visible(&Filter::default(), None);
        assert_eq!(ids(&m, &v), ["a.md", "b.md", "c.md", "d.md", "e.md", "g.md"]);
    }

    #[test]
    fn any_and_all_modes() {
        let m = model();
        let mut f = Filter { sel: vec!["work".into(), "x".into()], ..Filter::default() };
        assert_eq!(ids(&m, &m.visible(&f, None)), ["a.md", "b.md", "d.md", "e.md", "g.md"]);
        f.mode = Mode::All;
        assert_eq!(ids(&m, &m.visible(&f, None)), ["a.md", "g.md"]);
    }

    #[test]
    fn local_scope_keeps_centre() {
        let m = model();
        let c = m.graph.get("c.md");
        let f = Filter { sel: vec!["work".into()], mode: Mode::Any, scope: 1 };
        // within 1 link of c: c, a. Tag filter keeps a; c stays as the centre.
        assert_eq!(ids(&m, &m.visible(&f, c)), ["a.md", "c.md"]);
        let f2 = Filter { scope: 2, ..Filter::default() };
        assert_eq!(ids(&m, &m.visible(&f2, c)), ["a.md", "b.md", "c.md", "g.md"]);
    }

    #[test]
    fn colors_skip_catch_all_tags() {
        let m = model();
        let (node, tag) = m.colors(&Filter::default(), &[], 0.5);
        let a = m.graph.get("a.md").unwrap();
        // work is on 3/5 notes (> 50%): a gets x instead
        let x = tag["x"];
        assert_eq!(node[a], Some(x));
        let b = m.graph.get("b.md").unwrap();
        assert_eq!(node[b], Some(tag["work"]), "only tag: fallback to it");
        assert_eq!(node[m.graph.get("g.md").unwrap()], None, "ghosts are uncolored");
    }

    #[test]
    fn colors_follow_priority_and_selection() {
        let m = model();
        let (node, tag) = m.colors(&Filter::default(), &["work".into()], 0.5);
        assert_eq!(tag["work"], 0);
        assert_eq!(node[m.graph.get("a.md").unwrap()], Some(0));
        let f = Filter { sel: vec!["x".into(), "work".into()], ..Filter::default() };
        let (node, tag) = m.colors(&f, &[], 0.5);
        assert_eq!((tag["x"], tag["work"]), (0, 1));
        assert_eq!(node[m.graph.get("e.md").unwrap()], Some(1));
        assert_eq!(node[m.graph.get("c.md").unwrap()], None);
    }

    #[test]
    fn search_ranks_exact_prefix_substring() {
        let m = model();
        assert_eq!(m.search("x", &[], 10), ["x"]);
        assert_eq!(m.search("o", &[], 10), ["work", "home"]);
        assert_eq!(m.search("W", &["work".into()], 10), Vec::<String>::new());
    }
}
