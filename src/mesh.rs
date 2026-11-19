//! Batched, anti-aliased primitives. Thousands of edges and nodes as individual egui
//! shapes cost more to tessellate than everything else in a frame together; building one
//! mesh directly (with a 1-pixel feather for anti-aliasing) is ~10x cheaper.

use eframe::egui::epaint::{Vertex, WHITE_UV};
use eframe::egui::{Color32, Mesh, Pos2, Shape, Vec2};

pub struct Batch {
    mesh: Mesh,
    /// Width of the anti-aliasing fringe, in points (one physical pixel).
    feather: f32,
}

fn scale(c: Color32, k: f32) -> Color32 {
    if k >= 1.0 {
        c
    } else {
        c.gamma_multiply(k.max(0.0))
    }
}

impl Batch {
    pub fn new(pixels_per_point: f32) -> Batch {
        Batch { mesh: Mesh::default(), feather: 1.0 / pixels_per_point.max(0.1) }
    }

    fn v(&mut self, pos: Pos2, color: Color32) -> u32 {
        let i = self.mesh.vertices.len() as u32;
        self.mesh.vertices.push(Vertex { pos, uv: WHITE_UV, color });
        i
    }

    fn quad(&mut self, a: u32, b: u32, c: u32, d: u32) {
        self.mesh.indices.extend_from_slice(&[a, b, c, a, c, d]);
    }

    /// A straight line of `width` points. Lines thinner than the feather are drawn one
    /// feather wide with proportionally less alpha (as egui does).
    pub fn line(&mut self, a: Pos2, b: Pos2, width: f32, color: Color32) {
        let d = b - a;
        let len = d.length();
        if len < 1e-3 {
            return;
        }
        let n = Vec2::new(-d.y, d.x) / len;
        let f = self.feather;
        let color = scale(color, width / f);
        let inner = (width - f).max(0.0) * 0.5;
        let outer = inner + f;
        let t = Color32::TRANSPARENT;
        if inner == 0.0 {
            // hairline: an opaque spine with a fringe on each side (6 vertices)
            let (ao, am, ao2) = (self.v(a + n * outer, t), self.v(a, color), self.v(a - n * outer, t));
            let (bo, bm, bo2) = (self.v(b + n * outer, t), self.v(b, color), self.v(b - n * outer, t));
            self.quad(ao, am, bm, bo);
            self.quad(am, ao2, bo2, bm);
            return;
        }
        let (ao, ai, bi, bo) = (
            self.v(a + n * outer, t),
            self.v(a + n * inner, color),
            self.v(b + n * inner, color),
            self.v(b + n * outer, t),
        );
        let (ai2, ao2, bo2, bi2) = (
            self.v(a - n * inner, color),
            self.v(a - n * outer, t),
            self.v(b - n * outer, t),
            self.v(b - n * inner, color),
        );
        self.quad(ao, ai, bi, bo); // fringe, + side
        self.quad(ai, ai2, bi2, bi); // core
        self.quad(ai2, ao2, bo2, bi2); // fringe, - side
    }

    /// A filled circle.
    pub fn disc(&mut self, c: Pos2, r: f32, color: Color32) {
        let f = self.feather;
        let k = ((r * 1.6).ceil() as usize).clamp(8, 64);
        let inner = (r - f * 0.5).max(0.0);
        let outer = r + f * 0.5;
        let (s, co) = (std::f32::consts::TAU / k as f32).sin_cos();
        let center = self.v(c, color);
        let base = self.mesh.vertices.len() as u32;
        let mut u = Vec2::new(1.0, 0.0);
        for _ in 0..k {
            self.v(c + u * inner, color);
            self.v(c + u * outer, Color32::TRANSPARENT);
            u = Vec2::new(u.x * co - u.y * s, u.x * s + u.y * co);
        }
        for j in 0..k as u32 {
            let (i0, o0) = (base + 2 * j, base + 2 * j + 1);
            let next = (j + 1) % k as u32;
            let (i1, o1) = (base + 2 * next, base + 2 * next + 1);
            self.mesh.indices.extend_from_slice(&[center, i0, i1]);
            self.quad(i0, o0, o1, i1);
        }
    }

    pub fn into_shape(self) -> Shape {
        Shape::mesh(self.mesh)
    }
}

#[cfg(test)]
mod bench {
    //! `cargo test --release bench_tessellation -- --ignored --nocapture`
    use super::*;
    use eframe::egui::epaint::{ClippedShape, Stroke, TessellationOptions, Tessellator};
    use eframe::egui::{pos2, Rect};
    use std::time::Instant;

    fn tessellate(shapes: Vec<Shape>) -> (f32, usize) {
        let clip = Rect::from_min_size(pos2(0.0, 0.0), eframe::egui::vec2(1400.0, 950.0));
        let clipped: Vec<ClippedShape> = shapes.into_iter().map(|shape| ClippedShape { clip_rect: clip, shape }).collect();
        let t0 = Instant::now();
        let mut tess = Tessellator::new(1.0, TessellationOptions::default(), [2048, 2048], vec![]);
        let out = tess.tessellate_shapes(clipped);
        let verts = out.iter().map(|p| match &p.primitive {
            eframe::egui::epaint::Primitive::Mesh(m) => m.vertices.len(),
            _ => 0,
        }).sum();
        (t0.elapsed().as_secs_f32() * 1000.0, verts)
    }

    #[test]
    #[ignore]
    fn bench_tessellation() {
        let (nodes, edges) = (3000usize, 9000usize);
        let p = |i: usize| pos2(((i * 7919) % 1400) as f32, ((i * 104729) % 950) as f32);
        let c = Color32::from_rgb(120, 140, 200);
        for _ in 0..2 {
            let t0 = Instant::now();
            let mut shapes = Vec::new();
            for e in 0..edges {
                shapes.push(Shape::line_segment([p(e), p(e * 3 + 1)], Stroke::new(1.0, c)));
            }
            for n in 0..nodes {
                shapes.push(Shape::circle_filled(p(n), 3.0 + (n % 7) as f32, c));
            }
            let build = t0.elapsed().as_secs_f32() * 1000.0;
            let (ms, v) = tessellate(shapes);
            println!("individual shapes: build {build:.2}ms + tessellate {ms:.2}ms, {v} vertices");

            let t0 = Instant::now();
            let (mut eb, mut nb) = (Batch::new(1.0), Batch::new(1.0));
            for e in 0..edges {
                eb.line(p(e), p(e * 3 + 1), 1.0, c);
            }
            for n in 0..nodes {
                nb.disc(p(n), 3.0 + (n % 7) as f32, c);
            }
            let shapes = vec![eb.into_shape(), nb.into_shape()];
            let build = t0.elapsed().as_secs_f32() * 1000.0;
            let (ms, v) = tessellate(shapes);
            println!("batched meshes:    build {build:.2}ms + tessellate {ms:.2}ms, {v} vertices");
        }
    }
}
