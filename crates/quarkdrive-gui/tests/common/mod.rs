//! Headless harness for driving the GUI in tests.
//!
//! Two pieces:
//!
//! * [`pump`] — runs real egui frames (`Context::run`, no window server
//!   involved) until the app's worker threads settle, exactly like the
//!   eframe event loop would;
//! * [`Renderer`] — a tiny software rasteriser. egui hands us the tessellated
//!   triangles and the font/thumbnail textures every frame; blending them
//!   ourselves turns the actual UI into a PNG, so screenshots in CI are the
//!   real thing, not a mock.

use quarkdrive_gui::app::App;
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

pub const WIDTH: f32 = 1200.0;
pub const HEIGHT: f32 = 800.0;

/// Run frames until `app.busy` is zero for two consecutive frames. Texture
/// uploads (font atlas, thumbnails) are merged into the renderer every
/// frame, exactly like a real renderer would.
pub fn pump(
    app: &mut App,
    ctx: &egui::Context,
    r: &mut Renderer,
    budget_secs: f64,
    label: &str,
) {
    let start = Instant::now();
    let mut t = 0.0f64;
    let mut idle = 0;
    loop {
        assert!(
            start.elapsed() < Duration::from_secs_f64(budget_secs),
            "timed out waiting for {label} (busy={}, note={:?}, login_err={:?})",
            app.busy,
            app.note,
            app.login_err
        );
        t += 1.0 / 60.0;
        let out = run_frame(app, ctx, t);
        r.absorb(&out);
        if app.busy == 0 {
            idle += 1;
            if idle >= 2 {
                return;
            }
        } else {
            idle = 0;
        }
    }
}

pub fn run_frame(app: &mut App, ctx: &egui::Context, t: f64) -> egui::FullOutput {
    let raw = egui::RawInput {
        time: Some(t),
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(WIDTH, HEIGHT),
        )),
        ..Default::default()
    };
    ctx.run(raw, |ctx| quarkdrive_gui::ui::draw(app, ctx))
}

/// Accumulating software renderer: egui streams texture uploads (the font
/// atlas, each decoded thumbnail) frame by frame, so the texture set has to
/// persist across frames.
pub struct Renderer {
    textures: HashMap<egui::TextureId, (usize, usize, Vec<[f32; 4]>)>,
}

impl Renderer {
    pub fn new() -> Renderer {
        Renderer {
            textures: HashMap::new(),
        }
    }

    /// Render one frame and save it as a PNG.
    pub fn screenshot(
        &mut self,
        app: &mut App,
        ctx: &egui::Context,
        path: &Path,
    ) -> std::io::Result<()> {
        let out = run_frame(app, ctx, 10.0);
        self.absorb(&out);
        let frame = self.rasterize(ctx, &out);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        image::save_buffer(
            path,
            &frame.0,
            frame.1 as u32,
            frame.2 as u32,
            image::ColorType::Rgba8,
        )
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
    }

    /// Merge one frame's texture uploads. Deltas are either a full image
    /// (`pos: None`) or a sub-rectangle update of an existing texture (the
    /// font atlas grows this way), which must be blitted in place.
    fn absorb(&mut self, out: &egui::FullOutput) {
        for (id, delta) in &out.textures_delta.set {
            let (dw, dh, dpx) = decode_image(&delta.image);
            match delta.pos {
                None => {
                    self.textures.insert(*id, (dw, dh, dpx));
                }
                Some([ox, oy]) => {
                    let (tw, th, px) = self
                        .textures
                        .entry(*id)
                        .or_insert_with(|| (dw, dh, vec![[0.0f32; 4]; dw * dh]));
                    let need_w = ox + dw;
                    let need_h = oy + dh;
                    if need_w > *tw || need_h > *th {
                        let nw = need_w.max(*tw);
                        let nh = need_h.max(*th);
                        let mut np = vec![[0.0f32; 4]; nw * nh];
                        for y in 0..*th {
                            let src = &px[y * *tw..y * *tw + *tw];
                            np[y * nw..y * nw + *tw].copy_from_slice(src);
                        }
                        *tw = nw;
                        *th = nh;
                        *px = np;
                    }
                    for y in 0..dh {
                        let off = (oy + y) * *tw + ox;
                        px[off..off + dw].copy_from_slice(&dpx[y * dw..(y + 1) * dw]);
                    }
                }
            }
        }
        for id in &out.textures_delta.free {
            self.textures.remove(id);
        }
    }

    fn rasterize(
        &mut self,
        ctx: &egui::Context,
        out: &egui::FullOutput,
    ) -> (Vec<u8>, usize, usize) {
        let ppi = out.pixels_per_point;

        let w = (WIDTH * ppi).round() as usize;
        let h = (HEIGHT * ppi).round() as usize;
        // Opaque page background; every panel paints over it.
        let mut buf = vec![0u8; w * h * 4];
        for px in buf.chunks_exact_mut(4) {
            px.copy_from_slice(&[0x0F, 0x10, 0x13, 0xFF]);
        }

        let meshes = ctx.tessellate(out.shapes.clone(), ppi);
        for clipped in meshes {
            let clip = clipped.clip_rect;
            // The GUI only emits mesh primitives; callbacks (if a future
            // widget adds one) would need a real renderer anyway.
            let mesh = match clipped.primitive {
                egui::epaint::Primitive::Mesh(m) => m,
                egui::epaint::Primitive::Callback { .. } => continue,
            };
            let white: &[[f32; 4]] = &[[1.0, 1.0, 1.0, 1.0]];
            let (tw, th, tpx) = match self.textures.get(&mesh.texture_id) {
                Some((w, h, px)) => (*w, *h, px.as_slice()),
                // egui's implicit 1×1 white texture.
                None => (1usize, 1usize, white),
            };

            let cx0 = (clip.min.x * ppi).floor().max(0.0) as usize;
            let cy0 = (clip.min.y * ppi).floor().max(0.0) as usize;
            let cx1 = ((clip.max.x * ppi).ceil() as usize).min(w);
            let cy1 = ((clip.max.y * ppi).ceil() as usize).min(h);

            for tri in mesh.indices.chunks_exact(3) {
                let vs: Vec<egui::epaint::Vertex> = tri
                    .iter()
                    .map(|&i| mesh.vertices[i as usize])
                    .collect();
                let p: Vec<[f32; 2]> = vs
                    .iter()
                    .map(|v| [v.pos.x * ppi, v.pos.y * ppi])
                    .collect();

                let min_x = p[0][0].min(p[1][0]).min(p[2][0]).floor().max(0.0) as usize;
                let min_y = p[0][1].min(p[1][1]).min(p[2][1]).floor().max(0.0) as usize;
                let max_x = (p[0][0].max(p[1][0]).max(p[2][0]).ceil() as usize + 1).min(cx1);
                let max_y = (p[0][1].max(p[1][1]).max(p[2][1]).ceil() as usize + 1).min(cy1);

                let denom = (p[1][1] - p[2][1]) * (p[0][0] - p[2][0])
                    + (p[2][0] - p[1][0]) * (p[0][1] - p[2][1]);
                if denom.abs() < 1e-9 {
                    continue;
                }

                for y in min_y.max(cy0)..max_y {
                    for x in min_x.max(cx0)..max_x {
                        let fx = x as f32 + 0.5;
                        let fy = y as f32 + 0.5;
                        let w0 = ((p[1][1] - p[2][1]) * (fx - p[2][0])
                            + (p[2][0] - p[1][0]) * (fy - p[2][1]))
                            / denom;
                        let w1 = ((p[2][1] - p[0][1]) * (fx - p[2][0])
                            + (p[0][0] - p[2][0]) * (fy - p[2][1]))
                            / denom;
                        let w2 = 1.0 - w0 - w1;
                        if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                            continue;
                        }

                        let u = w0 * vs[0].uv.x + w1 * vs[1].uv.x + w2 * vs[2].uv.x;
                        let v = w0 * vs[0].uv.y + w1 * vs[1].uv.y + w2 * vs[2].uv.y;
                        // egui mesh uvs are normalised 0..1 across the texture.
                        let rgba = sample(
                            tw,
                            th,
                            tpx,
                            u * tw as f32,
                            v * th as f32,
                        );
                        let col = [
                            w0 * f32::from(vs[0].color.r()) / 255.0
                                + w1 * f32::from(vs[1].color.r()) / 255.0
                                + w2 * f32::from(vs[2].color.r()) / 255.0,
                            w0 * f32::from(vs[0].color.g()) / 255.0
                                + w1 * f32::from(vs[1].color.g()) / 255.0
                                + w2 * f32::from(vs[2].color.g()) / 255.0,
                            w0 * f32::from(vs[0].color.b()) / 255.0
                                + w1 * f32::from(vs[1].color.b()) / 255.0
                                + w2 * f32::from(vs[2].color.b()) / 255.0,
                            w0 * f32::from(vs[0].color.a()) / 255.0
                                + w1 * f32::from(vs[1].color.a()) / 255.0
                                + w2 * f32::from(vs[2].color.a()) / 255.0,
                        ];

                        // texture * vertex colour, then source-over blend
                        let a = rgba[3] * col[3];
                        if a < 1.0 / 255.0 {
                            continue;
                        }
                        let off = (y * w + x) * 4;
                        for c in 0..3 {
                            let src = rgba[c] * col[c] * 255.0;
                            let dst = buf[off + c] as f32;
                            buf[off + c] = (src * a + dst * (1.0 - a)).round() as u8;
                        }
                        let dst_a = buf[off + 3] as f32 / 255.0;
                        buf[off + 3] =
                            ((a + dst_a * (1.0 - a)) * 255.0).round().min(255.0) as u8;
                    }
                }
            }
        }
        (buf, w, h)
    }
}

/// egui image payloads become flat RGBA f32 buffers for the rasteriser.
/// Colour pixels arrive premultiplied; font pixels are glyph coverage.
fn decode_image(img: &egui::ImageData) -> (usize, usize, Vec<[f32; 4]>) {
    match img {
        egui::ImageData::Color(c) => (
            c.width(),
            c.height(),
            c.pixels
                .iter()
                .map(|p| {
                    [
                        f32::from(p.r()) / 255.0,
                        f32::from(p.g()) / 255.0,
                        f32::from(p.b()) / 255.0,
                        f32::from(p.a()) / 255.0,
                    ]
                })
                .collect(),
        ),
        egui::ImageData::Font(f) => (
            f.width(),
            f.height(),
            f.pixels
                .iter()
                .map(|v| [1.0, 1.0, 1.0, v.clamp(0.0, 1.0)])
                .collect(),
        ),
    }
}

/// Bilinear texture sample; uv is in texel coordinates.
fn sample(tw: usize, th: usize, tpx: &[[f32; 4]], u: f32, v: f32) -> [f32; 4] {
    if tw == 0 || th == 0 {
        return [1.0, 1.0, 1.0, 1.0];
    }
    let fx = (u - 0.5).max(0.0);
    let fy = (v - 0.5).max(0.0);
    let x0 = fx.floor() as usize;
    let y0 = fy.floor() as usize;
    let dx = fx - x0 as f32;
    let dy = fy - y0 as f32;
    let get = |x: usize, y: usize| {
        let x = x.min(tw - 1);
        let y = y.min(th - 1);
        tpx[y * tw + x]
    };
    let (c00, c10, c01, c11) = (
        get(x0, y0),
        get(x0 + 1, y0),
        get(x0, y0 + 1),
        get(x0 + 1, y0 + 1),
    );
    let mut out = [0.0f32; 4];
    for c in 0..4 {
        out[c] = c00[c] * (1.0 - dx) * (1.0 - dy)
            + c10[c] * dx * (1.0 - dy)
            + c01[c] * (1.0 - dx) * dy
            + c11[c] * dx * dy;
    }
    out
}
