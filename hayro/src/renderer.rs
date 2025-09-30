use crate::ctx::RenderContext;
use crate::encode::{Buffer, x_y_advances};
use crate::mask::Mask;
use crate::paint::{Image, PaintType};
use crate::pixmap::Pixmap;
use hayro_interpret::color::AlphaColor;
use hayro_interpret::font::Glyph;
use hayro_interpret::hayro_syntax::object::ObjectIdentifier;
use hayro_interpret::pattern::Pattern;
use hayro_interpret::{
    CacheKey, ClipPath, Device, FillRule, GlyphDrawMode, LumaData, MaskType, Paint, PathDrawMode,
    RgbData, SoftMask, StrokeProps,
};
use image::imageops::FilterType;
use image::{DynamicImage, ImageBuffer};
use kurbo::{Affine, BezPath, Point, Rect};
use std::collections::HashMap;
use std::sync::Arc;

pub(crate) struct Renderer {
    pub(crate) ctx: RenderContext,
    pub(crate) inside_pattern: bool,
    pub(crate) soft_mask_cache: HashMap<ObjectIdentifier, Mask>,
    pub(crate) glyph_cache: HashMap<u128, BezPath>,
    pub(crate) cur_mask: Option<Mask>,
}

impl Renderer {
    pub(crate) fn new(width: u16, height: u16) -> Self {
        Self {
            ctx: RenderContext::new(width, height),
            inside_pattern: false,
            soft_mask_cache: Default::default(),
            glyph_cache: Default::default(),
            cur_mask: None,
        }
    }

    fn set_stroke_properties(&mut self, stroke_props: &StrokeProps) {
        // Best-effort attempt to ensure a line width of at least 1.
        let min_factor = min_factor(&self.ctx.transform);
        let mut line_width = stroke_props.line_width.max(0.01);
        let transformed_width = line_width * min_factor;

        // Only enforce line width if not inside of pattern.
        if transformed_width < 1.0 && !self.inside_pattern {
            line_width /= transformed_width;
        }

        let stroke = kurbo::Stroke {
            width: line_width as f64,
            join: stroke_props.line_join,
            miter_limit: stroke_props.miter_limit as f64,
            start_cap: stroke_props.line_cap,
            end_cap: stroke_props.line_cap,
            dash_pattern: stroke_props.dash_array.iter().map(|n| *n as f64).collect(),
            dash_offset: stroke_props.dash_offset as f64,
        };

        self.ctx.set_stroke(stroke);
    }

    fn draw_image(&mut self, rgb_data: RgbData, alpha_data: Option<LumaData>, is_stencil: bool) {
        let mut cur_transform = self.ctx.transform;

        let (x_scale, y_scale) = {
            let (x, y) = x_y_advances(&cur_transform);
            (x.length() as f32, y.length() as f32)
        };
        let mut rgb_width = rgb_data.width;
        let mut rgb_height = rgb_data.height;

        let interpolate = rgb_data.interpolate;

        let rgb_data = if x_scale >= 1.0 && y_scale >= 1.0 {
            rgb_data.data
        } else {
            // Resize the image, either doing down- or upsampling.
            let new_width = (rgb_width as f32 * x_scale).ceil().max(1.0) as u32;
            let new_height = (rgb_height as f32 * y_scale).ceil().max(1.0) as u32;

            let image = DynamicImage::ImageRgb8(
                ImageBuffer::from_raw(rgb_width, rgb_height, rgb_data.data.clone()).unwrap(),
            );
            let resized = image.resize_exact(new_width, new_height, FilterType::CatmullRom);

            let new_width = resized.width();
            let new_height = resized.height();
            let t_scale_x = rgb_width as f32 / new_width as f32;
            let t_scale_y = rgb_height as f32 / new_height as f32;

            cur_transform *= Affine::scale_non_uniform(t_scale_x as f64, t_scale_y as f64);
            self.ctx.set_transform(cur_transform);

            rgb_width = new_width;
            rgb_height = new_height;

            resized.to_rgb8().into_raw()
        };

        let alpha_data = if let Some(alpha_data) = alpha_data {
            if alpha_data.width != rgb_width || alpha_data.height != rgb_height {
                let image = DynamicImage::ImageLuma8(
                    ImageBuffer::from_raw(
                        alpha_data.width,
                        alpha_data.height,
                        alpha_data.data.clone(),
                    )
                    .unwrap(),
                );
                let resized = image.resize_exact(rgb_width, rgb_height, FilterType::CatmullRom);
                resized.to_luma8().into_raw()
            } else {
                alpha_data.data
            }
        } else {
            vec![255; rgb_width as usize * rgb_height as usize]
        };

        let rgba_data = rgb_data
            .chunks_exact(3)
            .zip(alpha_data)
            .flat_map(|(rgb, a)| [rgb[0], rgb[1], rgb[2], a])
            .collect::<Vec<_>>();

        let mut buffer = Buffer::<4>::new_u8(rgba_data, rgb_width, rgb_height);
        buffer.premultiply();

        let image = Image {
            buffer: Arc::new(buffer),
            interpolate,
            is_stencil,
            is_pattern: false,
            transform: self.ctx.transform,
        };

        self.ctx.fill_rect(
            &Rect::new(0.0, 0.0, rgb_width as f64, rgb_height as f64),
            image.into(),
            None,
        );
    }

    fn convert_paint(&mut self, paint: &hayro_interpret::Paint, is_stroke: bool) -> PaintType {
        match paint.clone() {
            hayro_interpret::Paint::Color(c) => c.to_rgba().into(),
            hayro_interpret::Paint::Pattern(p) => {
                match *p {
                    Pattern::Shading(s) => s.into(),
                    Pattern::Tiling(t) => {
                        const MAX_PIXMAP_SIZE: f32 = 3000.0;
                        // TODO: Raise this limit and perform downsampling if reached
                        // (see pdftc_100k_0138.pdf).
                        const MIN_PIXMAP_SIZE: f32 = 1.0;

                        let bbox = t.bbox;
                        let max_x_scale = MAX_PIXMAP_SIZE / bbox.width() as f32;
                        let min_x_scale = MIN_PIXMAP_SIZE / bbox.width() as f32;
                        let max_y_scale = MAX_PIXMAP_SIZE / bbox.height() as f32;
                        let min_y_scale = MIN_PIXMAP_SIZE / bbox.height() as f32;

                        let (mut xs, mut ys) = {
                            let (x, y) = x_y_advances(&(t.matrix));
                            (x.length() as f32, y.length() as f32)
                        };
                        xs = xs.max(min_x_scale).min(max_x_scale);
                        ys = ys.max(min_y_scale).min(max_y_scale);

                        let x_step = xs * t.x_step;
                        let y_step = ys * t.y_step;

                        let scaled_width = bbox.width() as f32 * xs;
                        let scaled_height = bbox.height() as f32 * ys;
                        let pix_width = x_step.abs().round() as u16;
                        let pix_height = y_step.abs().round() as u16;

                        let mut renderer = Renderer {
                            ctx: RenderContext::new(pix_width, pix_height),
                            cur_mask: None,
                            inside_pattern: true,
                            glyph_cache: HashMap::new(),
                            soft_mask_cache: Default::default(),
                        };
                        let mut initial_transform =
                            Affine::new([xs as f64, 0.0, 0.0, ys as f64, -bbox.x0, -bbox.y0]);
                        t.interpret(&mut renderer, initial_transform, is_stroke);
                        let mut pix = Pixmap::new(pix_width, pix_height);
                        renderer.ctx.render_to_pixmap(&mut pix);

                        // TODO: Fix these
                        if x_step < 0.0 {
                            initial_transform *=
                                Affine::new([-1.0, 0.0, 0.0, 1.0, scaled_width as f64, 0.0]);
                        }

                        if y_step < 0.0 {
                            initial_transform *=
                                Affine::new([1.0, 0.0, 0.0, -1.0, 0.0, scaled_height as f64]);
                        }

                        let buffer = Buffer::new_u8(
                            pix.data_as_u8_slice().to_vec(),
                            pix_width as u32,
                            pix_height as u32,
                        );

                        let final_transform = t.matrix * initial_transform.inverse();

                        PaintType::Image(Image {
                            buffer: Arc::new(buffer),
                            interpolate: true,
                            is_stencil: false,
                            is_pattern: true,
                            transform: final_transform,
                        })
                    }
                }
            }
        }
    }

    fn stroke_path(
        &mut self,
        path: &BezPath,
        transform: Affine,
        paint: &Paint,
        stroke_props: &StrokeProps,
    ) {
        self.ctx.set_transform(transform);
        self.set_stroke_properties(stroke_props);

        let paint_type = self.convert_paint(paint, true);
        self.ctx
            .stroke_path(path, paint_type, self.cur_mask.clone());
    }

    fn fill_path(&mut self, path: &BezPath, transform: Affine, paint: &Paint, fill_rule: FillRule) {
        self.ctx.set_fill_rule(fill_rule);
        self.ctx.set_transform(transform);
        let paint_type = self.convert_paint(paint, false);
        self.ctx.fill_path(path, paint_type, self.cur_mask.clone());
    }

    fn fill_glyph(
        &mut self,
        glyph: &Glyph<'_>,
        transform: Affine,
        glyph_transform: Affine,
        paint: &Paint,
    ) {
        match glyph {
            Glyph::Outline(o) => {
                let id = o.identifier().cache_key();

                // Can't use `fill_path` here because we need to borrow the outline from the glyph
                // cache.
                self.ctx.set_fill_rule(FillRule::NonZero);
                self.ctx.set_transform(transform * glyph_transform);
                let paint_type = self.convert_paint(paint, false);
                let base_outline = self.glyph_cache.entry(id).or_insert_with(|| o.outline());
                self.ctx
                    .fill_path(base_outline, paint_type, self.cur_mask.clone());
            }
            Glyph::Type3(s) => {
                s.interpret(self, transform, glyph_transform, paint);
            }
        }
    }

    fn stroke_glyph(
        &mut self,
        glyph: &Glyph<'_>,
        transform: Affine,
        glyph_transform: Affine,
        paint: &Paint,
        stroke_props: &StrokeProps,
    ) {
        match glyph {
            Glyph::Outline(o) => {
                let id = o.identifier().cache_key();
                let base_outline = self
                    .glyph_cache
                    .entry(id)
                    .or_insert_with(|| o.outline())
                    .clone();

                self.stroke_path(
                    &(glyph_transform * base_outline),
                    transform,
                    paint,
                    stroke_props,
                );
            }
            Glyph::Type3(s) => {
                s.interpret(self, transform, glyph_transform, paint);
            }
        }
    }
}

impl<'a> Device<'a> for Renderer {
    fn draw_image(&mut self, image: hayro_interpret::Image<'a, '_>, transform: Affine) {
        match image {
            hayro_interpret::Image::Stencil(s) => {
                s.with_stencil(|stencil, paint| {
                    self.ctx.set_transform(transform);
                    self.ctx.set_anti_aliasing(false);
                    self.ctx.push_layer(None, Some(1.0), self.cur_mask.clone());
                    let old_rule = self.ctx.fill_rule;
                    self.ctx.set_fill_rule(FillRule::NonZero);
                    let converted_paint = self.convert_paint(paint, false);
                    self.ctx.fill_rect(
                        &Rect::new(0.0, 0.0, stencil.width as f64, stencil.height as f64),
                        converted_paint,
                        None,
                    );
                    let rgb_data = RgbData {
                        data: vec![0; stencil.width as usize * stencil.height as usize * 3],
                        width: stencil.width,
                        height: stencil.height,
                        interpolate: stencil.interpolate,
                    };
                    self.draw_image(rgb_data, Some(stencil), true);
                    self.ctx.pop_layer();

                    self.ctx.set_fill_rule(old_rule);
                    self.ctx.set_anti_aliasing(true);
                });
            }
            hayro_interpret::Image::Raster(r) => {
                r.with_rgba(|rgb, alpha| {
                    self.ctx.set_transform(transform);
                    if let Some(ref mask) = self.cur_mask {
                        self.ctx.push_layer(None, None, Some(mask.clone()));
                    }

                    self.ctx.set_anti_aliasing(false);
                    self.draw_image(rgb, alpha, false);
                    self.ctx.set_anti_aliasing(true);

                    if self.cur_mask.is_some() {
                        self.ctx.pop_layer();
                    }
                });
            }
        }
    }

    fn push_clip_path(&mut self, clip_path: &ClipPath) {
        self.ctx.set_fill_rule(clip_path.fill);
        self.ctx.push_layer(Some(&clip_path.path), None, None)
    }

    fn push_transparency_group(&mut self, opacity: f32, mask: Option<SoftMask>) {
        self.ctx.push_layer(
            None,
            Some(opacity),
            // TODO: Deduplicate
            mask.map(|m| {
                let width = self.ctx.width;
                let height = self.ctx.height;

                self.soft_mask_cache
                    .entry(m.id())
                    .or_insert_with(|| draw_soft_mask(&m, width, height))
                    .clone()
            }),
        );
    }

    fn pop_clip_path(&mut self) {
        self.ctx.pop_layer();
    }

    fn pop_transparency_group(&mut self) {
        self.ctx.pop_layer();
    }

    fn set_soft_mask(&mut self, mask: Option<SoftMask>) {
        self.cur_mask = mask.map(|m| {
            let width = self.ctx.width;
            let height = self.ctx.height;

            self.soft_mask_cache
                .entry(m.id())
                .or_insert_with(|| draw_soft_mask(&m, width, height))
                .clone()
        });
    }

    fn draw_path(
        &mut self,
        path: &BezPath,
        transform: Affine,
        paint: &Paint<'_>,
        draw_mode: &PathDrawMode,
    ) {
        match draw_mode {
            PathDrawMode::Fill(f) => {
                Self::fill_path(self, path, transform, paint, *f);
            }
            PathDrawMode::Stroke(s) => {
                Self::stroke_path(self, path, transform, paint, s);
            }
        }
    }

    fn draw_glyph(
        &mut self,
        glyph: &Glyph<'_>,
        transform: Affine,
        glyph_transform: Affine,
        paint: &Paint<'_>,
        draw_mode: &GlyphDrawMode,
    ) {
        match draw_mode {
            GlyphDrawMode::Fill => {
                Self::fill_glyph(self, glyph, transform, glyph_transform, paint);
            }
            GlyphDrawMode::Stroke(s) => {
                Self::stroke_glyph(self, glyph, transform, glyph_transform, paint, s);
            }
        }
    }
}

fn draw_soft_mask(mask: &SoftMask, width: u16, height: u16) -> Mask {
    let mut renderer = Renderer::new(width, height);

    let bg_color = mask.background_color().to_rgba();
    let apply_bg = bg_color.to_rgba8() != AlphaColor::BLACK.to_rgba8();

    if apply_bg {
        let paint_type = bg_color.into();
        renderer.ctx.fill_rect(
            &Rect::new(0.0, 0.0, width as f64, height as f64),
            paint_type,
            None,
        );
        renderer.ctx.push_layer(None, None, None);
    }

    mask.interpret(&mut renderer);

    if apply_bg {
        renderer.ctx.pop_layer();
    }

    let mut pix = Pixmap::new(width, height);
    renderer.ctx.render_to_pixmap(&mut pix);

    let mut rendered_mask = match mask.mask_type() {
        MaskType::Luminosity => Mask::new_luminance(&pix),
        MaskType::Alpha => Mask::new_alpha(&pix),
    };

    if let Some(transfer_function) = mask.transfer_function() {
        let mut map = Vec::new();
        for i in 0u8..=255 {
            map.push((transfer_function.apply(i as f32 / 255.0) * 255.0 + 0.5) as u8);
        }

        for pixel in Arc::make_mut(&mut rendered_mask.data) {
            *pixel = map[*pixel as usize];
        }
    }

    rendered_mask
}

pub(crate) fn min_factor(transform: &Affine) -> f32 {
    let scale_skew_transform = {
        let c = transform.as_coeffs();
        Affine::new([c[0], c[1], c[2], c[3], 0.0, 0.0])
    };

    let x_advance = scale_skew_transform * Point::new(1.0, 0.0);
    let y_advance = scale_skew_transform * Point::new(0.0, 1.0);

    x_advance
        .to_vec2()
        .length()
        .min(y_advance.to_vec2().length()) as f32
}
