; kompressor Windows installer, built by scripts/build/windows.sh with makensis.
;
; Two modes, chosen on the first page:
;   * for the current user (default): kompressor.exe in
;     %LOCALAPPDATA%\Programs\kompressor and on the user's PATH; HKCU only, no
;     elevation, no config, nothing started;
;   * as a system service: Program Files, the system PATH, HKLM, a default
;     config in %ProgramData%\kompressor and an automatic service (WinSW runs
;     `kompressor serve`). Needs elevation: the installer restarts itself
;     through UAC with /SYSTEM.
; An existing install keeps its mode; switching means uninstalling first.
; Silent: /S for the user mode, /S /SYSTEM from an elevated prompt for the service.
;
; Defines: VERSION, VERSION_NUM (numeric part), EXE, WINSW, ROOT, LICENSE and
; GUIDE (CRLF copies), OUTFILE; SIGN when signing. Build-time paths use forward
; slashes, which makensis accepts on every host.

Unicode true
SetCompressor /SOLID lzma
RequestExecutionLevel user

!include "MUI2.nsh"
!include "nsDialogs.nsh"
!include "LogicLib.nsh"
!include "WordFunc.nsh"
!include "FileFunc.nsh"
!include "x64.nsh"
!include "WinVer.nsh"

!define APP       "kompressor"
!define PUBLISHER "Hystax"
!define UNINST    "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP}"
!define SERVICE   "$INSTDIR\kompressor-service.exe"
; 64-bit PowerShell, the one every test of these commands ran in: this
; installer is a 32-bit process, where $SYSDIR is SysWOW64.
!define POWERSHELL '"$WINDIR\Sysnative\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -Command'
!define USER_ENV   "HKCU:\Environment"
!define SYSTEM_ENV "HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Environment"
; LocalSystem, Administrators, LocalService, Users: SIDs, so any Windows
; language works.
!define SID_SYSTEM  "*S-1-5-18"
!define SID_ADMINS  "*S-1-5-32-544"
!define SID_SERVICE "*S-1-5-19"
!define SID_USERS   "*S-1-5-32-545"

Name "OptScale Kompressor ${VERSION}"
OutFile "${OUTFILE}"
ShowInstDetails show
ShowUninstDetails show

VIProductVersion "${VERSION_NUM}.0"
VIAddVersionKey "ProductName"     "OptScale Kompressor"
VIAddVersionKey "ProductVersion"  "${VERSION}"
VIAddVersionKey "FileVersion"     "${VERSION}"
VIAddVersionKey "CompanyName"     "${PUBLISHER}"
VIAddVersionKey "FileDescription" "OptScale Kompressor installer"
VIAddVersionKey "LegalCopyright"  "MIT License"

; Sign the uninstaller before it is packed, and the finished setup program.
; `= 0`: a failed signature fails the build.
!ifdef SIGN
    !uninstfinalize '"${SIGN}" "%1"' = 0
    !finalize '"${SIGN}" "%1"' = 0
!endif

Var SystemMode   ; "1" for the service install
Var CmdDir       ; /D=, if given
Var PrevVersion
Var PrevNum      ; numeric part of the installed version
Var PrevDir
Var CheckSystem
Var RadioRemove
Var FinishText

!define MUI_ABORTWARNING
Page custom ModePage ModeLeave
Page custom ReinstallPage ReinstallLeave
!insertmacro MUI_PAGE_LICENSE "${LICENSE}"
!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipOnUpgrade
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_TEXT_LARGE
!define MUI_FINISHPAGE_TEXT "$FinishText"
!define MUI_FINISHPAGE_SHOWREADME "$INSTDIR\doc\GETTING-STARTED.txt"
!define MUI_FINISHPAGE_SHOWREADME_TEXT "Open the getting-started guide"
!define MUI_FINISHPAGE_LINK "Documentation"
!define MUI_FINISHPAGE_LINK_LOCATION "https://github.com/hystax/optscale-kompressor#readme"
!define MUI_PAGE_CUSTOMFUNCTION_SHOW FinishShow
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!define MUI_FINISHPAGE_TEXT_LARGE
!define MUI_FINISHPAGE_TEXT "$FinishText"
!insertmacro MUI_UNPAGE_FINISH
!insertmacro MUI_LANGUAGE "English"

; ---------------------------------------------------------------- helpers ---

; Paths reach PowerShell through the environment, not its command line, so a
; quote in a path cannot break the command.
!macro SetEnv NAME VALUE
    System::Call 'kernel32::SetEnvironmentVariable(t "${NAME}", t "${VALUE}") i'
!macroend

; Why a PowerShell step failed, in words, from what nsExec returned: "error"
; when powershell.exe could not be started at all, which is how a policy that
; blocks it outright shows up, or the script's exit code.
!macro PowerShellFailure RESULT OUT
    ${If} ${RESULT} == "error"
        StrCpy ${OUT} "PowerShell could not be started; a policy on this computer may block it."
    ${Else}
        StrCpy ${OUT} "PowerShell reported an error (exit ${RESULT}); a policy on this computer may restrict it."
    ${EndIf}
!macroend

; Add the install directory to PATH, or ("remove") take it out; KEY is the
; user's or the system's environment key. PowerShell because NSIS strings stop
; at 1024 characters and a longer PATH would be wiped. Only cmdlets and string
; methods, so it also runs where policy holds PowerShell to Constrained
; Language Mode; there the raw read is refused and %VAR% entries come back
; expanded, which changes their spelling but not the PATH. The broadcast lets
; new terminals see the change.
!macro EditPath VERB KEY
    !insertmacro SetEnv "KOMPRESSOR_SETUP_DIR" "$INSTDIR"
    !insertmacro SetEnv "KOMPRESSOR_SETUP_KEY" "${KEY}"
    nsExec::ExecToLog `${POWERSHELL} "$$ErrorActionPreference = 'Stop'; $$d = $$env:KOMPRESSOR_SETUP_DIR.TrimEnd('\'); $$key = $$env:KOMPRESSOR_SETUP_KEY; try { $$p = [string](Get-Item -LiteralPath $$key).GetValue('Path', '', 'DoNotExpandEnvironmentNames') } catch { $$p = [string](Get-ItemProperty -LiteralPath $$key -Name Path -ErrorAction SilentlyContinue).Path }; $$keep = @($$p -split ';' | Where-Object { $$_ -and $$_.TrimEnd('\') -ne $$d }); if ('${VERB}' -eq 'add') { $$keep += $$d }; Set-ItemProperty -LiteralPath $$key -Name Path -Value ($$keep -join ';') -Type ExpandString"`
    Pop $0
    ${If} $0 != 0
        DetailPrint "could not update PATH (exit $0)"
        !insertmacro PowerShellFailure $0 $1
        ${If} ${VERB} == "add"
            MessageBox MB_OK|MB_ICONEXCLAMATION "Could not add $INSTDIR to PATH: $1$\r$\n$\r$\nAdd it by hand (Settings > System > About > Advanced system settings > Environment Variables), or run kompressor by its full path." /SD IDOK
        ${Else}
            MessageBox MB_OK|MB_ICONEXCLAMATION "Could not remove $INSTDIR from PATH: $1$\r$\n$\r$\nRemove it by hand in Environment Variables." /SD IDOK
        ${EndIf}
    ${EndIf}
    SendMessage ${HWND_BROADCAST} ${WM_SETTINGCHANGE} 0 "STR:Environment" /TIMEOUT=5000
!macroend

!macro WriteUninstallKey ROOT
    WriteRegStr   ${ROOT} "${UNINST}" "DisplayName"          "OptScale Kompressor"
    WriteRegStr   ${ROOT} "${UNINST}" "DisplayVersion"       "${VERSION}"
    WriteRegStr   ${ROOT} "${UNINST}" "VersionNumber"        "${VERSION_NUM}"
    WriteRegStr   ${ROOT} "${UNINST}" "Publisher"            "${PUBLISHER}"
    WriteRegStr   ${ROOT} "${UNINST}" "DisplayIcon"          "$INSTDIR\kompressor.exe"
    WriteRegStr   ${ROOT} "${UNINST}" "InstallLocation"      "$INSTDIR"
    WriteRegStr   ${ROOT} "${UNINST}" "UninstallString"      '"$INSTDIR\uninstall.exe"'
    WriteRegStr   ${ROOT} "${UNINST}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
    WriteRegDWORD ${ROOT} "${UNINST}" "NoModify" 1
    WriteRegDWORD ${ROOT} "${UNINST}" "NoRepair" 1
!macroend

; MUI's finish text is only 60u tall. Move the checkbox down to just above the
; link and stretch the text from under the title to above the checkbox.
Function FinishShow
    System::Call '*(i 120, i 160, i 195, i 10) p.r9'
    System::Call 'user32::MapDialogRect(p $mui.FinishPage, p r9)'
    System::Call '*$9(i .r5, i .r6, i .r7, i .r8)' ; left, checkbox top, width, checkbox height
    System::Call '*$9(i 0, i 7, i 0, i 0)'
    System::Call 'user32::MapDialogRect(p $mui.FinishPage, p r9)'
    System::Call '*$9(i, i .r3)' ; the 7u gap
    System::Call 'user32::GetWindowRect(p $mui.FinishPage.Title, p r9)'
    System::Call 'user32::MapWindowPoints(p 0, p $mui.FinishPage, p r9, i 2)'
    System::Call '*$9(i, i, i, i .r4)' ; the title's bottom
    System::Free $9
    IntOp $4 $4 + $3 ; text top
    IntOp $2 $6 - $3
    IntOp $2 $2 - $4 ; text height
    System::Call 'user32::SetWindowPos(p $mui.FinishPage.Text, p 0, i r5, i r4, i r7, i r2, i 0x14)' ; SWP_NOZORDER|SWP_NOACTIVATE
    System::Call 'user32::SetWindowPos(p $mui.FinishPage.ShowReadme, p 0, i r5, i r6, i r7, i r8, i 0x14)'
FunctionEnd

; "Admin" in OUT only when this process is elevated.
!macro IsAdmin OUT
    UserInfo::GetAccountType
    Pop ${OUT}
!macroend

; ------------------------------------------------------------------- mode ---

Function .onInit
    ; The setup stub runs anywhere; kompressor.exe is x86_64 and needs Windows 10.
    ${IfNot} ${RunningX64}
    ${OrIfNot} ${AtLeastWin10}
        MessageBox MB_OK|MB_ICONSTOP "OptScale Kompressor needs 64-bit Windows 10 or later." /SD IDOK
        SetErrorLevel 1150 ; ERROR_OLD_WIN_VERSION
        Quit
    ${EndIf}
    ; NSIS is a 32-bit process; without this HKLM writes land in WOW6432Node.
    SetRegView 64
    StrCpy $CmdDir $INSTDIR
    ${GetParameters} $R0
    ClearErrors
    ${GetOptions} $R0 "/SYSTEM" $R1
    ${IfNot} ${Errors}
        StrCpy $SystemMode 1
    ${EndIf}

    ; A previous install fixes the mode.
    ReadRegStr $PrevVersion HKLM "${UNINST}" "DisplayVersion"
    ReadRegStr $PrevNum     HKLM "${UNINST}" "VersionNumber"
    ReadRegStr $PrevDir     HKLM "${UNINST}" "InstallLocation"
    ${If} $PrevVersion != ""
        StrCpy $SystemMode 1
    ${Else}
        ReadRegStr $PrevVersion HKCU "${UNINST}" "DisplayVersion"
        ReadRegStr $PrevNum     HKCU "${UNINST}" "VersionNumber"
        ReadRegStr $PrevDir     HKCU "${UNINST}" "InstallLocation"
        ${If} $PrevVersion != ""
        ${AndIf} $SystemMode == 1
            MessageBox MB_OK|MB_ICONEXCLAMATION "OptScale Kompressor $PrevVersion is installed for the current user. Uninstall it first to install the system service." /SD IDOK
            Abort
        ${EndIf}
    ${EndIf}

    ${If} $SystemMode == 1
        Call Elevate
        ${If} $0 == "denied"
            MessageBox MB_OK|MB_ICONEXCLAMATION "The system service install needs administrator rights." /SD IDOK
            Abort
        ${EndIf}
    ${EndIf}
    Call ApplyMode
FunctionEnd

; Start FILE with ARGS through UAC. OUT is ShellExecute's result: above 32
; when it started, 32 or below when it did not (a declined prompt included).
!macro RunElevated FILE ARGS OUT
    System::Call 'shell32::ShellExecuteW(p $HWNDPARENT, w "runas", w "${FILE}", w "${ARGS}", p 0, i 1) p.${OUT}'
!macroend

; Leaves "admin" in $0 when already elevated and "denied" when UAC was declined;
; otherwise restarts this installer elevated with /SYSTEM and quits.
Function Elevate
    !insertmacro IsAdmin $0
    ${If} $0 == "Admin"
        StrCpy $0 "admin"
        Return
    ${EndIf}
    ${If} ${Silent}
        SetErrorLevel 740 ; ERROR_ELEVATION_REQUIRED
        Quit
    ${EndIf}
    ${GetParameters} $R0
    !insertmacro RunElevated "$EXEPATH" "/SYSTEM $R0" r1
    ${If} $1 > 32
        Quit
    ${EndIf}
    StrCpy $0 "denied"
FunctionEnd

Function ApplyMode
    ${If} $SystemMode == 1
        SetShellVarContext all
        StrCpy $INSTDIR "$PROGRAMFILES64\${APP}"
        StrCpy $FinishText "Thank you for installing OptScale Kompressor.$\r$\n$\r$\nThe OptScale Kompressor service is running and starts with Windows. Its configuration is $APPDATA\${APP}\config.toml.$\r$\n$\r$\nTo use kompressor, open a new PowerShell window and type kompressor."
    ${Else}
        SetShellVarContext current
        StrCpy $INSTDIR "$LOCALAPPDATA\Programs\${APP}"
        StrCpy $FinishText "Thank you for installing OptScale Kompressor.$\r$\n$\r$\nTo get started, open a new PowerShell window and type kompressor."
    ${EndIf}
    ${If} $CmdDir != ""
        StrCpy $INSTDIR $CmdDir
    ${EndIf}
    ${If} $PrevDir != ""
        StrCpy $INSTDIR $PrevDir
    ${EndIf}
FunctionEnd

Function ModePage
    ${If} $PrevVersion != ""
    ${OrIf} $SystemMode == 1
        Abort
    ${EndIf}
    !insertmacro MUI_HEADER_TEXT "Installation type" "Choose how OptScale Kompressor is installed."
    nsDialogs::Create 1018
    Pop $0
    ${If} $0 == error
        Abort
    ${EndIf}
    ${NSD_CreateLabel} 0 0 100% 30u "By default kompressor is installed for you only, in your AppData folder, without administrator rights, and you run it yourself from PowerShell."
    Pop $0
    ${NSD_CreateCheckbox} 0 40u 100% 12u "Install as a system service (all users, starts with Windows)"
    Pop $CheckSystem
    ${NSD_CreateLabel} 12u 56u 100% 36u "Installs into Program Files, writes a default configuration to ProgramData and registers the OptScale Kompressor service. Asks for administrator rights."
    Pop $0
    nsDialogs::Show
FunctionEnd

Function ModeLeave
    ${NSD_GetState} $CheckSystem $0
    ${If} $0 != ${BST_CHECKED}
        Return
    ${EndIf}
    Call Elevate
    ${If} $0 == "denied"
        MessageBox MB_OK|MB_ICONEXCLAMATION "The system service install needs administrator rights."
        Abort
    ${EndIf}
    ; Already elevated: switch in place.
    StrCpy $SystemMode 1
    Call ApplyMode
FunctionEnd

; --------------------------------------------------------- already installed ---

Function ReinstallPage
    ${If} $PrevVersion == ""
    ${OrIf} $PrevDir == ""
        Abort
    ${EndIf}

    ; Numbers only: VersionCompare misreads pre-release suffixes.
    ${If} $PrevNum == ""
        StrCpy $PrevNum $PrevVersion
    ${EndIf}
    ${VersionCompare} $PrevNum "${VERSION_NUM}" $0
    ${If} $0 == 0
        StrCpy $1 "Reinstall OptScale Kompressor ${VERSION}"
    ${ElseIf} $0 == 2
        StrCpy $1 "Upgrade to OptScale Kompressor ${VERSION}"
    ${Else}
        StrCpy $1 "Replace it with the older version ${VERSION}"
    ${EndIf}

    !insertmacro MUI_HEADER_TEXT "Already installed" "Choose what to do with the installed version."
    nsDialogs::Create 1018
    Pop $0
    ${If} $0 == error
        Abort
    ${EndIf}
    ${NSD_CreateLabel} 0 0 100% 20u "OptScale Kompressor $PrevVersion is installed in $PrevDir."
    Pop $0
    ${NSD_CreateRadioButton} 10u 28u 100% 12u $1
    Pop $0
    ${NSD_Check} $0
    ${NSD_CreateRadioButton} 10u 44u 100% 12u "Uninstall OptScale Kompressor $PrevVersion"
    Pop $RadioRemove
    ${IfNot} ${FileExists} "$PrevDir\uninstall.exe"
        EnableWindow $RadioRemove 0
    ${EndIf}
    ${NSD_CreateLabel} 0 68u 100% 20u "Either way the configuration and data are kept."
    Pop $0
    nsDialogs::Show
FunctionEnd

Function ReinstallLeave
    ${NSD_GetState} $RadioRemove $0
    ${If} $0 != ${BST_CHECKED}
        Return
    ${EndIf}
    ; _?= runs the uninstaller in place and waits for it; it cannot delete itself
    ; from there, so its file and the directory are removed here.
    HideWindow
    ExecWait '"$PrevDir\uninstall.exe" _?=$PrevDir' $1
    BringToFront
    ${If} $1 == 0
        Delete "$PrevDir\uninstall.exe"
        RMDir "$PrevDir"
        Quit
    ${EndIf}
    ; Cancelled, or stopped because kompressor was running: stay here.
    Abort
FunctionEnd

; An upgrade keeps the installed directory.
Function SkipOnUpgrade
    ${If} $PrevDir != ""
        Abort
    ${EndIf}
FunctionEnd

; ---------------------------------------------------------------- install ---

; A running kompressor.exe cannot be replaced. Checked here rather than on a
; page so /S stops cleanly too. The read-only attribute is cleared first: the
; file is about to be replaced anyway, and it would otherwise look running.
Function CheckNotRunning
    ${IfNot} ${FileExists} "$INSTDIR\kompressor.exe"
        Return
    ${EndIf}
    SetFileAttributes "$INSTDIR\kompressor.exe" NORMAL
    retry:
    ClearErrors
    FileOpen $2 "$INSTDIR\kompressor.exe" a
    ${IfNot} ${Errors}
        FileClose $2
        Return
    ${EndIf}
    MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "$INSTDIR\kompressor.exe cannot be replaced: it is running, or another program has it open. Stop kompressor (close the wrap or serve that runs it, or: Get-Process kompressor | Stop-Process) and retry." /SD IDCANCEL IDRETRY retry
    Abort "kompressor.exe is in use"
FunctionEnd

Function InstallService
    ; Everyone may read the stores, as on Linux; only the service and
    ; administrators write them, and only they read the CA key.
    CreateDirectory "$APPDATA\${APP}\logs"
    CreateDirectory "$APPDATA\${APP}\ca"
    nsExec::ExecToLog '"$SYSDIR\icacls.exe" "$APPDATA\${APP}" /inheritance:r /grant:r ${SID_SYSTEM}:(OI)(CI)F ${SID_ADMINS}:(OI)(CI)F ${SID_SERVICE}:(OI)(CI)M ${SID_USERS}:(OI)(CI)RX'
    Pop $0
    nsExec::ExecToLog '"$SYSDIR\icacls.exe" "$APPDATA\${APP}\ca" /inheritance:r /grant:r ${SID_SYSTEM}:(OI)(CI)F ${SID_ADMINS}:(OI)(CI)F ${SID_SERVICE}:(OI)(CI)M'
    Pop $1
    ${If} $0 != 0
    ${OrIf} $1 != 0
        DetailPrint "could not set permissions on $APPDATA\${APP}"
    ${EndIf}

    ; The default config, only if there is none: cmdlets only, so Constrained
    ; Language Mode runs it too. Written as UTF-8 (with a BOM, which the loader
    ; accepts); the state path is absolute with forward slashes (TOML escapes
    ; backslashes, the loader expands nothing).
    SetOutPath "$INSTDIR"
    File "/oname=config.toml.in" "${ROOT}/packaging/macos/config.toml.in"
    !insertmacro SetEnv "KOMPRESSOR_SETUP_TEMPLATE" "$INSTDIR\config.toml.in"
    !insertmacro SetEnv "KOMPRESSOR_SETUP_CONFIG"   "$APPDATA\${APP}\config.toml"
    !insertmacro SetEnv "KOMPRESSOR_SETUP_STATE"    "$APPDATA\${APP}"
    nsExec::ExecToLog `${POWERSHELL} "$$ErrorActionPreference = 'Stop'; $$c = $$env:KOMPRESSOR_SETUP_CONFIG; if (Test-Path -LiteralPath $$c) { 'kept existing ' + $$c; exit 0 }; $$s = $$env:KOMPRESSOR_SETUP_STATE.Replace('\', '/'); $$t = (Get-Content -LiteralPath $$env:KOMPRESSOR_SETUP_TEMPLATE -Raw -Encoding UTF8).Replace('@STATE@', $$s); Set-Content -LiteralPath $$c -Value $$t -Encoding UTF8 -NoNewline; 'wrote ' + $$c"`
    Pop $0
    ${If} $0 != 0
        DetailPrint "could not write the config (exit $0)"
        !insertmacro PowerShellFailure $0 $1
        MessageBox MB_OK|MB_ICONEXCLAMATION "Could not write $APPDATA\${APP}\config.toml: $1$\r$\n$\r$\nCopy $INSTDIR\config.toml.in there, replace @STATE@ with $APPDATA\${APP} written with forward slashes, and restart the OptScale Kompressor service." /SD IDOK
    ${EndIf}

    ; WinSW reads the XML file named after itself. Name, start mode and failure
    ; actions are fixed at registration, so an upgrade unregisters the old
    ; service (already stopped by then) and registers it again.
    File "/oname=kompressor-service.exe" "${WINSW}"
    File "/oname=kompressor-service.xml" "${ROOT}/packaging/windows/kompressor-service.xml"
    nsExec::ExecToLog '"${SERVICE}" uninstall'
    Pop $0
    nsExec::ExecToLog '"${SERVICE}" install'
    Pop $0
    ${If} $0 != 0
        DetailPrint "could not register the service (exit $0)"
        StrCpy $FinishText "OptScale Kompressor is installed, but its service could not be registered.$\r$\n$\r$\nFrom an administrator PowerShell:$\r$\n      & $\"$INSTDIR\kompressor-service.exe$\" install"
        MessageBox MB_OK|MB_ICONEXCLAMATION "The OptScale Kompressor service could not be registered. If Windows says it is marked for deletion, close the Services window and run setup again, or restart Windows." /SD IDOK
        Return
    ${EndIf}
    ; WinSW registers the service as LocalSystem; it is started only once it is
    ; switched to LocalService, so a failed switch never runs it with more.
    nsExec::ExecToLog '"$SYSDIR\sc.exe" config ${APP} obj= "NT AUTHORITY\LocalService" password= ""'
    Pop $0
    ${If} $0 != 0
        DetailPrint "could not run the service as LocalService (exit $0); not started"
        StrCpy $FinishText "OptScale Kompressor is installed, but its service was not started: it could not be set to run as LocalService.$\r$\n$\r$\nFrom an administrator PowerShell:$\r$\n      sc.exe config ${APP} obj= $\"NT AUTHORITY\LocalService$\" password= $\"$\"$\r$\n      Start-Service ${APP}"
        MessageBox MB_OK|MB_ICONEXCLAMATION "The OptScale Kompressor service could not be set to run as LocalService, so it was not started. The last page says how to finish by hand." /SD IDOK
        Return
    ${EndIf}
    nsExec::ExecToLog '"${SERVICE}" start'
    Pop $0
    ${If} $0 != 0
        StrCpy $FinishText "OptScale Kompressor is installed, but its service did not start. Its logs are in $APPDATA\${APP}\logs."
        MessageBox MB_OK|MB_ICONEXCLAMATION "The OptScale Kompressor service did not start. Its logs are in $APPDATA\${APP}\logs." /SD IDOK
        Return
    ${EndIf}

    ; Started is not serving: WinSW reports success once it has launched serve,
    ; and restarts a serve that exits (port taken, broken config) in a loop. Wait
    ; for a listener owned by a kompressor in session 0, the service's; one the
    ; user runs is in their own session and would answer /healthz just the same.
    ; netstat rather than Get-NetTCPConnection, whose module Constrained Language
    ; Mode will not load; a listener is the TCP row whose remote address is
    ; port 0, which reads the same in every Windows language.
    DetailPrint "waiting for the service to listen ..."
    nsExec::ExecToLog `${POWERSHELL} "for ($$i = 0; $$i -lt 20; $$i++) { $$l = @(netstat -ano -p TCP | ForEach-Object { $$f = -split $$_; if ($$f.Count -ge 5 -and $$f[0] -eq 'TCP' -and $$f[2] -match ':0$$') { $$p = Get-Process -Id $$f[4] -ErrorAction SilentlyContinue; if ($$p -and $$p.ProcessName -eq 'kompressor' -and $$p.SessionId -eq 0) { $$f[1] } } }); if ($$l.Count) { 'serving on ' + ($$l -join ', '); exit 0 }; Start-Sleep -Seconds 1 }; exit 3"`
    Pop $0
    ${If} $0 == 3
        StrCpy $FinishText "OptScale Kompressor is installed and its service is registered, but it is not serving: kompressor keeps exiting. The usual cause is port 8817 already in use, by a kompressor wrap or serve you started; stop it and the service recovers within seconds. Its logs are in $APPDATA\${APP}\logs."
        MessageBox MB_OK|MB_ICONEXCLAMATION "The OptScale Kompressor service started but is not serving. Is port 8817 in use (a kompressor wrap or serve you started)? Its logs are in $APPDATA\${APP}\logs." /SD IDOK
    ${ElseIf} $0 != 0
        DetailPrint "could not check that the service is serving (exit $0)"
    ${EndIf}
FunctionEnd

Section "kompressor" Main
    ; An upgrade replaces kompressor.exe under the running service. `stopwait`,
    ; not `stop`: WinSW's `stop` returns before serve has exited.
    ${If} $SystemMode == 1
    ${AndIf} ${FileExists} "${SERVICE}"
        nsExec::ExecToLog '"${SERVICE}" stopwait'
        Pop $0
    ${EndIf}
    Call CheckNotRunning

    SetOutPath "$INSTDIR"
    File "/oname=kompressor.exe" "${EXE}"
    SetOutPath "$INSTDIR\doc"
    File "/oname=GETTING-STARTED.txt" "${GUIDE}"
    File "${ROOT}/README.md"
    File "${ROOT}/docs/QUICKSTART.md"
    File "/oname=LICENSE.txt" "${LICENSE}"
    SetOutPath "$INSTDIR"
    WriteUninstaller "$INSTDIR\uninstall.exe"

    ${If} $SystemMode == 1
        Call InstallService
        !insertmacro EditPath "add" "${SYSTEM_ENV}"
        !insertmacro WriteUninstallKey HKLM
    ${Else}
        !insertmacro EditPath "add" "${USER_ENV}"
        !insertmacro WriteUninstallKey HKCU
    ${EndIf}
SectionEnd

; -------------------------------------------------------------- uninstall ---

Function un.onInit
    SetRegView 64
    ; Only the system mode installs the service's XML next to the binary.
    ${If} ${FileExists} "$INSTDIR\kompressor-service.xml"
        StrCpy $SystemMode 1
        SetShellVarContext all
        !insertmacro IsAdmin $1
        ${If} $1 != "Admin"
            ${If} ${Silent}
                SetErrorLevel 740 ; ERROR_ELEVATION_REQUIRED
                Quit
            ${EndIf}
            !insertmacro RunElevated "$INSTDIR\uninstall.exe" "" r1
            ${If} $1 <= 32
                MessageBox MB_OK|MB_ICONEXCLAMATION "Removing the OptScale Kompressor service needs administrator rights."
            ${EndIf}
            Quit
        ${EndIf}
        StrCpy $FinishText "OptScale Kompressor has been removed.$\r$\n$\r$\nThe service's configuration, data and logs were kept, for a later reinstall:$\r$\n      $APPDATA\${APP}$\r$\n$\r$\nDelete that folder if you no longer need it."
    ${Else}
        ; Only folders that exist: the user mode creates none of them itself.
        StrCpy $2 ""
        ${If} ${FileExists} "$PROFILE\.config\${APP}\*.*"
            StrCpy $2 "$2$\r$\n      $PROFILE\.config\${APP}"
        ${EndIf}
        ${If} ${FileExists} "$LOCALAPPDATA\${APP}\*.*"
            StrCpy $2 "$2$\r$\n      $LOCALAPPDATA\${APP}"
        ${EndIf}
        ${If} ${FileExists} "$PROFILE\.kompressor\*.*"
            StrCpy $2 "$2$\r$\n      $PROFILE\.kompressor"
        ${EndIf}
        StrCpy $FinishText "OptScale Kompressor has been removed."
        ${If} $2 != ""
            StrCpy $FinishText "$FinishText$\r$\n$\r$\nFolders kompressor created outside its program folder were kept:$2$\r$\n$\r$\nDelete them if you no longer need them."
        ${EndIf}
    ${EndIf}
FunctionEnd

; Removes the binary first and stops if it is still running, so nothing is
; half-uninstalled. Configuration and data are kept, as on macOS.
Section "Uninstall"
    ${If} $SystemMode == 1
    ${AndIf} ${FileExists} "${SERVICE}"
        nsExec::ExecToLog '"${SERVICE}" stopwait'
        Pop $0
    ${EndIf}
    SetFileAttributes "$INSTDIR\kompressor.exe" NORMAL
    Delete "$INSTDIR\kompressor.exe"
    ${If} ${FileExists} "$INSTDIR\kompressor.exe"
        ${If} $SystemMode == 1
            nsExec::ExecToLog '"${SERVICE}" start'
            Pop $0
        ${EndIf}
        MessageBox MB_OK|MB_ICONEXCLAMATION "$INSTDIR\kompressor.exe cannot be removed: it is running, or another program has it open. Stop kompressor (close the wrap or serve that runs it, or: Get-Process kompressor | Stop-Process) and uninstall again." /SD IDOK
        Abort "kompressor.exe is in use; nothing was removed"
    ${EndIf}

    ${If} $SystemMode == 1
        nsExec::ExecToLog '"${SERVICE}" uninstall'
        Pop $0
        !insertmacro EditPath "remove" "${SYSTEM_ENV}"
        DeleteRegKey HKLM "${UNINST}"
        DetailPrint "kept configuration and data: $APPDATA\${APP}"
    ${Else}
        !insertmacro EditPath "remove" "${USER_ENV}"
        DeleteRegKey HKCU "${UNINST}"
    ${EndIf}
    Delete "$INSTDIR\kompressor-service.exe"
    Delete "$INSTDIR\kompressor-service.xml"
    Delete "$INSTDIR\config.toml.in"
    RMDir /r "$INSTDIR\doc"
    Delete "$INSTDIR\uninstall.exe"
    RMDir "$INSTDIR"
SectionEnd
