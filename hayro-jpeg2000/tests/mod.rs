use hayro_jpeg2000::bitmap::Bitmap;
use hayro_jpeg2000::read;
use image::{DynamicImage, ImageBuffer, ImageFormat, Rgba, RgbaImage};
use std::cmp::max;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

const REPLACE: Option<&str> = option_env!("REPLACE");

static WORKSPACE_PATH: LazyLock<PathBuf> =
    LazyLock::new(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(""));

static ASSETS_PATH: LazyLock<PathBuf> = LazyLock::new(|| WORKSPACE_PATH.join("assets"));
static SNAPSHOTS_PATH: LazyLock<PathBuf> = LazyLock::new(|| WORKSPACE_PATH.join("snapshots"));

static DIFFS_PATH: LazyLock<PathBuf> = LazyLock::new(|| {
    let path = WORKSPACE_PATH.join("diffs");
    let _ = fs::remove_dir_all(&path);
    let _ = fs::create_dir_all(&path);
    path
});

macro_rules! snapshot_test {
    ($name:ident, $file:expr) => {
        #[test]
        fn $name() {
            run_asset_test($file);
        }
    };
}

snapshot_test!(
    kakadu_lossless_gray_u8_prog1_layers1_res6,
    "kakadu-lossless-gray-u8-prog1-layers1-res6.jp2"
);
snapshot_test!(
    kakadu_lossless_gray_alpha_u8_prog1_layers1_res6,
    "kakadu-lossless-gray-alpha-u8-prog1-layers1-res6.jp2"
);

fn run_asset_test(file_name: &str) {
    let asset_path = ASSETS_PATH.join(file_name);
    let data = fs::read(&asset_path).expect("failed to read asset");
    let bitmap = read(&data).expect("failed to decode jp2 file");

    let rgba = bitmap_to_dynamic_image(bitmap).into_rgba8();
    let reference_name = Path::new(file_name)
        .with_extension("png")
        .file_name()
        .unwrap()
        .to_owned();

    let snapshot_path = SNAPSHOTS_PATH.join(&reference_name);
    let diff_path = DIFFS_PATH.join(&reference_name);

    fs::create_dir_all(&*SNAPSHOTS_PATH).expect("failed to create snapshots directory");

    if !snapshot_path.exists() {
        rgba.save_with_format(&snapshot_path, ImageFormat::Png)
            .expect("failed to save snapshot");
        panic!("new reference image was created for {}", file_name);
    }

    let expected = image::open(&snapshot_path)
        .expect("failed to load snapshot")
        .into_rgba8();
    let (diff_image, pixel_diff) = get_diff(&expected, &rgba);

    if pixel_diff > 0 {
        diff_image
            .save_with_format(&diff_path, ImageFormat::Png)
            .expect("failed to save diff");

        if REPLACE.is_some() {
            rgba.save_with_format(&snapshot_path, ImageFormat::Png)
                .expect("failed to replace snapshot");
            panic!("snapshot was replaced for {}", file_name);
        }

        panic!("pixel diff {} detected for {}", pixel_diff, file_name);
    }

    if diff_path.exists() {
        let _ = fs::remove_file(diff_path);
    }
}

fn bitmap_to_dynamic_image(bitmap: Bitmap) -> DynamicImage {
    let Bitmap { channels, metadata } = bitmap;
    let (width, height) = (metadata.width, metadata.height);

    let has_alpha = channels.iter().any(|c| c.is_alpha);
    let num_channels = channels.len();

    let channels = channels
        .into_iter()
        .map(|c| c.into_8bit())
        .collect::<Vec<_>>();

    let interleaved = if num_channels == 1 {
        channels[0].clone()
    } else {
        let mut interleaved = vec![];
        let num_samples = channels.iter().map(|c| c.len()).min().unwrap();

        for sample_idx in 0..num_samples {
            for channel_idx in 0..num_channels {
                interleaved.push(channels[channel_idx][sample_idx]);
            }
        }

        interleaved
    };

    match (num_channels, has_alpha) {
        (1, false) => {
            DynamicImage::ImageLuma8(ImageBuffer::from_raw(width, height, interleaved).unwrap())
        }
        (2, true) => {
            DynamicImage::ImageLumaA8(ImageBuffer::from_raw(width, height, interleaved).unwrap())
        }
        _ => unimplemented!(),
    }
}

fn get_diff(expected_image: &RgbaImage, actual_image: &RgbaImage) -> (RgbaImage, u32) {
    let width = max(expected_image.width(), actual_image.width());
    let height = max(expected_image.height(), actual_image.height());

    let mut diff_image = RgbaImage::new(width * 3, height);
    let mut pixel_diff = 0;

    for x in 0..width {
        for y in 0..height {
            let actual_pixel = get_pixel_checked(actual_image, x, y);
            let expected_pixel = get_pixel_checked(expected_image, x, y);

            match (actual_pixel, expected_pixel) {
                (Some(actual), Some(expected)) => {
                    diff_image.put_pixel(x, y, expected);
                    diff_image.put_pixel(x + width, y, diff_pixel(expected, actual));
                    diff_image.put_pixel(x + 2 * width, y, actual);

                    if is_pixel_different(expected, actual) {
                        pixel_diff += 1;
                    }
                }
                (Some(actual), None) => {
                    pixel_diff += 1;
                    diff_image.put_pixel(x + width, y, Rgba([255, 0, 0, 255]));
                    diff_image.put_pixel(x + 2 * width, y, actual);
                }
                (None, Some(expected)) => {
                    pixel_diff += 1;
                    diff_image.put_pixel(x, y, expected);
                    diff_image.put_pixel(x + width, y, Rgba([255, 0, 0, 255]));
                }
                (None, None) => {}
            }
        }
    }

    (diff_image, pixel_diff)
}

fn get_pixel_checked(image: &RgbaImage, x: u32, y: u32) -> Option<Rgba<u8>> {
    if x < image.width() && y < image.height() {
        Some(*image.get_pixel(x, y))
    } else {
        None
    }
}

fn diff_pixel(expected: Rgba<u8>, actual: Rgba<u8>) -> Rgba<u8> {
    if is_pixel_different(expected, actual) {
        Rgba([255, 0, 0, 255])
    } else {
        Rgba([0, 0, 0, 255])
    }
}

fn is_pixel_different(lhs: Rgba<u8>, rhs: Rgba<u8>) -> bool {
    if lhs[3] == 0 && rhs[3] == 0 {
        return false;
    }

    lhs[0] != rhs[0] || lhs[1] != rhs[1] || lhs[2] != rhs[2] || lhs[3] != rhs[3]
}
