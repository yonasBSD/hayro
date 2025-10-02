//! PDF shadings.

#![allow(clippy::needless_range_loop)]

use crate::CacheKey;
use crate::cache::Cache;
use crate::color::{ColorComponents, ColorSpace};
use crate::function::{Function, Values, interpolate};
use crate::util::{Float32Ext, PointExt};
use hayro_syntax::bit_reader::{BitReader, BitSize};
use hayro_syntax::object::Array;
use hayro_syntax::object::Dict;
use hayro_syntax::object::Object;
use hayro_syntax::object::Rect;
use hayro_syntax::object::Stream;
use hayro_syntax::object::dict::keys::{
    BACKGROUND, BBOX, BITS_PER_COMPONENT, BITS_PER_COORDINATE, BITS_PER_FLAG, COLORSPACE, COORDS,
    DECODE, DOMAIN, EXTEND, FUNCTION, MATRIX, SHADING_TYPE, VERTICES_PER_ROW,
};
use kurbo::{Affine, BezPath, CubicBez, ParamCurve, Point, Shape};
use log::warn;
use smallvec::{SmallVec, smallvec};
use std::sync::Arc;

/// The function supplied to a shading.
#[derive(Debug, Clone)]
pub enum ShadingFunction {
    /// A single function, which should be used to evaluate all components of the shading.
    Single(Function),
    /// Multiple functions, one for each color component.
    Multiple(SmallVec<[Function; 4]>),
}

impl ShadingFunction {
    /// Evaluate the shading function.
    pub fn eval(&self, input: &Values) -> Option<Values> {
        match self {
            ShadingFunction::Single(s) => s.eval(input.clone()),
            ShadingFunction::Multiple(m) => {
                // 1-in, 1-out function for each color component.

                let mut out = smallvec![];

                for func in m {
                    out.push(*func.eval(input.clone())?.first()?);
                }

                Some(out)
            }
        }
    }
}

/// A type of shading.
#[derive(Debug)]
pub enum ShadingType {
    /// A function-based shading.
    FunctionBased {
        /// The domain of the function.
        domain: [f32; 4],
        /// A transform to apply to the shading.
        matrix: Affine,
        /// The function that should be used to evaluate the shading.
        function: ShadingFunction,
    },
    /// A radial-axial shading.
    RadialAxial {
        /// The coordinates of the shading.
        ///
        /// For axial shadings, only the first 4 entries are relevant, representing the x/y coordinates
        /// of the first point and the coordinates for the second point.
        ///
        /// For radial shadings, the coordinates contain the x/y coordinates as well as the radius
        /// for both circles.
        coords: [f32; 6],
        /// The domain of the shading.
        domain: [f32; 2],
        /// The function forming the basis of the shading.
        function: ShadingFunction,
        /// The extends in the left/right direction of the shading.
        extend: [bool; 2],
        /// Whether the shading is axial or radial.
        axial: bool,
    },
    /// A triangle-mesh shading.
    TriangleMesh {
        /// The triangles making up the shading.
        triangles: Vec<Triangle>,
        /// An optional function used for calculating the sampled color values.
        function: Option<ShadingFunction>,
    },
    /// A coons-patch-mesh shading.
    CoonsPatchMesh {
        /// The patches that make up the shading.
        patches: Vec<CoonsPatch>,
        /// An optional function used for calculating the sampled color values.
        function: Option<ShadingFunction>,
    },
    /// A tensor-product-patch-mesh shading.
    TensorProductPatchMesh {
        /// The patches that make up the shading.
        patches: Vec<TensorProductPatch>,
        /// An optional function used for calculating the sampled color values.
        function: Option<ShadingFunction>,
    },
    /// A dummy shading that should just be drawn transparent.
    Dummy,
}

/// A PDF shading.
#[derive(Clone, Debug)]
pub struct Shading {
    cache_key: u128,
    /// The type of shading.
    pub shading_type: Arc<ShadingType>,
    /// The color space of the shading.
    pub color_space: ColorSpace,
    /// A clip path that should be applied to the shading.
    pub clip_path: Option<BezPath>,
    /// The background color of the shading.
    pub background: Option<SmallVec<[f32; 4]>>,
}

impl Shading {
    pub(crate) fn new(dict: &Dict, stream: Option<&Stream>, cache: &Cache) -> Option<Self> {
        let cache_key = dict.cache_key();

        let shading_num = dict.get::<u8>(SHADING_TYPE)?;

        let color_space = ColorSpace::new(dict.get(COLORSPACE)?, cache)?;

        let shading_type = match shading_num {
            1 => {
                let domain = dict.get::<[f32; 4]>(DOMAIN).unwrap_or([0.0, 1.0, 0.0, 1.0]);
                let matrix = dict
                    .get::<[f64; 6]>(MATRIX)
                    .map(Affine::new)
                    .unwrap_or_default();
                let function = read_function(dict, &color_space)?;

                ShadingType::FunctionBased {
                    domain,
                    matrix,
                    function,
                }
            }
            2 | 3 => {
                let domain = dict.get::<[f32; 2]>(DOMAIN).unwrap_or([0.0, 1.0]);
                let function = read_function(dict, &color_space)?;
                let extend = dict.get::<[bool; 2]>(EXTEND).unwrap_or([false, false]);
                let (coords, invalid) = if shading_num == 2 {
                    let read = dict.get::<[f32; 4]>(COORDS)?;
                    let invalid = (read[0] - read[2]).is_nearly_zero()
                        && (read[1] - read[3]).is_nearly_zero();
                    ([read[0], read[1], read[2], read[3], 0.0, 0.0], invalid)
                } else {
                    let read = dict.get::<[f32; 6]>(COORDS)?;
                    let invalid = (read[0] - read[3]).is_nearly_zero()
                        && (read[1] - read[4]).is_nearly_zero()
                        && (read[2] - read[5]).is_nearly_zero();
                    (read, invalid)
                };

                let axial = shading_num == 2;

                if invalid {
                    ShadingType::Dummy
                } else {
                    ShadingType::RadialAxial {
                        domain,
                        function,
                        extend,
                        coords,
                        axial,
                    }
                }
            }
            4 => {
                let stream = stream?;
                let stream_data = stream.decoded().ok()?;
                let bp_coord = dict.get::<u8>(BITS_PER_COORDINATE)?;
                let bp_comp = dict.get::<u8>(BITS_PER_COMPONENT)?;
                let bpf = dict.get::<u8>(BITS_PER_FLAG)?;
                let function = read_function(dict, &color_space);
                let decode = dict.get::<Array>(DECODE)?.iter::<f32>().collect::<Vec<_>>();

                let triangles = read_free_form_triangles(
                    stream_data.as_ref(),
                    bpf,
                    bp_coord,
                    bp_comp,
                    function.is_some(),
                    &decode,
                )?;

                ShadingType::TriangleMesh {
                    triangles,
                    function,
                }
            }
            5 => {
                let stream = stream?;
                let stream_data = stream.decoded().ok()?;
                let bp_coord = dict.get::<u8>(BITS_PER_COORDINATE)?;
                let bp_comp = dict.get::<u8>(BITS_PER_COMPONENT)?;
                let function = read_function(dict, &color_space);
                let decode = dict.get::<Array>(DECODE)?.iter::<f32>().collect::<Vec<_>>();
                let vertices_per_row = dict.get::<u32>(VERTICES_PER_ROW)?;

                let triangles = read_lattice_triangles(
                    stream_data.as_ref(),
                    bp_coord,
                    bp_comp,
                    function.is_some(),
                    vertices_per_row,
                    &decode,
                )?;

                ShadingType::TriangleMesh {
                    triangles,
                    function,
                }
            }
            6 => {
                let stream = stream?;
                let stream_data = stream.decoded().ok()?;
                let bp_coord = dict.get::<u8>(BITS_PER_COORDINATE)?;
                let bp_comp = dict.get::<u8>(BITS_PER_COMPONENT)?;
                let bpf = dict.get::<u8>(BITS_PER_FLAG)?;
                let function = read_function(dict, &color_space);
                let decode = dict.get::<Array>(DECODE)?.iter::<f32>().collect::<Vec<_>>();

                let patches = read_coons_patch_mesh(
                    stream_data.as_ref(),
                    bpf,
                    bp_coord,
                    bp_comp,
                    function.is_some(),
                    &decode,
                )?;

                ShadingType::CoonsPatchMesh { patches, function }
            }
            7 => {
                let stream = stream?;
                let stream_data = stream.decoded().ok()?;
                let bp_coord = dict.get::<u8>(BITS_PER_COORDINATE)?;
                let bp_comp = dict.get::<u8>(BITS_PER_COMPONENT)?;
                let bpf = dict.get::<u8>(BITS_PER_FLAG)?;
                let function = read_function(dict, &color_space);
                let decode = dict.get::<Array>(DECODE)?.iter::<f32>().collect::<Vec<_>>();

                let patches = read_tensor_product_patch_mesh(
                    stream_data.as_ref(),
                    bpf,
                    bp_coord,
                    bp_comp,
                    function.is_some(),
                    &decode,
                )?;

                ShadingType::TensorProductPatchMesh { patches, function }
            }
            _ => return None,
        };

        let bbox = dict.get::<Rect>(BBOX);
        let background = dict
            .get::<Array>(BACKGROUND)
            .map(|a| a.iter::<f32>().collect::<SmallVec<_>>());

        Some(Self {
            cache_key,
            shading_type: Arc::new(shading_type),
            color_space,
            clip_path: bbox.map(|r| r.to_path(0.1)),
            background,
        })
    }
}

impl CacheKey for Shading {
    fn cache_key(&self) -> u128 {
        self.cache_key
    }
}

/// A triangle made up of three vertices.
#[derive(Clone, Debug)]
pub struct Triangle {
    /// The first vertex.
    pub p0: TriangleVertex,
    /// The second vertex.
    pub p1: TriangleVertex,
    /// The third vertex.
    pub p2: TriangleVertex,
    kurbo_tri: kurbo::Triangle,
    d00: f64,
    d01: f64,
    d11: f64,
}

impl Triangle {
    /// Create a new triangle.
    pub fn new(p0: TriangleVertex, p1: TriangleVertex, p2: TriangleVertex) -> Self {
        let v0 = p1.point - p0.point;
        let v1 = p2.point - p0.point;

        let d00 = v0.dot(v0);
        let d01 = v0.dot(v1);
        let d11 = v1.dot(v1);

        let kurbo_tri = kurbo::Triangle::new(p0.point, p1.point, p2.point);

        Self {
            p0,
            p1,
            kurbo_tri,
            p2,
            d00,
            d01,
            d11,
        }
    }

    /// Get the interpolated colors of the point from the triangle.
    ///
    /// Returns `None` if the point is not inside of the triangle.
    pub fn interpolate(&self, pos: Point) -> ColorComponents {
        let (u, v, w) = self.barycentric_coords(pos);

        let mut result = smallvec![];

        for i in 0..self.p0.colors.len() {
            let c0 = self.p0.colors[i];
            let c1 = self.p1.colors[i];
            let c2 = self.p2.colors[i];
            result.push(u * c0 + v * c1 + w * c2);
        }

        result
    }

    /// Return whether the point is contained within the triangle.
    pub fn contains_point(&self, pos: Point) -> bool {
        self.kurbo_tri.winding(pos) != 0
    }

    /// Return the bounding box of the triangle.
    pub fn bounding_box(&self) -> kurbo::Rect {
        self.kurbo_tri.bounding_box()
    }

    fn barycentric_coords(&self, p: Point) -> (f32, f32, f32) {
        let (a, b, c) = (self.p0.point, self.p1.point, self.p2.point);
        let v0 = b - a;
        let v1 = c - a;
        let v2 = p - a;

        let d00 = self.d00;
        let d01 = self.d01;
        let d11 = self.d11;
        let d20 = v2.dot(v0);
        let d21 = v2.dot(v1);

        let denom = d00 * d11 - d01 * d01;
        let v = (d11 * d20 - d01 * d21) / denom;
        let w = (d00 * d21 - d01 * d20) / denom;
        let u = (1.0 - v - w) as f32;

        (u, v as f32, w as f32)
    }
}

/// A triangle vertex.
#[derive(Clone, Debug)]
pub struct TriangleVertex {
    flag: u32,
    /// The position of the vertex.
    pub point: Point,
    /// The color component of the vertex.
    pub colors: ColorComponents,
}

/// A coons patch.
#[derive(Clone, Debug)]
pub struct CoonsPatch {
    /// The control points of the coons patch.
    pub control_points: [Point; 12],
    /// The colors at each corner of the coons patch.
    pub colors: [ColorComponents; 4],
}

/// A tensor-product patch.
#[derive(Clone, Debug)]
pub struct TensorProductPatch {
    /// The control points of the tensor-product patch (4x4 grid = 16 points).
    pub control_points: [Point; 16],
    /// The colors at each corner of the tensor-product patch.
    pub colors: [ColorComponents; 4],
}

impl CoonsPatch {
    /// Map the point to the coordinates of the coons patch.
    pub fn map_coordinate(&self, p: Point) -> Point {
        let (u, v) = (p.x, p.y);

        let cp = &self.control_points;

        let c1 = CubicBez::new(cp[0], cp[11], cp[10], cp[9]);
        let c2 = CubicBez::new(cp[3], cp[4], cp[5], cp[6]);
        let d1 = CubicBez::new(cp[0], cp[1], cp[2], cp[3]);
        let d2 = CubicBez::new(cp[9], cp[8], cp[7], cp[6]);

        let sc = (1.0 - v) * c1.eval(u).to_vec2() + v * c2.eval(u).to_vec2();
        let sd = (1.0 - u) * d1.eval(v).to_vec2() + u * d2.eval(v).to_vec2();
        let sb = (1.0 - v) * ((1.0 - u) * c1.eval(0.0).to_vec2() + u * c1.eval(1.0).to_vec2())
            + v * ((1.0 - u) * c2.eval(0.0).to_vec2() + u * c2.eval(1.0).to_vec2());

        (sc + sd - sb).to_point()
    }

    /// Approximate the patch by triangles.
    pub fn to_triangles(&self) -> Vec<Triangle> {
        generate_patch_triangles(|p| self.map_coordinate(p), |p| self.interpolate(p))
    }

    /// Get the interpolated colors of the point from the patch.
    pub fn interpolate(&self, pos: Point) -> ColorComponents {
        let (u, v) = (pos.x, pos.y);
        let (c0, c1, c2, c3) = {
            (
                &self.colors[0],
                &self.colors[1],
                &self.colors[2],
                &self.colors[3],
            )
        };

        let mut result = SmallVec::new();
        for i in 0..c0.len() {
            let val = (1.0 - u) * (1.0 - v) * c0[i] as f64
                + u * (1.0 - v) * c3[i] as f64
                + u * v * c2[i] as f64
                + (1.0 - u) * v * c1[i] as f64;
            result.push(val as f32);
        }

        result
    }
}

impl TensorProductPatch {
    /// Evaluate Bernstein polynomial B_i(t) for tensor-product patches.
    fn bernstein(i: usize, t: f64) -> f64 {
        match i {
            0 => (1.0 - t).powi(3),
            1 => 3.0 * t * (1.0 - t).powi(2),
            2 => 3.0 * t.powi(2) * (1.0 - t),
            3 => t.powi(3),
            _ => 0.0,
        }
    }

    /// Map the point to the coordinates of the tensor product patch.
    pub fn map_coordinate(&self, p: Point) -> Point {
        let (u, v) = (p.x, p.y);

        let mut x = 0.0;
        let mut y = 0.0;

        fn idx(i: usize, j: usize) -> usize {
            match (i, j) {
                (0, 0) => 0,
                (0, 1) => 1,
                (0, 2) => 2,
                (0, 3) => 3,
                (1, 0) => 11,
                (1, 1) => 12,
                (1, 2) => 13,
                (1, 3) => 4,
                (2, 0) => 10,
                (2, 1) => 15,
                (2, 2) => 14,
                (2, 3) => 5,
                (3, 0) => 9,
                (3, 1) => 8,
                (3, 2) => 7,
                (3, 3) => 6,
                _ => panic!("Invalid index"),
            }
        }

        for i in 0..4 {
            for j in 0..4 {
                let control_point_idx = idx(i, j);
                let basis = Self::bernstein(i, u) * Self::bernstein(j, v);

                x += self.control_points[control_point_idx].x * basis;
                y += self.control_points[control_point_idx].y * basis;
            }
        }

        Point::new(x, y)
    }

    /// Approximate the tensor product patch mesh by triangles.
    pub fn to_triangles(&self) -> Vec<Triangle> {
        generate_patch_triangles(|p| self.map_coordinate(p), |p| self.interpolate(p))
    }

    /// Get the interpolated colors of the point from the patch.
    pub fn interpolate(&self, pos: Point) -> ColorComponents {
        let (u, v) = (pos.x, pos.y);
        let (c0, c1, c2, c3) = {
            (
                &self.colors[0],
                &self.colors[1],
                &self.colors[2],
                &self.colors[3],
            )
        };

        let mut result = SmallVec::new();
        for i in 0..c0.len() {
            let val = (1.0 - u) * (1.0 - v) * c0[i] as f64
                + u * (1.0 - v) * c3[i] as f64
                + u * v * c2[i] as f64
                + (1.0 - u) * v * c1[i] as f64;
            result.push(val as f32);
        }

        result
    }
}

fn read_free_form_triangles(
    data: &[u8],
    bpf: u8,
    bp_cord: u8,
    bp_comp: u8,
    has_function: bool,
    decode: &[f32],
) -> Option<Vec<Triangle>> {
    let bpf = BitSize::from_u8(bpf)?;
    let bp_cord = BitSize::from_u8(bp_cord)?;
    let bp_comp = BitSize::from_u8(bp_comp)?;

    let mut triangles = vec![];

    let ([x_min, x_max, y_min, y_max], decode) = split_decode(decode)?;
    let mut reader = BitReader::new(data);
    let helpers = InterpolationHelpers::new(bp_cord, bp_comp, x_min, x_max, y_min, y_max);

    let read_single = |reader: &mut BitReader| -> Option<TriangleVertex> {
        helpers.read_triangle_vertex(reader, bpf, has_function, decode)
    };

    let mut a = None;
    let mut b = None;
    let mut c = None;

    loop {
        let Some(first) = read_single(&mut reader) else {
            break;
        };

        if first.flag == 0 {
            let second = read_single(&mut reader)?;
            let third = read_single(&mut reader)?;

            a = Some(first.clone());
            b = Some(second.clone());
            c = Some(third.clone());
        } else if first.flag == 1 {
            a = Some(b.clone()?);
            b = Some(c.clone()?);
            c = Some(first);
        } else if first.flag == 2 {
            b = Some(c.clone()?);
            c = Some(first);
        }

        let (p0, p1, p2) = (a.clone()?, b.clone()?, c.clone()?);

        if p0.point.nearly_same(p1.point) || p1.point.nearly_same(p2.point) {
            continue;
        }

        triangles.push(Triangle::new(a.clone()?, b.clone()?, c.clone()?));
    }

    Some(triangles)
}

/// Common interpolation functions used across different shading types.
struct InterpolationHelpers {
    bp_coord: BitSize,
    bp_comp: BitSize,
    coord_max: f32,
    comp_max: f32,
    x_min: f32,
    x_max: f32,
    y_min: f32,
    y_max: f32,
}

impl InterpolationHelpers {
    fn new(
        bp_coord: BitSize,
        bp_comp: BitSize,
        x_min: f32,
        x_max: f32,
        y_min: f32,
        y_max: f32,
    ) -> Self {
        let coord_max = 2.0f32.powi(bp_coord.bits() as i32) - 1.0;
        let comp_max = 2.0f32.powi(bp_comp.bits() as i32) - 1.0;
        Self {
            bp_coord,
            bp_comp,
            coord_max,
            comp_max,
            x_min,
            x_max,
            y_min,
            y_max,
        }
    }

    fn interpolate_coord(&self, n: u32, d_min: f32, d_max: f32) -> f32 {
        interpolate(n as f32, 0.0, self.coord_max, d_min, d_max)
    }

    fn interpolate_comp(&self, n: u32, d_min: f32, d_max: f32) -> f32 {
        interpolate(n as f32, 0.0, self.comp_max, d_min, d_max)
    }

    fn read_point(&self, reader: &mut BitReader) -> Option<Point> {
        let x = self.interpolate_coord(reader.read(self.bp_coord)?, self.x_min, self.x_max);
        let y = self.interpolate_coord(reader.read(self.bp_coord)?, self.y_min, self.y_max);
        Some(Point::new(x as f64, y as f64))
    }

    fn read_colors(
        &self,
        reader: &mut BitReader,
        has_function: bool,
        decode: &[f32],
    ) -> Option<ColorComponents> {
        let mut colors = smallvec![];
        if has_function {
            colors.push(self.interpolate_comp(
                reader.read(self.bp_comp)?,
                *decode.first()?,
                *decode.get(1)?,
            ));
        } else {
            let num_components = decode.len() / 2;
            for (_, decode) in (0..num_components).zip(decode.chunks_exact(2)) {
                colors.push(self.interpolate_comp(
                    reader.read(self.bp_comp)?,
                    decode[0],
                    decode[1],
                ));
            }
        }
        Some(colors)
    }

    fn read_triangle_vertex(
        &self,
        reader: &mut BitReader,
        bpf: BitSize,
        has_function: bool,
        decode: &[f32],
    ) -> Option<TriangleVertex> {
        let flag = reader.read(bpf)?;
        let point = self.read_point(reader)?;
        let colors = self.read_colors(reader, has_function, decode)?;
        reader.align();

        Some(TriangleVertex {
            flag,
            point,
            colors,
        })
    }
}

/// Split decode array into coordinate bounds and component decode values.
fn split_decode(decode: &[f32]) -> Option<([f32; 4], &[f32])> {
    decode.split_first_chunk::<4>().map(|(a, b)| (*a, b))
}

/// Generate triangles from a grid of points using a mapping function.
fn generate_patch_triangles<F, I>(map_coordinate: F, interpolate: I) -> Vec<Triangle>
where
    F: Fn(Point) -> Point,
    I: Fn(Point) -> ColorComponents,
{
    const GRID_SIZE: usize = 20;
    let mut grid = vec![vec![Point::ZERO; GRID_SIZE]; GRID_SIZE];

    // Create grid by mapping unit square coordinates.
    for i in 0..GRID_SIZE {
        for j in 0..GRID_SIZE {
            let u = i as f64 / (GRID_SIZE - 1) as f64; // 0.0 to 1.0 (left to right).
            let v = j as f64 / (GRID_SIZE - 1) as f64; // 0.0 to 1.0 (top to bottom).

            // Map unit square coordinate to patch coordinate.
            let unit_point = Point::new(u, v);
            grid[i][j] = map_coordinate(unit_point);
        }
    }

    // Create triangles from adjacent grid points.
    let mut triangles = vec![];

    for i in 0..(GRID_SIZE - 1) {
        for j in 0..(GRID_SIZE - 1) {
            let p00 = grid[i][j];
            let p10 = grid[i + 1][j];
            let p01 = grid[i][j + 1];
            let p11 = grid[i + 1][j + 1];

            // Calculate unit square coordinates for color interpolation.
            let u0 = i as f64 / (GRID_SIZE - 1) as f64;
            let u1 = (i + 1) as f64 / (GRID_SIZE - 1) as f64;
            let v0 = j as f64 / (GRID_SIZE - 1) as f64;
            let v1 = (j + 1) as f64 / (GRID_SIZE - 1) as f64;

            // Create triangle vertices with interpolated colors.
            let v00 = TriangleVertex {
                flag: 0,
                point: p00,
                colors: interpolate(Point::new(u0, v0)),
            };
            let v10 = TriangleVertex {
                flag: 0,
                point: p10,
                colors: interpolate(Point::new(u1, v0)),
            };
            let v01 = TriangleVertex {
                flag: 0,
                point: p01,
                colors: interpolate(Point::new(u0, v1)),
            };
            let v11 = TriangleVertex {
                flag: 0,
                point: p11,
                colors: interpolate(Point::new(u1, v1)),
            };

            triangles.push(Triangle::new(v00.clone(), v10.clone(), v01.clone()));
            triangles.push(Triangle::new(v10.clone(), v11.clone(), v01.clone()));
        }
    }

    triangles
}

fn read_lattice_triangles(
    data: &[u8],
    bp_cord: u8,
    bp_comp: u8,
    has_function: bool,
    vertices_per_row: u32,
    decode: &[f32],
) -> Option<Vec<Triangle>> {
    let bp_cord = BitSize::from_u8(bp_cord)?;
    let bp_comp = BitSize::from_u8(bp_comp)?;

    let mut lattices = vec![];

    let ([x_min, x_max, y_min, y_max], decode) = split_decode(decode)?;
    let mut reader = BitReader::new(data);
    let helpers = InterpolationHelpers::new(bp_cord, bp_comp, x_min, x_max, y_min, y_max);

    let read_single = |reader: &mut BitReader| -> Option<TriangleVertex> {
        let point = helpers.read_point(reader)?;
        let colors = helpers.read_colors(reader, has_function, decode)?;
        reader.align();

        Some(TriangleVertex {
            flag: 0,
            point,
            colors,
        })
    };

    'outer: loop {
        let mut single_row = vec![];

        for _ in 0..vertices_per_row {
            let Some(next) = read_single(&mut reader) else {
                break 'outer;
            };

            single_row.push(next);
        }

        lattices.push(single_row);
    }

    let mut triangles = vec![];

    for i in 0..(lattices.len() - 1) {
        for j in 0..(vertices_per_row as usize - 1) {
            triangles.push(Triangle::new(
                lattices[i][j].clone(),
                lattices[i + 1][j].clone(),
                lattices[i][j + 1].clone(),
            ));

            triangles.push(Triangle::new(
                lattices[i + 1][j + 1].clone(),
                lattices[i + 1][j].clone(),
                lattices[i][j + 1].clone(),
            ));
        }
    }

    Some(triangles)
}

fn read_coons_patch_mesh(
    data: &[u8],
    bpf: u8,
    bp_coord: u8,
    bp_comp: u8,
    has_function: bool,
    decode: &[f32],
) -> Option<Vec<CoonsPatch>> {
    read_patch_mesh(
        data,
        bpf,
        bp_coord,
        bp_comp,
        has_function,
        decode,
        12,
        |control_points, colors| {
            let mut coons_points = [Point::ZERO; 12];
            coons_points.copy_from_slice(&control_points[0..12]);
            CoonsPatch {
                control_points: coons_points,
                colors,
            }
        },
    )
}

/// Generic patch mesh reading function that works for both Coons and Tensor Product patches.
#[allow(clippy::too_many_arguments)]
fn read_patch_mesh<P, F>(
    data: &[u8],
    bpf: u8,
    bp_coord: u8,
    bp_comp: u8,
    has_function: bool,
    decode: &[f32],
    control_points_count: usize,
    create_patch: F,
) -> Option<Vec<P>>
where
    F: Fn([Point; 16], [ColorComponents; 4]) -> P,
{
    let bpf = BitSize::from_u8(bpf)?;
    let bp_coord = BitSize::from_u8(bp_coord)?;
    let bp_comp = BitSize::from_u8(bp_comp)?;

    let ([x_min, x_max, y_min, y_max], decode) = split_decode(decode)?;
    let mut reader = BitReader::new(data);
    let helpers = InterpolationHelpers::new(bp_coord, bp_comp, x_min, x_max, y_min, y_max);

    let read_colors = |reader: &mut BitReader| -> Option<ColorComponents> {
        helpers.read_colors(reader, has_function, decode)
    };

    let mut prev_patch_points: Option<Vec<Point>> = None;
    let mut prev_patch_colors: Option<[ColorComponents; 4]> = None;
    let mut patches = vec![];

    while let Some(flag) = reader.read(bpf) {
        let mut control_points = vec![Point::ZERO; 16]; // Always allocate 16, use subset as needed.
        let mut colors = [smallvec![], smallvec![], smallvec![], smallvec![]];

        match flag {
            0 => {
                for i in 0..control_points_count {
                    control_points[i] = helpers.read_point(&mut reader)?;
                }

                for i in 0..4 {
                    colors[i] = read_colors(&mut reader)?;
                }

                prev_patch_points = Some(control_points.clone());
                prev_patch_colors = Some(colors.clone());
            }
            1..=3 => {
                let prev_points = prev_patch_points.as_ref()?;
                let prev_colors = prev_patch_colors.as_ref()?;

                copy_patch_control_points(flag, prev_points, &mut control_points);

                match flag {
                    1 => {
                        colors[0] = prev_colors[1].clone();
                        colors[1] = prev_colors[2].clone();
                    }
                    2 => {
                        colors[0] = prev_colors[2].clone();
                        colors[1] = prev_colors[3].clone();
                    }
                    3 => {
                        colors[0] = prev_colors[3].clone();
                        colors[1] = prev_colors[0].clone();
                    }
                    _ => unreachable!(),
                }

                for i in 4..control_points_count {
                    control_points[i] = helpers.read_point(&mut reader)?;
                }

                colors[2] = read_colors(&mut reader)?;
                colors[3] = read_colors(&mut reader)?;

                prev_patch_points = Some(control_points.clone());
                prev_patch_colors = Some(colors.clone());
            }
            _ => break,
        }

        let mut fixed_points = [Point::ZERO; 16];
        for i in 0..16 {
            if i < control_points.len() {
                fixed_points[i] = control_points[i];
            }
        }

        patches.push(create_patch(fixed_points, colors));
    }
    Some(patches)
}

fn copy_patch_control_points(
    flag: u32,
    prev_control_points: &[Point],
    control_points: &mut [Point],
) {
    match flag {
        1 => {
            control_points[0] = prev_control_points[3];
            control_points[1] = prev_control_points[4];
            control_points[2] = prev_control_points[5];
            control_points[3] = prev_control_points[6];
        }
        2 => {
            control_points[0] = prev_control_points[6];
            control_points[1] = prev_control_points[7];
            control_points[2] = prev_control_points[8];
            control_points[3] = prev_control_points[9];
        }
        3 => {
            control_points[0] = prev_control_points[9];
            control_points[1] = prev_control_points[10];
            control_points[2] = prev_control_points[11];
            control_points[3] = prev_control_points[0];
        }
        _ => {}
    }
}

fn read_tensor_product_patch_mesh(
    data: &[u8],
    bpf: u8,
    bp_coord: u8,
    bp_comp: u8,
    has_function: bool,
    decode: &[f32],
) -> Option<Vec<TensorProductPatch>> {
    read_patch_mesh(
        data,
        bpf,
        bp_coord,
        bp_comp,
        has_function,
        decode,
        16,
        |control_points, colors| TensorProductPatch {
            control_points,
            colors,
        },
    )
}

fn read_function(dict: &Dict, color_space: &ColorSpace) -> Option<ShadingFunction> {
    if let Some(arr) = dict.get::<Array>(FUNCTION) {
        let arr: Option<SmallVec<_>> = arr.iter::<Object>().map(|o| Function::new(&o)).collect();
        let arr = arr?;

        if arr.len() != color_space.num_components() as usize {
            warn!("function array of shading has wrong size");

            return None;
        }

        Some(ShadingFunction::Multiple(arr))
    } else if let Some(obj) = dict.get::<Object>(FUNCTION) {
        Some(ShadingFunction::Single(Function::new(&obj)?))
    } else {
        None
    }
}
