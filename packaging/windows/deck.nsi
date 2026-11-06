; Deck installer: per user, no admin.
;
;     makensis -DVERSION=0.1.0 -DSRC=dist/windows/Deck -DOUTFILE=dist/Deck-0.1.0-setup.exe packaging/windows/deck.nsi
;
; scripts/build-windows.sh runs it. Builds with stock NSIS 3 (System and
; nsExec plugins only), on Linux or Windows.
;
; What it does, all under HKCU:
;   - deck.exe into %LOCALAPPDATA%\Programs\Deck
;   - a Start menu shortcut whose AppUserModelID is Deck.Deck, the id the
;     app files its toasts under (other/notify.rs)
;   - the install folder on the user PATH, so `deck` works in any terminal
;   - deck:// links open in Deck
;   - an entry in Settings > Apps
; No Run key: the app turns its background service's autostart on and off.

Unicode true
SetCompressor /SOLID lzma
RequestExecutionLevel user
ManifestDPIAware true

!ifndef VERSION
  !define VERSION "0.0.0"
!endif
!ifndef SRC
  !define SRC "..\..\dist\windows\Deck"
!endif
!ifndef OUTFILE
  !define OUTFILE "Deck-${VERSION}-setup.exe"
!endif

!define APPNAME "Deck"
!define AUMID "Deck.Deck"
!define UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\Deck"
; The Evergreen WebView2 runtime's EdgeUpdate client id.
!define WEBVIEW2 "{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"
!define WEBVIEW2_URL "https://go.microsoft.com/fwlink/p/?LinkId=2124703"

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "WinMessages.nsh"
!include "x64.nsh"
!include "Win\Propkey.nsh"

Name "${APPNAME}"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\Deck"
InstallDirRegKey HKCU "${UNINSTKEY}" "InstallLocation"
BrandingText "Deck ${VERSION}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "Deck"
VIAddVersionKey "FileDescription" "Deck installer"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "MIT"

!define MUI_ICON "deck.ico"
!define MUI_UNICON "deck.ico"
!define MUI_FINISHPAGE_RUN "$INSTDIR\deck.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Open Deck"

!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

; ---------------------------------------------------------------------------
; Shared helpers. Each comes in an installer and an uninstaller copy (un.).

; Ends every deck.exe of this user: the window, the background service and
; the jobs it runs, so the files can be replaced or removed.
!macro STOP_DECK un
Function ${un}StopDeck
  nsExec::Exec 'cmd /c tasklist /FI "IMAGENAME eq deck.exe" /NH | find /I "deck.exe"'
  Pop $0
  ${If} $0 == 0
    MessageBox MB_OKCANCEL|MB_ICONEXCLAMATION "Deck is running. Its window, its background service and the jobs that service runs will be closed." /SD IDOK IDOK +2
    Abort
    nsExec::Exec 'taskkill /F /T /IM deck.exe'
    Pop $0
    Sleep 500
  ${EndIf}
FunctionEnd
!macroend
!insertmacro STOP_DECK ""
!insertmacro STOP_DECK "un."

; The user Path as $0, and its length in characters as $1. $2 is 1 when
; it is too long for an NSIS string (NSIS_MAX_STRLEN, 1024 in stock builds):
; ReadRegStr would hand back a cut-off copy, and writing that back would
; lose the rest, so the caller leaves Path alone then.
!macro READ_PATH un
Function ${un}ReadPath
  StrCpy $2 0
  StrCpy $1 0
  ; RegGetValueW with no buffer: the size in bytes, NUL included
  ; (RRF_RT_ANY | RRF_NOEXPAND, so %VARS% stay as written).
  System::Call 'advapi32::RegGetValueW(p 0x80000001, w "Environment", w "Path", i 0x1000ffff, p 0, p 0, *i 0 r3) i .r4'
  ${If} $4 = 0
    IntOp $1 $3 / 2
    ; Room for one more folder (MAX_PATH) and its ";".
    IntOp $3 $1 + 261
    ${If} $3 >= ${NSIS_MAX_STRLEN}
      StrCpy $2 1
      StrCpy $0 ""
      Return
    ${EndIf}
  ${EndIf}
  ClearErrors
  ReadRegStr $0 HKCU "Environment" "Path"
  StrLen $1 $0
FunctionEnd
!macroend
!insertmacro READ_PATH ""
!insertmacro READ_PATH "un."

; $0 = Path, $5 = folder. Leaves $0 as Path without any entry equal to the
; folder (case-insensitive, with or without a trailing backslash), $6 = 1
; when there was one.
!macro DROP_ENTRY un
Function ${un}DropEntry
  StrCpy $6 0
  StrCpy $7 ""   ; result
  StrCpy $8 0    ; read position
  StrCpy $9 ""   ; current entry
  loop:
    StrCpy $3 $0 1 $8
    ${If} $3 == ";"
    ${OrIf} $3 == ""
      ${If} $9 == $5
      ${OrIf} $9 == "$5\"
        StrCpy $6 1
      ${ElseIf} $9 != ""
        ${If} $7 == ""
          StrCpy $7 $9
        ${Else}
          StrCpy $7 "$7;$9"
        ${EndIf}
      ${EndIf}
      StrCpy $9 ""
      ${If} $3 == ""
        Goto done
      ${EndIf}
    ${Else}
      StrCpy $9 "$9$3"
    ${EndIf}
    IntOp $8 $8 + 1
    Goto loop
  done:
  StrCpy $0 $7
FunctionEnd
!macroend
!insertmacro DROP_ENTRY ""
!insertmacro DROP_ENTRY "un."

!macro BROADCAST_ENV
  SendMessage ${HWND_BROADCAST} ${WM_SETTINGCHANGE} 0 "STR:Environment" /TIMEOUT=5000
!macroend

; Sets the AppUserModelID of the shortcut at $0 to ${AUMID}: load it through
; IShellLink's IPersistFile, set PKEY_AppUserModel_ID on its IPropertyStore,
; save. Plain COM through the System plugin, so no extra NSIS plugin.
Function SetShortcutAumid
  ; CLSID_ShellLink, asking for IPersistFile.
  System::Call 'ole32::CoCreateInstance(g "{00021401-0000-0000-C000-000000000046}", p 0, i 1, g "{0000010B-0000-0000-C000-000000000046}", *p .r1) i .r2'
  ${If} $2 <> 0
    DetailPrint "Shortcut AppUserModelID: CoCreateInstance failed ($2)"
    Return
  ${EndIf}
  ; IPersistFile::Load(path, STGM_READWRITE)
  System::Call '$1->5(w r0, i 2) i .r2'
  ${If} $2 = 0
    ; QueryInterface(IID_IPropertyStore)
    System::Call '$1->0(g "{886D8EEB-8CF2-4446-8D02-CDBA1DBDCF99}", *p .r3) i .r2'
    ${If} $2 = 0
      System::Call '*${SYSSTRUCT_PROPERTYKEY}(${PKEY_AppUserModel_ID}) p .r4'
      ; PROPVARIANT { vt = VT_LPWSTR, 6 reserved bytes, pwszVal, pad }
      System::Call '*(&w64 "${AUMID}") p .r5'
      System::Call '*(&i2 ${VT_LPWSTR}, &i6 0, p r5, p 0) p .r6'
      ; IPropertyStore::SetValue, ::Commit
      System::Call '$3->6(p r4, p r6) i .r2'
      ${If} $2 = 0
        System::Call '$3->7() i .r2'
      ${EndIf}
      System::Free $4
      System::Free $5
      System::Free $6
      System::Call '$3->2()'
      ${If} $2 = 0
        ; IPersistFile::Save(NULL, TRUE)
        System::Call '$1->6(p 0, i 1) i .r2'
      ${EndIf}
    ${EndIf}
  ${EndIf}
  ${If} $2 <> 0
    DetailPrint "Shortcut AppUserModelID: failed ($2)"
  ${EndIf}
  System::Call '$1->2()'
FunctionEnd

; ---------------------------------------------------------------------------

Function .onInit
  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "Deck needs 64-bit Windows 10 (1809) or later." /SD IDOK
    Abort
  ${EndIf}
  SetRegView 64
FunctionEnd

Function un.onInit
  SetRegView 64
FunctionEnd

Section "Deck"
  Call StopDeck

  SetOutPath "$INSTDIR"
  File "${SRC}\deck.exe"
  WriteUninstaller "$INSTDIR\uninstall.exe"

  ; Start menu, with the toast id.
  CreateShortCut "$SMPROGRAMS\Deck.lnk" "$INSTDIR\deck.exe" "" "$INSTDIR\deck.exe" 0
  StrCpy $0 "$SMPROGRAMS\Deck.lnk"
  Call SetShortcutAumid
  WriteRegStr HKCU "Software\Classes\AppUserModelId\${AUMID}" "DisplayName" "Deck"
  WriteRegStr HKCU "Software\Classes\AppUserModelId\${AUMID}" "IconUri" "$INSTDIR\deck.exe"

  ; `deck` in every new terminal.
  Call ReadPath
  ${If} $2 = 1
    DetailPrint "User PATH is $1 characters, too long to edit safely; add $INSTDIR to it by hand."
  ${Else}
    StrCpy $5 "$INSTDIR"
    Call DropEntry
    ${If} $6 = 0
      ${If} $0 == ""
        StrCpy $0 "$INSTDIR"
      ${Else}
        StrCpy $0 "$0;$INSTDIR"
      ${EndIf}
      WriteRegExpandStr HKCU "Environment" "Path" $0
      !insertmacro BROADCAST_ENV
    ${EndIf}
  ${EndIf}

  ; deck:// links.
  WriteRegStr HKCU "Software\Classes\deck" "" "URL:Deck"
  WriteRegStr HKCU "Software\Classes\deck" "URL Protocol" ""
  WriteRegStr HKCU "Software\Classes\deck\DefaultIcon" "" "$INSTDIR\deck.exe,0"
  WriteRegStr HKCU "Software\Classes\deck\shell\open\command" "" '"$INSTDIR\deck.exe" "%1"'

  ; Settings > Apps.
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayName" "Deck"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTKEY}" "Publisher" "Deck"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayIcon" "$INSTDIR\deck.exe,0"
  WriteRegStr HKCU "${UNINSTKEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTKEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINSTKEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoRepair" 1

  ; Pages are WebView2. Windows 10 and 11 ship the runtime; a stripped or
  ; old install may not have it.
  ReadRegStr $0 HKLM "SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\${WEBVIEW2}" "pv"
  ${If} $0 == ""
  ${OrIf} $0 == "0.0.0.0"
    ReadRegStr $0 HKCU "Software\Microsoft\EdgeUpdate\Clients\${WEBVIEW2}" "pv"
  ${EndIf}
  ${If} $0 == ""
  ${OrIf} $0 == "0.0.0.0"
    MessageBox MB_YESNO|MB_ICONQUESTION "Deck shows web pages with the Microsoft Edge WebView2 Runtime, which is not installed. Open its download page?" /SD IDNO IDNO +2
    ExecShell "open" "${WEBVIEW2_URL}"
  ${EndIf}
SectionEnd

Section "Uninstall"
  Call un.StopDeck

  Delete "$SMPROGRAMS\Deck.lnk"
  DeleteRegKey HKCU "Software\Classes\AppUserModelId\${AUMID}"
  DeleteRegKey HKCU "Software\Classes\deck"
  ; Autostart the app may have turned on for its background service
  ; (value "Deck").
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Deck service"

  Call un.ReadPath
  ${If} $2 = 0
    StrCpy $5 "$INSTDIR"
    Call un.DropEntry
    ${If} $6 = 1
      WriteRegExpandStr HKCU "Environment" "Path" $0
      !insertmacro BROADCAST_ENV
    ${EndIf}
  ${EndIf}

  DeleteRegKey HKCU "${UNINSTKEY}"
  ; Only what the installer put there, then the folder if that left it
  ; empty: $INSTDIR may be a folder the user picked that holds other things.
  Delete "$INSTDIR\deck.exe"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
SectionEnd
