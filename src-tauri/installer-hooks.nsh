!macro NSIS_HOOK_PREINSTALL
  IfFileExists "$INSTDIR\duckling-proxy.exe" 0 duckling_proxy_preinstall_done

  DetailPrint "Stopping Duckling proxy workers before replacing application files"
  nsExec::ExecToStack '"$SYSDIR\taskkill.exe" /F /T /IM "duckling-proxy.exe"'
  Pop $0
  Pop $1
  Sleep 1000

  ; Removing the old sidecar first prevents NSIS from retaining a same-version
  ; or previously locked executable while updating the main application.
  Delete "$INSTDIR\duckling-proxy.exe"

  duckling_proxy_preinstall_done:
!macroend
