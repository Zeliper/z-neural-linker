//! 바깥에서 들어온 데이터에 거는 상한.
//!
//! 프로젝트 파일·데이터셋·체크포인트·HTTP 페이로드는 모두 신뢰할 수 없는 입력이다. 상한이 없으면
//! 작은 파일 하나가 수십 GB 할당을 시도해 프로세스를 **패닉이 아니라 abort** 로 끝낸다
//! (할당 실패는 잡을 수 없다). 여기 모아 둔 값은 "정상적인 사용을 막지 않으면서 폭탄을 거르는"
//! 선이며, 넘으면 언제나 `Err` 로 돌려준다.

/// 이미지 한 변의 최대 픽셀 수.
pub const MAX_IMAGE_DIM: u32 = 8192;

/// 이미지 하나의 최대 픽셀 수 (변마다 상한을 넘지 않아도 전체가 크면 거절).
pub const MAX_IMAGE_PIXELS: u64 = 4096 * 4096;

/// 이미지 디코더가 잡을 수 있는 최대 메모리.
pub const MAX_IMAGE_ALLOC: u64 = 256 * 1024 * 1024;

/// 텐서 하나의 최대 원소 수 (f32 기준 약 1 GiB).
pub const MAX_TENSOR_ELEMS: usize = 256 * 1024 * 1024;

/// `OneHot`·`ClassLabel` 이 만들 수 있는 최대 클래스 수.
pub const MAX_CLASSES: usize = 1_000_000;

/// `Tokenize` 의 최대 길이.
pub const MAX_TOKENS: usize = 1_000_000;

/// 체크포인트 파일의 최대 바이트.
pub const MAX_WEIGHTS_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// 체크포인트가 담을 수 있는 최대 텐서 개수.
pub const MAX_WEIGHTS_TENSORS: usize = 100_000;

/// 파라미터 이름의 최대 길이 (바이트).
pub const MAX_PARAM_NAME_LEN: usize = 1024;

/// CSV 파일의 최대 바이트.
pub const MAX_CSV_BYTES: u64 = 512 * 1024 * 1024;

/// CSV 의 최대 행 수.
pub const MAX_CSV_ROWS: usize = 5_000_000;

/// CSV 한 행의 최대 열 수.
pub const MAX_CSV_COLS: usize = 4096;

/// 데이터셋 하나가 담을 수 있는 최대 f32 원소 수 (약 4 GiB).
pub const MAX_DATASET_ELEMS: usize = 1024 * 1024 * 1024;

/// `labels.jsonl` 의 최대 바이트.
pub const MAX_LABELS_BYTES: u64 = 64 * 1024 * 1024;

/// 곱이 `MAX_TENSOR_ELEMS` 를 넘지 않는지 검사하며 형상의 원소 수를 센다.
///
/// 래핑도 거대 할당도 없이 실패한다 — `usize` 곱셈은 릴리스 빌드에서 조용히 래핑하므로
/// `shape.iter().product()` 를 그대로 믿으면 안 된다.
pub fn checked_elems(shape: &[usize], what: &str) -> anyhow::Result<usize> {
    let mut n: usize = 1;
    for &d in shape {
        n = n
            .checked_mul(d)
            .ok_or_else(|| anyhow::anyhow!("{what} 형상 {shape:?} 의 원소 수가 너무 큽니다"))?;
    }
    if n > MAX_TENSOR_ELEMS {
        anyhow::bail!("{what} 형상 {shape:?} 은 원소 {n} 개로 상한 {MAX_TENSOR_ELEMS} 를 넘습니다");
    }
    Ok(n)
}

/// 파일이 상한 안인지 본다. 크기를 알 수 없으면 통과시킨다 (파이프 등).
pub fn check_file_size(path: &std::path::Path, max: u64, what: &str) -> anyhow::Result<()> {
    let Ok(meta) = std::fs::metadata(path) else {
        return Ok(());
    };
    if meta.len() > max {
        anyhow::bail!(
            "{what} 이 {:.1} MiB 로 상한 {:.1} MiB 를 넘습니다: {}",
            meta.len() as f64 / (1024.0 * 1024.0),
            max as f64 / (1024.0 * 1024.0),
            path.display()
        );
    }
    Ok(())
}

/// 이미지 디코더에 걸 한도. 기본값은 픽셀 치수 상한이 없어(`image` 0.25 확인) 직접 채운다.
pub fn image_limits() -> image::Limits {
    let mut l = image::Limits::default();
    l.max_image_width = Some(MAX_IMAGE_DIM);
    l.max_image_height = Some(MAX_IMAGE_DIM);
    l.max_alloc = Some(MAX_IMAGE_ALLOC);
    l
}

/// 바이트 버퍼를 이미지로 디코드한다. 포맷은 내용으로 추측하고 상한을 건다.
pub fn decode_image(bytes: &[u8], what: &str) -> anyhow::Result<image::DynamicImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .with_context_what(what)?;
    reader.limits(image_limits());
    let img = reader
        .decode()
        .map_err(|e| anyhow::anyhow!("{what} 디코드 실패: {e}"))?;
    check_image_size(img.width(), img.height(), what)?;
    Ok(img)
}

/// 파일에서 이미지를 디코드한다 (같은 상한).
pub fn decode_image_file(path: &std::path::Path) -> anyhow::Result<image::DynamicImage> {
    check_file_size(path, MAX_IMAGE_ALLOC, "이미지 파일")?;
    let mut reader = image::ImageReader::open(path)
        .map_err(|e| anyhow::anyhow!("이미지 열기 실패 ({}): {e}", path.display()))?
        .with_guessed_format()
        .map_err(|e| anyhow::anyhow!("이미지 포맷을 알 수 없습니다 ({}): {e}", path.display()))?;
    reader.limits(image_limits());
    let img = reader
        .decode()
        .map_err(|e| anyhow::anyhow!("이미지 디코드 실패 ({}): {e}", path.display()))?;
    check_image_size(img.width(), img.height(), &format!("이미지 {}", path.display()))?;
    Ok(img)
}

/// 픽셀 치수가 상한 안인지. 디코더가 통과시켜도 뒤따르는 f32 변환이 터질 수 있어 한 번 더 본다.
pub fn check_image_size(w: u32, h: u32, what: &str) -> anyhow::Result<()> {
    if w == 0 || h == 0 {
        anyhow::bail!("{what} 의 크기가 0 입니다 ({w}×{h})");
    }
    if w > MAX_IMAGE_DIM || h > MAX_IMAGE_DIM {
        anyhow::bail!("{what} 이 {w}×{h} 로 한 변 상한 {MAX_IMAGE_DIM} 을 넘습니다");
    }
    if (w as u64) * (h as u64) > MAX_IMAGE_PIXELS {
        anyhow::bail!("{what} 이 {w}×{h} 로 픽셀 상한 {MAX_IMAGE_PIXELS} 를 넘습니다");
    }
    Ok(())
}

/// `with_guessed_format` 의 io 오류에 맥락을 붙이는 작은 도우미.
trait ContextWhat<T> {
    fn with_context_what(self, what: &str) -> anyhow::Result<T>;
}

impl<T, E: std::fmt::Display> ContextWhat<T> for Result<T, E> {
    fn with_context_what(self, what: &str) -> anyhow::Result<T> {
        self.map_err(|e| anyhow::anyhow!("{what} 의 이미지 포맷을 알 수 없습니다: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_elems_rejects_overflow_and_bombs() {
        assert_eq!(checked_elems(&[2, 3, 4], "t").unwrap(), 24);
        assert_eq!(checked_elems(&[], "t").unwrap(), 1);
        // 곱이 usize 를 넘는다 — 래핑해서 작은 수가 되면 안 된다.
        let huge = [usize::MAX / 2, 4];
        assert!(checked_elems(&huge, "t").is_err());
        // 래핑은 안 하지만 상한은 넘는다.
        assert!(checked_elems(&[3, 100_000, 100_000], "t").is_err());
    }

    #[test]
    fn image_size_guard_rejects_zero_and_bombs() {
        assert!(check_image_size(0, 8, "i").is_err());
        assert!(check_image_size(8, 0, "i").is_err());
        assert!(check_image_size(MAX_IMAGE_DIM + 1, 8, "i").is_err());
        assert!(check_image_size(8000, 8000, "i").is_err(), "픽셀 수 상한");
        assert!(check_image_size(1920, 1080, "i").is_ok());
    }

    #[test]
    fn decode_image_rejects_garbage() {
        let e = decode_image(b"not an image at all", "입력").unwrap_err().to_string();
        assert!(e.contains("입력"), "{e}");
    }
}
