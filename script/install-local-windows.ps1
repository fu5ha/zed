[CmdletBinding()]
param(
    [switch]$DebugBuild,
    [switch]$StageOnly,
    [switch]$InstallStaged,
    [switch]$Restart
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if ([Environment]::OSVersion.Platform -ne 'Win32NT') {
    throw 'This script requires Windows. See docs/src/development/windows.md.'
}
if ($InstallStaged -and ($DebugBuild -or $StageOnly)) {
    throw '-InstallStaged cannot be combined with -DebugBuild or -StageOnly.'
}
if ($StageOnly -and $Restart) {
    throw '-StageOnly cannot be combined with -Restart.'
}

$repository = Split-Path $PSScriptRoot -Parent
$installRoot = Join-Path $env:LOCALAPPDATA 'Programs\Zed-JJ'
$current = Join-Path $installRoot 'current'
$staged = Join-Path $installRoot 'staged'
$previous = Join-Path $installRoot 'previous'
$installMarker = Join-Path $installRoot 'install.json'

function Invoke-Native {
    param([string]$Program, [string[]]$Arguments)
    & $Program @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Program failed with exit code $LASTEXITCODE. The installed version was not replaced."
    }
}

function Test-Executable {
    param([string]$Program, [string]$Argument)
    $information = New-Object Diagnostics.ProcessStartInfo
    $information.FileName = $Program
    $information.Arguments = $Argument
    $information.UseShellExecute = $false
    $information.RedirectStandardOutput = $true
    $information.RedirectStandardError = $true
    $process = New-Object Diagnostics.Process
    $process.StartInfo = $information
    try {
        if (!$process.Start()) { throw "Failed to start $Program." }
        $output = $process.StandardOutput.ReadToEndAsync()
        $errors = $process.StandardError.ReadToEndAsync()
        $process.WaitForExit()
        $outputText = $output.GetAwaiter().GetResult()
        $errorText = $errors.GetAwaiter().GetResult()
        if ($process.ExitCode -ne 0) {
            throw "$Program $Argument failed with exit code $($process.ExitCode). $errorText"
        }
        Write-Host $outputText.TrimEnd()
        if ($errorText) { Write-Host $errorText.TrimEnd() }
    }
    finally {
        $process.Dispose()
    }
}

function Get-BuildArchitecture {
    if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString() -ne 'X64') {
        throw 'This local installer currently supports x64 Windows only.'
    }
    $rustInformation = & rustc --version --verbose
    if ($LASTEXITCODE -ne 0 -or $rustInformation -notcontains 'host: x86_64-pc-windows-msvc') {
        throw 'This local installer requires the x86_64-pc-windows-msvc Rust host toolchain.'
    }
    if ($env:CARGO_BUILD_TARGET -and $env:CARGO_BUILD_TARGET -ne 'x86_64-pc-windows-msvc') {
        throw 'Remove CARGO_BUILD_TARGET or set it to x86_64-pc-windows-msvc before building the local installation.'
    }
    return 'x64'
}

function Add-AmdRuntime {
    param([string]$Destination, [string]$Architecture, [string]$TargetDirectory)
    if ($Architecture -ne 'x64') { return }
    $cache = Join-Path $TargetDirectory 'local-install-runtime\AGS-6.3.0'
    $library = Join-Path $cache 'AGS_SDK-6.3.0\ags_lib\lib\amd_ags_x64.dll'
    if (!(Test-Path -LiteralPath $library)) {
        New-Item -ItemType Directory -Path $cache -Force | Out-Null
        $archive = Join-Path $cache 'AGS.zip'
        Invoke-WebRequest -UseBasicParsing -Uri 'https://codeload.github.com/GPUOpen-LibrariesAndSDKs/AGS_SDK/zip/refs/tags/v6.3.0' -OutFile $archive
        Expand-Archive -LiteralPath $archive -DestinationPath $cache -Force
    }
    Copy-Item -LiteralPath $library -Destination (Join-Path $Destination 'amd_ags_x64.dll')
}

function Build-StagedVersion {
    $temporary = Join-Path $installRoot ("building-" + [Guid]::NewGuid().ToString('N'))
    Push-Location $repository
    try {
        $architecture = Get-BuildArchitecture
        $arguments = @('zed-build')
        $profile = 'release-fast'
        if ($DebugBuild) {
            $arguments = @('build', '--locked', '--package', 'zed', '--bin', 'zed', '--package', 'cli', '--bin', 'cli')
            $profile = 'debug'
        }
        $channel = if ($env:ZED_RELEASE_CHANNEL) {
            $env:ZED_RELEASE_CHANNEL.Trim()
        } else {
            (Get-Content -LiteralPath (Join-Path $repository 'crates\zed\RELEASE_CHANNEL') -Raw).Trim()
        }
        $executables = @{}
        $arguments += '--message-format=json-render-diagnostics'
        & cargo @arguments | ForEach-Object {
            $message = $_ | ConvertFrom-Json
            if ($message.reason -eq 'compiler-artifact' -and $message.executable -and ($message.target.kind -contains 'bin')) {
                $executables[$message.target.name] = $message.executable
            }
        }
        if ($LASTEXITCODE -ne 0) {
            throw "Cargo failed with exit code $LASTEXITCODE. The installed version was not replaced."
        }
        if (!$executables.ContainsKey('zed') -or !$executables.ContainsKey('cli')) {
            throw 'Cargo did not report both Zed and CLI executables.'
        }
        $metadataText = & cargo metadata --locked --no-deps --format-version 1
        if ($LASTEXITCODE -ne 0) { throw 'Cargo metadata failed.' }
        $metadata = $metadataText | ConvertFrom-Json
        $output = Split-Path $executables['zed'] -Parent
        if ((Split-Path $executables['cli'] -Parent) -ne $output) {
            throw 'Cargo reported different Zed and CLI output directories.'
        }
        $image = [IO.File]::OpenRead($executables['zed'])
        try {
            $reader = New-Object IO.BinaryReader($image)
            $image.Position = 0x3c
            $header = $reader.ReadInt32()
            $image.Position = $header
            if ($reader.ReadUInt32() -ne 0x00004550 -or $reader.ReadUInt16() -ne 0x8664) {
                throw 'Cargo produced a non-x64 Windows executable; check Cargo build.target configuration.'
            }
        }
        finally {
            $image.Dispose()
        }
        New-Item -ItemType Directory -Path (Join-Path $temporary 'bin') -Force | Out-Null
        Copy-Item -LiteralPath (Join-Path $output 'zed.exe') -Destination (Join-Path $temporary 'Zed.exe')
        Copy-Item -LiteralPath (Join-Path $output 'cli.exe') -Destination (Join-Path $temporary 'bin\zed.exe')
        # Zed's build script supplies the same ConPTY package as bundle-windows.ps1.
        foreach ($runtime in @('conpty.dll', 'OpenConsole.exe')) {
            $source = Join-Path $output $runtime
            if (!(Test-Path -LiteralPath $source)) {
                throw "Required terminal runtime $source is missing. Retry the build with network access."
            }
            Copy-Item -LiteralPath $source -Destination (Join-Path $temporary $runtime)
        }
        New-Item -ItemType Directory -Path (Join-Path $temporary $architecture) | Out-Null
        Copy-Item -LiteralPath (Join-Path $output 'OpenConsole.exe') -Destination (Join-Path $temporary "$architecture\OpenConsole.exe")
        Add-AmdRuntime $temporary $architecture $metadata.target_directory
        Test-Executable (Join-Path $temporary 'Zed.exe') '--help'
        Test-Executable (Join-Path $temporary 'bin\zed.exe') '--version'
        @{
            profile = $profile
            channel = $channel
            built_at = [DateTime]::UtcNow.ToString('o')
            checkout = $repository
        } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $temporary 'build.json') -Encoding UTF8
        if (Test-Path -LiteralPath $staged) { Remove-Item -LiteralPath $staged -Recurse -Force }
        Move-Item -LiteralPath $temporary -Destination $staged
        Write-Host "Successful build staged at $staged"
    }
    finally {
        Pop-Location
        if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Recurse -Force }
    }
}

function Get-InstalledProcesses {
    $prefix = "$current\"
    @(Get-Process | Where-Object {
        $path = $_.Path
        $path -and $path.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)
    })
}

function Get-ZedProcesses {
    @(Get-Process -Name zed -ErrorAction SilentlyContinue)
}

function Backup-SharedData {
    if (Test-Path -LiteralPath $installMarker) { return }
    if (@(Get-ZedProcesses).Count -gt 0) {
        throw "First installation needs a consistent settings/database backup. Close all Zed windows, then rerun with -InstallStaged. The build remains staged at $staged."
    }
    $backup = Join-Path $installRoot ('shared-data-backup-' + [DateTime]::Now.ToString('yyyyMMdd-HHmmss'))
    New-Item -ItemType Directory -Path $backup | Out-Null
    $config = Join-Path $env:APPDATA 'Zed'
    $database = Join-Path $env:LOCALAPPDATA 'Zed\db'
    if (Test-Path -LiteralPath $config) {
        Copy-Item -LiteralPath $config -Destination (Join-Path $backup 'config') -Recurse
    }
    if (Test-Path -LiteralPath $database) {
        Copy-Item -LiteralPath $database -Destination (Join-Path $backup 'db') -Recurse
    }
    return $backup
}

function Set-LauncherPath {
    $launcher = Join-Path $current 'bin'
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $paths = @($userPath -split ';' | Where-Object {
        $_ -and $_.TrimEnd('\') -ine $launcher.TrimEnd('\')
    })
    [Environment]::SetEnvironmentVariable('Path', (@($launcher) + $paths -join ';'), 'User')
    $env:PATH = "$launcher;$env:PATH"
    $machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    foreach ($directory in ($machinePath -split ';')) {
        if ($directory) {
            $expanded = [Environment]::ExpandEnvironmentVariables($directory)
            foreach ($name in @('zed.exe', 'zed.cmd', 'zed.bat')) {
                if (Test-Path -LiteralPath (Join-Path $expanded $name)) {
                    Write-Warning "Machine PATH contains $expanded\$name and normally precedes User PATH. This shell uses the fork, but new shells may not. Move that PATH entry to User PATH manually, or prepend $launcher in your shell profile. No system PATH or official installation was modified."
                }
            }
        }
    }
    if (-not ('LocalZedEnvironment' -as [type])) {
        Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class LocalZedEnvironment {
    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern IntPtr SendMessageTimeout(IntPtr window, uint message, UIntPtr value,
        string text, uint flags, uint timeout, out UIntPtr result);
}
'@
    }
    $result = [UIntPtr]::Zero
    [LocalZedEnvironment]::SendMessageTimeout([IntPtr]0xffff, 0x1a, [UIntPtr]::Zero, 'Environment', 2, 5000, [ref]$result) | Out-Null
    Write-Host "User PATH now starts with $launcher. Open a new terminal and verify with: where.exe zed"
}

function Install-StagedVersion {
    if (!(Test-Path -LiteralPath (Join-Path $staged 'build.json'))) {
        throw "No successful staged build exists at $staged."
    }
    $processes = @(Get-InstalledProcesses)
    $wasRunning = $processes.Count -gt 0
    if ($wasRunning) {
        if ($Restart) {
            foreach ($process in $processes) {
                if ($process.MainWindowHandle -ne 0) {
                    if (!$process.CloseMainWindow()) {
                        Write-Warning "Zed did not accept a close request. Save your work and close it manually."
                    }
                }
            }
            $deadline = [DateTime]::UtcNow.AddSeconds(30)
            while (@(Get-InstalledProcesses).Count -gt 0 -and [DateTime]::UtcNow -lt $deadline) {
                Start-Sleep -Milliseconds 500
            }
        }
        if (@(Get-InstalledProcesses).Count -gt 0) {
            Write-Warning "Installed fork is still running. Nothing was replaced. Save your work, close it, and run this script with -InstallStaged."
            return
        }
    }
    $backup = Backup-SharedData
    $retired = Join-Path $installRoot ("retired-" + [Guid]::NewGuid().ToString('N'))
    $movedPrevious = $false
    if (Test-Path -LiteralPath $previous) {
        $previousPrefix = "$previous\"
        if (@(Get-Process | Where-Object { $_.Path -and $_.Path.StartsWith($previousPrefix, [StringComparison]::OrdinalIgnoreCase) }).Count -gt 0) {
            throw 'The previous version is still running; close it before deployment.'
        }
        Move-Item -LiteralPath $previous -Destination $retired
        $movedPrevious = $true
    }
    $movedCurrent = $false
    try {
        if (Test-Path -LiteralPath $current) {
            Move-Item -LiteralPath $current -Destination $previous
            $movedCurrent = $true
        }
        Move-Item -LiteralPath $staged -Destination $current
    }
    catch {
        if ($movedCurrent -and !(Test-Path -LiteralPath $current)) {
            Move-Item -LiteralPath $previous -Destination $current
        }
        if ($movedPrevious -and !(Test-Path -LiteralPath $previous)) {
            Move-Item -LiteralPath $retired -Destination $previous
        }
        throw
    }
    if ($movedPrevious) {
        try {
            Remove-Item -LiteralPath $retired -Recurse -Force
        }
        catch {
            Write-Warning "New version installed, but older backup cleanup failed: $retired. $($_.Exception.Message)"
        }
    }
    if (!(Test-Path -LiteralPath $installMarker)) {
        @{
            shared_data_backup = $backup
            installed_at = [DateTime]::UtcNow.ToString('o')
        } | ConvertTo-Json | Set-Content -LiteralPath $installMarker -Encoding UTF8
    }
    Set-LauncherPath
    $shortcutPath = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\Zed JJ.lnk'
    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($shortcutPath)
    $shortcut.TargetPath = Join-Path $current 'Zed.exe'
    $shortcut.WorkingDirectory = $env:USERPROFILE
    $shortcut.Description = 'Locally built Zed with jj integration; shares official Zed settings'
    $shortcut.Save()
    Write-Host "Installed at $current. Official Zed and shared settings were not modified."
    $build = Get-Content -LiteralPath (Join-Path $current 'build.json') -Raw | ConvertFrom-Json
    if ($build.channel -ne 'stable') {
        Write-Warning 'This fork shares Zed settings/databases, but different channels use separate instance mutexes. Do not run it alongside official Zed.'
    }
    if ($Restart -and $wasRunning) {
        $others = @(Get-ZedProcesses)
        if ($others.Count -gt 0) {
            Write-Warning 'Another Zed instance is running. Close it before launching the fork to avoid sharing the database between instances.'
        }
        else {
            Start-Process -FilePath (Join-Path $current 'Zed.exe')
        }
    }
}

New-Item -ItemType Directory -Path $installRoot -Force | Out-Null
$lockPath = Join-Path $installRoot 'deploy.lock'
$lock = $null
try {
    try {
        $lock = [IO.File]::Open($lockPath, [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    }
    catch {
        throw 'Another local build/deployment is in progress. Wait for it to finish.'
    }
    if (!$InstallStaged) { Build-StagedVersion }
    if (!$StageOnly) { Install-StagedVersion }
}
finally {
    if ($null -ne $lock) { $lock.Dispose() }
}
