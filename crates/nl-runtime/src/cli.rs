//! 명령줄 인자 해석. GUI 와 헤드리스 양쪽에서 같은 결과를 쓴다.

use nl_core::DevicePref;
use std::path::PathBuf;

pub const USAGE: &str = "\
사용법: nl-runtime [옵션] [파일.nlapp]

  파일을 주지 않으면 실행 파일에 첨부된 번들을 실행합니다.

옵션
  --headless            창 없이 파이프라인만 실행합니다 (Ctrl+C 로 종료).
  --run-for <초>        헤드리스에서 이 시간이 지나면 파이프라인을 정지하고 종료합니다 (스크립트·테스트용).
  --device <장치>       cpu | gpu:<번호> | auto (기본값은 번들 설정).
  --no-update           시작할 때 새 버전을 확인하지 않습니다.
                        (매니페스트 주소는 NL_UPDATE_URL 환경 변수로 덮어쓸 수 있습니다.)
  --version, -V         버전을 출력합니다.
  --help, -h            이 도움말을 출력합니다.

환경 변수
  NL_HTTP_TOKEN         파이프라인의 HTTP 서버 노드가 요구할 토큰을 덮어씁니다.
  NL_HTTP_TOKEN_<포트>  그 포트의 서버에만 적용합니다 (NL_HTTP_TOKEN 보다 우선).
                        번들에 박힌 토큰은 받은 사람이 모두 볼 수 있으므로, 서버로 돌릴 때는
                        실행 환경에서 새 토큰을 주는 편이 안전합니다.";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Options {
    /// 실행할 `.nlapp` 파일. 없으면 자기 실행 파일의 첨부 번들.
    pub bundle_path: Option<PathBuf>,
    pub headless: bool,
    /// 헤드리스 자동 종료 시간 (초). 없으면 Ctrl+C 까지.
    pub run_for: Option<f64>,
    /// 지정하지 않으면 번들 매니페스트의 기본 장치.
    pub device: Option<DevicePref>,
    /// 시작할 때 업데이트를 확인하지 않는다.
    pub no_update: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Version,
    Help,
    Run(Options),
    /// 인자가 잘못됐다. 메시지를 보여 주고 종료 코드 2.
    Error(String),
}

pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Command {
    let mut opts = Options::default();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--version" | "-V" => return Command::Version,
            "--help" | "-h" => return Command::Help,
            "--headless" => opts.headless = true,
            "--no-update" => opts.no_update = true,
            "--run-for" => {
                let Some(v) = it.next() else {
                    return Command::Error("--run-for 뒤에 초 단위 숫자가 없습니다".into());
                };
                match v.parse::<f64>() {
                    Ok(sec) if sec >= 0.0 => opts.run_for = Some(sec),
                    _ => return Command::Error(format!("--run-for 값이 잘못됐습니다: {v}")),
                }
            }
            "--device" => {
                let Some(v) = it.next() else {
                    return Command::Error("--device 뒤에 장치가 없습니다 (cpu | gpu:0 | auto)".into());
                };
                match parse_device(&v) {
                    Some(d) => opts.device = Some(d),
                    None => return Command::Error(format!("알 수 없는 장치입니다: {v} (cpu | gpu:0 | auto)")),
                }
            }
            other if other.starts_with("--device=") => {
                let v = &other["--device=".len()..];
                match parse_device(v) {
                    Some(d) => opts.device = Some(d),
                    None => return Command::Error(format!("알 수 없는 장치입니다: {v} (cpu | gpu:0 | auto)")),
                }
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Command::Error(format!("알 수 없는 옵션입니다: {other}"));
            }
            other => {
                if opts.bundle_path.is_some() {
                    return Command::Error(format!("파일은 하나만 받습니다: {other}"));
                }
                opts.bundle_path = Some(PathBuf::from(other));
            }
        }
    }
    Command::Run(opts)
}

/// `cpu` · `auto` · `gpu` · `gpu:2` → `DevicePref`.
pub fn parse_device(s: &str) -> Option<DevicePref> {
    let s = s.trim().to_ascii_lowercase();
    match s.as_str() {
        "cpu" => Some(DevicePref::Cpu),
        "auto" => Some(DevicePref::Auto),
        "gpu" => Some(DevicePref::Gpu { index: 0 }),
        _ => {
            let index = s.strip_prefix("gpu:")?.parse().ok()?;
            Some(DevicePref::Gpu { index })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(args: &[&str]) -> Command {
        parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn version_and_help_win_over_everything() {
        assert_eq!(parse_str(&["a.nlapp", "--version"]), Command::Version);
        assert_eq!(parse_str(&["-V"]), Command::Version);
        assert_eq!(parse_str(&["--help"]), Command::Help);
    }

    #[test]
    fn options_and_file() {
        let Command::Run(o) = parse_str(&["--headless", "--device", "gpu:2", "앱.nlapp"]) else {
            panic!("Run 이어야 합니다");
        };
        assert!(o.headless);
        assert_eq!(o.device, Some(DevicePref::Gpu { index: 2 }));
        assert_eq!(o.bundle_path, Some(PathBuf::from("앱.nlapp")));

        let Command::Run(o) = parse_str(&["--device=cpu"]) else { panic!("Run 이어야 합니다") };
        assert_eq!(o.device, Some(DevicePref::Cpu));
        assert!(o.bundle_path.is_none());
        assert!(!o.headless);
    }

    #[test]
    fn no_update_turns_off_the_check() {
        let Command::Run(o) = parse_str(&["--no-update"]) else { panic!("Run 이어야 합니다") };
        assert!(o.no_update);
        assert!(!o.headless, "다른 옵션과 섞이지 않는다");

        let Command::Run(o) = parse_str(&["--headless", "--no-update", "앱.nlapp"]) else {
            panic!("Run 이어야 합니다")
        };
        assert!(o.no_update);
        assert!(o.headless);
        assert_eq!(o.bundle_path, Some(PathBuf::from("앱.nlapp")));

        let Command::Run(o) = parse_str(&[]) else { panic!("Run 이어야 합니다") };
        assert!(!o.no_update, "기본은 확인한다");
    }

    #[test]
    fn no_args_runs_attached_bundle() {
        assert_eq!(parse_str(&[]), Command::Run(Options::default()));
    }

    #[test]
    fn bad_input_is_reported() {
        assert!(matches!(parse_str(&["--device"]), Command::Error(_)));
        assert!(matches!(parse_str(&["--device", "tpu"]), Command::Error(_)));
        assert!(matches!(parse_str(&["--nope"]), Command::Error(_)));
        assert!(matches!(parse_str(&["a.nlapp", "b.nlapp"]), Command::Error(_)));
    }

    #[test]
    fn device_strings() {
        assert_eq!(parse_device("CPU"), Some(DevicePref::Cpu));
        assert_eq!(parse_device(" auto "), Some(DevicePref::Auto));
        assert_eq!(parse_device("gpu"), Some(DevicePref::Gpu { index: 0 }));
        assert_eq!(parse_device("gpu:11"), Some(DevicePref::Gpu { index: 11 }));
        assert_eq!(parse_device("gpu:x"), None);
        assert_eq!(parse_device(""), None);
    }
}
