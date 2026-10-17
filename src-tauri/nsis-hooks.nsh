!macro NSIS_HOOK_POSTUNINSTALL
  ReadRegStr $0 HKCU "Software\Classes\magnet" "DDD"
  StrCmp $0 "1" 0 +2
    DeleteRegKey HKCU "Software\Classes\magnet"
  ReadRegStr $0 HKCU "Software\Classes\.torrent" ""
  StrCmp $0 "DDD.torrent" 0 +2
    DeleteRegKey HKCU "Software\Classes\.torrent"
  DeleteRegKey HKCU "Software\Classes\DDD.torrent"
!macroend
