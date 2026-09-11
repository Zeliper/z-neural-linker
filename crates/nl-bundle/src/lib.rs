//! `.nlapp` 번들 (zip) 읽기/쓰기와 런타임 바이너리 첨부. 규약은 `nl_core::bundle`.

pub mod icon;
pub mod inno;
pub mod manifest;
pub mod tools;

pub use icon::{png_bytes_to_ico, png_bytes_to_square_png, png_to_ico};
pub use manifest::{build_manifest, write_manifest, MANIFEST_FILE};
// 빌더가 매니페스트를 만들 때 nl-update 를 따로 의존하지 않아도 되도록 다시 내보낸다.
pub use nl_update::{Asset, AssetKind, Manifest as UpdateManifest};
pub use inno::{app_id, find_inno_setup, render_iss, windows_installer, InnoSetup};
pub use tools::{install_inno_setup_plan, run_tool_plan, ToolPlan, ToolProgress};

use nl_core::bundle::{trailer, BUNDLE_TRAILER_MAGIC, MANIFEST_NAME, PROJECT_NAME, WEIGHTS_DIR};
use nl_core::{BundleManifest, Project, ProjectFile};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

/// zip 안에서 에셋이 놓이는 디렉터리.
pub const ASSETS_DIR: &str = "assets";
/// 실행 파일 꼬리표 길이 (u64 길이 + 매직 6바이트).
pub const TRAILER_LEN: u64 = 14;

/// 메모리에 풀린 번들.
#[derive(Clone, Debug)]
pub struct Bundle {
    pub manifest: BundleManifest,
    pub project: Project,
    /// `weights/<file>` → 바이트.
    pub weights: std::collections::BTreeMap<String, Vec<u8>>,
    /// `assets/<path>` → 바이트.
    pub assets: std::collections::BTreeMap<String, Vec<u8>>,
}

impl Bundle {
    /// 매니페스트와 프로젝트만 담은 빈 번들.
    pub fn new(manifest: BundleManifest, project: Project) -> Self {
        Self { manifest, project, weights: BTreeMap::new(), assets: BTreeMap::new() }
    }

    /// zip 바이트로 직렬화.
    pub fn to_zip(&self) -> anyhow::Result<Vec<u8>> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::<u8>::new()));
        let opts = file_options();

        zip.start_file(MANIFEST_NAME, opts)?;
        zip.write_all(serde_json::to_string_pretty(&self.manifest)?.as_bytes())?;

        zip.start_file(PROJECT_NAME, opts)?;
        zip.write_all(ProjectFile::new(self.project.clone()).to_json().as_bytes())?;

        for (name, bytes) in &self.weights {
            zip.start_file(entry_path(WEIGHTS_DIR, name), opts)?;
            zip.write_all(bytes)?;
        }
        for (name, bytes) in &self.assets {
            zip.start_file(entry_path(ASSETS_DIR, name), opts)?;
            zip.write_all(bytes)?;
        }
        Ok(zip.finish()?.into_inner())
    }

    pub fn from_zip(bytes: &[u8]) -> anyhow::Result<Self> {
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes))?;

        let manifest: BundleManifest = serde_json::from_slice(&read_entry(&mut zip, MANIFEST_NAME)?)
            .map_err(|e| anyhow::anyhow!("{MANIFEST_NAME} 을(를) 읽지 못했습니다: {e}"))?;
        let project = ProjectFile::from_json(&String::from_utf8(read_entry(&mut zip, PROJECT_NAME)?)?)
            .map_err(|e| anyhow::anyhow!("{PROJECT_NAME} 을(를) 읽지 못했습니다: {e}"))?
            .project;

        let mut weights = BTreeMap::new();
        let mut assets = BTreeMap::new();
        for i in 0..zip.len() {
            let mut f = zip.by_index(i)?;
            if f.is_dir() {
                continue;
            }
            let name = f.name().to_string();
            let target = match (strip_dir(&name, WEIGHTS_DIR), strip_dir(&name, ASSETS_DIR)) {
                (Some(rest), _) => Some((&mut weights, rest.to_string())),
                (None, Some(rest)) => Some((&mut assets, rest.to_string())),
                (None, None) => None,
            };
            let Some((map, key)) = target else { continue };
            if key.is_empty() {
                continue;
            }
            let mut buf = Vec::with_capacity(f.size() as usize);
            f.read_to_end(&mut buf)?;
            map.insert(key, buf);
        }
        Ok(Self { manifest, project, weights, assets })
    }

    /// 가중치를 임시 폴더에 풀어 `Session::load` 가 읽을 경로를 돌려준다.
    /// 키는 `weights` 맵의 키 그대로라 `BundledModel::weights_file` 로 바로 찾을 수 있다.
    pub fn materialize_weights(&self, dir: &Path) -> anyhow::Result<std::collections::BTreeMap<String, PathBuf>> {
        std::fs::create_dir_all(dir)?;
        let mut out = BTreeMap::new();
        for (name, bytes) in &self.weights {
            let rel = safe_relative(name)
                .ok_or_else(|| anyhow::anyhow!("가중치 이름이 폴더 밖을 가리킵니다: {name}"))?;
            let path = dir.join(&rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, bytes)?;
            out.insert(name.clone(), path);
        }
        Ok(out)
    }

    /// 에셋도 같은 규칙으로 풀어 놓는다 (아이콘·라벨 목록 등).
    pub fn materialize_assets(&self, dir: &Path) -> anyhow::Result<std::collections::BTreeMap<String, PathBuf>> {
        std::fs::create_dir_all(dir)?;
        let mut out = BTreeMap::new();
        for (name, bytes) in &self.assets {
            let rel =
                safe_relative(name).ok_or_else(|| anyhow::anyhow!("에셋 이름이 폴더 밖을 가리킵니다: {name}"))?;
            let path = dir.join(&rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, bytes)?;
            out.insert(name.clone(), path);
        }
        Ok(out)
    }
}

fn file_options() -> zip::write::SimpleFileOptions {
    zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644)
}

/// `weights` + `a/b.bin` → `weights/a/b.bin`. 이미 접두사가 붙어 있으면 그대로 둔다.
fn entry_path(dir: &str, name: &str) -> String {
    let name = name.trim_start_matches('/');
    if name == dir || name.starts_with(&format!("{dir}/")) {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// zip 항목 이름에서 `<dir>/` 접두사를 떼어낸다. 다른 디렉터리면 `None`.
fn strip_dir<'a>(name: &'a str, dir: &str) -> Option<&'a str> {
    name.strip_prefix(dir)?.strip_prefix('/')
}

/// `..`·절대 경로·드라이브 접두사를 걸러 낸 상대 경로.
fn safe_relative(name: &str) -> Option<PathBuf> {
    use std::path::Component;
    let p = Path::new(name);
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(s) => out.push(s),
            Component::CurDir => {}
            _ => return None,
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

fn read_entry<R: Read + Seek>(zip: &mut zip::ZipArchive<R>, name: &str) -> anyhow::Result<Vec<u8>> {
    let mut f = zip.by_name(name).map_err(|_| anyhow::anyhow!("번들에 {name} 이(가) 없습니다"))?;
    let mut buf = Vec::with_capacity(f.size() as usize);
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

// ───────────────────────────── 실행 파일 첨부 ─────────────────────────────

/// 런타임 실행 파일 + 번들 + 꼬리표 → `out`. 실행 권한 유지.
pub fn attach(runtime_exe: &Path, bundle_zip: &[u8], out: &Path) -> anyhow::Result<()> {
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut src = File::open(runtime_exe)
        .map_err(|e| anyhow::anyhow!("런타임 실행 파일을 열지 못했습니다 ({}): {e}", runtime_exe.display()))?;
    let meta = src.metadata()?;
    let mut dst = File::create(out)?;
    std::io::copy(&mut src, &mut dst)?;
    dst.write_all(bundle_zip)?;
    dst.write_all(&trailer(bundle_zip.len() as u64))?;
    dst.flush()?;
    drop(dst);
    set_executable(out, &meta)?;
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path, src_meta: &std::fs::Metadata) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // 원본 모드를 잇되 소유자/그룹/기타 실행 비트는 반드시 켜 둔다.
    let mode = (src_meta.permissions().mode() & 0o7777) | 0o755;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path, _src_meta: &std::fs::Metadata) -> anyhow::Result<()> {
    // Windows 는 확장자로 실행 여부가 정해져 별도 권한 설정이 없다.
    Ok(())
}

/// 현재 실행 파일(또는 주어진 파일)에 첨부된 번들을 읽는다. 없으면 `Ok(None)`.
/// 실행 파일 전체를 메모리에 올리지 않도록 꼬리표 14바이트만 읽고 `seek` 로 번들 구간만 가져온다.
pub fn read_attached(exe: &Path) -> anyhow::Result<Option<Bundle>> {
    let mut f = File::open(exe)?;
    let total = f.seek(SeekFrom::End(0))?;
    if total < TRAILER_LEN {
        return Ok(None);
    }
    f.seek(SeekFrom::Start(total - TRAILER_LEN))?;
    let mut tail = [0u8; TRAILER_LEN as usize];
    f.read_exact(&mut tail)?;
    let Some(range) = attached_range(total, &tail) else { return Ok(None) };
    f.seek(SeekFrom::Start(range.start))?;
    let mut zip = vec![0u8; (range.end - range.start) as usize];
    f.read_exact(&mut zip)?;
    Bundle::from_zip(&zip).map(Some)
}

/// `nl_core::bundle::find_attached` 와 같은 규약을 파일 전체를 읽지 않고 적용한다.
/// 두 구현이 어긋나지 않는지는 `attached_range_matches_core` 테스트가 지킨다.
fn attached_range(total: u64, tail: &[u8; TRAILER_LEN as usize]) -> Option<Range<u64>> {
    if &tail[8..] != BUNDLE_TRAILER_MAGIC {
        return None;
    }
    let len = u64::from_le_bytes(tail[..8].try_into().ok()?);
    let end = total - TRAILER_LEN;
    if len > end {
        return None;
    }
    Some(end - len..end)
}

/// 대상 플랫폼의 런타임 실행 파일을 찾는다.
/// ① 현재 실행 파일 옆 `runtimes/<triple>/<file>` ② 현재 실행 파일 옆 `<file>` ③ `NL_RUNTIMES_DIR`.
pub fn find_runtime(target: Target) -> Option<PathBuf> {
    let file = target.runtime_file_name();
    let triple = target.triple();
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        candidates.push(dir.join("runtimes").join(triple).join(file));
        candidates.push(dir.join(file));
    }
    if let Some(dir) = std::env::var_os("NL_RUNTIMES_DIR") {
        let dir = PathBuf::from(dir);
        candidates.push(dir.join(triple).join(file));
        candidates.push(dir.join(file));
    }
    candidates.into_iter().find(|p| p.is_file())
}

// ───────────────────────────── 배포 아카이브 ─────────────────────────────

const INSTALL_SH: &str = include_str!("../templates/install.sh");
const DESKTOP: &str = include_str!("../templates/app.desktop");
const README_LINUX: &str = include_str!("../templates/README-linux.txt");
const README_WINDOWS: &str = include_str!("../templates/README-windows.txt");

/// `archive_with` 에 넘기는 설정. 기존 `archive` 가 받던 것에 아이콘이 더해졌다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveOptions<'a> {
    pub target: Target,
    /// 번들이 첨부된 앱 실행 파일.
    pub app_exe: &'a Path,
    pub app_name: &'a str,
    pub version: &'a str,
    pub out_dir: &'a Path,
    /// 앱 아이콘 PNG. Linux 아카이브에 `<slug>.png` 로 들어가고 `install.sh` 가 아이콘 테마에 설치한다.
    pub icon: Option<&'a Path>,
}

impl<'a> ArchiveOptions<'a> {
    pub fn new(target: Target, app_exe: &'a Path, app_name: &'a str, version: &'a str, out_dir: &'a Path) -> Self {
        Self { target, app_exe, app_name, version, out_dir, icon: None }
    }

    pub fn icon(mut self, icon: Option<&'a Path>) -> Self {
        self.icon = icon;
        self
    }
}

/// 배포 아카이브: Linux `tar.gz`(실행 파일 + install.sh + .desktop), Windows `zip`. 산출물 경로와 sha256 을 돌려준다.
/// 아이콘까지 넣으려면 `archive_with` 를 쓴다.
pub fn archive(target: Target, app_exe: &Path, app_name: &str, version: &str, out_dir: &Path) -> anyhow::Result<Artifact> {
    archive_with(ArchiveOptions::new(target, app_exe, app_name, version, out_dir))
}

/// 아이콘을 비롯한 추가 설정까지 받는 배포 아카이브.
pub fn archive_with(opts: ArchiveOptions<'_>) -> anyhow::Result<Artifact> {
    let ArchiveOptions { target, app_exe, app_name, version, out_dir, icon } = opts;
    let slug = slugify(app_name);
    let exe_bytes = std::fs::read(app_exe)
        .map_err(|e| anyhow::anyhow!("앱 실행 파일을 읽지 못했습니다 ({}): {e}", app_exe.display()))?;

    // 아이콘은 아이콘 테마가 요구하는 정사각 PNG 로 맞춰 둔다.
    let icon_png = match icon {
        Some(p) => {
            let raw = std::fs::read(p)
                .map_err(|e| anyhow::anyhow!("아이콘을 읽지 못했습니다 ({}): {e}", p.display()))?;
            Some(icon::png_bytes_to_square_png(&raw, icon::LINUX_ICON_SIZE)?)
        }
        None => None,
    };

    std::fs::create_dir_all(out_dir)?;
    let path = match target {
        Target::LinuxX64 => {
            let out = out_dir.join(format!("{slug}-{version}-linux-x86_64.tar.gz"));
            write_tar_gz(&out, &slug, app_name, version, &exe_bytes, icon_png.as_deref())?;
            out
        }
        Target::WindowsX64 => {
            let out = out_dir.join(format!("{slug}-{version}-windows-x86_64.zip"));
            write_windows_zip(&out, &slug, app_name, version, &exe_bytes)?;
            out
        }
    };

    let bytes = std::fs::read(&path)?;
    Ok(Artifact { sha256: sha256_hex(&bytes), size: bytes.len() as u64, path })
}

fn write_tar_gz(
    out: &Path,
    slug: &str,
    app_name: &str,
    version: &str,
    exe: &[u8],
    icon_png: Option<&[u8]>,
) -> anyhow::Result<()> {
    let file = File::create(out)?;
    let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);

    let install = fill(INSTALL_SH, slug, app_name, version);
    let desktop = fill(DESKTOP, slug, app_name, version);
    let readme = fill(README_LINUX, slug, app_name, version);

    tar_append(&mut tar, &format!("{slug}/{slug}"), exe, 0o755)?;
    tar_append(&mut tar, &format!("{slug}/install.sh"), install.as_bytes(), 0o755)?;
    tar_append(&mut tar, &format!("{slug}/{slug}.desktop"), desktop.as_bytes(), 0o644)?;
    tar_append(&mut tar, &format!("{slug}/README.txt"), readme.as_bytes(), 0o644)?;
    if let Some(png) = icon_png {
        tar_append(&mut tar, &format!("{slug}/{slug}.png"), png, 0o644)?;
    }

    tar.into_inner()?.finish()?;
    Ok(())
}

fn tar_append<W: Write>(tar: &mut tar::Builder<W>, path: &str, data: &[u8], mode: u32) -> anyhow::Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(mode);
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    tar.append_data(&mut header, path, data)?;
    Ok(())
}

fn write_windows_zip(out: &Path, slug: &str, app_name: &str, version: &str, exe: &[u8]) -> anyhow::Result<()> {
    let mut zip = zip::ZipWriter::new(File::create(out)?);
    zip.start_file(format!("{slug}.exe"), file_options().unix_permissions(0o755))?;
    zip.write_all(exe)?;
    zip.start_file("README.txt", file_options())?;
    zip.write_all(fill(README_WINDOWS, slug, app_name, version).as_bytes())?;
    zip.finish()?.sync_all()?;
    Ok(())
}

fn fill(template: &str, slug: &str, app_name: &str, version: &str) -> String {
    fill_tokens(
        template,
        "{{",
        &[("{{APP_SLUG}}", slug), ("{{APP_NAME}}", app_name), ("{{APP_VERSION}}", version)],
    )
}

/// 템플릿을 한 번만 훑어 치환한다. 값 안에 다른 자리표시자 문자열이 들어 있어도 다시 치환되지 않는다.
/// `open` 은 자리표시자가 시작하는 글자(`@` 또는 `{{`), `pairs` 는 구분자를 포함한 전체 토큰과 값이다.
pub(crate) fn fill_tokens(template: &str, open: &str, pairs: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + 256);
    let mut rest = template;
    'scan: while let Some(at) = rest.find(open) {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        for (token, value) in pairs {
            if let Some(next) = tail.strip_prefix(*token) {
                out.push_str(value);
                rest = next;
                continue 'scan;
            }
        }
        // 아는 토큰이 아니면 여는 글자 하나만 흘려보내고 계속 찾는다.
        out.push_str(&tail[..open.len()]);
        rest = &tail[open.len()..];
    }
    out.push_str(rest);
    out
}

/// 파일·디렉터리 이름으로 안전한 소문자 ASCII 슬러그. 한글 등 비 ASCII 는 `-` 로 접힌다.
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "app".into()
    } else {
        out
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    LinuxX64,
    WindowsX64,
}

impl Target {
    pub fn label(&self) -> &'static str {
        match self {
            Target::LinuxX64 => "Linux x86_64",
            Target::WindowsX64 => "Windows x86_64",
        }
    }
    pub fn triple(&self) -> &'static str {
        match self {
            Target::LinuxX64 => "x86_64-unknown-linux-gnu",
            Target::WindowsX64 => "x86_64-pc-windows-msvc",
        }
    }
    pub fn runtime_file_name(&self) -> &'static str {
        match self {
            Target::LinuxX64 => "nl-runtime",
            Target::WindowsX64 => "nl-runtime.exe",
        }
    }
    pub fn host() -> Option<Target> {
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            Some(Target::LinuxX64)
        } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
            Some(Target::WindowsX64)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Artifact {
    pub path: PathBuf,
    pub sha256: String,
    pub size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use nl_core::bundle::{find_attached, BundledModel};

    fn sample() -> Bundle {
        let mut project = Project::new("샘플 프로젝트");
        let m1 = project.add_model("분류기");
        let m2 = project.add_model("회귀");
        let mut manifest = BundleManifest::new("내 앱", "1.2.3");
        manifest.built_with = "nl-app 테스트".into();
        manifest.models = vec![
            BundledModel { model: m1, weights_file: "classifier.safetensors".into() },
            BundledModel { model: m2, weights_file: "regressor.safetensors".into() },
        ];
        let mut b = Bundle::new(manifest, project);
        b.weights.insert("classifier.safetensors".into(), vec![1, 2, 3, 4, 5, 0, 255]);
        b.weights.insert("regressor.safetensors".into(), (0u8..64).collect());
        b.assets.insert("labels.txt".into(), "고양이\n개\n".as_bytes().to_vec());
        b.assets.insert("icons/app.png".into(), vec![0x89, b'P', b'N', b'G']);
        b
    }

    #[test]
    fn zip_round_trips() {
        let b = sample();
        let bytes = b.to_zip().unwrap();
        let back = Bundle::from_zip(&bytes).unwrap();
        assert_eq!(back.manifest, b.manifest);
        assert_eq!(back.manifest.models.len(), 2);
        assert_eq!(back.project, b.project);
        assert_eq!(back.project.models.len(), 2);
        assert_eq!(back.weights, b.weights);
        assert_eq!(back.assets, b.assets);
    }

    #[test]
    fn materialize_writes_weight_bytes() {
        let b = sample();
        let dir = tempfile::tempdir().unwrap();
        let map = b.materialize_weights(dir.path()).unwrap();
        assert_eq!(map.len(), 2);
        for (name, path) in &map {
            assert_eq!(&std::fs::read(path).unwrap(), &b.weights[name]);
        }
        let assets = b.materialize_assets(dir.path()).unwrap();
        assert_eq!(std::fs::read(&assets["icons/app.png"]).unwrap(), b.assets["icons/app.png"]);
    }

    #[test]
    fn attach_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let fake_exe = dir.path().join("nl-runtime");
        let exe_bytes: Vec<u8> = b"\x7fELF fake runtime binary bytes".iter().copied().chain(0u8..200).collect();
        std::fs::write(&fake_exe, &exe_bytes).unwrap();

        let b = sample();
        let zip = b.to_zip().unwrap();
        let out = dir.path().join("myapp");
        attach(&fake_exe, &zip, &out).unwrap();

        // 앞부분은 원본 실행 파일 그대로여야 한다.
        let written = std::fs::read(&out).unwrap();
        assert_eq!(&written[..exe_bytes.len()], &exe_bytes[..]);
        assert_eq!(written.len(), exe_bytes.len() + zip.len() + TRAILER_LEN as usize);

        let back = read_attached(&out).unwrap().expect("첨부 번들");
        assert_eq!(back.manifest, b.manifest);
        assert_eq!(back.weights, b.weights);
        assert_eq!(back.assets, b.assets);

        // 꼬리표가 없는 파일은 None.
        assert!(read_attached(&fake_exe).unwrap().is_none());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&out).unwrap().permissions().mode() & 0o111, 0o111);
        }
    }

    #[test]
    fn attached_range_matches_core() {
        let mut exe = b"binary".to_vec();
        let bundle = b"PK\x03\x04 zip bytes";
        exe.extend_from_slice(bundle);
        exe.extend_from_slice(&trailer(bundle.len() as u64));

        let core = find_attached(&exe).unwrap();
        let tail: [u8; TRAILER_LEN as usize] = exe[exe.len() - TRAILER_LEN as usize..].try_into().unwrap();
        let ours = attached_range(exe.len() as u64, &tail).unwrap();
        assert_eq!(ours.start as usize, core.start);
        assert_eq!(ours.end as usize, core.end);

        // 길이가 파일보다 크면 둘 다 거부한다.
        let mut bad = b"tiny".to_vec();
        bad.extend_from_slice(&trailer(9_999));
        assert!(find_attached(&bad).is_none());
        let tail: [u8; TRAILER_LEN as usize] = bad[bad.len() - TRAILER_LEN as usize..].try_into().unwrap();
        assert!(attached_range(bad.len() as u64, &tail).is_none());
    }

    #[test]
    fn linux_archive_has_launcher_files() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("built-app");
        std::fs::write(&exe, b"fake app binary").unwrap();

        let art = archive(Target::LinuxX64, &exe, "내 앱", "1.2.3", dir.path()).unwrap();
        assert_eq!(art.path.file_name().unwrap(), "app-1.2.3-linux-x86_64.tar.gz");
        assert!(art.size > 0);
        assert_eq!(art.sha256.len(), 64);
        assert_eq!(art.sha256, sha256_hex(&std::fs::read(&art.path).unwrap()));

        let gz = flate2::read::GzDecoder::new(File::open(&art.path).unwrap());
        let mut tar = tar::Archive::new(gz);
        let mut names = Vec::new();
        let mut install = String::new();
        for e in tar.entries().unwrap() {
            let mut e = e.unwrap();
            let name = e.path().unwrap().to_string_lossy().to_string();
            if name.ends_with("install.sh") {
                e.read_to_string(&mut install).unwrap();
                assert_eq!(e.header().mode().unwrap() & 0o111, 0o111);
            }
            if name.ends_with("/app") {
                assert_eq!(e.header().mode().unwrap() & 0o111, 0o111);
            }
            names.push(name);
        }
        assert!(names.len() >= 3, "아카이브 항목이 부족합니다: {names:?}");
        for want in ["app/app", "app/install.sh", "app/app.desktop", "app/README.txt"] {
            assert!(names.iter().any(|n| n == want), "{want} 가 없습니다: {names:?}");
        }
        assert!(install.contains("내 앱"), "install.sh 에 앱 이름이 없습니다");
        assert!(!install.contains("{{APP_SLUG}}"), "치환되지 않은 자리표시자가 남았습니다");
    }

    #[test]
    fn windows_archive_has_exe_and_readme() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("built-app.exe");
        std::fs::write(&exe, b"MZ fake").unwrap();

        let art = archive(Target::WindowsX64, &exe, "Demo App", "0.9.0", dir.path()).unwrap();
        assert_eq!(art.path.file_name().unwrap(), "demo-app-0.9.0-windows-x86_64.zip");

        let mut zip = zip::ZipArchive::new(File::open(&art.path).unwrap()).unwrap();
        let names: Vec<String> = zip.file_names().map(str::to_string).collect();
        assert!(names.contains(&"demo-app.exe".to_string()), "{names:?}");
        assert!(names.contains(&"README.txt".to_string()), "{names:?}");
        let mut readme = String::new();
        zip.by_name("README.txt").unwrap().read_to_string(&mut readme).unwrap();
        assert!(readme.contains("Demo App 0.9.0"));
    }

    #[test]
    fn linux_archive_with_icon_adds_png_and_installs_it() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("built-app");
        std::fs::write(&exe, b"fake app binary").unwrap();
        let icon = dir.path().join("icon.png");
        std::fs::write(&icon, icon::sample_png(300, 300)).unwrap();

        let art = archive_with(
            ArchiveOptions::new(Target::LinuxX64, &exe, "내 앱", "1.2.3", dir.path()).icon(Some(&icon)),
        )
        .unwrap();

        let gz = flate2::read::GzDecoder::new(File::open(&art.path).unwrap());
        let mut tar = tar::Archive::new(gz);
        let mut names = Vec::new();
        let mut png = Vec::new();
        let mut install = String::new();
        for e in tar.entries().unwrap() {
            let mut e = e.unwrap();
            let name = e.path().unwrap().to_string_lossy().to_string();
            if name.ends_with(".png") {
                e.read_to_end(&mut png).unwrap();
            }
            if name.ends_with("install.sh") {
                e.read_to_string(&mut install).unwrap();
            }
            names.push(name);
        }
        assert!(names.iter().any(|n| n == "app/app.png"), "{names:?}");
        // 아이콘 테마가 요구하는 256×256 정사각으로 맞춰 들어간다.
        let img = image::load_from_memory_with_format(&png, image::ImageFormat::Png).unwrap();
        assert_eq!((img.width(), img.height()), (icon::LINUX_ICON_SIZE, icon::LINUX_ICON_SIZE));
        assert!(install.contains("hicolor/256x256/apps"), "install.sh 가 아이콘을 설치하지 않습니다");
        assert!(install.contains("app.png"));
    }

    #[test]
    fn linux_archive_without_icon_has_no_png() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("built-app");
        std::fs::write(&exe, b"x").unwrap();
        let art = archive(Target::LinuxX64, &exe, "내 앱", "1.0.0", dir.path()).unwrap();

        let gz = flate2::read::GzDecoder::new(File::open(&art.path).unwrap());
        let mut tar = tar::Archive::new(gz);
        let names: Vec<String> =
            tar.entries().unwrap().map(|e| e.unwrap().path().unwrap().to_string_lossy().to_string()).collect();
        assert!(!names.iter().any(|n| n.ends_with(".png")), "{names:?}");
        assert_eq!(names.len(), 4);
    }

    #[test]
    fn fill_does_not_substitute_inside_substituted_values() {
        // 앱 이름이 다른 자리표시자처럼 생겨도 한 번만 치환된다.
        let out = fill("이름={{APP_NAME}} 버전={{APP_VERSION}}", "slug", "{{APP_VERSION}}", "9.9");
        assert_eq!(out, "이름={{APP_VERSION}} 버전=9.9");
    }

    #[test]
    fn fill_tokens_leaves_unknown_markers_alone() {
        assert_eq!(fill_tokens("a@B@c", "@", &[("@X@", "1")]), "a@B@c");
        assert_eq!(fill_tokens("a@X@c", "@", &[("@X@", "1")]), "a1c");
        assert_eq!(fill_tokens("{{A}}{{B}}", "{{", &[("{{A}}", "1")]), "1{{B}}");
    }

    #[test]
    fn slugify_folds_non_ascii() {
        assert_eq!(slugify("내 앱"), "app");
        assert_eq!(slugify("Demo App"), "demo-app");
        assert_eq!(slugify("Foo__Bar 2"), "foo-bar-2");
        assert_eq!(slugify(""), "app");
        assert_eq!(slugify("---"), "app");
    }

    #[test]
    fn entry_paths_do_not_double_prefix() {
        assert_eq!(entry_path("weights", "a.bin"), "weights/a.bin");
        assert_eq!(entry_path("weights", "weights/a.bin"), "weights/a.bin");
        assert_eq!(strip_dir("weights/a.bin", "weights"), Some("a.bin"));
        assert_eq!(strip_dir("assets/a.bin", "weights"), None);
        assert!(safe_relative("../escape").is_none());
        assert!(safe_relative("/etc/passwd").is_none());
    }

    /// `NL_RUNTIMES_DIR` 은 프로세스 전역이라 이 시험 하나로 두 대상을 함께 본다 —
    /// 나눠 두면 병렬 실행 때 서로의 환경 변수를 지워 경쟁한다.
    #[test]
    fn find_runtime_reads_env_dir() {
        let dir = tempfile::tempdir().unwrap();
        for target in [Target::LinuxX64, Target::WindowsX64] {
            let triple_dir = dir.path().join(target.triple());
            std::fs::create_dir_all(&triple_dir).unwrap();
            std::fs::write(triple_dir.join(target.runtime_file_name()), b"MZ").unwrap();
        }

        unsafe { std::env::set_var("NL_RUNTIMES_DIR", dir.path()) };
        let linux = find_runtime(Target::LinuxX64);
        let windows = find_runtime(Target::WindowsX64);
        unsafe { std::env::remove_var("NL_RUNTIMES_DIR") };

        // 현재 실행 파일 옆에 nl-runtime 이 있으면 그쪽이 먼저다 — 이름만 확인한다.
        let linux = linux.expect("Linux 런타임을 찾지 못했습니다");
        assert_eq!(linux.file_name().unwrap(), "nl-runtime");

        // Windows 용은 테스트 바이너리 옆에 있을 리 없으니 triple 폴더에서 찾아야 한다.
        let windows = windows.expect("Windows 런타임을 찾지 못했습니다");
        assert_eq!(windows.file_name().unwrap(), "nl-runtime.exe");
        assert!(windows.starts_with(dir.path()), "{}", windows.display());
        assert!(
            windows.parent().unwrap().ends_with("x86_64-pc-windows-msvc"),
            "크로스 빌드 산출물을 두는 triple 폴더에서 찾아야 합니다: {}",
            windows.display()
        );
    }
}
