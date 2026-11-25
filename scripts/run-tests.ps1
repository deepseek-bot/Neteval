$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
Push-Location $repoRoot

try {
    $logsDir = Join-Path $repoRoot "logs"
    New-Item -ItemType Directory -Force -Path $logsDir | Out-Null
    $latestLog = Join-Path $logsDir "latest.log"
    [System.IO.File]::WriteAllText($latestLog, "")

    function Write-ScriptLog {
        param([string]$Level, [string]$Msg)
        $ts = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
        $line = "$Level [$ts] [run-tests] $Msg"
        Add-Content -Path $latestLog -Value $line -Encoding UTF8
        Write-Host $line
    }

    function Assert-True {
        param([bool]$Condition, [string]$Label, [string]$Expected, [string]$Actual)
        if (-not $Condition) {
            Write-ScriptLog "ERROR" "FAIL [$Label]: expected=$Expected actual=$Actual"
            throw "FAIL [$Label]: expected=$Expected actual=$Actual"
        }
        Write-ScriptLog "INFO" "PASS [$Label]: actual=$Actual"
    }

    function Invoke-Native {
        param([scriptblock]$Cmd)
        $prev = $ErrorActionPreference
        $ErrorActionPreference = "Continue"
        try {
            & $Cmd
        }
        finally {
            $ErrorActionPreference = $prev
        }
    }

    # Strip WinLibs from PATH so rustc uses its bundled self-contained MinGW linker
    $env:PATH = ($env:PATH -split ';' | Where-Object { $_ -and ($_ -notmatch 'WinLibs') }) -join ';'

    $mingwPkg = Get-ChildItem "$env:LOCALAPPDATA\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.MSVCRT*" -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($mingwPkg) {
        $gccHit = Get-ChildItem $mingwPkg.FullName -Recurse -Filter "gcc.exe" -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($gccHit) {
            $env:WINLIBS_BIN = $gccHit.DirectoryName
        }
    }

    # Build dlltool & windres shims for x86_64-pc-windows-gnu if absent or outdated
    $toolsDir = Join-Path $repoRoot "target\tools"
    New-Item -ItemType Directory -Force -Path $toolsDir | Out-Null
    $dlltoolExe = Join-Path $toolsDir "dlltool.exe"
    $dlltoolSrc = Join-Path $repoRoot "scripts\dlltool_shim.rs"
    $rebuildDlltool = (-not (Test-Path $dlltoolExe)) -or (
        (Test-Path $dlltoolSrc) -and
        ((Get-Item $dlltoolSrc).LastWriteTimeUtc -gt (Get-Item $dlltoolExe).LastWriteTimeUtc)
    )
    if ($rebuildDlltool) {
        Write-ScriptLog "INFO" "building target/tools/dlltool.exe shim"
        rustc $dlltoolSrc -o $dlltoolExe
        if ($LASTEXITCODE -ne 0) {
            throw "FAIL [build-dlltool-shim]: exit=$LASTEXITCODE"
        }
    }
    $windresExe = Join-Path $toolsDir "windres.exe"
    $windresSrc = Join-Path $repoRoot "scripts\windres_shim.rs"
    $rebuildWindres = (Test-Path $windresSrc) -and (
        (-not (Test-Path $windresExe)) -or
        ((Get-Item $windresSrc).LastWriteTimeUtc -gt (Get-Item $windresExe).LastWriteTimeUtc)
    )
    if ($rebuildWindres) {
        Write-ScriptLog "INFO" "building target/tools/windres.exe shim"
        rustc $windresSrc -o $windresExe
        if ($LASTEXITCODE -ne 0) {
            throw "FAIL [build-windres-shim]: exit=$LASTEXITCODE"
        }
    }
    $env:PATH = "$toolsDir;$env:PATH"

    $guiDistIndex = Join-Path $repoRoot "apps\gui\dist\index.html"
    if (-not (Test-Path $guiDistIndex)) {
        $guiDir = Join-Path $repoRoot "apps\gui"
        if (Test-Path (Join-Path $guiDir "package.json")) {
            Write-ScriptLog "INFO" "building apps/gui/dist before workspace tests"
            Push-Location $guiDir
            try {
                Invoke-Native { npm run build }
            } finally {
                Pop-Location
            }
        }
    }

    Write-ScriptLog "INFO" "running cargo test --workspace"
    Invoke-Native { cargo test --workspace }
    $testExit = $LASTEXITCODE
    Assert-True ($testExit -eq 0) "cargo-test-workspace" "exit=0" "exit=$testExit"

    Write-ScriptLog "INFO" "building neteval-server and neteval-cli binaries"
    Invoke-Native { cargo build --bin neteval-server --bin neteval-cli }
    $buildExit = $LASTEXITCODE
    Assert-True ($buildExit -eq 0) "cargo-build-bins" "exit=0" "exit=$buildExit"

    $serverBin = Join-Path $repoRoot "target\debug\neteval-server.exe"
    $cliBin = Join-Path $repoRoot "target\debug\neteval-cli.exe"
    Assert-True (Test-Path $serverBin) "server-bin-exists" "True" "$(Test-Path $serverBin)"
    Assert-True (Test-Path $cliBin) "cli-bin-exists" "True" "$(Test-Path $cliBin)"

    # Pick free ephemeral UDP/TCP ports
    function Get-FreeUdpPort {
        $udp = New-Object System.Net.Sockets.UdpClient(0)
        $port = ([System.Net.IPEndPoint]$udp.Client.LocalEndPoint).Port
        $udp.Close()
        return $port
    }

    function Get-FreeTcpPort {
        $tcp = New-Object System.Net.Sockets.TcpListener([System.Net.IPAddress]::Loopback, 0)
        $tcp.Start()
        $port = ([System.Net.IPEndPoint]$tcp.LocalEndpoint).Port
        $tcp.Stop()
        return $port
    }

    $p1Port = Get-FreeUdpPort
    $p2Port = Get-FreeUdpPort
    $helperPort = Get-FreeUdpPort
    $wsPort = Get-FreeTcpPort
    $token = "e2e-local-token"
    $e2eDataDir = Join-Path $repoRoot "target\e2e-cli-data"
    $p2pDataARel = "target/e2e-p2p-a"
    $p2pDataBRel = "target/e2e-p2p-b"
    $p2pDataA = Join-Path $repoRoot $p2pDataARel
    $p2pDataB = Join-Path $repoRoot $p2pDataBRel
    foreach ($d in @($e2eDataDir, $p2pDataA, $p2pDataB)) {
        if (Test-Path $d) {
            Remove-Item -Recurse -Force $d
        }
    }

    Write-ScriptLog "INFO" "starting helper S2 on 127.0.0.1:$helperPort and primary S1+S2 on 127.0.0.1:$p1Port / $p2Port / ws:$wsPort"
    $helperErr = Join-Path $repoRoot "target\e2e-helper.err"
    $primaryErr = Join-Path $repoRoot "target\e2e-primary.err"
    $peerAOut = Join-Path $repoRoot "target\e2e-p2p-a.out"
    $peerAErr = Join-Path $repoRoot "target\e2e-p2p-a.err"
    $peerBOut = Join-Path $repoRoot "target\e2e-p2p-b.out"
    $peerBErr = Join-Path $repoRoot "target\e2e-p2p-b.err"
    Remove-Item $helperErr, $primaryErr, $peerAOut, $peerAErr, $peerBOut, $peerBErr -ErrorAction SilentlyContinue

    $helperProc = Start-Process -FilePath $serverBin -WorkingDirectory $repoRoot -ArgumentList @(
        "--p1", "127.0.0.1:$helperPort",
        "--token", $token,
        "--responder-id", "helper",
        "--log-file", "logs/latest.log"
    ) -RedirectStandardError $helperErr -PassThru -WindowStyle Hidden

    $primaryProc = Start-Process -FilePath $serverBin -WorkingDirectory $repoRoot -ArgumentList @(
        "--p1", "127.0.0.1:$p1Port",
        "--p2", "127.0.0.1:$p2Port",
        "--ws", "127.0.0.1:$wsPort",
        "--helper", "127.0.0.1:$helperPort",
        "--token", $token,
        "--responder-id", "primary",
        "--log-file", "logs/latest.log"
    ) -RedirectStandardError $primaryErr -PassThru -WindowStyle Hidden

    $peerAProc = $null
    $peerBProc = $null

    try {
        $ready = $false
        for ($i = 0; $i -lt 50; $i++) {
            if ($helperProc.HasExited) {
                $errTxt = if (Test-Path $helperErr) { Get-Content $helperErr -Raw } else { "" }
                throw "FAIL [helper-server-start]: exited early code=$($helperProc.ExitCode) stderr=$errTxt"
            }
            if ($primaryProc.HasExited) {
                $errTxt = if (Test-Path $primaryErr) { Get-Content $primaryErr -Raw } else { "" }
                throw "FAIL [primary-server-start]: exited early code=$($primaryProc.ExitCode) stderr=$errTxt"
            }
            $logText = Get-Content $latestLog -Raw -ErrorAction SilentlyContinue
            if (
                $logText -match "bound p1 socket on 127\.0\.0\.1:$helperPort" -and
                $logText -match "bound p1 socket on 127\.0\.0\.1:$p1Port" -and
                $logText -match "bound ws socket on 127\.0\.0\.1:$wsPort"
            ) {
                $ready = $true
                break
            }
            Start-Sleep -Milliseconds 100
        }
        Assert-True $ready "servers-bound-ready" "True" "$ready"

        # 1. CompleteFull CLI run against primary + helper S2
        $fullOutRaw = & $cliBin --data-dir $e2eDataDir --log-file $latestLog --json nat --no-default-stun --server-p1 "127.0.0.1:$p1Port" --server-p2 "127.0.0.1:$p2Port" --helper-p1 "127.0.0.1:$helperPort" --token $token --timeout-ms 800
        Assert-True ($LASTEXITCODE -eq 0) "cli-nat-full-exit" "exit=0" "exit=$LASTEXITCODE"
        $fullJson = ($fullOutRaw -join "`n") | ConvertFrom-Json
        Assert-True ($fullJson.nat_report.status -eq "complete_full") "cli-nat-full-status" "complete_full" "$($fullJson.nat_report.status)"
        Assert-True ($fullJson.nat_report.mapping -eq "endpoint_independent") "cli-nat-full-mapping" "endpoint_independent" "$($fullJson.nat_report.mapping)"
        Assert-True ($fullJson.nat_report.filtering -eq "full_cone_nat1") "cli-nat-full-filtering" "full_cone_nat1" "$($fullJson.nat_report.filtering)"
        $rep1Id = $fullJson.report_id

        # 2. CompleteSingleVps CLI run against primary p1+p2
        $singleOutRaw = & $cliBin --data-dir $e2eDataDir --log-file $latestLog --json nat --no-default-stun --server-p1 "127.0.0.1:$p1Port" --server-p2 "127.0.0.1:$p2Port" --token $token --timeout-ms 800
        Assert-True ($LASTEXITCODE -eq 0) "cli-nat-single-exit" "exit=0" "exit=$LASTEXITCODE"
        $singleJson = ($singleOutRaw -join "`n") | ConvertFrom-Json
        Assert-True ($singleJson.nat_report.status -eq "complete_single_vps") "cli-nat-single-status" "complete_single_vps" "$($singleJson.nat_report.status)"
        Assert-True ($singleJson.nat_report.filtering -eq "cone_nat1_or_nat2") "cli-nat-single-filtering" "cone_nat1_or_nat2" "$($singleJson.nat_report.filtering)"
        $rep2Id = $singleJson.report_id

        # 3. History list / show / delete
        $listRaw = & $cliBin --data-dir $e2eDataDir --log-file $latestLog --json history list
        Assert-True ($LASTEXITCODE -eq 0) "cli-history-list-exit" "exit=0" "exit=$LASTEXITCODE"
        $listJson = ($listRaw -join "`n") | ConvertFrom-Json
        Assert-True ($listJson.Count -eq 2) "cli-history-list-count" "2" "$($listJson.Count)"

        $showRaw = & $cliBin --data-dir $e2eDataDir --log-file $latestLog --json history show $rep1Id
        Assert-True ($LASTEXITCODE -eq 0) "cli-history-show-exit" "exit=0" "exit=$LASTEXITCODE"
        $showJson = ($showRaw -join "`n") | ConvertFrom-Json
        Assert-True ($showJson.report_id -eq $rep1Id) "cli-history-show-id" "$rep1Id" "$($showJson.report_id)"

        & $cliBin --data-dir $e2eDataDir --log-file $latestLog --json history delete $rep1Id | Out-Null
        Assert-True ($LASTEXITCODE -eq 0) "cli-history-delete-exit" "exit=0" "exit=$LASTEXITCODE"

        $listAfterRaw = & $cliBin --data-dir $e2eDataDir --log-file $latestLog --json history list
        $listAfterJson = ($listAfterRaw -join "`n") | ConvertFrom-Json
        Assert-True ($listAfterJson.Count -eq 1) "cli-history-after-delete-count" "1" "$($listAfterJson.Count)"
        Assert-True ($listAfterJson[0].report_id -eq $rep2Id) "cli-history-remaining-id" "$rep2Id" "$($listAfterJson[0].report_id)"

        # 4. Dual concurrent neteval-cli p2p processes against live S1+S2 neteval-server
        Write-ScriptLog "INFO" "launching dual neteval-cli p2p processes against ws://127.0.0.1:$wsPort"
        function Start-CliAsync {
            param([string[]]$ArgList)
            $psi = New-Object System.Diagnostics.ProcessStartInfo
            $psi.FileName = $cliBin
            $psi.WorkingDirectory = $repoRoot
            $psi.Arguments = ($ArgList -join " ")
            $psi.UseShellExecute = $false
            $psi.CreateNoWindow = $true
            $psi.RedirectStandardOutput = $true
            $psi.RedirectStandardError = $true
            $p = [System.Diagnostics.Process]::Start($psi)
            return @{
                Proc = $p
                OutTask = $p.StandardOutput.ReadToEndAsync()
                ErrTask = $p.StandardError.ReadToEndAsync()
            }
        }

        $peerARun = Start-CliAsync @(
            "--data-dir", $p2pDataARel,
            "--log-file", "logs/latest.log",
            "--json",
            "p2p",
            "--no-default-stun",
            "--ws", "ws://127.0.0.1:$wsPort",
            "--room", "e2eroom1",
            "--label", "peer-a",
            "--token", $token,
            "--echo-v4", "127.0.0.1:$p1Port",
            "--window-secs", "5",
            "--rate-hz", "4"
        )
        $peerAProc = $peerARun.Proc

        $peerBRun = Start-CliAsync @(
            "--data-dir", $p2pDataBRel,
            "--log-file", "logs/latest.log",
            "--json",
            "p2p",
            "--no-default-stun",
            "--ws", "ws://127.0.0.1:$wsPort",
            "--room", "e2eroom1",
            "--label", "peer-b",
            "--token", $token,
            "--echo-v4", "127.0.0.1:$p1Port",
            "--window-secs", "5",
            "--rate-hz", "4"
        )
        $peerBProc = $peerBRun.Proc

        $aDone = $peerAProc.WaitForExit(25000)
        $bDone = $peerBProc.WaitForExit(25000)
        Assert-True ($aDone -and $bDone) "cli-p2p-both-exited" "True" "A=$aDone B=$bDone"
        $peerAProc.WaitForExit()
        $peerBProc.WaitForExit()
        Assert-True ($peerAProc.ExitCode -eq 0) "cli-p2p-a-exit" "0" "$($peerAProc.ExitCode)"
        Assert-True ($peerBProc.ExitCode -eq 0) "cli-p2p-b-exit" "0" "$($peerBProc.ExitCode)"

        $p2pAJson = $peerARun.OutTask.GetAwaiter().GetResult() | ConvertFrom-Json
        $p2pBJson = $peerBRun.OutTask.GetAwaiter().GetResult() | ConvertFrom-Json
        Assert-True ($p2pAJson.p2p_report.punched -eq $true) "cli-p2p-a-punched" "True" "$($p2pAJson.p2p_report.punched)"
        Assert-True ($p2pBJson.p2p_report.punched -eq $true) "cli-p2p-b-punched" "True" "$($p2pBJson.p2p_report.punched)"
        Assert-True ($p2pAJson.p2p_report.verdict -eq "excellent") "cli-p2p-a-verdict" "excellent" "$($p2pAJson.p2p_report.verdict)"
        Assert-True ($p2pBJson.p2p_report.verdict -eq "excellent") "cli-p2p-b-verdict" "excellent" "$($p2pBJson.p2p_report.verdict)"

        $p2pHistRaw = & $cliBin --data-dir $p2pDataA --log-file $latestLog --json history list
        Assert-True ($LASTEXITCODE -eq 0) "cli-p2p-history-list-exit" "0" "$LASTEXITCODE"
        $p2pHistJson = ($p2pHistRaw -join "`n") | ConvertFrom-Json
        Assert-True ($p2pHistJson.Count -eq 1 -and $p2pHistJson[0].has_p2p -eq $true) "cli-p2p-history-saved" "count=1,has_p2p=True" "count=$($p2pHistJson.Count)"
    }
    finally {
        foreach ($proc in @($peerAProc, $peerBProc, $primaryProc, $helperProc)) {
            if ($proc -and -not $proc.HasExited) {
                Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
            }
        }
        foreach ($d in @($e2eDataDir, $p2pDataA, $p2pDataB)) {
            if (Test-Path $d) {
                Remove-Item -Recurse -Force $d -ErrorAction SilentlyContinue
            }
        }
        Remove-Item $helperErr, $primaryErr, $peerAOut, $peerAErr, $peerBOut, $peerBErr -ErrorAction SilentlyContinue
    }

    Write-ScriptLog "INFO" "all workspace tests and real S1+S2+CLI end-to-end checks passed"
}
finally {
    Pop-Location
}
