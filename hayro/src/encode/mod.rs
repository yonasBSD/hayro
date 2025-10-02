// Copyright 2025 the Vello Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

pub(crate) mod image;
pub(crate) mod shading;

use crate::encode::image::EncodedImage;
use crate::fine::Sampler;
use crate::paint::Paint;
use hayro_interpret::encode::EncodedShadingPattern;
use kurbo::{Affine, Point, Vec2};

#[derive(Debug)]
pub(crate) struct Shader<T: Sampler> {
    pub(crate) transform: Affine,
    pub(crate) x_advance: Vec2,
    pub(crate) y_advance: Vec2,
    pub(crate) sampler: T,
}

impl Shader<EncodedImage> {
    pub(crate) fn new(base_transform: Affine, image: EncodedImage) -> Shader<EncodedImage> {
        Self::new_inner(base_transform, image)
    }
}

impl Shader<EncodedShadingPattern> {
    pub(crate) fn new(shading: EncodedShadingPattern) -> Shader<EncodedShadingPattern> {
        Self::new_inner(shading.base_transform, shading)
    }
}

impl<T: Sampler> Shader<T> {
    fn new_inner(base_transform: Affine, sampler: T) -> Shader<T> {
        let transform = base_transform * Affine::translate((0.5, 0.5));
        let (x_advance, y_advance) = x_y_advances(&transform);

        Shader {
            transform,
            x_advance,
            y_advance,
            sampler,
        }
    }

    #[inline]
    pub(crate) fn sample(&self, pos: Point) -> [f32; 4] {
        self.sampler.sample(pos)
    }
}

pub(crate) trait EncodeExt {
    fn encode_into(&self, paints: &mut Vec<EncodedPaint>) -> Paint;
}

#[derive(Debug)]
pub(crate) enum EncodedPaint {
    Image(Shader<EncodedImage>),
    Mask(Shader<EncodedImage>),
    Shading(Shader<EncodedShadingPattern>),
}

pub(crate) fn x_y_advances(transform: &Affine) -> (Vec2, Vec2) {
    let scale_skew_transform = {
        let c = transform.as_coeffs();
        Affine::new([c[0], c[1], c[2], c[3], 0.0, 0.0])
    };

    let x_advance = scale_skew_transform * Point::new(1.0, 0.0);
    let y_advance = scale_skew_transform * Point::new(0.0, 1.0);

    (
        Vec2::new(x_advance.x, x_advance.y),
        Vec2::new(y_advance.x, y_advance.y),
    )
}

#[derive(Debug)]
pub(crate) struct Buffer<const C: usize> {
    buffer: Vec<f32>,
    width: u32,
    height: u32,
}

impl<const C: usize> Buffer<C> {
    pub(crate) fn new_u8(buffer: Vec<u8>, width: u32, height: u32) -> Buffer<C> {
        let converted = buffer.iter().map(|v| *v as f32 * (1.0 / 255.0)).collect();
        Self::new_f32(converted, width, height)
    }

    pub(crate) fn new_f32(buffer: Vec<f32>, width: u32, height: u32) -> Buffer<C> {
        assert_eq!(buffer.len() as u32, width * height * C as u32);

        Self {
            buffer,
            width,
            height,
        }
    }

    pub(crate) fn sample(&self, pos: Point) -> [f32; C] {
        let x = pos.x as u32;
        let y = pos.y as u32;

        if x >= self.width || y >= self.height {
            [0.0; C]
        } else {
            self.buffer[C * (x + y * self.width) as usize..][..C]
                .try_into()
                .unwrap()
        }
    }
}

impl Buffer<4> {
    pub(crate) fn premultiply(&mut self) {
        for ch in self.buffer.chunks_exact_mut(4) {
            let alpha = ch[3];
            ch[0] *= alpha;
            ch[1] *= alpha;
            ch[2] *= alpha;
        }
    }
}
