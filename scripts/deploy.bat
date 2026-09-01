@echo off
rem ===========================================================================
rem Release packaging script for Windows (cmd.exe).
rem macOS / Linux: use scripts/deploy.sh instead.
rem
rem In (everything below is produced by "cargo xtask build"):
rem   bin\oj.exe                 main executable
rem   bin\plugins\<triple>\      plugin cdylibs (.dll)
rem   bin\devkit\                DevKit docs
rem
rem Out:
rem   dist\oj-v<version>-<triple>.zip
rem   dist\oj-v<version>-<triple>.zip.sha256
rem
rem <triple> is read from "rustc -vV" -- the same source xtask uses to place
rem plugins (tools/xtask/src/main.rs host_triple). No platform is hardcoded, so
rem musl / aarch64 variants sort themselves out.
rem
rem The archive keeps plugins\<triple>\ (never flattened): the plugin loader
rem discovers plugins under <exe>\plugins\<triple>\, so the unpacked tree has to
rem be usable as-is.
rem
rem NOTE: this file is deliberately pure ASCII. cmd.exe re-interprets bytes
rem according to the console code page (936/GBK on zh-CN Windows), so non-ASCII
rem -- even inside comments -- can garble the script or break findstr matches.
rem ===========================================================================

setlocal EnableExtensions EnableDelayedExpansion

rem --- repo root (this script lives in <root>\scripts) ----------------------
for %%I in ("%~dp0..") do set "ROOT=%%~fI"
pushd "%ROOT%"
if errorlevel 1 (
    echo ERROR: cannot enter %ROOT% 1>&2
    exit /b 1
)

rem --- version: first line-initial `version = "..."` in oj\Cargo.toml -------
rem Dependency versions are written inline inside braces, so they never match
rem a line-anchored search.
set "VERSION="
for /f "usebackq tokens=2 delims==" %%V in (`findstr /b /c:"version =" oj\Cargo.toml`) do (
    if not defined VERSION (
        set "RAW=%%V"
        set "RAW=!RAW:"=!"
        for /f "tokens=*" %%A in ("!RAW!") do set "VERSION=%%A"
    )
)
if not defined VERSION (
    echo ERROR: cannot parse version from oj\Cargo.toml 1>&2
    exit /b 1
)

rem --- platform triple: the `host:` line of `rustc -vV` ---------------------
where rustc >nul 2>nul
if errorlevel 1 (
    echo ERROR: rustc not found in PATH 1>&2
    exit /b 1
)
set "TRIPLE="
for /f "usebackq tokens=2" %%T in (`rustc -vV ^| findstr /b /c:"host: "`) do (
    if not defined TRIPLE set "TRIPLE=%%T"
)
if not defined TRIPLE (
    echo ERROR: cannot parse host triple from [rustc -vV] 1>&2
    exit /b 1
)

echo host triple : %TRIPLE%
echo version     : %VERSION%

set "PKG=oj-v%VERSION%-%TRIPLE%"
set "DIST=%ROOT%\dist"
set "TMPDIR=%DIST%\%PKG%"
set "ARCHIVE=%DIST%\%PKG%.zip"

rem --- clean dist ----------------------------------------------------------
if exist "%DIST%" rmdir /s /q "%DIST%"
if errorlevel 1 (
    echo ERROR: cannot clean %DIST% 1>&2
    exit /b 1
)
mkdir "%DIST%"
if errorlevel 1 (
    echo ERROR: cannot create %DIST% 1>&2
    exit /b 1
)

rem --- build oj + all first-party plugins + devkit into bin\ ---------------
echo Building release (oj + plugins + devkit) into bin\ ...
cargo xtask build
if errorlevel 1 (
    echo ERROR: cargo xtask build failed 1>&2
    exit /b 1
)

rem --- validate artifacts --------------------------------------------------
if not exist "bin\oj.exe" (
    echo ERROR: main binary missing: bin\oj.exe 1>&2
    exit /b 1
)
if not exist "bin\plugins\%TRIPLE%\" (
    echo ERROR: plugin dir missing: bin\plugins\%TRIPLE% 1>&2
    echo        existing under bin\plugins: 1>&2
    dir /b "bin\plugins" 1>&2
    exit /b 1
)
if not exist "bin\devkit\api-manual.md" (
    echo ERROR: devkit artifacts missing under bin\devkit 1>&2
    exit /b 1
)
if not exist "bin\devkit\global.d.ts" (
    echo ERROR: devkit artifacts missing under bin\devkit 1>&2
    exit /b 1
)

rem --- assemble: oj.exe + plugins\<triple>\ + devkit\ ----------------------
mkdir "%TMPDIR%\plugins"
mkdir "%TMPDIR%\devkit"
copy /y "bin\oj.exe" "%TMPDIR%\oj.exe" >nul
if errorlevel 1 (
    echo ERROR: copy oj.exe failed 1>&2
    exit /b 1
)
rem xcopy exit codes: 0 = ok, 1 = no files found, >=2 = real failure.
xcopy /e /i /y /q "bin\plugins\%TRIPLE%" "%TMPDIR%\plugins\%TRIPLE%\" >nul
if errorlevel 2 (
    echo ERROR: copy plugins failed 1>&2
    exit /b 1
)
xcopy /e /i /y /q "bin\devkit" "%TMPDIR%\devkit\" >nul
if errorlevel 2 (
    echo ERROR: copy devkit failed 1>&2
    exit /b 1
)

rem --- archive: tar.exe is bsdtar, built into Windows 10 17063+ / Server 2016+.
rem -a picks the format from the .zip extension, so no external tool is needed.
where tar >nul 2>nul
if errorlevel 1 (
    echo ERROR: tar.exe not found in PATH. It ships with Windows 10 17063+ / 1>&2
    echo        Server 2016+; on older hosts install 7-Zip or run 1>&2
    echo        scripts/deploy.sh from WSL. 1>&2
    exit /b 1
)
pushd "%DIST%"
tar -a -c -f "%PKG%.zip" "%PKG%"
if errorlevel 1 (
    popd
    echo ERROR: tar failed to create %PKG%.zip 1>&2
    exit /b 1
)
popd
rem Must not leave the staging dir behind: dist\* is what gets uploaded as the
rem release asset, and a stray directory would be picked up too.
rmdir /s /q "%TMPDIR%"
if errorlevel 1 (
    echo ERROR: cannot clean staging dir %TMPDIR% 1>&2
    exit /b 1
)

rem --- checksum: certutil is built in (Vista+). Its output is 3 lines with the
rem hash on the second one, hence skip=1.
set "HASH="
for /f "skip=1 tokens=1" %%H in ('certutil -hashfile "%ARCHIVE%" SHA256') do (
    if not defined HASH set "HASH=%%H"
)
if not defined HASH (
    echo ERROR: certutil -hashfile failed on %ARCHIVE% 1>&2
    exit /b 1
)
>"%ARCHIVE%.sha256" echo %HASH%  %PKG%.zip

echo Deployment complete!
echo Package: %ARCHIVE%
dir "%ARCHIVE%"
