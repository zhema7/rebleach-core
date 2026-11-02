; Rebleach Agent 安装包钩子
; 目标：单安装包完成全部部署，无需用户再手动执行任何 PowerShell
;   1. 把用户级旧身份密钥迁移到机器级（避免 SYSTEM 下重新生成 → 主控端 pubkey_mismatch）
;   2. 注册并启动 RebleachAgent 服务（LocalSystem / auto），实现开机未登录即可上线
;   3. 卸载时先停止并移除服务，避免文件被占用残留

!macro NSIS_HOOK_PREINSTALL
  ; 覆盖安装：先停服务、结束托盘/服务进程，否则 exe 被占用导致替换失败
  IfFileExists "$INSTDIR\rebleach.exe" 0 rebleach_pre_no_old_service
    ExecWait '"$INSTDIR\rebleach.exe" --uninstall-service'
  rebleach_pre_no_old_service:
  nsExec::Exec 'taskkill /f /im rebleach-ui.exe'
  nsExec::Exec 'taskkill /f /im rebleach.exe'
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; 用户令牌托盘（UIAccess）需要写 %ProgramData%\Rebleach 下的日志/PID 文件，
  ; 该目录由 SYSTEM（服务/安装器）创建，默认 Users 不可写 → 授权 Users 组修改
  DetailPrint "授权数据目录（用户态托盘需写日志/PID）..."
  CreateDirectory "$COMMONPROGRAMDATA\Rebleach"
  nsExec::Exec 'icacls "$COMMONPROGRAMDATA\Rebleach" /grant *S-1-5-32-545:(OI)(CI)(M) /T'

  ; UIAccess 依赖代码签名证书链：自签根证书必须进本机根存储，否则
  ; ShellExecute 派生 rebleach-ui.exe 时签名不受信任，UIAccess 被拒 → 托盘回退 SYSTEM
  DetailPrint "安装 UIAccess 代码签名证书..."
  nsExec::ExecToLog '"$SYSDIR\certutil.exe" -addstore -f Root "$INSTDIR\resources\rebleach-uiaaccess.cer"'
  nsExec::ExecToLog '"$SYSDIR\certutil.exe" -addstore -f TrustedPublisher "$INSTDIR\resources\rebleach-uiaaccess.cer"'

  DetailPrint "迁移被控端身份密钥..."
  ExecWait '"$INSTDIR\rebleach.exe" --migrate-identity'

  DetailPrint "注册 RebleachAgent 服务..."
  ExecWait '"$INSTDIR\rebleach.exe" --install-service'

  ; ====== 虚拟显示器驱动自动安装 ======
  ; 装完被控端后，立刻用随包的 usbmmidd 驱动创建 ROOT\DISPLAY 设备节点 + 插第一块虚拟屏。
  ; 幂等：脚本内部用 Get-PnpDevice 检查 ROOT\DISPLAY 是否已存在，存在就跳过。
  ; 注意：NSIS 是 32 位进程，$SYSDIR 在 64 位系统上会被重定向到 SysWOW64，
  ; 拿到的是 32 位 PowerShell。这里必须显式走 Sysnative 拿 64 位解释器。
  DetailPrint "安装 usbmmidd 虚拟显示器驱动..."
  CreateDirectory "$INSTDIR\resources\virtual-display"
  IfFileExists "$WINDIR\Sysnative\WindowsPowerShell\v1.0\powershell.exe" 0 rebleach_ps32
    ExecWait '"$WINDIR\Sysnative\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File "$INSTDIR\resources\virtual-display\Install-VirtualDisplay.ps1" -DriverDir "$INSTDIR\resources\virtual-display"'
    Goto rebleach_ps_done
  rebleach_ps32:
    ExecWait '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File "$INSTDIR\resources\virtual-display\Install-VirtualDisplay.ps1" -DriverDir "$INSTDIR\resources\virtual-display"'
  rebleach_ps_done:

  ; 确保升级后新 exe 生效：重启服务，避免旧进程继续跑老代码
  DetailPrint "重启 RebleachAgent 服务..."
  nsExec::Exec 'sc stop RebleachAgent'
  Sleep 1500
  nsExec::Exec 'sc start RebleachAgent'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  DetailPrint "停止并移除 RebleachAgent 服务..."
  ExecWait '"$INSTDIR\rebleach.exe" --uninstall-service'
  ; 卸载时移除自签根证书，避免在系统根存储中长期残留
  nsExec::Exec '"$SYSDIR\certutil.exe" -delstore Root "Rebleach UIAccess"'
  nsExec::Exec '"$SYSDIR\certutil.exe" -delstore TrustedPublisher "Rebleach UIAccess"'
!macroend
