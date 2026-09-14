<#
.SYNOPSIS
    배포 앱을 로그온할 때마다 헤드리스 서버로 띄운다 (Windows).

.DESCRIPTION
    Linux 의 systemd 사용자 유닛에 대응하는 자리다. 작업 스케줄러에 ONLOGON 작업을 만든다.
    관리자 권한이 필요 없다 — /RL LIMITED 로 지금 사용자 권한 그대로 돈다.

    토큰 같은 비밀은 **명령줄에 넣지 않는다.** 작업 스케줄러에는 환경 변수를 넣어 주는 기능이 없어
    `cmd /c "set TOKEN=... && app.exe"` 로 감싸는 방법이 흔히 쓰이지만, 그러면 토큰이 작업 목록
    (`schtasks /Query /XML`)과 명령줄에 그대로 드러난다. 대신 앱의 `--env-file` 로 읽게 한다.

.PARAMETER App
    배포 앱 실행 파일 경로 (`.exe`).

.PARAMETER Name
    작업 이름. 기본은 실행 파일 이름에서 뽑는다. 작업 폴더 이름으로도 쓴다.

.PARAMETER Device
    `--device` 값. 기본 cpu — 헤드리스 서버에서 GPU 는 드라이버 문제를 일으키기 쉽다.

.PARAMETER Uninstall
    작업을 지운다. 작업 폴더와 환경 파일은 남긴다(그 안에 비밀이 있다).

.EXAMPLE
    .\install-service.ps1 -App C:\apps\내앱.exe
    .\install-service.ps1 -App C:\apps\내앱.exe -Uninstall

.NOTES
    확인: schtasks /Query /TN NeuralLinker\<이름> /V /FO LIST
    로그: 작업 폴더의 app.log (표준 출력을 그리로 넘긴다)
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $false)][string]$App,
    [string]$Name,
    [string]$Device = 'cpu',
    [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'

if (-not $Name) {
    if (-not $App) { throw '-App 이나 -Name 중 하나는 필요합니다' }
    $Name = [System.IO.Path]::GetFileNameWithoutExtension($App)
}
$TaskPath = 'NeuralLinker'
$TaskName = "$TaskPath\$Name"
$WorkDir  = Join-Path $env:LOCALAPPDATA "neural-linker\$Name"
$EnvFile  = Join-Path $WorkDir 'local\app.env'
$LogFile  = Join-Path $WorkDir 'app.log'

if ($Uninstall) {
    # 돌고 있으면 먼저 세운다. 없으면 조용히 넘어간다.
    schtasks /End /TN $TaskName 2>$null | Out-Null
    schtasks /Delete /TN $TaskName /F 2>$null | Out-Null
    Write-Host "작업을 지웠습니다: $TaskName"
    if (Test-Path $EnvFile) {
        Write-Host "환경 파일은 남겨 두었습니다(비밀이 들어 있습니다): $EnvFile"
    }
    if (Test-Path $WorkDir) {
        Write-Host "작업 폴더도 남겨 두었습니다: $WorkDir"
    }
    exit 0
}

if (-not $App) { throw '-App 에 배포 앱 경로가 필요합니다' }
$AppPath = (Resolve-Path -LiteralPath $App).Path
if (-not (Test-Path -LiteralPath $AppPath -PathType Leaf)) {
    throw "배포 앱을 찾지 못했습니다: $App"
}

# 작업 폴더와 local/ 을 미리 만든다. 앱도 만들지만, 환경 파일을 먼저 놓을 수 있어야 한다.
New-Item -ItemType Directory -Force -Path (Join-Path $WorkDir 'local') | Out-Null

if (-not (Test-Path $EnvFile)) {
    # KEY=VALUE 한 줄씩. 앱이 `--env-file` 로 읽는다.
    @(
        '# 배포 앱 환경 변수. 한 줄에 KEY=VALUE 하나.',
        '# NL_HTTP_TOKEN=여기에_토큰'
    ) | Set-Content -LiteralPath $EnvFile -Encoding UTF8
}

# Windows 에는 유닉스 권한 비트가 없다. ACL 로 "나만 읽기" 를 만든다 —
# 상속을 끊지 않으면 Users 같은 그룹이 그대로 남는다.
try {
    $acl = Get-Acl -LiteralPath $EnvFile
    $acl.SetAccessRuleProtection($true, $false)   # 상속 끊고, 물려받은 규칙은 복사하지 않는다
    $me = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name
    $rule = New-Object System.Security.AccessControl.FileSystemAccessRule(
        $me, 'FullControl', 'Allow')
    $acl.SetAccessRule($rule)
    Set-Acl -LiteralPath $EnvFile -AclObject $acl
} catch {
    Write-Warning "환경 파일 권한을 조이지 못했습니다: $($_.Exception.Message)"
    Write-Warning "여러 사람이 쓰는 기계라면 $EnvFile 의 권한을 직접 확인하세요."
}

# 작업이 실행할 명령. 토큰은 여기 없다 — 앱이 --env-file 에서 읽는다.
# 표준 출력을 파일로 넘겨야 로그가 남는다. 작업 스케줄러는 출력을 잡아 두지 않는다.
$inner = '"{0}" --headless --device {1} --work-dir "{2}" --env-file "{3}" >> "{4}" 2>&1' -f `
    $AppPath, $Device, $WorkDir, $EnvFile, $LogFile
$action = 'cmd /c ' + ('"' + $inner + '"')

schtasks /Create /TN $TaskName /TR $action /SC ONLOGON /RL LIMITED /F | Out-Null

# 실패하면 다시 띄운다. /Create 에는 이 옵션이 없어 XML 로 고친다.
# 1분 간격으로 3번까지 — systemd 유닛의 Restart=on-failure + StartLimitBurst 와 같은 뜻이다.
$xml = [xml](schtasks /Query /TN $TaskName /XML ONE)
$settings = $xml.Task.Settings
$settings.RestartInterval = 'PT1M'
$settings.RestartCount = '3'
# 배터리로 돌 때도, 유휴가 아니어도 계속 돈다. 서버니까.
$settings.DisallowStartIfOnBatteries = 'false'
$settings.StopIfGoingOnBatteries = 'false'
$settings.ExecutionTimeLimit = 'PT0S'            # 시간 제한 없음
$tmp = Join-Path $env:TEMP "nl-task-$([guid]::NewGuid()).xml"
$xml.Save($tmp)
try {
    schtasks /Create /TN $TaskName /XML $tmp /F | Out-Null
} finally {
    Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue
}

Write-Host ''
Write-Host "작업을 만들었습니다: $TaskName"
Write-Host "  앱     $AppPath"
Write-Host "  작업   $WorkDir"
Write-Host "  파일   $WorkDir\local  (인증서 등 — 번들을 갱신해도 남습니다)"
Write-Host "  토큰   $EnvFile 에 NL_HTTP_TOKEN=... 을 적으세요"
Write-Host "  로그   $LogFile"
Write-Host ''
Write-Host '지금 바로 띄우려면:  schtasks /Run /TN ' -NoNewline; Write-Host $TaskName
Write-Host '상태를 보려면:       schtasks /Query /TN ' -NoNewline; Write-Host "$TaskName /V /FO LIST"
Write-Host '지우려면:            .\install-service.ps1 -Name ' -NoNewline; Write-Host "$Name -Uninstall"
