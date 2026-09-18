@echo off
rem ===========================================================================
rem MSVC build environment for Windows (cmd.exe).
rem
rem WHY THIS EXISTS
rem   oj-bus-kafka depends on rdkafka -> rdkafka-sys, which builds librdkafka
rem   from source via CMake (feature "cmake-build", enabled only on Windows).
rem   That build fails out of the box on a stock Windows box:
rem
rem     1) CMake Error: Could not create named generator Visual Studio 18 2026
rem        rdkafka-sys probes VS and hardcodes -G "Visual Studio <ver>". If the
rem        installed CMake predates that generator name, cmake aborts. Pinning
rem        the NMake generator decouples us from the VS version/name mismatch.
rem
rem     2) CRT mismatch (LNK4098 / dangling __imp__*)
rem        librdkafka defaults to /MD (dynamic CRT) while .cargo/config.toml
rem        sets +crt-static (/MT) for windows-msvc. cl.exe appends _CL_ to the
rem        end of its command line, and later same-class flags win, so _CL_=-MT
rem        reliably overrides cmake's /MD. Must be the dash form: a slash form
rem        (/MT) gets mangled into a POSIX path by MSYS shells.
rem
rem     3) Git for Windows ships a GNU link.exe under usr\bin that shadows the
rem        MSVC linker -> "missing operand after ...".
rem
rem   This set of steps previously lived ONLY in the CI workflows
rem   (.github/workflows/plugin-matrix.yml and release.yml), so local Windows
rem   builds failed with no hint as to why. This script is the local counterpart.
rem
rem USAGE
rem   scripts\windows-msvc-env.bat <command> [args...]
rem
rem   The command runs inside the configured environment. Examples:
rem     scripts\windows-msvc-env.bat cargo xtask build
rem     scripts\windows-msvc-env.bat cargo build --release
rem     scripts\windows-msvc-env.bat cargo test --workspace --release
rem
rem   With no arguments it prepares vcvars, prints the settings, and drops you
rem   into a configured cmd.exe (so you can then run cargo by hand). Type exit
rem   to leave.
rem
rem WHY A WRAPPER RATHER THAN "source THIS SCRIPT"
rem   cmd.exe cannot modify its parent's environment: a child .bat can only
rem   affect its own process. vcvars64.bat sets dozens of variables (PATH,
rem   INCLUDE, LIB, LIBPATH, WindowsSdkDir, ...), and hand-copying them out of a
rem   setlocal scope via "endlocal & set ..." is error-prone and easy to get
rem   subtly wrong. Running the command *inside* the configured scope is the
rem   only robust approach, so that is what this script does.
rem
rem   Git Bash / MSYS2 users: use scripts/windows-msvc-env.sh instead.
rem
rem NOTE: this file is deliberately pure ASCII. cmd.exe re-interprets bytes
rem according to the console code page (936/GBK on zh-CN Windows), so non-ASCII
rem -- even inside comments -- can garble the script or break findstr matches.
rem (Same constraint as scripts/deploy.bat.)
rem ===========================================================================

setlocal EnableExtensions

rem --- 1. locate vcvars64.bat (MSVC toolchain: cl.exe / nmake.exe) -----------
rem vswhere is the supported way to find a VS install; it ships with VS 2017+
rem under Program Files (x86)\Microsoft Visual Studio\Installer.
set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
if not exist "%VSWHERE%" set "VSWHERE=%ProgramFiles%\Microsoft Visual Studio\Installer\vswhere.exe"

set "VCVARS="
if exist "%VSWHERE%" (
    for /f "usebackq tokens=*" %%I in (`"%VSWHERE%" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do (
        if exist "%%I\VC\Auxiliary\Build\vcvars64.bat" set "VCVARS=%%I\VC\Auxiliary\Build\vcvars64.bat"
    )
)

rem Fallback: probe the usual Community/Professional/Enterprise/BuildTools paths.
rem Covers VS installs that predate vswhere or use a non-standard layout.
rem VS 2026 reports major version 18; older releases use the year.
if not defined VCVARS (
    for %%E in (Community Professional Enterprise BuildTools) do (
        for %%V in (18 2026 2022 2019) do (
            if not defined VCVARS (
                if exist "%ProgramFiles%\Microsoft Visual Studio\%%V\%%E\VC\Auxiliary\Build\vcvars64.bat" (
                    set "VCVARS=%ProgramFiles%\Microsoft Visual Studio\%%V\%%E\VC\Auxiliary\Build\vcvars64.bat"
                )
            )
        )
    )
)

if not defined VCVARS (
    echo ERROR: cannot find vcvars64.bat 1>&2
    echo. 1>&2
    echo   Looked via vswhere and the usual Visual Studio install paths. 1>&2
    echo   Install the "Desktop development with C++" workload ^(which provides 1>&2
    echo   cl.exe / nmake.exe^), or run this script from a VS Developer Command 1>&2
    echo   Prompt instead. 1>&2
    exit /b 1
)

rem Import the MSVC environment. vcvars64.bat prints a banner; the redirect
rem silences it while keeping its exit code observable.
call "%VCVARS%" >nul
if errorlevel 1 (
    echo ERROR: vcvars64.bat failed: %VCVARS% 1>&2
    exit /b 1
)
echo [env] MSVC toolchain: %VCVARS%

rem --- 2. pin CMake generator (do not clobber an explicit user value) --------
if not defined CMAKE_GENERATOR (
    set "CMAKE_GENERATOR=NMake Makefiles"
    echo [env] CMAKE_GENERATOR=NMake Makefiles
) else (
    echo [env] CMAKE_GENERATOR already set to "%CMAKE_GENERATOR%" -- left as-is
)

rem --- 3. force static CRT for librdkafka ------------------------------------
if not defined _CL_ (
    set "_CL_=-MT"
    echo [env] _CL_=-MT
) else (
    echo [env] _CL_ already set to "%_CL_%" -- left as-is
)

rem --- 4. neutralise Git for Windows' GNU link.exe ---------------------------
rem Git's usr\bin\link.exe precedes MSVC's in PATH and rustc picks the wrong
rem one. There is no way to fix this from the environment (PATH order is
rem inherited), so the file has to go.
rem
rem Renamed to a .disabled suffix rather than deleted: this is the user's own
rem Git installation, outside the repo. Restore with:
rem   ren link.exe.disabled link.exe
rem
rem Done once per invocation and cheap to redo; the "if exist ... .disabled"
rem branch handles the already-moved case.
for %%G in (
    "%ProgramFiles%\Git\usr\bin\link.exe"
    "%ProgramFiles(x86)%\Git\usr\bin\link.exe"
    "%LocalAppData%\Programs\Git\usr\bin\link.exe"
) do (
    if exist "%%~G" (
        move /y "%%~G" "%%~G.disabled" >nul 2>nul
        if errorlevel 1 (
            echo warning: cannot move %%~G aside ^(in use / read-only^) 1>&2
        ) else (
            echo [env] shadowing linker disabled: %%~G.disabled
        )
    )
)

rem --- run the caller's command inside this configured environment -----------
if "%~1"=="" (
    echo.
    echo Windows MSVC build environment ready. Suggested next steps:
    echo   cargo xtask build                 ^(oj + all plugins, into bin\^)
    echo   cargo build --release             ^(workspace^)
    echo   cargo test --workspace --release
    echo.
    echo Dropping into a configured shell -- type exit to leave.
    cmd /k
    exit /b %errorlevel%
)

echo [env] running: %*
echo.
%*
exit /b %errorlevel%
