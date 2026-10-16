!define BSM_HOOK_DIR "${__FILEDIR__}"

!macro NSIS_HOOK_PREINSTALL
  SetOutPath "$PLUGINSDIR"
  File "${BSM_HOOK_DIR}\replacement-preinstall.ps1"
  ExecWait '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File "$PLUGINSDIR\replacement-preinstall.ps1" "${PRODUCTNAME}"' $0
  Delete "$PLUGINSDIR\replacement-preinstall.ps1"
  SetOutPath "$INSTDIR"
  ${If} $0 != 0
    MessageBox MB_OK|MB_ICONSTOP "The existing BSManager installation could not be removed safely. Installer cancelled. Your settings and registration backups are under LocalAppData\BSManager\migration-backups."
    Abort
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  SetOutPath "$PLUGINSDIR"
  File "${BSM_HOOK_DIR}\restore-protocols.ps1"
  ExecWait '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File "$PLUGINSDIR\restore-protocols.ps1" "${PRODUCTNAME}"' $0
  Delete "$PLUGINSDIR\restore-protocols.ps1"
  SetOutPath "$INSTDIR"
  ${If} $0 != 0
    MessageBox MB_OK|MB_ICONSTOP "Protocol registrations could not be restored. Uninstall was cancelled. See the backup under LocalAppData\${PRODUCTNAME}\protocol-backups."
    Abort
  ${EndIf}
!macroend
