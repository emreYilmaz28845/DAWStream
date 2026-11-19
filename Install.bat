@echo off
setlocal EnableDelayedExpansion
"%SystemRoot%\System32\chcp.com" 65001 >nul
echo ============================================
echo          DAWStream Installer
echo ============================================
echo.

set "SYS_XCOPY=%SystemRoot%\System32\xcopy.exe"
set "SYS_PS=%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe"
set "SYS_NET=%SystemRoot%\System32\net.exe"

set "DIST_DIR=%~dp0dist"
if not exist "!DIST_DIR!\DAWStream.vst3" (
    set "DIST_DIR=%~dp0"
)
if "!DIST_DIR:~-1!"=="\" set "DIST_DIR=!DIST_DIR:~0,-1!"

if not exist "!DIST_DIR!\DAWStream.exe" (
    echo ERROR: DAWStream.exe not found next to Install.bat.
    pause
    exit /b 1
)

REM Detect elevation cleanly to separate per-machine vs per-user installs
"%SYS_NET%" session >nul 2>&1
if %errorlevel% equ 0 (
    set "IS_ADMIN=1"
) else (
    set "IS_ADMIN=0"
)

if "!IS_ADMIN!"=="1" (
    echo Mode: Administrator ^(System-wide installation^)
    set "APP_DIR=%ProgramFiles%\DAWStream"
    set "VST3_DIR=%ProgramFiles%\Common Files\VST3"
    set "CLAP_DIR=%ProgramFiles%\Common Files\CLAP"
) else (
    echo Mode: Standard User ^(Per-user installation^)
    set "APP_DIR=%LOCALAPPDATA%\DAWStream"
    set "VST3_DIR=%LOCALAPPDATA%\Programs\Common\VST3"
    set "CLAP_DIR=%LOCALAPPDATA%\Programs\Common\CLAP"
)

echo.
echo [1/4] Installing DAWStream companion app...
if not exist "!APP_DIR!" mkdir "!APP_DIR!"
copy /Y "!DIST_DIR!\DAWStream.exe" "!APP_DIR!\DAWStream.exe" >nul
if %errorlevel% equ 0 (
    echo   OK: !APP_DIR!\DAWStream.exe
) else (
    echo   WARNING: Could not copy DAWStream.exe ^(close DAWStream if running^).
)

echo [2/4] Installing VST3 plugin...
if not exist "!VST3_DIR!" mkdir "!VST3_DIR!"
"%SYS_XCOPY%" /E /I /Y "!DIST_DIR!\DAWStream.vst3" "!VST3_DIR!\DAWStream.vst3" >nul
if %errorlevel% equ 0 (
    echo   OK: !VST3_DIR!\DAWStream.vst3
) else (
    echo   WARNING: Could not install VST3 plugin ^(close your DAW and retry^).
)

echo [3/4] Installing CLAP plugin...
if exist "!DIST_DIR!\DAWStream.clap" (
    if not exist "!CLAP_DIR!" mkdir "!CLAP_DIR!"
    copy /Y "!DIST_DIR!\DAWStream.clap" "!CLAP_DIR!\DAWStream.clap" >nul
    if %errorlevel% equ 0 (
        echo   OK: !CLAP_DIR!\DAWStream.clap
    ) else (
        echo   WARNING: Could not install CLAP plugin.
    )
) else (
    echo   Skipped ^(DAWStream.clap not present^).
)

echo [4/4] Creating Desktop shortcut...
set "TARGET_APP_DIR=!APP_DIR!"
"%SYS_PS%" -NoProfile -ExecutionPolicy Bypass -Command "$s=(New-Object -COM WScript.Shell).CreateShortcut((Join-Path ([Environment]::GetFolderPath('Desktop')) 'DAWStream.lnk'));$s.TargetPath=(Join-Path $env:TARGET_APP_DIR 'DAWStream.exe');$s.WorkingDirectory=$env:TARGET_APP_DIR;$s.Description='DAWStream - Discord DAW Streaming';$s.Save()"
echo   OK: Desktop\DAWStream.lnk

echo.
echo Installation complete!
if "!IS_ADMIN!"=="0" (
    echo Note: If your DAW only scans C:\Program Files\Common Files\VST3,
    echo right-click Install.bat and select "Run as administrator".
)
echo Rescan plugins in your DAW, add DAWStream to your Master channel,
echo open DAWStream from Desktop, and share its window in Discord.
echo.
pause
endlocal
