use crate::context::Context;
use crate::device::Device;
use crate::font::glyph_simulator::GlyphSimulator;
use crate::font::true_type::{read_encoding, read_widths};
use crate::font::{Encoding, Glyph, Type3Glyph, UNITS_PER_EM};
use crate::interpret::state::TextState;
use crate::soft_mask::SoftMask;
use crate::{BlendMode, interpret};
use crate::{CacheKey, ClipPath, GlyphDrawMode, PathDrawMode};
use crate::{Image, Paint};
use hayro_syntax::content::TypedIter;
use hayro_syntax::content::ops::TypedInstruction;
use hayro_syntax::object::Dict;
use hayro_syntax::object::Stream;
use hayro_syntax::object::dict::keys::{CHAR_PROCS, FONT_MATRIX, RESOURCES};
use hayro_syntax::page::Resources;
use kurbo::{Affine, BezPath, Rect};
use skrifa::GlyphId;
use std::collections::HashMap;

#[derive(Debug)]
pub(crate) struct Type3<'a> {
    widths: Vec<f32>,
    encoding: Encoding,
    encodings: HashMap<u8, String>,
    dict: Dict<'a>,
    char_procs: HashMap<String, Stream<'a>>,
    glyph_simulator: GlyphSimulator,
    matrix: Affine,
}

impl<'a> Type3<'a> {
    pub(crate) fn new(dict: &Dict<'a>) -> Self {
        let (encoding, encodings) = read_encoding(dict);
        let widths = read_widths(dict, dict);

        let matrix = Affine::new(
            dict.get::<[f64; 6]>(FONT_MATRIX)
                .unwrap_or([0.001, 0.0, 0.0, 0.001, 0.0, 0.0]),
        );

        let char_procs = {
            let mut procs = HashMap::new();
            let dict = dict.get::<Dict>(CHAR_PROCS).unwrap_or_default();

            for name in dict.keys() {
                if let Some(prog) = dict.get::<Stream>(name.clone()) {
                    procs.insert(name.as_str().to_string(), prog.clone());
                }
            }

            procs
        };

        Self {
            glyph_simulator: GlyphSimulator::new(),
            encoding,
            char_procs,
            widths,
            encodings,
            matrix,
            dict: dict.clone(),
        }
    }

    pub(crate) fn map_code(&self, code: u8) -> GlyphId {
        self.encodings
            .get(&code)
            .map(|s| s.as_str())
            .or_else(|| self.encoding.map_code(code))
            .map(|g| self.glyph_simulator.string_to_glyph(g))
            .unwrap_or(GlyphId::NOTDEF)
    }

    pub(crate) fn glyph_width(&self, code: u8) -> f32 {
        (*self.widths.get(code as usize).unwrap_or(&0.0) * self.matrix.as_coeffs()[0] as f32)
            * UNITS_PER_EM
    }

    pub(crate) fn render_glyph(
        &self,
        glyph: &Type3Glyph<'a>,
        transform: Affine,
        glyph_transform: Affine,
        paint: &Paint<'a>,
        device: &mut impl Device<'a>,
    ) -> Option<()> {
        let mut state = glyph.state.clone();
        let root_transform =
            transform * glyph_transform * self.matrix * Affine::scale(UNITS_PER_EM as f64);
        state.ctm = root_transform;

        // Not sure if this is mentioned anywhere, but I do think we need to reset the text state
        // (though the graphics state itself should be preserved).
        state.text_state = TextState::default();

        let mut context = Context::new_with(
            state.ctm,
            // TODO: Get a proper bbox.
            Rect::new(0.0, 0.0, 1.0, 1.0),
            glyph.cache.clone(),
            glyph.xref,
            glyph.settings.clone(),
            state,
        );

        let name = self.glyph_simulator.glyph_to_string(glyph.glyph_id)?;
        let program = self.char_procs.get(&name)?;
        let decoded = program.decoded().ok()?;
        let iter = TypedIter::new(decoded.as_ref());

        let is_shape_glyph = {
            let iter = iter.clone();
            let mut is_shape_glyph = true;

            for op in iter {
                match op {
                    TypedInstruction::ShapeGlyph(_) => {
                        break;
                    }
                    TypedInstruction::ColorGlyph(_) => {
                        is_shape_glyph = false;
                        break;
                    }
                    _ => {}
                }
            }

            is_shape_glyph
        };

        let mut resources = Resources::from_parent(
            self.dict.get(RESOURCES).unwrap_or_default(),
            glyph.parent_resources.clone(),
        );

        // Technically not valid, but also support by Adobe Acrobat. See PDFBOX-5294.
        if let Some(procs_resources) = program.dict().get::<Dict>(RESOURCES) {
            resources = Resources::from_parent(procs_resources, resources)
        }

        if is_shape_glyph {
            let mut device = Type3ShapeGlyphDevice::new(device, paint.clone());
            interpret(iter, &resources, &mut context, &mut device);
        } else {
            interpret(iter, &resources, &mut context, device);
        }

        Some(())
    }
}

impl CacheKey for Type3<'_> {
    fn cache_key(&self) -> u128 {
        self.dict.cache_key()
    }
}

struct Type3ShapeGlyphDevice<'a, 'b, T: Device<'a>> {
    inner: &'b mut T,
    paint: Paint<'a>,
}

impl<'a, 'b, T: Device<'a>> Type3ShapeGlyphDevice<'a, 'b, T> {
    pub fn new(device: &'b mut T, paint: Paint<'a>) -> Self {
        Self {
            inner: device,
            paint,
        }
    }
}

// Only filling, stroking of paths and stencil masks are allowed.
impl<'a, T: Device<'a>> Device<'a> for Type3ShapeGlyphDevice<'a, '_, T> {
    fn set_soft_mask(&mut self, _: Option<SoftMask>) {}

    fn draw_path(
        &mut self,
        path: &BezPath,
        transform: Affine,
        _: &Paint,
        draw_mode: &PathDrawMode,
    ) {
        self.inner
            .draw_path(path, transform, &self.paint, draw_mode)
    }

    fn push_clip_path(&mut self, clip_path: &ClipPath) {
        self.inner.push_clip_path(clip_path)
    }

    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask>, _: BlendMode) {}

    fn draw_glyph(
        &mut self,
        g: &Glyph<'a>,
        transform: Affine,
        glyph_transform: Affine,
        p: &Paint<'a>,
        draw_mode: &GlyphDrawMode,
    ) {
        self.inner
            .draw_glyph(g, transform, glyph_transform, p, draw_mode);
    }

    fn pop_clip_path(&mut self) {
        self.inner.pop_clip_path()
    }

    fn pop_transparency_group(&mut self) {}

    fn draw_image(&mut self, image: Image<'a, '_>, transform: Affine) {
        if let Image::Stencil(mut s) = image {
            s.paint = self.paint.clone();
            self.inner.draw_image(Image::Stencil(s), transform)
        }
    }

    fn set_blend_mode(&mut self, _: BlendMode) {}
}
