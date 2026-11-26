# Rebleach Agent 一键安装脚本（由服务端动态生成：__BASE__ / __TOKEN__ 已在响应中替换）
#
# 用法（管理员 PowerShell）：
#   irm "__BASE__/bootstrap/install.ps1?token=__TOKEN__" | iex
#
# 步骤：校验管理员 → 领取设备 ID → 下载安装包 → 静默安装 → 写配置 → 重启服务
param(
    [string]$Server = "__BASE__",
    [string]$Token  = "__TOKEN__"
)

$ErrorActionPreference = "Stop"
$ServiceName = "RebleachAgent"

function Test-IsAdmin {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    $p = New-Object Security.Principal.WindowsPrincipal($id)
    return $p.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

if (-not (Test-IsAdmin)) {
    Write-Host "请以管理员身份运行 PowerShell 后再执行本脚本。" -ForegroundColor Red
    exit 1
}

Write-Host "==> 1/5 校验安装令牌并查询安装包" -ForegroundColor Cyan
# 注意：必须在 claim 之前查询，安装令牌是单次使用的
try {
    $pkg = Invoke-RestMethod -Method Get -Uri "$Server/bootstrap/agent?token=$Token" -TimeoutSec 20
} catch {
    Write-Host "获取安装包信息失败：$($_.Exception.Message)" -ForegroundColor Red
    Write-Host "请确认令牌未过期，且服务器 packages 目录下已放置 Rebleach Agent 安装包。"
    exit 3
}

Write-Host "==> 2/5 领取设备 ID" -ForegroundColor Cyan
$claimBody = @{ token = $Token; hostname = $env:COMPUTERNAME } | ConvertTo-Json -Compress
try {
    $claim = Invoke-RestMethod -Method Post `
        -Uri "$Server/bootstrap/claim" `
        -ContentType "application/json; charset=utf-8" `
        -Body ([System.Text.Encoding]::UTF8.GetBytes($claimBody)) `
        -TimeoutSec 20
} catch {
    Write-Host "安装令牌校验失败：$($_.Exception.Message)" -ForegroundColor Red
    Write-Host "请联系管理员重新生成一次性安装令牌（默认 15 分钟有效）。"
    exit 2
}

$DeviceId = $claim.device_id
$WsServer = $claim.server
Write-Host "    设备 ID: $DeviceId"
Write-Host "    信令地址: $WsServer"

Write-Host "==> 3/5 下载 Agent 安装包" -ForegroundColor Cyan
$installer = Join-Path $env:TEMP $pkg.name
Write-Host "    $($pkg.name) ($([math]::Round($pkg.size / 1MB, 1)) MB)"
Invoke-WebRequest -Uri $pkg.url -OutFile $installer -UseBasicParsing -TimeoutSec 300

Write-Host "==> 4/5 静默安装（约需 1 分钟，将自动注册并启动服务）" -ForegroundColor Cyan
$proc = Start-Process -FilePath $installer -ArgumentList "/S" -Wait -PassThru
if ($proc.ExitCode -ne 0) {
    Write-Host "安装程序返回错误码 $($proc.ExitCode)，请检查日志后重试。" -ForegroundColor Red
    exit 4
}
Remove-Item $installer -Force -ErrorAction SilentlyContinue

Write-Host "==> 5/5 写入被控端配置并重启服务" -ForegroundColor Cyan
# 注意：必须在安装之后写配置，再用重启服务让 Agent 读到新配置
$dataDir = Join-Path $env:ProgramData "Rebleach"
New-Item -ItemType Directory -Force -Path $dataDir | Out-Null
$config = @{
    enabled   = $true
    server    = $WsServer
    id        = $DeviceId
    autostart = $true
} | ConvertTo-Json
[System.IO.File]::WriteAllText(
    (Join-Path $dataDir "agent.json"),
    $config,
    (New-Object System.Text.UTF8Encoding($false))
)

Write-Host "==> 5/5 重启服务使配置生效" -ForegroundColor Cyan
& sc.exe stop $ServiceName | Out-Null
Start-Sleep -Seconds 2
& sc.exe start $ServiceName | Out-Null
Start-Sleep -Seconds 2
$svc = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if ($null -ne $svc -and $svc.Status -eq "Running") {
    Write-Host "    $ServiceName 正在运行" -ForegroundColor Green
} else {
    Write-Host "    警告：服务未处于 Running 状态，请打开“服务”检查 $ServiceName" -ForegroundColor Yellow
}

Write-Host ""
Write-Host "安装完成。" -ForegroundColor Green
Write-Host "  设备 ID : $DeviceId"
Write-Host "  服务器  : $WsServer"
Write-Host "下一步："
Write-Host "  1. 在管理端为该设备与主控端创建授权（grants）"
Write-Host "  2. 主控端执行 rebleach-client setup --device <主控端ID> 生成 SSH 配置"
Write-Host "  3. ssh rbl-$DeviceId 即可接入"
