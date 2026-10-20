# Install the SonicCraft CLI (soniccraft.exe and soniccraft-mcp.exe) from a GitHub release on Windows, in
# Windows PowerShell 5.1 or PowerShell 7:
#
#   irm https://raw.githubusercontent.com/user-github-me/soniccraft/main/install.ps1 | iex
#
# Environment variables:
#   SONICCRAFT_VERSION           the release to install, e.g. 0.1.0 (default: the latest release)
#   SONICCRAFT_INSTALL_DIR       where to put the binaries (default: %LOCALAPPDATA%\Programs\soniccraft\bin)
#   SONICCRAFT_NO_MP3=1          use the archive without the LGPL MP3 encoder (mp3 output then needs ffmpeg)
#   SONICCRAFT_NO_MODIFY_PATH=1  do not add the install directory to the user PATH
#
# The archive is checked against the SHA-256 published with the release before anything is installed.

& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'  # the progress bar makes Invoke-WebRequest very slow
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $repo = 'user-github-me/soniccraft'
    $version = $env:SONICCRAFT_VERSION
    if (-not $version) {
        $version = (Invoke-RestMethod -UseBasicParsing -Uri "https://api.github.com/repos/$repo/releases/latest").tag_name
    }
    $version = $version.TrimStart('v')
    $name = "soniccraft-v$version-x86_64-pc-windows-msvc"
    if ($env:SONICCRAFT_NO_MP3 -eq '1') { $name += '-no-mp3' }
    $base = "https://github.com/$repo/releases/download/v$version"
    $dir = $env:SONICCRAFT_INSTALL_DIR
    if (-not $dir) { $dir = Join-Path $env:LOCALAPPDATA 'Programs\soniccraft\bin' }

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ('soniccraft-' + [Guid]::NewGuid())
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        $zip = Join-Path $tmp "$name.zip"
        Write-Host "soniccraft-install: downloading $name.zip"
        Invoke-WebRequest -UseBasicParsing -Uri "$base/$name.zip" -OutFile $zip
        Invoke-WebRequest -UseBasicParsing -Uri "$base/$name.zip.sha256" -OutFile "$zip.sha256"
        $want = ((Get-Content -Raw "$zip.sha256").Trim() -split '\s+')[0].ToLowerInvariant()
        $got = (Get-FileHash -Algorithm SHA256 -Path $zip).Hash.ToLowerInvariant()
        if ($want -ne $got) { throw "checksum mismatch for $name.zip: expected $want, got $got" }

        Expand-Archive -Path $zip -DestinationPath (Join-Path $tmp 'x')
        $found = Get-ChildItem -Path (Join-Path $tmp 'x') -Recurse -Filter 'soniccraft.exe' | Select-Object -First 1
        if (-not $found) { throw "soniccraft.exe is missing from $name.zip" }
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        foreach ($bin in 'soniccraft.exe', 'soniccraft-mcp.exe') {
            $target = Join-Path $dir $bin
            # Windows cannot overwrite a running .exe but can rename it: move the old one aside first.
            Remove-Item -Force "$target.old" -ErrorAction SilentlyContinue
            if (Test-Path $target) { Move-Item -Force $target "$target.old" }
            Copy-Item -Force (Join-Path $found.DirectoryName $bin) $target
            Remove-Item -Force "$target.old" -ErrorAction SilentlyContinue
        }
    } finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }
    Write-Host "soniccraft-install: installed SonicCraft $version to $dir"

    if ($env:OS -eq 'Windows_NT') {
        if ($env:SONICCRAFT_NO_MODIFY_PATH -ne '1') {
            $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
            if (-not (($userPath -split ';') -contains $dir)) {
                $newPath = if ($userPath) { "$userPath;$dir" } else { $dir }
                [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
                Write-Host "soniccraft-install: added $dir to your user PATH (new terminals pick it up)"
            }
        }
        if (-not (($env:Path -split ';') -contains $dir)) { $env:Path = "$dir;$env:Path" }
        # The binaries need the Visual C++ runtime, which a fresh Windows install may lack.
        if (Test-Path (Join-Path $env:SystemRoot 'System32\vcruntime140.dll')) {
            & (Join-Path $dir 'soniccraft.exe') --version
        } else {
            Write-Warning 'The Microsoft Visual C++ Redistributable is missing; install it with: winget install Microsoft.VCRedist.2015+.x64'
        }
    }
    Write-Host 'soniccraft-install: try it: soniccraft init demo; soniccraft render demo\timeline.yaml --preview'
}
