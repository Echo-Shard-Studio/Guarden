; Guarden hooks for Tauri's NSIS installer (bundle.windows.nsis.installerHooks).
; The bundle installs per machine (Program Files): only admins can replace guarden.exe, which the app runs elevated.

!macro NSIS_HOOK_POSTINSTALL
  ; One-time admin setup: let Guarden's sandboxes list C:\ and C:\Users. Idempotent, reruns on update.
  nsExec::Exec '"$INSTDIR\guarden.exe" setup'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; Hand each launcher back its own launch, unless this is an update replacing the files.
  ; Fails harmlessly for a launcher that isn't installed.
  ${If} $UpdateMode <> 1
    nsExec::Exec '"$INSTDIR\guarden.exe" restore modrinth'
    nsExec::Exec '"$INSTDIR\guarden.exe" restore prism'
    nsExec::Exec '"$INSTDIR\guarden.exe" restore curseforge'
    nsExec::Exec '"$INSTDIR\guarden.exe" restore official'
  ${EndIf}
!macroend
