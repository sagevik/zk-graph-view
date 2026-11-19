//! Force-directed layout, d3-force style: link springs, Barnes-Hut many-body repulsion,
//! weak gravity towards the origin, velocity Verlet with decay, and an `alpha` that cools
//! the simulation down to rest. O(n log n) per tick.

use eframe::egui::{vec2, Vec2};

#[derive(Debug, Clone, Copy)]
pub struct Params {
    /// Many-body strength (negative = repulsion).
    pub charge: f32,
    /// Rest length of a link, added to the radii of both endpoints.
    pub link_distance: f32,
    /// Pull towards the origin; keeps disconnected components from drifting away.
    pub gravity: f32,
    /// Barnes-Hut accuracy (theta squared). Lower = more exact, slower.
    pub theta2: f32,
    /// Fraction of velocity lost per tick.
    pub velocity_decay: f32,
    pub alpha_min: f32,
    pub alpha_decay: f32,
    /// Minimum squared distance for repulsion (avoids blow-ups on near-overlap).
    pub distance_min2: f32,
}

impl Default for Params {
    fn default() -> Params {
        Params {
            charge: -220.0,
            link_distance: 55.0,
            gravity: 0.035,
            theta2: 0.81,
            velocity_decay: 0.4,
            alpha_min: 0.001,
            alpha_decay: 1.0 - 0.001f32.powf(1.0 / 300.0),
            distance_min2: 16.0,
        }
    }
}

pub struct Sim {
    pub pos: Vec<Vec2>,
    vel: Vec<Vec2>,
    /// Nodes held in place (being dragged).
    pub fixed: Vec<Option<Vec2>>,
    links: Vec<(u32, u32)>,
    link_len: Vec<f32>,
    link_strength: Vec<f32>,
    link_bias: Vec<f32>,
    pub alpha: f32,
    pub alpha_target: f32,
    pub params: Params,
    tree: Tree,
    seed: u32,
    /// Split the many-body pass across threads for big graphs.
    pub parallel: bool,
}

/// Deterministic starting position for the i-th node (d3's phyllotaxis arrangement).
pub fn phyllotaxis(i: usize) -> Vec2 {
    let r = 12.0 * (0.5 + i as f32).sqrt();
    let a = i as f32 * std::f32::consts::PI * (3.0 - 5f32.sqrt());
    vec2(r * a.cos(), r * a.sin())
}

impl Sim {
    /// `radius` per node is added to link rest lengths so big hubs get room.
    pub fn new(pos: Vec<Vec2>, links: &[(u32, u32)], radius: &[f32], params: Params) -> Sim {
        let n = pos.len();
        let mut degree = vec![0u32; n];
        for &(s, t) in links {
            degree[s as usize] += 1;
            degree[t as usize] += 1;
        }
        let mut link_strength = Vec::with_capacity(links.len());
        let mut link_bias = Vec::with_capacity(links.len());
        let mut link_len = Vec::with_capacity(links.len());
        for &(s, t) in links {
            let (ds, dt) = (degree[s as usize] as f32, degree[t as usize] as f32);
            link_strength.push(1.0 / ds.min(dt));
            link_bias.push(ds / (ds + dt));
            link_len.push(params.link_distance + radius[s as usize] + radius[t as usize]);
        }
        Sim {
            vel: vec![Vec2::ZERO; n],
            fixed: vec![None; n],
            pos,
            links: links.to_vec(),
            link_len,
            link_strength,
            link_bias,
            alpha: 1.0,
            alpha_target: 0.0,
            params,
            tree: Tree::default(),
            seed: 1,
            parallel: true,
        }
    }

    pub fn running(&self) -> bool {
        self.alpha >= self.params.alpha_min || self.alpha_target > 0.0
    }

    pub fn reheat(&mut self, alpha: f32) {
        self.alpha = self.alpha.max(alpha);
    }

    fn jiggle(&mut self) -> f32 {
        // xorshift: deterministic, tiny, only used to separate exact overlaps
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 17;
        self.seed ^= self.seed << 5;
        (self.seed as f32 / u32::MAX as f32 - 0.5) * 1e-3
    }

    pub fn tick(&mut self) {
        let p = self.params;
        self.alpha += (self.alpha_target - self.alpha) * p.alpha_decay;
        let alpha = self.alpha;
        let n = self.pos.len();
        if n == 0 {
            return;
        }

        // springs
        for k in 0..self.links.len() {
            let (s, t) = (self.links[k].0 as usize, self.links[k].1 as usize);
            let mut d = self.pos[t] + self.vel[t] - self.pos[s] - self.vel[s];
            if d == Vec2::ZERO {
                d = vec2(self.jiggle(), self.jiggle());
            }
            let l = d.length();
            let f = d * ((l - self.link_len[k]) / l * alpha * self.link_strength[k]);
            let b = self.link_bias[k];
            self.vel[t] -= f * b;
            self.vel[s] += f * (1.0 - b);
        }

        // many-body (in parallel for big graphs: it is most of the tick)
        self.tree.build(&self.pos);
        let k = p.charge * alpha;
        let (tree, pos) = (&self.tree, &self.pos);
        let apply = |start: usize, vel: &mut [Vec2]| {
            let mut stack = Vec::with_capacity(64);
            for (j, v) in vel.iter_mut().enumerate() {
                let f = tree.force(start + j, pos, k, p.theta2, p.distance_min2, &mut stack);
                if f.x.is_finite() && f.y.is_finite() {
                    *v += f;
                }
            }
        };
        let threads = if self.parallel && n >= 800 { workers() } else { 1 };
        if threads > 1 {
            let chunk = n.div_ceil(threads);
            std::thread::scope(|s| {
                for (c, vel) in self.vel.chunks_mut(chunk).enumerate() {
                    s.spawn(move || apply(c * chunk, vel));
                }
            });
        } else {
            apply(0, &mut self.vel);
        }

        // gravity + integrate
        let keep = 1.0 - p.velocity_decay;
        for i in 0..n {
            if let Some(fp) = self.fixed[i] {
                self.pos[i] = fp;
                self.vel[i] = Vec2::ZERO;
                continue;
            }
            self.vel[i] -= self.pos[i] * (p.gravity * alpha);
            self.vel[i] *= keep;
            self.pos[i] += self.vel[i];
        }
    }
}

fn workers() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 8))
}

// ---- Barnes-Hut quadtree ----------------------------------------------------------------

const NONE: u32 = u32::MAX;

#[derive(Clone, Copy)]
struct Cell {
    x0: f32,
    y0: f32,
    size: f32,
    mass: f32,
    center: Vec2,
    child: [u32; 4],
    /// First body of the chain stored in a leaf (bodies at ~the same spot share a leaf).
    body: u32,
    leaf: bool,
}

impl Cell {
    fn new(x0: f32, y0: f32, size: f32) -> Cell {
        Cell { x0, y0, size, mass: 0.0, center: Vec2::ZERO, child: [NONE; 4], body: NONE, leaf: true }
    }
    fn quadrant(&self, p: Vec2) -> usize {
        let h = self.size * 0.5;
        (p.x >= self.x0 + h) as usize | ((p.y >= self.y0 + h) as usize) << 1
    }
}

#[derive(Default)]
struct Tree {
    cells: Vec<Cell>,
    next: Vec<u32>,
}

impl Tree {
    fn build(&mut self, pos: &[Vec2]) {
        self.cells.clear();
        self.next.clear();
        self.next.resize(pos.len(), NONE);
        let (mut lo, mut hi) = (vec2(f32::MAX, f32::MAX), vec2(f32::MIN, f32::MIN));
        for p in pos {
            lo = lo.min(*p);
            hi = hi.max(*p);
        }
        let size = (hi - lo).max_elem() + 1.0;
        self.cells.push(Cell::new(lo.x - 0.5, lo.y - 0.5, size));
        for b in 0..pos.len() {
            self.insert(b as u32, pos);
        }
        self.accumulate(0, pos);
    }

    fn child(&mut self, c: usize, q: usize) -> usize {
        if self.cells[c].child[q] == NONE {
            let p = self.cells[c];
            let h = p.size * 0.5;
            let cell = Cell::new(p.x0 + (q & 1) as f32 * h, p.y0 + (q >> 1) as f32 * h, h);
            self.cells[c].child[q] = self.cells.len() as u32;
            self.cells.push(cell);
        }
        self.cells[c].child[q] as usize
    }

    fn insert(&mut self, b: u32, pos: &[Vec2]) {
        let pb = pos[b as usize];
        let mut c = 0usize;
        loop {
            let cell = self.cells[c];
            if !cell.leaf {
                c = self.child(c, cell.quadrant(pb));
                continue;
            }
            if cell.body == NONE {
                self.cells[c].body = b;
                return;
            }
            let po = pos[cell.body as usize];
            if (po - pb).length_sq() < 1e-6 || cell.size < 1e-2 {
                self.next[b as usize] = cell.body; // chain it
                self.cells[c].body = b;
                return;
            }
            // split: push the existing chain down one level, then retry here
            self.cells[c].leaf = false;
            self.cells[c].body = NONE;
            let ch = self.child(c, cell.quadrant(po));
            self.cells[ch].body = cell.body;
        }
    }

    fn accumulate(&mut self, c: usize, pos: &[Vec2]) {
        let cell = self.cells[c];
        let (mut mass, mut sum) = (0.0f32, Vec2::ZERO);
        if cell.leaf {
            let mut b = cell.body;
            while b != NONE {
                mass += 1.0;
                sum += pos[b as usize];
                b = self.next[b as usize];
            }
        } else {
            for q in 0..4 {
                let ch = cell.child[q];
                if ch != NONE {
                    self.accumulate(ch as usize, pos);
                    let cc = self.cells[ch as usize];
                    mass += cc.mass;
                    sum += cc.center * cc.mass;
                }
            }
        }
        self.cells[c].mass = mass;
        self.cells[c].center = if mass > 0.0 { sum / mass } else { Vec2::ZERO };
    }

    /// Velocity change for body `i`: sum of `k * d / |d|^2` over all other bodies.
    fn force(&self, i: usize, pos: &[Vec2], k: f32, theta2: f32, dmin2: f32, stack: &mut Vec<u32>) -> Vec2 {
        let pi = pos[i];
        let mut f = Vec2::ZERO;
        let scaled = |d: Vec2, m: f32| {
            let mut l2 = d.length_sq();
            if l2 < dmin2 {
                l2 = (dmin2 * l2).sqrt().max(1e-6);
            }
            d * (k * m / l2)
        };
        stack.clear();
        stack.push(0);
        while let Some(c) = stack.pop() {
            let cell = self.cells[c as usize];
            if cell.mass == 0.0 {
                continue;
            }
            if !cell.leaf {
                let d = cell.center - pi;
                if cell.size * cell.size / theta2 < d.length_sq() {
                    f += scaled(d, cell.mass);
                } else {
                    stack.extend(cell.child.iter().copied().filter(|&ch| ch != NONE));
                }
                continue;
            }
            let mut b = cell.body;
            while b != NONE {
                if b as usize != i {
                    let mut d = pos[b as usize] - pi;
                    if d == Vec2::ZERO {
                        // exact overlap: deterministic nudge by index order
                        d = if (b as usize) < i { vec2(-1e-3, 0.0) } else { vec2(1e-3, 0.0) };
                    }
                    f += scaled(d, 1.0);
                }
                b = self.next[b as usize];
            }
        }
        f
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn points(n: usize) -> Vec<Vec2> {
        let mut s = 12345u32;
        let mut r = move || {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            s as f32 / u32::MAX as f32
        };
        (0..n).map(|_| vec2(r() * 1000.0, r() * 1000.0)).collect()
    }

    #[test]
    fn barnes_hut_matches_brute_force() {
        let pos = points(600);
        let mut tree = Tree::default();
        tree.build(&pos);
        let (mut err, mut norm) = (0.0f32, 0.0f32);
        for i in 0..pos.len() {
            let bh = tree.force(i, &pos, -1.0, 0.81, 1.0, &mut Vec::new());
            let mut exact = Vec2::ZERO;
            for j in 0..pos.len() {
                if i != j {
                    let d = pos[j] - pos[i];
                    exact += d * (-1.0 / d.length_sq().max(1.0));
                }
            }
            err += (bh - exact).length();
            norm += exact.length();
        }
        let rel = err / norm;
        assert!(rel < 0.05, "relative error {rel}");
    }

    #[test]
    fn handles_coincident_points() {
        let pos = vec![vec2(5.0, 5.0); 50];
        let mut tree = Tree::default();
        tree.build(&pos);
        for i in 0..pos.len() {
            let f = tree.force(i, &pos, -30.0, 0.81, 16.0, &mut Vec::new());
            assert!(f.x.is_finite() && f.y.is_finite());
        }
        let mut sim = Sim::new(pos, &[], &[0.0; 50], Params::default());
        for _ in 0..50 {
            sim.tick();
        }
        assert!(sim.pos.iter().all(|p| p.x.is_finite() && p.y.is_finite()));
        let spread = sim.pos.iter().map(|p| (*p - sim.pos[0]).length()).fold(0.0, f32::max);
        assert!(spread > 10.0, "overlapping nodes separate, spread {spread}");
    }

    #[test]
    fn converges_and_keeps_linked_nodes_close() {
        // a ring of 60 plus 20 isolated nodes
        let n = 80;
        let links: Vec<(u32, u32)> = (0..60).map(|i| (i, (i + 1) % 60)).collect();
        let pos = (0..n).map(phyllotaxis).collect();
        let mut sim = Sim::new(pos, &links, &vec![6.0; n], Params::default());
        let mut ticks = 0;
        while sim.running() && ticks < 2000 {
            sim.tick();
            ticks += 1;
        }
        assert!(!sim.running(), "cools down (ticks {ticks})");
        assert!(sim.pos.iter().all(|p| p.x.is_finite() && p.y.is_finite()));
        let link_avg: f32 = links
            .iter()
            .map(|&(a, b)| (sim.pos[a as usize] - sim.pos[b as usize]).length())
            .sum::<f32>()
            / links.len() as f32;
        let mut all = 0.0;
        let mut cnt = 0.0;
        for a in 0..60 {
            for b in a + 1..60 {
                all += (sim.pos[a] - sim.pos[b]).length();
                cnt += 1.0;
            }
        }
        assert!(link_avg < 0.5 * all / cnt, "linked {link_avg} vs mean {}", all / cnt);
        let extent = sim.pos.iter().map(|p| p.length()).fold(0.0, f32::max);
        assert!(extent < 5000.0, "gravity bounds the layout, extent {extent}");
    }

    #[test]
    fn parallel_tick_matches_serial() {
        let n = 1200; // above the parallel threshold
        let links: Vec<(u32, u32)> = (0..n as u32 - 1).map(|i| (i, i + 1)).collect();
        let pos: Vec<Vec2> = (0..n).map(phyllotaxis).collect();
        let mut par = Sim::new(pos.clone(), &links, &vec![6.0; n], Params::default());
        let mut ser = Sim::new(pos, &links, &vec![6.0; n], Params::default());
        ser.parallel = false;
        for _ in 0..5 {
            par.tick();
            ser.tick();
        }
        for i in 0..n {
            assert!((par.pos[i] - ser.pos[i]).length() < 1e-3, "node {i} differs");
        }
    }

    #[test]
    fn fixed_nodes_stay_put() {
        let pos = (0..10).map(phyllotaxis).collect();
        let mut sim = Sim::new(pos, &[(0, 1)], &[5.0; 10], Params::default());
        sim.fixed[3] = Some(vec2(100.0, -50.0));
        for _ in 0..30 {
            sim.tick();
        }
        assert_eq!(sim.pos[3], vec2(100.0, -50.0));
    }
}
