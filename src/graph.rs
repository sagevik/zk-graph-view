//! The note graph: parsed from `zk graph --format=json`, plus change summaries between
//! successive loads (for the "Updated: ..." toast).

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// A note, or a ghost: a link endpoint outside the current zk result set (with `zk_args`
/// filters, e.g. `--tag work`, links can point at notes that were filtered out).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// Notebook-relative path as reported by zk; also the id nvim sends in `current`.
    pub id: String,
    pub title: String,
    pub path: PathBuf,
    pub tags: Vec<String>,
    pub backlinks: u32,
    pub words: u32,
    pub ghost: bool,
}

/// Immutable snapshot. Nodes are sorted by id; edges are deduplicated, directed
/// `(source, target)` index pairs, sorted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub edges: Vec<(u32, u32)>,
    pub index: HashMap<String, u32>,
}

#[derive(Deserialize)]
struct ZkNote {
    #[serde(default)]
    path: String,
    #[serde(default, rename = "absPath")]
    abs_path: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default, rename = "filenameStem")]
    filename_stem: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default, rename = "wordCount")]
    word_count: Option<u32>,
}

#[derive(Deserialize)]
struct ZkLink {
    #[serde(default, rename = "sourcePath")]
    source_path: Option<String>,
    #[serde(default, rename = "targetPath")]
    target_path: Option<String>,
}

#[derive(Deserialize)]
struct ZkGraph {
    #[serde(default)]
    notes: Vec<ZkNote>,
    #[serde(default)]
    links: Vec<ZkLink>,
}

fn stem(p: &str) -> String {
    Path::new(p)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string())
}

impl Graph {
    /// Builds a graph from `zk graph --format=json` output. `root` resolves ghost paths.
    pub fn parse(data: &[u8], root: &Path) -> Result<Graph, String> {
        let raw: ZkGraph =
            serde_json::from_slice(data).map_err(|e| format!("parse zk graph json: {e}"))?;

        let mut nodes: BTreeMap<String, Node> = BTreeMap::new();
        for n in raw.notes {
            if n.path.is_empty() {
                continue;
            }
            let title = n
                .title
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .or(n.filename_stem.filter(|s| !s.is_empty()))
                .unwrap_or_else(|| stem(&n.path));
            let path = n
                .abs_path
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| root.join(&n.path));
            nodes.insert(
                n.path.clone(),
                Node {
                    id: n.path,
                    title,
                    path,
                    tags: n.tags.unwrap_or_default(),
                    backlinks: 0,
                    words: n.word_count.unwrap_or(0),
                    ghost: false,
                },
            );
        }

        let mut links: BTreeSet<(String, String)> = BTreeSet::new();
        for l in raw.links {
            let (Some(s), Some(t)) = (l.source_path, l.target_path) else { continue };
            if s.is_empty() || t.is_empty() || s == t {
                continue; // unresolved or self link
            }
            for p in [&s, &t] {
                nodes.entry(p.clone()).or_insert_with(|| Node {
                    id: p.clone(),
                    title: stem(p),
                    path: root.join(p),
                    tags: Vec::new(),
                    backlinks: 0,
                    words: 0,
                    ghost: true,
                });
            }
            links.insert((s, t));
        }
        for (_, t) in &links {
            if let Some(n) = nodes.get_mut(t) {
                n.backlinks += 1;
            }
        }

        let nodes: Vec<Node> = nodes.into_values().collect();
        let index: HashMap<String, u32> =
            nodes.iter().enumerate().map(|(i, n)| (n.id.clone(), i as u32)).collect();
        let edges = links.iter().map(|(s, t)| (index[s], index[t])).collect();
        Ok(Graph { nodes, edges, index })
    }

    pub fn get(&self, id: &str) -> Option<usize> {
        self.index.get(id).map(|&i| i as usize)
    }

    pub fn note_count(&self) -> usize {
        self.nodes.iter().filter(|n| !n.ghost).count()
    }
}

/// What changed between two loads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    pub added: usize,
    pub changed: usize,
    pub removed: usize,
    pub links_added: usize,
    pub links_removed: usize,
}

impl Changes {
    pub fn between(old: &Graph, new: &Graph) -> Changes {
        let mut c = Changes::default();
        for n in &new.nodes {
            match old.get(&n.id).map(|i| &old.nodes[i]) {
                None if !n.ghost => c.added += 1,
                None => {}
                Some(o) if o != n => c.changed += 1,
                Some(_) => {}
            }
        }
        c.removed = old.nodes.iter().filter(|n| !n.ghost && new.get(&n.id).is_none()).count();
        let ids = |g: &Graph| -> BTreeSet<(String, String)> {
            g.edges
                .iter()
                .map(|&(s, t)| (g.nodes[s as usize].id.clone(), g.nodes[t as usize].id.clone()))
                .collect()
        };
        let (eo, en) = (ids(old), ids(new));
        c.links_added = en.difference(&eo).count();
        c.links_removed = eo.difference(&en).count();
        c
    }

    pub fn describe(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.added > 0 {
            parts.push(format!("{} added", self.added));
        }
        if self.changed > 0 {
            parts.push(format!("{} changed", self.changed));
        }
        if self.removed > 0 {
            parts.push(format!("{} removed", self.removed));
        }
        if self.links_added + self.links_removed > 0 {
            parts.push(format!("links +{} -{}", self.links_added, self.links_removed));
        }
        (!parts.is_empty()).then(|| format!("Updated: {}", parts.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "notes": [
        {"path": "a.md", "absPath": "/nb/a.md", "title": "A", "filenameStem": "a", "tags": ["x", "y"], "wordCount": 3},
        {"path": "sub/b c.md", "absPath": "/nb/sub/b c.md", "title": "", "filenameStem": "b c", "tags": null},
        {"path": "c.md", "absPath": "/nb/c.md", "title": "C", "tags": ["x"]}
      ],
      "links": [
        {"sourcePath": "a.md", "targetPath": "sub/b c.md"},
        {"sourcePath": "a.md", "targetPath": "sub/b c.md"},
        {"sourcePath": "c.md", "targetPath": "sub/b c.md"},
        {"sourcePath": "c.md", "targetPath": "c.md"},
        {"sourcePath": "c.md", "targetPath": ""},
        {"sourcePath": "c.md"},
        {"sourcePath": "a.md", "targetPath": "outside.md"}
      ]
    }"#;

    fn parse(s: &str) -> Graph {
        Graph::parse(s.as_bytes(), Path::new("/nb")).unwrap()
    }

    #[test]
    fn parses_notes_links_and_ghosts() {
        let g = parse(SAMPLE);
        assert_eq!(g.nodes.len(), 4, "3 notes + 1 ghost");
        assert_eq!(g.note_count(), 3);
        let b = &g.nodes[g.get("sub/b c.md").unwrap()];
        assert_eq!(b.title, "b c", "empty title falls back to the filename stem");
        assert!(b.tags.is_empty());
        assert_eq!(b.backlinks, 2, "duplicate link collapsed");
        let ghost = &g.nodes[g.get("outside.md").unwrap()];
        assert!(ghost.ghost);
        assert_eq!(ghost.path, PathBuf::from("/nb/outside.md"));
        assert_eq!(ghost.title, "outside");
        assert_eq!(g.edges.len(), 3, "dup, self, unresolved and missing links dropped");
        let ids: Vec<&str> = g.nodes.iter().map(|n| n.id.as_str()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted, "nodes sorted by id");
    }

    #[test]
    fn identical_loads_compare_equal() {
        assert_eq!(parse(SAMPLE), parse(SAMPLE));
        assert_eq!(Changes::between(&parse(SAMPLE), &parse(SAMPLE)).describe(), None);
    }

    #[test]
    fn summarizes_changes() {
        let old = parse(SAMPLE);
        let new = parse(
            r#"{"notes": [
              {"path": "a.md", "absPath": "/nb/a.md", "title": "A renamed", "tags": ["x", "y"], "wordCount": 3},
              {"path": "c.md", "absPath": "/nb/c.md", "title": "C", "tags": ["x", "z"]},
              {"path": "d.md", "absPath": "/nb/d.md", "title": "D", "tags": []}
            ], "links": [{"sourcePath": "d.md", "targetPath": "a.md"}]}"#,
        );
        let c = Changes::between(&old, &new);
        assert_eq!(c.added, 1);
        assert_eq!(c.changed, 2, "a: title and backlinks, c: tags");
        assert_eq!(c.removed, 1, "ghosts are not counted as removed notes");
        assert_eq!((c.links_added, c.links_removed), (1, 3));
        assert_eq!(
            c.describe().unwrap(),
            "Updated: 1 added, 2 changed, 1 removed, links +1 -3"
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(Graph::parse(b"not json", Path::new("/nb")).is_err());
    }
}
