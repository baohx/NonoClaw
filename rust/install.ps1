<# NonoClaw Windows installer. Run from the repo root:
   powershell -ExecutionPolicy Bypass -File install.ps1
   # Builds frontend + Rust release, installs binary + frontend dist centrally
   # so `nonoclaw --serve-http 127.0.0.1:8765` works from any directory.
#>

$ErrorActionPreference = "Stop"

$SCRIPT_DIR = Split-Path -Parent $MyInvocation.MyCommand.Path
$PROJECT_DIR = Split-Path -Parent $SCRIPT_DIR
$RUST_DIR = $SCRIPT_DIR
$FRONTEND_DIR = Join-Path $PROJECT_DIR "frontend"
$BIN_SRC   = Join-Path $RUST_DIR "target\release\nonoclaw.exe"

# Binary destination — prefer %LOCALAPPDATA%\nonoclaw\bin (no admin needed),
# add it to PATH if not already there.
$BIN_DIR = if ($env:NONOCLAW_BIN_DIR) { $env:NONOCLAW_BIN_DIR }
           else { Join-Path $env:LOCALAPPDATA "nonoclaw\bin" }
$BIN_DST = Join-Path $BIN_DIR "nonoclaw.exe"

# Frontend destination (fixed location so --serve-http finds it anywhere).
$DATA_DIR = if ($env:NONOCLAW_DATA_DIR) { $env:NONOCLAW_DATA_DIR }
            else { Join-Path $env:LOCALAPPDATA "nonoclaw" }
$FRONTEND_DST = Join-Path $DATA_DIR "frontend"

Write-Host "=== NonoClaw Windows 安装 ===" -ForegroundColor Cyan
Write-Host "项目目录:   $PROJECT_DIR"
Write-Host "Rust 源码:  $RUST_DIR"
Write-Host "前端源码:   $FRONTEND_DIR"
Write-Host "二进制目标: $BIN_DST"
Write-Host "前端目标:   $FRONTEND_DST\dist"
Write-Host ""

# --- 1. 构建前端 (Vite -> dist/) ---
if (Test-Path "$FRONTEND_DIR\package.json") {
    Write-Host "[1/4] 构建前端 (npm install + build)..." -ForegroundColor Green
    Push-Location $FRONTEND_DIR
    if (-not (Test-Path "node_modules")) { npm install }
    npm run build
    if ($LASTEXITCODE -ne 0) { throw "前端构建失败" }
    Pop-Location
    Write-Host "   -> $FRONTEND_DIR\dist"
    Write-Host ""
} else {
    Write-Host "[1/4] 未找到前端 package.json，跳过前端构建" -ForegroundColor Yellow
    Write-Host ""
}

# --- 2. 构建 release 二进制 ---
Write-Host "[2/4] 构建 release 二进制..." -ForegroundColor Green
Push-Location $RUST_DIR
cargo build --release
if ($LASTEXITCODE -ne 0) { throw "Rust 构建失败" }
Pop-Location
Write-Host "   -> $BIN_SRC"
Write-Host ""

# --- 3. 安装二进制到 PATH ---
Write-Host "[3/4] 安装 nonoclaw.exe -> $BIN_DST" -ForegroundColor Green
New-Item -ItemType Directory -Force -Path $BIN_DIR | Out-Null
Copy-Item -Force $BIN_SRC $BIN_DST
# Register in PATH for the current user (persistent).
$userPath = [Environment]::GetEnvironmentVariable("PATH", "User")
if ($userPath -notlike "*$BIN_DIR*") {
    Write-Host "   正在将 $BIN_DIR 添加到用户 PATH..."
    [Environment]::SetEnvironmentVariable("PATH", "$userPath;$BIN_DIR", "User")
    # Also set for the current session.
    $env:PATH = "$env:PATH;$BIN_DIR"
}
Write-Host ""

# --- 4. 安装前端 dist 到固定路径 ---
Write-Host "[4/4] 安装前端 -> $FRONTEND_DST\dist" -ForegroundColor Green
New-Item -ItemType Directory -Force -Path $FRONTEND_DST | Out-Null
if (Test-Path "$FRONTEND_DST\dist") { Remove-Item -Recurse -Force "$FRONTEND_DST\dist" }
if (Test-Path "$FRONTEND_DIR\dist") {
    Copy-Item -Recurse "$FRONTEND_DIR\dist" "$FRONTEND_DST\dist"
    Write-Host "   -> $FRONTEND_DST\dist\index.html"
} else {
    Write-Host "   WARNING: 未找到 frontend/dist，前端未安装（--serve-http 将仅提供 WebSocket）" -ForegroundColor Yellow
}
Write-Host ""

# --- 验证 ---
Write-Host "=== 验证 ===" -ForegroundColor Cyan
& $BIN_DST --version
Write-Host ""
Write-Host "安装完成！在任意目录执行即可启动 Web UI：" -ForegroundColor Green
Write-Host ""
Write-Host "    nonoclaw --serve-http 127.0.0.1:8765"
Write-Host ""
Write-Host "配置文件: $env:USERPROFILE\.nonoclaw\settings.json"
Write-Host ""
Write-Host "提示：如果 PowerShell 找不到 nonoclaw，请重启终端或运行："
Write-Host '    $env:PATH = "$env:PATH;' + $BIN_DIR + '"'
