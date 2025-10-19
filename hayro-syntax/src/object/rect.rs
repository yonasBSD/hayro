//! Rectangles.

use crate::object::Array;
use crate::object::{Object, ObjectLike};
use crate::reader::{Readable, ReaderContext, ReaderExt};

use crate::reader::Reader;
pub use kurbo::Rect;

impl<'a> Readable<'a> for Rect {
    fn read(r: &mut Reader<'a>, ctx: &ReaderContext<'a>) -> Option<Self> {
        let arr = r.read::<Array>(ctx)?;
        from_arr(&arr)
    }
}

fn from_arr(array: &Array) -> Option<Rect> {
    let mut iter = array.iter::<f32>();
    let x0 = iter.next()? as f64;
    let y0 = iter.next()? as f64;
    let x1 = iter.next()? as f64;
    let y1 = iter.next()? as f64;

    Some(Rect::new(x0.min(x1), y0.min(y1), x1.max(x0), y1.max(y0)))
}

impl TryFrom<Object<'_>> for Rect {
    type Error = ();

    fn try_from(value: Object<'_>) -> Result<Self, Self::Error> {
        match value {
            Object::Array(arr) => from_arr(&arr).ok_or(()),
            _ => Err(()),
        }
    }
}

impl ObjectLike<'_> for Rect {}
