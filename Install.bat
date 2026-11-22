@echo off
setlocal
"C:\Windows\System32\chcp.com" 65001 >nul 2>&1
echo ============================================
echo          DAWStream Installer
echo ============================================
echo.

REM Capture %~dp0 BEFORE enabling delayed expansion so paths containing '!' are preserved
set "RAW_DIR=%~dp0"
if "%RAW_DIR:~-1%"=="\" set "RAW_DIR=%RAW_DIR:~0,-1%"
if exist "%RAW_DIR%\dist\DAWStream.vst3" (
    set "DIST_DIR=%RAW_DIR%\dist"
) else (
    set "DIST_DIR=%RAW_DIR%"
)

set "SYS_XCOPY=C:\Windows\System32\xcopy.exe"
set "SYS_PS=C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"
set "SYS_WHOAMI=C:\Windows\System32\whoami.exe"
set "SYS_FINDSTR=C:\Windows\System32\findstr.exe"

setlocal EnableDelayedExpansion

if not exist "!DIST_DIR!\DAWStream.exe" (
    echo ERROR: DAWStream.exe not found next to Install.bat.
    pause
    exit /b 1
)

if not exist "!DIST_DIR!\DAWStream.vst3" (
    echo ERROR: DAWStream.vst3 not found next to Install.bat.
    pause
    exit /b 1
)

REM Reliable High-IL (Administrator) check via Mandatory Label SID S-1-16-12288
"%SYS_WHOAMI%" /groups | "%SYS_FINDSTR%" /I "S-1-16-12288" >nul 2>&1
if !errorlevel! equ 0 (
    set "IS_ADMIN=1"
) else (
    set "IS_ADMIN=0"
)

if "!IS_ADMIN!"=="1" (
    echo Mode: Administrator ^(System-wide installation^)
    set "APP_DIR=C:\Program Files\DAWStream"
    set "VST3_DIR=C:\Program Files\Common Files\VST3"
    set "CLAP_DIR=C:\Program Files\Common Files\CLAP"
    REM Remove any earlier per-user copies: an elevated DAW also scans the user-writable
    REM %LOCALAPPDATA%\Programs\Common\VST3 folder and would load that copy at High IL.
    for /f "usebackq delims=" %%P in (`"%SYS_PS%" -NoProfile -Command "[Environment]::GetFolderPath('LocalApplicationData')"`) do set "USER_LOCALAPPDATA=%%P"
    if defined USER_LOCALAPPDATA (
        if exist "!USER_LOCALAPPDATA!\Programs\Common\VST3\DAWStream.vst3" (
            rmdir /S /Q "!USER_LOCALAPPDATA!\Programs\Common\VST3\DAWStream.vst3" >nul 2>&1
            echo   Removed per-user VST3 copy.
        )
        if exist "!USER_LOCALAPPDATA!\Programs\Common\CLAP\DAWStream.clap" (
            del /F /Q "!USER_LOCALAPPDATA!\Programs\Common\CLAP\DAWStream.clap" >nul 2>&1
            echo   Removed per-user CLAP copy.
        )
        if exist "!USER_LOCALAPPDATA!\DAWStream\DAWStream.exe" (
            rmdir /S /Q "!USER_LOCALAPPDATA!\DAWStream" >nul 2>&1
            echo   Removed per-user app copy.
        )
    )
) else (
    echo Mode: Standard User ^(Per-user installation^)
    for /f "usebackq delims=" %%P in (`"%SYS_PS%" -NoProfile -Command "[Environment]::GetFolderPath('LocalApplicationData')"`) do set "REAL_LOCALAPPDATA=%%P"
    if not defined REAL_LOCALAPPDATA (
        echo ERROR: Could not resolve LocalApplicationData folder.
        pause
        exit /b 1
    )
    set "APP_DIR=!REAL_LOCALAPPDATA!\DAWStream"
    set "VST3_DIR=!REAL_LOCALAPPDATA!\Programs\Common\VST3"
    set "CLAP_DIR=!REAL_LOCALAPPDATA!\Programs\Common\CLAP"
)

echo.
echo [1/4] Installing DAWStream companion app...
if not exist "!APP_DIR!" mkdir "!APP_DIR!"
copy /Y "!DIST_DIR!\DAWStream.exe" "!APP_DIR!\DAWStream.exe" >nul
if !errorlevel! neq 0 (
    echo   ERROR: Could not copy DAWStream.exe ^(close DAWStream if running and retry^).
    pause
    exit /b 1
)
echo   OK: !APP_DIR!\DAWStream.exe

echo [2/4] Installing VST3 plugin...
if not exist "!VST3_DIR!" mkdir "!VST3_DIR!"
if exist "!VST3_DIR!\DAWStream.vst3" (
    rmdir /S /Q "!VST3_DIR!\DAWStream.vst3" >nul 2>&1
)
"%SYS_XCOPY%" /E /I /Y "!DIST_DIR!\DAWStream.vst3" "!VST3_DIR!\DAWStream.vst3" >nul
if !errorlevel! neq 0 (
    echo   ERROR: Could not install VST3 plugin ^(close your DAW and retry^).
    pause
    exit /b 1
)
echo   OK: !VST3_DIR!\DAWStream.vst3

echo [3/4] Installing CLAP plugin...
if exist "!DIST_DIR!\DAWStream.clap" (
    if not exist "!CLAP_DIR!" mkdir "!CLAP_DIR!"
    copy /Y "!DIST_DIR!\DAWStream.clap" "!CLAP_DIR!\DAWStream.clap" >nul
    if !errorlevel! neq 0 (
        echo   ERROR: Could not install CLAP plugin ^(close your DAW and retry^).
        pause
        exit /b 1
    )
    echo   OK: !CLAP_DIR!\DAWStream.clap
) else (
    echo   Skipped ^(DAWStream.clap not present^).
)

echo [4/4] Creating Desktop shortcut...
set "TARGET_APP_DIR=!APP_DIR!"
"%SYS_PS%" -NoProfile -ExecutionPolicy Bypass -Command "$p=Join-Path ([Environment]::GetFolderPath('Desktop')) 'DAWStream.lnk';if(Test-Path -LiteralPath $p){Remove-Item -LiteralPath $p -Force -ErrorAction Stop};$s=(New-Object -COM WScript.Shell).CreateShortcut($p);$s.TargetPath=(Join-Path $env:TARGET_APP_DIR 'DAWStream.exe');$s.WorkingDirectory=$env:TARGET_APP_DIR;$s.Description='DAWStream - Discord DAW Streaming';$s.Save()"
if !errorlevel! neq 0 (
    echo   WARNING: Could not create Desktop shortcut.
) else (
    echo   OK: Desktop\DAWStream.lnk
)

echo.
echo Installation complete!
if "!IS_ADMIN!"=="0" (
    echo Note: If your DAW runs as Administrator or only scans C:\Program Files\Common Files\VST3,
    echo right-click Install.bat and select "Run as administrator".
)
echo Rescan plugins in your DAW, add DAWStream to your Master channel,
echo open DAWStream from Desktop, and share its window in Discord.
echo.
pause
endlocal
endlocal
