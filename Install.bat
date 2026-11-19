@echo off
chcp 65001 >nul
echo ============================================
echo          DAWStream Installer
echo ============================================
echo.

set "DIST_DIR=%~dp0dist"
if not exist "%DIST_DIR%\DAWStream.vst3" (
    set "DIST_DIR=%~dp0"
)

echo [1/4] Installing VST3 plugin (User folder)...
set "USER_VST3=%LOCALAPPDATA%\Programs\Common\VST3"
if not exist "%USER_VST3%" mkdir "%USER_VST3%"
xcopy /E /I /Y "%DIST_DIR%\DAWStream.vst3" "%USER_VST3%\DAWStream.vst3" >nul
if %errorlevel% equ 0 (
    echo   OK: %USER_VST3%\DAWStream.vst3
) else (
    echo   WARNING: Could not write to user VST3 folder ^(close your DAW and retry^).
)

if exist "%DIST_DIR%\DAWStream.clap" (
    echo [2/4] Installing CLAP plugin ^(User folder^)...
    set "USER_CLAP=%LOCALAPPDATA%\Programs\Common\CLAP"
    if not exist "%USER_CLAP%" mkdir "%USER_CLAP%"
    copy /Y "%DIST_DIR%\DAWStream.clap" "%USER_CLAP%\DAWStream.clap" >nul
    if %errorlevel% equ 0 (
        echo   OK: %USER_CLAP%\DAWStream.clap
    ) else (
        echo   WARNING: Could not write to user CLAP folder.
    )
) else (
    echo [2/4] Skipping CLAP ^(not present^).
)

echo [3/4] Installing to System VST3/CLAP folders ^(if running as Administrator^)...
xcopy /E /I /Y "%DIST_DIR%\DAWStream.vst3" "C:\Program Files\Common Files\VST3\DAWStream.vst3" >nul 2>&1
if %errorlevel% equ 0 (
    echo   OK: C:\Program Files\Common Files\VST3\DAWStream.vst3
) else (
    echo   Note: Right-click Install.bat -^> "Run as administrator" to also install system-wide ^(optional^).
)
if exist "%DIST_DIR%\DAWStream.clap" (
    if not exist "C:\Program Files\Common Files\CLAP" mkdir "C:\Program Files\Common Files\CLAP" >nul 2>&1
    copy /Y "%DIST_DIR%\DAWStream.clap" "C:\Program Files\Common Files\CLAP\DAWStream.clap" >nul 2>&1
)

echo [4/4] Creating Desktop shortcut...
powershell -NoProfile -ExecutionPolicy Bypass -Command "$s=(New-Object -COM WScript.Shell).CreateShortcut((Join-Path ([Environment]::GetFolderPath('Desktop')) 'DAWStream.lnk'));$s.TargetPath=(Join-Path $env:DIST_DIR 'DAWStream.exe');$s.WorkingDirectory=$env:DIST_DIR;$s.Description='DAWStream - Discord DAW Streaming';$s.Save()"
echo   OK: Desktop\DAWStream.lnk

echo.
echo Installation complete!
echo Rescan plugins in your DAW, add DAWStream to your Master channel,
echo open DAWStream from Desktop, and share its window in Discord!
echo.
pause
