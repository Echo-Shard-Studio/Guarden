; Guarden hooks for Tauri's NSIS installer (bundle.windows.nsis.installerHooks).
; The bundle installs per machine (Program Files): only admins can replace guarden.exe, which the app runs elevated.

!macro NSIS_HOOK_POSTINSTALL
  ; One-time admin setup: let Guarden's sandboxes list C:\ and C:\Users. Idempotent, reruns on update.
  nsExec::Exec '"$INSTDIR\guarden.exe" setup'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; Hand Modrinth back its own launch, unless this is an update replacing the files.
  ${If} $UpdateMode <> 1
    nsExec::Exec '"$INSTDIR\guarden.exe" restore modrinth'
  ${EndIf}
!macroend
