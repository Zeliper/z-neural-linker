//! 앱 아이콘 변환. 빌더는 PNG 하나만 받고, 배포 대상이 요구하는 형식은 여기서 만든다.
//!
//! - Linux: `hicolor/256x256/apps/<slug>.png` 에 넣을 정사각 PNG.
//! - Windows: Inno Setup `SetupIconFile` 이 요구하는 `.ico` (여러 크기를 담은 다중 프레임).

use anyhow::Context;
use image::codecs::ico::{IcoEncoder, IcoFrame};
use image::codecs::png::PngEncoder;
use image::imageops::FilterType;
use image::{ExtendedColorType, ImageEncoder, ImageFormat, RgbaImage};
use std::path::Path;

/// `.ico` 에 담는 크기들. Windows 는 목록·작업 표시줄·바탕 화면에서 서로 다른 크기를 고른다.
pub const ICO_SIZES: &[u32] = &[16, 32, 48, 64, 128, 256];

/// Linux 아이콘 테마가 요구하는 크기.
pub const LINUX_ICON_SIZE: u32 = 256;

/// PNG 파일 → `.ico` 파일. 원본보다 큰 크기는 넣지 않는다(억지 확대 방지).
pub fn png_to_ico(png: &Path, out: &Path) -> anyhow::Result<()> {
    let bytes = std::fs::read(png).with_context(|| format!("아이콘을 읽지 못했습니다: {}", png.display()))?;
    let ico = png_bytes_to_ico(&bytes)?;
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(out, ico).with_context(|| format!("아이콘을 쓰지 못했습니다: {}", out.display()))?;
    Ok(())
}

/// PNG 바이트 → `.ico` 바이트. 프레임은 PNG 로 압축해 담는다(Windows Vista 이후 표준).
pub fn png_bytes_to_ico(png: &[u8]) -> anyhow::Result<Vec<u8>> {
    let src = load_png(png)?;
    let longest = src.width().max(src.height()).clamp(1, 256);

    let mut sizes: Vec<u32> = ICO_SIZES.iter().copied().filter(|s| *s <= longest).collect();
    if sizes.is_empty() {
        // 16px 보다 작은 아이콘은 원본 크기 한 장만 담는다.
        sizes.push(longest);
    }

    let mut frames = Vec::with_capacity(sizes.len());
    for size in sizes {
        let square = fit_square(&src, size);
        let frame = IcoFrame::as_png(square.as_raw(), size, size, ExtendedColorType::Rgba8)
            .with_context(|| format!("{size}px 아이콘 프레임을 만들지 못했습니다"))?;
        frames.push(frame);
    }

    let mut out = Vec::new();
    IcoEncoder::new(&mut out).encode_images(&frames).context("ico 로 묶지 못했습니다")?;
    Ok(out)
}

/// PNG 바이트를 `size`×`size` 정사각 PNG 로. 비율은 지키고 남는 자리는 투명하게 둔다.
pub fn png_bytes_to_square_png(png: &[u8], size: u32) -> anyhow::Result<Vec<u8>> {
    let square = fit_square(&load_png(png)?, size);
    let mut out = Vec::new();
    PngEncoder::new(&mut out)
        .write_image(square.as_raw(), size, size, ExtendedColorType::Rgba8)
        .context("png 로 다시 쓰지 못했습니다")?;
    Ok(out)
}

fn load_png(bytes: &[u8]) -> anyhow::Result<RgbaImage> {
    let img = image::load_from_memory_with_format(bytes, ImageFormat::Png)
        .context("아이콘은 PNG 여야 합니다")?
        .to_rgba8();
    if img.width() == 0 || img.height() == 0 {
        anyhow::bail!("아이콘 크기가 0 입니다");
    }
    Ok(img)
}

/// 비율을 지켜 `size` 안에 넣고 가운데 정렬한다. 정사각 원본이면 단순 리사이즈와 같다.
fn fit_square(src: &RgbaImage, size: u32) -> RgbaImage {
    let size = size.max(1);
    let scale = f64::from(size) / f64::from(src.width().max(src.height()));
    let w = ((f64::from(src.width()) * scale).round() as u32).clamp(1, size);
    let h = ((f64::from(src.height()) * scale).round() as u32).clamp(1, size);
    let resized = image::imageops::resize(src, w, h, FilterType::Lanczos3);

    if w == size && h == size {
        return resized;
    }
    let mut canvas = RgbaImage::new(size, size);
    image::imageops::replace(&mut canvas, &resized, ((size - w) / 2).into(), ((size - h) / 2).into());
    canvas
}

/// 테스트용 격자무늬 PNG. `inno` 모듈 테스트도 쓴다.
#[cfg(test)]
pub(crate) fn sample_png(w: u32, h: u32) -> Vec<u8> {
    let mut img = RgbaImage::new(w, h);
    for (x, y, px) in img.enumerate_pixels_mut() {
        *px = image::Rgba([(x * 7 % 256) as u8, (y * 11 % 256) as u8, 200, 255]);
    }
    let mut out = Vec::new();
    PngEncoder::new(&mut out).write_image(img.as_raw(), w, h, ExtendedColorType::Rgba8).expect("png 인코딩");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::GenericImageView as _;

    #[test]
    fn ico_round_trips_and_keeps_largest_size() {
        let png = sample_png(256, 256);
        let ico = png_bytes_to_ico(&png).unwrap();
        assert!(ico.starts_with(&[0, 0, 1, 0]), "ICONDIR 헤더가 아닙니다");
        // 프레임 개수는 헤더 5~6 바이트(u16 LE).
        assert_eq!(u16::from_le_bytes([ico[4], ico[5]]) as usize, ICO_SIZES.len());

        let back = image::load_from_memory_with_format(&ico, ImageFormat::Ico).unwrap();
        assert_eq!(back.dimensions(), (256, 256));
    }

    #[test]
    fn small_source_does_not_get_upscaled() {
        let ico = png_bytes_to_ico(&sample_png(40, 40)).unwrap();
        // 40px 이하인 16·32 만 들어간다.
        assert_eq!(u16::from_le_bytes([ico[4], ico[5]]), 2);
        let back = image::load_from_memory_with_format(&ico, ImageFormat::Ico).unwrap();
        assert_eq!(back.dimensions(), (32, 32));
    }

    #[test]
    fn tiny_source_keeps_one_frame() {
        let ico = png_bytes_to_ico(&sample_png(8, 8)).unwrap();
        assert_eq!(u16::from_le_bytes([ico[4], ico[5]]), 1);
        let back = image::load_from_memory_with_format(&ico, ImageFormat::Ico).unwrap();
        assert_eq!(back.dimensions(), (8, 8));
    }

    #[test]
    fn non_square_is_padded_not_stretched() {
        let square = png_bytes_to_square_png(&sample_png(200, 100), LINUX_ICON_SIZE).unwrap();
        let img = image::load_from_memory_with_format(&square, ImageFormat::Png).unwrap();
        assert_eq!(img.dimensions(), (LINUX_ICON_SIZE, LINUX_ICON_SIZE));
        // 위아래 여백은 투명해야 한다.
        assert_eq!(img.to_rgba8().get_pixel(128, 2)[3], 0);
        // 가운데는 불투명.
        assert_eq!(img.to_rgba8().get_pixel(128, 128)[3], 255);
    }

    #[test]
    fn file_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("icon.png");
        std::fs::write(&png, sample_png(64, 64)).unwrap();
        let ico = dir.path().join("out/icon.ico");
        png_to_ico(&png, &ico).unwrap();
        let back = image::load_from_memory_with_format(&std::fs::read(&ico).unwrap(), ImageFormat::Ico).unwrap();
        assert_eq!(back.dimensions(), (64, 64));
    }

    #[test]
    fn non_png_input_is_rejected() {
        assert!(png_bytes_to_ico(b"not a png").is_err());
    }
}
