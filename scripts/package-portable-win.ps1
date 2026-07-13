# scripts/package-portable-win.ps1 — TetherMoon 단일 포터블 exe (설치 불필요)
#
# web UI는 exe에 임베드됨(rust-embed) → 배포에 web\ 폴더 불필요. 남은 런타임 파일
# (Sony SDK DLL + CrAdapter\ + VC++ 런타임)을 payload.zip 하나로 묶고, Windows 내장
# IExpress로 자동압축해제 exe를 만든다. 더블클릭 시 %TEMP%에 풀린 뒤 서버가 실행된다.
#
# 사용 (repo 루트에서, 관리자 불필요):
#   set "LIBCLANG_PATH=C:\Program Files\LLVM\bin"   # 또는 pip libclang 경로(docs/WINDOWS-PORT.md §1.3)
#   powershell -ExecutionPolicy Bypass -File scripts\package-portable-win.ps1
# 산출물: dist\TetherMoon-portable.exe
#
# 주의:
#  - libusbK 카메라 드라이버는 포함 안 됨(설치형 아님) — 최초 1회 수동 설치 필요(README/§2.7 참조).
#  - IExpress는 실행 때마다 temp에 풀어 첫 기동이 약간 느리고, 서버 종료 후 temp를 정리한다.
#  - IExpress는 대화형 데스크톱을 쓴다 → 헤드리스(SSH 등)에선 빌드만 되고 더블클릭 실행 검증은 실제 세션에서.

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (-not $env:LIBCLANG_PATH) { Write-Warning "LIBCLANG_PATH 미설정 — bindgen이 실패할 수 있음" }

# 실행 중 인스턴스 정리(dist DLL을 잡고 있으면 스테이징 삭제가 거부됨)
Get-Process crsdk_server -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue

# 1) release 빌드 (build.rs가 Sony DLL을 target\release 옆에 복사)
cargo build --release -p crsdk_server
if ($LASTEXITCODE -ne 0) { throw "cargo build 실패" }

$rel  = Join-Path $root "target\release"
$work = Join-Path $root "dist\_sfx"
$stg  = Join-Path $work "stage"
Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $stg, (Join-Path $stg "CrAdapter") | Out-Null

# 2) 런타임 파일 스테이징 (web은 exe에 임베드 → 제외)
Copy-Item (Join-Path $rel "crsdk_server.exe") $stg
"Cr_Core.dll","monitor_protocol.dll","monitor_protocol_pf.dll" | ForEach-Object { Copy-Item (Join-Path $rel $_) $stg }
Get-ChildItem (Join-Path $rel "CrAdapter") -Filter *.dll | Copy-Item -Destination (Join-Path $stg "CrAdapter")

# 2b) VC++ 런타임(app-local) — exe·Cr_Core 모두 의존. Windows 기본 미포함이라 동봉(MS 재배포 허용).
$crtNeeded = @("msvcp140.dll","vcruntime140.dll","vcruntime140_1.dll")
$crtDir = $null
$cands = @()
if ($env:VCToolsRedistDir) { $cands += (Join-Path $env:VCToolsRedistDir "x64\Microsoft.VC*.CRT") }
$vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
if (Test-Path $vswhere) {
    $vs = & $vswhere -products * -latest -property installationPath 2>$null
    if ($vs) { $cands += (Join-Path $vs "VC\Redist\MSVC\*\x64\Microsoft.VC*.CRT") }
}
foreach ($c in $cands) {
    $d = Get-ChildItem $c -Directory -ErrorAction SilentlyContinue |
         Where-Object { Test-Path (Join-Path $_.FullName "vcruntime140.dll") } |
         Select-Object -First 1
    if ($d) { $crtDir = $d.FullName; break }
}
if ($crtDir) { foreach ($d in $crtNeeded) { Copy-Item (Join-Path $crtDir $d) $stg -Force } }
else { Write-Warning "VC++ 런타임 DLL을 못 찾음 — 대상 PC에 VC++ 2015-2022 재배포(x64) 필요" }

# 3) payload.zip (Compress-Archive가 CrAdapter\ 하위구조 보존)
$zip = Join-Path $work "payload.zip"
Compress-Archive -Path (Join-Path $stg "*") -DestinationPath $zip -Force

# 4) run.bat — 압축을 풀고(tar, 구조보존) 서버 실행. 서버가 블로킹이라 IExpress가 종료까지 temp 유지.
@"
@echo off
cd /d "%~dp0"
tar -xf payload.zip
crsdk_server.exe
"@ | Set-Content -Path (Join-Path $work "run.bat") -Encoding Ascii

# 5) IExpress SED → 자동압축해제 exe
$out = Join-Path $root "dist\TetherMoon-portable.exe"
@"
[Version]
Class=IEXPRESS
SEDVersion=3
[Options]
PackagePurpose=InstallApp
ShowInstallProgramWindow=1
HideExtractAnimation=1
UseLongFileName=1
InsideCompressed=0
RebootMode=N
TargetName=$out
FriendlyName=TetherMoon
AppLaunched=run.bat
PostInstallCmd=<None>
AdminQuietInstCmd=
UserQuietInstCmd=
SourceFiles=SourceFiles
[Strings]
FILE0=payload.zip
FILE1=run.bat
[SourceFiles]
SourceFiles0=$work
[SourceFiles0]
%FILE0%=
%FILE1%=
"@ | Set-Content -Path (Join-Path $work "tm.sed") -Encoding Ascii

Remove-Item $out -Force -ErrorAction SilentlyContinue
Start-Process iexpress -ArgumentList '/N','/Q',(Join-Path $work "tm.sed") -Wait -NoNewWindow
Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue

if (Test-Path $out) { Write-Output ("portable: " + $out + "  (" + [math]::Round((Get-Item $out).Length/1MB,2) + " MB)") }
else { throw "IExpress 패키징 실패 — dist\TetherMoon-portable.exe 없음" }
