[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $DistDirectory,
    [string] $WorkDirectory = (Join-Path $env:RUNNER_TEMP "bakbeat-libwdi"),
    [string] $LibwdiRevision = "30df0c0e051b0132c4b9ebed8c054bc8eb3aaaec",
    [string] $WdkDirectory = "C:/Program Files (x86)/Windows Kits/8.0",
    [string] $PlatformToolset = "v143"
)

$ErrorActionPreference = "Stop"
$repositoryRoot = Resolve-Path (Join-Path $PSScriptRoot "..\..")
$helperSource = Join-Path $repositoryRoot "tools\netmd-driver-installer\bakbeat-netmd-driver.c"
$libwdiRoot = Join-Path $WorkDirectory "libwdi"

if (Test-Path -LiteralPath $WorkDirectory) {
    Remove-Item -LiteralPath $WorkDirectory -Recurse -Force
}
New-Item -ItemType Directory -Force $WorkDirectory, $DistDirectory | Out-Null

git clone --quiet https://github.com/pbatard/libwdi.git $libwdiRoot
git -C $libwdiRoot checkout --quiet $LibwdiRevision
if ((git -C $libwdiRoot rev-parse HEAD).Trim() -ne $LibwdiRevision) {
    throw "libwdi did not resolve to the approved revision."
}

Copy-Item -LiteralPath $helperSource -Destination (Join-Path $libwdiRoot "examples\wdi-simple.c") -Force
$configPath = Join-Path $libwdiRoot "msvc\config.h"
$config = Get-Content -LiteralPath $configPath -Raw
$config = $config.Replace('#define WDK_DIR "C:/Program Files (x86)/Windows Kits/8.0"',
    ('#define WDK_DIR "' + $WdkDirectory.Replace('\', '/') + '"'))
$config = $config.Replace('#define LIBUSB0_DIR "D:/libusb-win32"',
    '/* BakBeat WinUSB-only build: LIBUSB0_DIR intentionally disabled. */')
$config = $config.Replace('#define LIBUSBK_DIR "D:/libusbK/bin"',
    '/* BakBeat WinUSB-only build: LIBUSBK_DIR intentionally disabled. */')
Set-Content -LiteralPath $configPath -Value $config -NoNewline

# The Windows package is currently x86/x64 only. libwdi's static-library project
# otherwise builds its ARM64 installer helper even for a Win32 target, which
# requires a separate ARM64 C++ toolset that the helper does not ship or use.
$libwdiProjectPath = Join-Path $libwdiRoot "libwdi\.msvc\libwdi_static.vcxproj"
$libwdiProject = Get-Content -LiteralPath $libwdiProjectPath -Raw
$libwdiProject = $libwdiProject -replace `
    '(?s)\s*<ProjectReference Include="installer_arm64\.vcxproj">.*?</ProjectReference>', ''
Set-Content -LiteralPath $libwdiProjectPath -Value $libwdiProject -NoNewline
$embedderFilesPath = Join-Path $libwdiRoot "libwdi\embedder_files.h"
$embedderFiles = Get-Content -LiteralPath $embedderFilesPath -Raw
$embedderFiles = $embedderFiles -replace `
    '(?ms)^#if defined\(OPT_ARM\)\r?\n.*?^#endif\r?\n', ''
Set-Content -LiteralPath $embedderFilesPath -Value $embedderFiles -NoNewline

$solutionPath = Join-Path $libwdiRoot "libwdi.sln"
$solution = Get-Content -LiteralPath $solutionPath -Raw
$solution = $solution -replace `
    '(?ms)^Project\("\{8BC9CEB8-8B4A-11D0-8D11-00A0C91BC942\}"\) = "installer_arm64".*?^EndProject\r?\n', ''
$solution = $solution -replace `
    '(?m)^\s*\{6AC16F78-F266-4AE0-BD63-550A55F54C15\}.*\r?\n', ''
$solution = $solution -replace `
    '(?ms)^Project\("\{8BC9CEB8-8B4A-11D0-8D11-00A0C91BC942\}"\) = "zadig".*?^EndProject\r?\n', ''
$solution = $solution -replace `
    '(?m)^\s*\{F7F7842F-2912-454E-ADF5-0B22987946E2\}.*\r?\n', ''
$solution = $solution -replace `
    '(?ms)^Project\("\{8BC9CEB8-8B4A-11D0-8D11-00A0C91BC942\}"\) = "libwdi \(dll\)".*?^EndProject\r?\n', ''
$solution = $solution -replace `
    '(?m)^\s*\{79275348-41A4-4D07-8990-4068C9594A2C\}.*\r?\n', ''
Set-Content -LiteralPath $solutionPath -Value $solution -NoNewline

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path -LiteralPath $vswhere)) {
    throw "Visual Studio Installer vswhere.exe is unavailable."
}
$msbuild = & $vswhere -latest -products * `
    -requires Microsoft.Component.MSBuild Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
    -find MSBuild\**\Bin\MSBuild.exe | Select-Object -First 1
if (-not $msbuild) {
    throw "MSBuild is unavailable."
}

& $msbuild $solutionPath /m:1 /t:Build /nologo /v:minimal `
    /p:Configuration=Release /p:Platform=Win32 "/p:PlatformToolset=$PlatformToolset"
if ($LASTEXITCODE -ne 0) {
    throw "libwdi helper build failed with exit code $LASTEXITCODE."
}

$binary = Get-ChildItem $libwdiRoot -Recurse -Filter wdi-simple.exe |
    Where-Object { $_.FullName -match '[\\/]Release[\\/]' } |
    Select-Object -First 1
if (-not $binary) {
    throw "wdi-simple.exe was not produced."
}
Copy-Item -LiteralPath $binary.FullName `
    -Destination (Join-Path $DistDirectory "bakbeat-netmd-driver.exe") -Force
Copy-Item -LiteralPath (Join-Path $libwdiRoot "COPYING-LGPL") `
    -Destination (Join-Path $DistDirectory "LICENSE-bakbeat-netmd-driver-LGPL-3.0.txt") -Force

$sourceStage = Join-Path $WorkDirectory "corresponding-source"
New-Item -ItemType Directory -Force $sourceStage | Out-Null
$libwdiSourceArchive = Join-Path $sourceStage "libwdi-source.zip"
git -C $libwdiRoot archive --format=zip "--output=$libwdiSourceArchive" HEAD
Copy-Item -LiteralPath $helperSource -Destination $sourceStage
Copy-Item -LiteralPath $PSCommandPath `
    -Destination (Join-Path $sourceStage "build-netmd-driver.ps1")
Set-Content -LiteralPath (Join-Path $sourceStage "PINNED-LIBWDI-REVISION.txt") `
    -Value ($LibwdiRevision + "`n") -NoNewline
Compress-Archive -Path (Join-Path $sourceStage "*") `
    -DestinationPath (Join-Path $DistDirectory "bakbeat-netmd-driver-corresponding-source.zip") -Force

# The helper carries a requireAdministrator manifest by design. Do not execute it
# in CI: even its invalid-argument path invokes UAC before main() is entered.
$packagedHelper = Get-Item -LiteralPath `
    (Join-Path $DistDirectory "bakbeat-netmd-driver.exe")
if ($packagedHelper.Length -le 0) {
    throw "The packaged helper is empty."
}
