; ReclaimArc NSIS Custom Hooks for Windows Explorer Integration
; Registers and unregisters SystemFileAssociations in HKCU during install/uninstall.

!macro NSIS_HOOK_POSTINSTALL
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.zip\shell\ReclaimArc" "" "Analyze & Extract with ReclaimArc"
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.zip\shell\ReclaimArc" "Icon" "$INSTDIR\ReclaimArc.exe"
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.zip\shell\ReclaimArc\command" "" '"$INSTDIR\ReclaimArc.exe" "%1"'

  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.rar\shell\ReclaimArc" "" "Analyze & Extract with ReclaimArc"
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.rar\shell\ReclaimArc" "Icon" "$INSTDIR\ReclaimArc.exe"
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.rar\shell\ReclaimArc\command" "" '"$INSTDIR\ReclaimArc.exe" "%1"'

  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.7z\shell\ReclaimArc" "" "Analyze & Extract with ReclaimArc"
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.7z\shell\ReclaimArc" "Icon" "$INSTDIR\ReclaimArc.exe"
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.7z\shell\ReclaimArc\command" "" '"$INSTDIR\ReclaimArc.exe" "%1"'

  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.tar\shell\ReclaimArc" "" "Analyze & Extract with ReclaimArc"
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.tar\shell\ReclaimArc" "Icon" "$INSTDIR\ReclaimArc.exe"
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.tar\shell\ReclaimArc\command" "" '"$INSTDIR\ReclaimArc.exe" "%1"'

  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.gz\shell\ReclaimArc" "" "Analyze & Extract with ReclaimArc"
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.gz\shell\ReclaimArc" "Icon" "$INSTDIR\ReclaimArc.exe"
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.gz\shell\ReclaimArc\command" "" '"$INSTDIR\ReclaimArc.exe" "%1"'

  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.tgz\shell\ReclaimArc" "" "Analyze & Extract with ReclaimArc"
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.tgz\shell\ReclaimArc" "Icon" "$INSTDIR\ReclaimArc.exe"
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.tgz\shell\ReclaimArc\command" "" '"$INSTDIR\ReclaimArc.exe" "%1"'

  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.bz2\shell\ReclaimArc" "" "Analyze & Extract with ReclaimArc"
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.bz2\shell\ReclaimArc" "Icon" "$INSTDIR\ReclaimArc.exe"
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.bz2\shell\ReclaimArc\command" "" '"$INSTDIR\ReclaimArc.exe" "%1"'

  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.tbz2\shell\ReclaimArc" "" "Analyze & Extract with ReclaimArc"
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.tbz2\shell\ReclaimArc" "Icon" "$INSTDIR\ReclaimArc.exe"
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.tbz2\shell\ReclaimArc\command" "" '"$INSTDIR\ReclaimArc.exe" "%1"'

  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.xz\shell\ReclaimArc" "" "Analyze & Extract with ReclaimArc"
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.xz\shell\ReclaimArc" "Icon" "$INSTDIR\ReclaimArc.exe"
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.xz\shell\ReclaimArc\command" "" '"$INSTDIR\ReclaimArc.exe" "%1"'

  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.txz\shell\ReclaimArc" "" "Analyze & Extract with ReclaimArc"
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.txz\shell\ReclaimArc" "Icon" "$INSTDIR\ReclaimArc.exe"
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.txz\shell\ReclaimArc\command" "" '"$INSTDIR\ReclaimArc.exe" "%1"'

  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.zst\shell\ReclaimArc" "" "Analyze & Extract with ReclaimArc"
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.zst\shell\ReclaimArc" "Icon" "$INSTDIR\ReclaimArc.exe"
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.zst\shell\ReclaimArc\command" "" '"$INSTDIR\ReclaimArc.exe" "%1"'

  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.cbr\shell\ReclaimArc" "" "Analyze & Extract with ReclaimArc"
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.cbr\shell\ReclaimArc" "Icon" "$INSTDIR\ReclaimArc.exe"
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.cbr\shell\ReclaimArc\command" "" '"$INSTDIR\ReclaimArc.exe" "%1"'

  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.cbz\shell\ReclaimArc" "" "Analyze & Extract with ReclaimArc"
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.cbz\shell\ReclaimArc" "Icon" "$INSTDIR\ReclaimArc.exe"
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\.cbz\shell\ReclaimArc\command" "" '"$INSTDIR\ReclaimArc.exe" "%1"'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  DeleteRegKey HKCU "Software\Classes\SystemFileAssociations\.zip\shell\ReclaimArc"
  DeleteRegKey HKCU "Software\Classes\SystemFileAssociations\.rar\shell\ReclaimArc"
  DeleteRegKey HKCU "Software\Classes\SystemFileAssociations\.7z\shell\ReclaimArc"
  DeleteRegKey HKCU "Software\Classes\SystemFileAssociations\.tar\shell\ReclaimArc"
  DeleteRegKey HKCU "Software\Classes\SystemFileAssociations\.gz\shell\ReclaimArc"
  DeleteRegKey HKCU "Software\Classes\SystemFileAssociations\.tgz\shell\ReclaimArc"
  DeleteRegKey HKCU "Software\Classes\SystemFileAssociations\.bz2\shell\ReclaimArc"
  DeleteRegKey HKCU "Software\Classes\SystemFileAssociations\.tbz2\shell\ReclaimArc"
  DeleteRegKey HKCU "Software\Classes\SystemFileAssociations\.xz\shell\ReclaimArc"
  DeleteRegKey HKCU "Software\Classes\SystemFileAssociations\.txz\shell\ReclaimArc"
  DeleteRegKey HKCU "Software\Classes\SystemFileAssociations\.zst\shell\ReclaimArc"
  DeleteRegKey HKCU "Software\Classes\SystemFileAssociations\.cbr\shell\ReclaimArc"
  DeleteRegKey HKCU "Software\Classes\SystemFileAssociations\.cbz\shell\ReclaimArc"
!macroend
