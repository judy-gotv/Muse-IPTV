# Muse-IPTV 一键安装脚本（Windows）
# 作者: Passwd Word
# 用法（管理员 PowerShell）：
#   irm https://raw.githubusercontent.com/judy-gotv/Muse-IPTV/main/install.ps1 | iex
#   # 或：powershell -ExecutionPolicy Bypass -File install.ps1 -Version v0.1.0
# 参数：
#   -Version vX.Y.Z      指定版本（默认查最新 Release）
#   -InstallDir DIR      安装目录（默认 $env:LOCALAPPDATA\Muse-IPTV）
#   -Port N              对外端口（首次安装时用；已安装时=只改端口并自动重启，不下载）
#   -AdminUser NAME      管理员账号（只在首次安装时生效，默认 admin）
#   -AdminPassword PASS  管理员密码（只在首次安装时生效；不给则交互式问，留空随机生成）
#   -Reinstall           已是最新版也强制重新下载安装
#   -NoService           不装计划任务，只装二进制并打印手动启动命令
#
# 行为：
#   全新安装：交互式问端口 / 账号 / 密码（明文显示，核对后确认），再下载安装。
#   重跑脚本：已是最新版则直接提示（交互式可改端口）；有新版则在线升级
#     （换二进制、保留 .env 与数据、重启计划任务）。
#   改端口：install.ps1 -Port <新端口>，只改 .env 并自动重启任务生效。
# 对外只暴露网关一个端口（默认 :8081）：门户 /、后台 /admin、API /api/* 与
# /healthz 全走它；API :8080 与门户/后台 :18081/:18082 只绑本机。
param(
  [string]$Version = "",
  [string]$InstallDir = "$env:LOCALAPPDATA\Muse-IPTV",
  [string]$Port = "",
  [string]$AdminUser = "",
  [string]$AdminPassword = "",
  [switch]$Reinstall,
  [switch]$NoService
)
$ErrorActionPreference = "Stop"
# 计划任务以 SYSTEM 身份开机自启，注册需要管理员权限；用法本就要求管理员 PowerShell。
$__isAdmin = ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $__isAdmin) { throw "请用管理员身份运行 PowerShell 再执行本脚本（右键 PowerShell → 以管理员身份运行）。" }
$Repo = "judy-gotv/Muse-IPTV"
$TaskNames = @("Muse-IPTV API", "Muse-IPTV Worker", "Muse-IPTV Portal", "Muse-IPTV Admin", "Muse-IPTV Gateway")

function Get-EnvValue($File, $Key) {
  if (-not (Test-Path $File)) { return "" }
  $line = Select-String -Path $File -Pattern "^$Key=" | Select-Object -First 1
  if ($null -eq $line) { return "" }
  return $line.Line.Substring($Key.Length + 1)
}
function Set-EnvValue($File, $Key, $Value) {
  $lines = @()
  if (Test-Path $File) { $lines = Get-Content $File | Where-Object { $_ -notmatch "^$Key=" } }
  $lines += "$Key=$Value"
  $lines | Out-File -Encoding utf8 $File
}
function Test-ValidPort($P) {
  if ($P -notmatch '^[0-9]+$') { return $false }
  $n = [int]$P
  return ($n -ge 1 -and $n -le 65535)
}
function Read-WithDefault($Prompt, $Default) {
  # 非交互环境下 Read-Host 会抛错，直接取默认值。
  try {
    $v = Read-Host "$Prompt [$Default]"
    if ([string]::IsNullOrWhiteSpace($v)) { return $Default }
    return $v.Trim()
  } catch {
    return $Default
  }
}
function Read-Port($Default) {
  while ($true) {
    $v = Read-WithDefault "对外端口" $Default
    if (Test-ValidPort $v) { return $v }
    Write-Host "端口无效：请输入 1-65535 的数字。"
  }
}
function Show-Summary($Port) {
  $u = Get-EnvValue $EnvFile "IPTV_ADMIN_USER"
  $p = Get-EnvValue $EnvFile "IPTV_ADMIN_PASSWORD"
  Write-Host ""
  Write-Host "==================== Muse-IPTV ===================="
  Write-Host "端口：$Port"
  Write-Host "管理员账号：$u"
  Write-Host "管理员密码：$p"
  Write-Host "门户：http://127.0.0.1:$Port（需先在后台建 Token）"
  Write-Host "后台：http://127.0.0.1:$Port/admin"
  Write-Host "健康检查：curl http://127.0.0.1:$Port/healthz"
  Write-Host "配置文件：$EnvFile"
  Write-Host "==================================================="
  Write-Host ""
}
function Test-Health($Port) {
  try {
    $h = Invoke-RestMethod "http://127.0.0.1:$Port/healthz"
    Write-Host "健康检查通过：$($h.status)"
    return $true
  } catch {
    Write-Host "健康检查未通过（端口 $Port），请检查计划任务 Muse-IPTV Gateway"
    return $false
  }
}
function Restart-Tasks {
  foreach ($t in $TaskNames) {
    Stop-ScheduledTask -TaskName $t -ErrorAction SilentlyContinue | Out-Null
  }
  Start-Sleep -Seconds 2
  foreach ($t in $TaskNames) {
    Start-ScheduledTask -TaskName $t -ErrorAction SilentlyContinue | Out-Null
  }
  Start-Sleep -Seconds 2
}

$EnvFile = Join-Path $InstallDir ".env"
$VersionFile = Join-Path $InstallDir "version.txt"

# --- 已安装检测 ---
$installed = Test-Path $EnvFile
$installedVersion = ""
$currentPort = "8081"
if ($installed) {
  if (Test-Path $VersionFile) { $installedVersion = (Get-Content $VersionFile -TotalCount 1).Trim() }
  $gw = Get-EnvValue $EnvFile "GATEWAY_BIND_ADDR"
  if ($gw -match ':([0-9]+)$' -and (Test-ValidPort $Matches[1])) { $currentPort = $Matches[1] }
}

# 端口早校验：别等查完最新版才发现端口非法。
if ($Port -and -not (Test-ValidPort $Port)) { throw "端口无效：$Port（须为 1-65535）" }

# --- 只改端口（已安装 + -Port，不下载不升级） ---
if ($installed -and $Port -and -not $Version -and -not $Reinstall) {
  if ($Port -eq $currentPort) {
    Write-Host "端口已经是 $Port，无需修改。"
    exit 0
  }
  $gwTask = Get-ScheduledTask -TaskName "Muse-IPTV Gateway" -ErrorAction SilentlyContinue
  $apiTask = Get-ScheduledTask -TaskName "Muse-IPTV API" -ErrorAction SilentlyContinue
  if ($null -ne $apiTask -and $null -eq $gwTask) {
    Write-Host "当前安装是旧版本（没有网关任务），请先重跑脚本升级，再改端口。"
    exit 1
  }
  Set-EnvValue $EnvFile "GATEWAY_BIND_ADDR" "0.0.0.0:$Port"
  Write-Host "端口已改成 $Port。"
  if (-not $NoService) { Restart-Tasks; Test-Health $Port | Out-Null }
  else { Write-Host "已跳过服务操作，请手动重启任务生效。" }
  Show-Summary $Port
  exit 0
}

# --- 确定目标版本 ---
if (-not $Version) {
  Write-Host "正在查询最新版本…"
  $rel = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest"
  $Version = $rel.tag_name
}
if (-not $Version) { throw "无法获取最新版本，请用 -Version 指定" }

# --- 已是最新版 ---
if ($installed -and $Version -eq $installedVersion -and -not $Reinstall) {
  if ($Port) {
    if ($Port -ne $currentPort) {
      Set-EnvValue $EnvFile "GATEWAY_BIND_ADDR" "0.0.0.0:$Port"
      Write-Host "已是最新版本 $Version，端口已改成 $Port。"
      if (-not $NoService) { Restart-Tasks; Test-Health $Port | Out-Null }
      Show-Summary $Port
    } else {
      Write-Host "已是最新版本 $Version，端口已经是 $Port。"
    }
    exit 0
  }
  $newPort = Read-Port $currentPort
  if ($newPort -ne $currentPort) {
    Set-EnvValue $EnvFile "GATEWAY_BIND_ADDR" "0.0.0.0:$newPort"
    Write-Host "已是最新版本 $Version，端口已改成 $newPort。"
    if (-not $NoService) { Restart-Tasks; Test-Health $newPort | Out-Null }
    Show-Summary $newPort
    exit 0
  }
  Write-Host "已是最新版本 $Version，无需升级。如需改端口：install.ps1 -Port <新端口>"
  exit 0
}

# --- 确定本次安装的端口 / 账号 / 密码 ---
if ($installed) {
  if ($AdminUser -or $AdminPassword) {
    Write-Host "注意：已有管理员账号，-AdminUser / -AdminPassword 只在首次安装时生效；要换密码请在后台新建管理员账号。"
  }
  if ($Port) { $Port = $Port }
  else { $Port = Read-Port $currentPort }
  Write-Host "升级到 $Version（端口 $Port）"
} else {
  if ($Port) { $Port = $Port } else { $Port = Read-Port "8081" }
  if ($AdminUser) { $adminUserFinal = $AdminUser }
  else { $adminUserFinal = Read-WithDefault "管理员账号" "admin" }
  if ($AdminPassword) { $adminPassFinal = $AdminPassword }
  else {
    $adminPassFinal = Read-WithDefault "管理员密码（明文输入，留空随机生成）" ""
    if ([string]::IsNullOrWhiteSpace($adminPassFinal)) {
      $adminPassFinal = -join ((1..12) | ForEach-Object { "{0:x2}" -f (Get-Random -Maximum 256) })
    }
  }
  Write-Host ""
  Write-Host "---------- 安装信息确认（明文显示，请核对） ----------"
  Write-Host "端口：$Port"
  Write-Host "管理员账号：$adminUserFinal"
  Write-Host "管理员密码：$adminPassFinal"
  Write-Host "------------------------------------------------------"
  $yn = Read-WithDefault "确认无误，开始安装？[Y/n]" "Y"
  if ($yn -match '^[nN]') { Write-Host "已取消，请重新运行脚本。"; exit 0 }
  Write-Host "安装 Muse-IPTV $Version（windows/x86_64）"
}

$File = "muse-iptv-$Version-windows-x86_64.zip"
$Url = "https://github.com/$Repo/releases/download/$Version/$File"
$Tmp = Join-Path $env:TEMP "muse-iptv-install"
New-Item -ItemType Directory -Force -Path $Tmp | Out-Null
Invoke-WebRequest -Uri $Url -OutFile (Join-Path $Tmp $File)
Invoke-WebRequest -Uri "https://github.com/$Repo/releases/download/$Version/SHA256SUMS" -OutFile (Join-Path $Tmp "SHA256SUMS")
# 校验 SHA-256
$__shaMatch = Select-String -Path (Join-Path $Tmp "SHA256SUMS") -Pattern $File
if (-not $__shaMatch) { throw "SHA-256 校验失败：SHA256SUMS 里找不到 $File（该 Release 可能未附 SHA256SUMS，或文件名与 Release 资产不符）。" }
$expected = $__shaMatch.Line.Split()[0]
$actual = (Get-FileHash -Algorithm SHA256 (Join-Path $Tmp $File)).Hash.ToLower()
if ($expected -ne $actual) { throw "SHA-256 校验失败：期望 $expected，实际 $actual" }
Write-Host "SHA-256 校验通过"
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $InstallDir "data") | Out-Null
# 升级时先停任务，否则运行中的 exe 占着文件，解压覆盖会失败。
if ($installed) {
  foreach ($t in $TaskNames) {
    Stop-ScheduledTask -TaskName $t -ErrorAction SilentlyContinue | Out-Null
  }
  Start-Sleep -Seconds 2
}
Expand-Archive -Path (Join-Path $Tmp $File) -DestinationPath $InstallDir -Force
# zip 里有顶层目录 muse-iptv-<版本>-windows-x86_64/，把内容移到安装目录根下
# （旧脚本直接按 $InstallDir\iptv-api.exe 取，顶层目录在就一定取不到——从未跑通过。）
$PkgDir = Join-Path $InstallDir "muse-iptv-$Version-windows-x86_64"
if (Test-Path $PkgDir) {
  Get-ChildItem $PkgDir | Move-Item -Destination $InstallDir -Force
  Remove-Item $PkgDir -Recurse -Force
}
# 老 Release 包没有网关，直接报错，别装出一个缺网关的半套。
foreach ($b in @("iptv-api.exe", "iptv-worker.exe", "iptv-portal.exe", "iptv-admin.exe", "iptv-gateway.exe")) {
  if (-not (Test-Path (Join-Path $InstallDir $b))) {
    throw "错误：Release 包 $File 里没有 $b（该版本太旧，不含单端口网关），请用更新的版本。"
  }
}
# 配置（不存在才生成，密钥只打印一次）
if (-not (Test-Path $EnvFile)) {
  $play = -join ((1..32) | ForEach-Object { "{0:x2}" -f (Get-Random -Maximum 256) })
  $adminSecret = -join ((1..32) | ForEach-Object { "{0:x2}" -f (Get-Random -Maximum 256) })
  @"
DATABASE_URL=sqlite://$($InstallDir.Replace('\','/'))/data/iptv.db
PLAY_SECRET=$play
ADMIN_SECRET=$adminSecret
# 对外只暴露网关一个端口（默认 :8081）：门户 /、后台 /admin、
# API /api/* 与 /healthz 全走它；API :8080 与门户/后台 :18081/:18082 只绑本机。
PORTAL_BASE_URL=http://127.0.0.1:$Port
BIND_ADDR=127.0.0.1:8080
GATEWAY_BIND_ADDR=0.0.0.0:$Port
IPTV_PORTAL_ADDR=0.0.0.0:18081
IPTV_ADMIN_ADDR=0.0.0.0:18082
IPTV_PORTAL_BASE=http://127.0.0.1:18081
IPTV_ADMIN_BASE=http://127.0.0.1:18082
# B4：前端 hydrate 产物位置（portal/admin 由此找 site/pkg/*.js）。
LEPTOS_SITE_ROOT=$InstallDir\site
IPTV_ADMIN_USER=$adminUserFinal
IPTV_ADMIN_PASSWORD=$adminPassFinal
"@ | Out-File -Encoding utf8 $EnvFile
} else {
  # 升级：保留原 .env，只做单端口布局迁移 + 本次改的端口。
  Set-EnvValue $EnvFile "GATEWAY_BIND_ADDR" "0.0.0.0:$Port"
  $bind = Get-EnvValue $EnvFile "BIND_ADDR"
  if ($bind -like "0.0.0.0:*") { Set-EnvValue $EnvFile "BIND_ADDR" "127.0.0.1:8080" }
  $pb = Get-EnvValue $EnvFile "PORTAL_BASE_URL"
  if ($pb -like "http://127.0.0.1:808*") { Set-EnvValue $EnvFile "PORTAL_BASE_URL" "http://127.0.0.1:$Port" }
  if (-not (Get-EnvValue $EnvFile "IPTV_PORTAL_ADDR")) { Set-EnvValue $EnvFile "IPTV_PORTAL_ADDR" "0.0.0.0:18081" }
  if (-not (Get-EnvValue $EnvFile "IPTV_ADMIN_ADDR")) { Set-EnvValue $EnvFile "IPTV_ADMIN_ADDR" "0.0.0.0:18082" }
  if (-not (Get-EnvValue $EnvFile "IPTV_PORTAL_BASE")) { Set-EnvValue $EnvFile "IPTV_PORTAL_BASE" "http://127.0.0.1:18081" }
  if (-not (Get-EnvValue $EnvFile "IPTV_ADMIN_BASE")) { Set-EnvValue $EnvFile "IPTV_ADMIN_BASE" "http://127.0.0.1:18082" }
  # B4：升级补前端产物位置（旧 .env 没有这一行时才写）。
  if (-not (Get-EnvValue $EnvFile "LEPTOS_SITE_ROOT")) { Set-EnvValue $EnvFile "LEPTOS_SITE_ROOT" "$InstallDir\site" }
  Write-Host "配置文件已存在：$EnvFile（保留原密钥与管理员密码，已按单端口布局迁移）"
}
$Version | Out-File -Encoding utf8 $VersionFile
# PATH（用户级）
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($userPath -notlike "*$InstallDir*") {
  [Environment]::SetEnvironmentVariable("Path", "$userPath;$InstallDir", "User")
  Write-Host "已把 $InstallDir 加入用户 PATH（新开终端生效）"
}
if (-not $NoService) {
  # 用计划任务实现开机自启（登录时运行；网关对外只暴露一个端口）
  $api = Join-Path $InstallDir "iptv-api.exe"
  $worker = Join-Path $InstallDir "iptv-worker.exe"
  $portal = Join-Path $InstallDir "iptv-portal.exe"
  $admin = Join-Path $InstallDir "iptv-admin.exe"
  $gateway = Join-Path $InstallDir "iptv-gateway.exe"
  $actionApi = New-ScheduledTaskAction -Execute $api -WorkingDirectory $InstallDir
  $actionWorker = New-ScheduledTaskAction -Execute $worker -WorkingDirectory $InstallDir
  $actionPortal = New-ScheduledTaskAction -Execute $portal -WorkingDirectory $InstallDir
  $actionAdmin = New-ScheduledTaskAction -Execute $admin -WorkingDirectory $InstallDir
  $actionGateway = New-ScheduledTaskAction -Execute $gateway -WorkingDirectory $InstallDir
  # 真正的开机自启：AtStartup + SYSTEM 账号（无须用户登录；此前 AtLogOn +
  # Interactive 只在用户登录时才跑，不算开机自启）。服务以 $InstallDir 为
  # 工作目录读 .env，不依赖用户环境变量。
  $trigger = New-ScheduledTaskTrigger -AtStartup
  $principal = New-ScheduledTaskPrincipal -UserId "SYSTEM" -LogonType ServiceAccount -RunLevel Highest
  Register-ScheduledTask -TaskName "Muse-IPTV API" -Action $actionApi -Trigger $trigger -Principal $principal -Force | Out-Null
  Register-ScheduledTask -TaskName "Muse-IPTV Worker" -Action $actionWorker -Trigger $trigger -Principal $principal -Force | Out-Null
  Register-ScheduledTask -TaskName "Muse-IPTV Portal" -Action $actionPortal -Trigger $trigger -Principal $principal -Force | Out-Null
  Register-ScheduledTask -TaskName "Muse-IPTV Admin" -Action $actionAdmin -Trigger $trigger -Principal $principal -Force | Out-Null
  Register-ScheduledTask -TaskName "Muse-IPTV Gateway" -Action $actionGateway -Trigger $trigger -Principal $principal -Force | Out-Null
  # 全新安装直接启动；升级时任务刚被停过，这里相当于重启新二进制。
  foreach ($t in $TaskNames) {
    Start-ScheduledTask -TaskName $t -ErrorAction SilentlyContinue | Out-Null
  }
  Start-Sleep -Seconds 2
  Test-Health $Port | Out-Null
} else {
  Write-Host "跳过服务安装。手动启动：$InstallDir\iptv-api.exe"
}
Show-Summary $Port
if ($installed) { Write-Host "升级完成：Muse-IPTV $Version" } else { Write-Host "安装完成：Muse-IPTV $Version" }
# 清理下载临时目录（此前固定目录从不清理）。
Remove-Item $Tmp -Recurse -Force -ErrorAction SilentlyContinue
