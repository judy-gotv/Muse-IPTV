# Muse-IPTV 一键安装脚本（Windows）
# 作者: Passwd Word
# 用法（管理员 PowerShell）：
#   irm https://raw.githubusercontent.com/judy-gotv/Muse-IPTV/main/install.ps1 | iex
#   # 或：powershell -ExecutionPolicy Bypass -File install.ps1 -Version v0.1.0
param(
  [string]$Version = "",
  [string]$InstallDir = "$env:LOCALAPPDATA\Muse-IPTV",
  [switch]$NoService
)
$ErrorActionPreference = "Stop"
$Repo = "judy-gotv/Muse-IPTV"

if (-not $Version) {
  Write-Host "正在查询最新版本…"
  $rel = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest"
  $Version = $rel.tag_name
}
Write-Host "安装 Muse-IPTV $Version（windows/x86_64）"
$File = "muse-iptv-$Version-windows-x86_64.zip"
$Url = "https://github.com/$Repo/releases/download/$Version/$File"
$Tmp = Join-Path $env:TEMP "muse-iptv-install"
New-Item -ItemType Directory -Force -Path $Tmp | Out-Null
Invoke-WebRequest -Uri $Url -OutFile (Join-Path $Tmp $File)
Invoke-WebRequest -Uri "https://github.com/$Repo/releases/download/$Version/SHA256SUMS" -OutFile (Join-Path $Tmp "SHA256SUMS")
# 校验 SHA-256
$expected = (Select-String -Path (Join-Path $Tmp "SHA256SUMS") -Pattern $File).Line.Split()[0]
$actual = (Get-FileHash -Algorithm SHA256 (Join-Path $Tmp $File)).Hash.ToLower()
if ($expected -ne $actual) { throw "SHA-256 校验失败：期望 $expected，实际 $actual" }
Write-Host "SHA-256 校验通过"
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
Expand-Archive -Path (Join-Path $Tmp $File) -DestinationPath $InstallDir -Force
# 配置（不存在才生成，密钥只打印一次）
$EnvFile = Join-Path $InstallDir ".env"
if (-not (Test-Path $EnvFile)) {
  $play = -join ((1..32) | ForEach-Object { "{0:x2}" -f (Get-Random -Maximum 256) })
  $adminSecret = -join ((1..32) | ForEach-Object { "{0:x2}" -f (Get-Random -Maximum 256) })
  $adminPass = -join ((1..12) | ForEach-Object { "{0:x2}" -f (Get-Random -Maximum 256) })
  @"
DATABASE_URL=sqlite://$InstallDir/data/iptv.db
PLAY_SECRET=$play
ADMIN_SECRET=$adminSecret
PORTAL_BASE_URL=http://127.0.0.1:8081
BIND_ADDR=0.0.0.0:8080
IPTV_ADMIN_USER=admin
IPTV_ADMIN_PASSWORD=$adminPass
"@ | Out-File -Encoding utf8 $EnvFile
  Write-Host ""
  Write-Host "==================== 首次安装信息（只显示一次） ===================="
  Write-Host "管理员账号：admin"
  Write-Host "管理员密码：$adminPass"
  Write-Host "配置文件：$EnvFile"
  Write-Host "=================================================================="
} else {
  Write-Host "配置文件已存在：$EnvFile（保留原密钥与管理员密码）"
}
# PATH（用户级）
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($userPath -notlike "*$InstallDir*") {
  [Environment]::SetEnvironmentVariable("Path", "$userPath;$InstallDir", "User")
  Write-Host "已把 $InstallDir 加入用户 PATH（新开终端生效）"
}
if (-not $NoService) {
  # 用计划任务实现开机自启（登录时运行 api；worker 同理）
  $api = Join-Path $InstallDir "iptv-api.exe"
  $worker = Join-Path $InstallDir "iptv-worker.exe"
  $actionApi = New-ScheduledTaskAction -Execute $api -WorkingDirectory $InstallDir
  $actionWorker = New-ScheduledTaskAction -Execute $worker -WorkingDirectory $InstallDir
  $trigger = New-ScheduledTaskTrigger -AtLogOn
  $principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Highest
  Register-ScheduledTask -TaskName "Muse-IPTV API" -Action $actionApi -Trigger $trigger -Principal $principal -Force | Out-Null
  Register-ScheduledTask -TaskName "Muse-IPTV Worker" -Action $actionWorker -Trigger $trigger -Principal $principal -Force | Out-Null
  Start-ScheduledTask -TaskName "Muse-IPTV API"
  Start-ScheduledTask -TaskName "Muse-IPTV Worker"
  Start-Sleep -Seconds 2
  try {
    $h = Invoke-RestMethod "http://127.0.0.1:8080/healthz"
    Write-Host "服务已启动，健康检查通过：$($h.status)"
  } catch {
    Write-Host "服务已安装但健康检查未通过，请检查计划任务 Muse-IPTV API / Worker"
  }
} else {
  Write-Host "跳过服务安装。手动启动：$InstallDir\iptv-api.exe"
}
Write-Host "安装完成：Muse-IPTV $Version"
Write-Host "门户：http://127.0.0.1:8081  后台：http://127.0.0.1:8082/admin"
