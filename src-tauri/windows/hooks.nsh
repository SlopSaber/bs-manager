!define BSM_HOOK_DIR "${__FILEDIR__}"

!macro NSIS_HOOK_PREUNINSTALL
  SetOutPath "$PLUGINSDIR"
  File "${BSM_HOOK_DIR}\restore-protocols.ps1"
  ExecWait '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File "$PLUGINSDIR\restore-protocols.ps1" "${PRODUCTNAME}"' $0
  Delete "$PLUGINSDIR\restore-protocols.ps1"
  ${If} $0 != 0
    MessageBox MB_OK|MB_ICONSTOP "Protocol registrations could not be restored. Uninstall was cancelled. See the backup under LocalAppData\${PRODUCTNAME}\protocol-backups."
    Abort
  ${EndIf}
!macroend
