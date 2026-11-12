# Windows test plan

## Scope and completion criteria

Validate the Windows build of `feat/windows-make-1` including the review fixes. Run against the exact commit and installer intended for release. Record each case as PASS, FAIL, BLOCKED, or N/A, with the version, Windows build, shell, and evidence. Explain each BLOCKED or N/A result.

The release gate requires the offline compression baseline, determinism, retrieval, wrapper cleanup, both installer modes, upgrade preservation, and service lifecycle checks to pass. Run credential-dependent agent checks for the authentication modes the release supports.

Use a disposable Windows VM for installation, ACL manipulation, forced termination, logoff, and reboot cases. Take a snapshot before each installer-mode sequence. Keep evidence outside the installation directory so uninstall retains it.

## 1. Test matrix

| Dimension | Coverage |
|---|---|
| OS | Windows 10 x64 and Windows 11 x64, record edition and build |
| Shell | Windows PowerShell 5.1, PowerShell 7, cmd, Git Bash if used |
| Privileges | Standard user, administrator with UAC, elevated PowerShell |
| Distribution | Portable GNU executable, per-user installer, system installer, native MSVC build |
| Paths | Default paths, spaces, Unicode user/path names, long existing PATH |
| Authentication | No credentials for offline tests, Codex API key, stored API key, ChatGPT login, Claude login |
| Shutdown | Normal exit, Ctrl-C, Ctrl-Break, window close, logoff, reboot, forced kill |
| Upgrade | Older supported version to candidate, same-version reinstall, downgrade with explicit user choice |

Complete the core checks on both OS versions. Run the shell/path variations against wrapper launch, config discovery, and cleanup. Repeat lifecycle checks at least three times to expose races.

## 2. Inputs and isolated test environment

Obtain the candidate `kompressor-<version>-x86_64-windows.exe`, matching setup executable, the source checkout with fixtures, and a previous supported installer. Keep the candidate and previous installer in separate folders. Installer artifacts must come from the candidate commit.

For source tests, install the Rust MSVC toolchain at or above the repository's `rust-version` and Visual Studio C++ build tools. Docker Desktop with Linux containers is needed for the hermetic end-to-end suite. Real-agent checks require installed agents and suitable logins.

Open a fresh PowerShell and edit the first three paths.

```powershell
$Repo = (Resolve-Path 'C:\src\optscale-kompressor').Path
$Exe = (Resolve-Path 'C:\builds\candidate\kompressor-1.5.19-x86_64-windows.exe').Path
$Setup = (Resolve-Path 'C:\builds\candidate\kompressor-1.5.19-x86_64-setup.exe').Path
$Lab = Join-Path $env:TEMP ('kompressor-review-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $Lab | Out-Null
$Config = Join-Path $Lab 'config.toml'
$Port = 18817
$Base = "http://127.0.0.1:$Port"
$Utf8 = New-Object System.Text.UTF8Encoding($false)
$ErrorActionPreference = 'Stop'
function Assert-True($Condition, $Message) {
    if (-not $Condition) { throw $Message }
}
function Assert-Exit {
    if ($LASTEXITCODE -ne 0) { throw "Native command failed with exit $LASTEXITCODE" }
}
Set-Location $Lab
$env:KOMPRESSOR_HOME = Join-Path $Lab 'wrap-home'
Remove-Item Env:KOMPRESSOR_CONFIG -ErrorAction SilentlyContinue
Get-ChildItem Env:KOMPRESSOR__* | ForEach-Object {
    Remove-Item ('Env:' + $_.Name)
}
$State = $Lab.Replace('\', '/')
$ConfigText = @"
[server]
listen = "127.0.0.1:$Port"
[engine]
mode = "recoverable"
[refs]
backend = "sqlite"
path = "$State/refs.db"
[stats]
enabled = true
path = "$State/stats.db"
[log]
level = "info"
format = "json"
"@
[IO.File]::WriteAllText($Config, $ConfigText, $Utf8)
& $Exe version
Assert-Exit
& $Exe check-config --config $Config
Assert-Exit
Get-FileHash $Exe, $Setup -Algorithm SHA256
Get-CimInstance Win32_OperatingSystem | Select-Object Caption, Version, BuildNumber
$PSVersionTable
```

Use a free port if 18817 is occupied. The environment edits above affect this PowerShell session. Keep `$Lab`, candidate commit SHA, hashes, and command outputs in the test record. Avoid capturing credentials or live request bodies in shared evidence.

## 3. Portable executable and metadata

- [ ] `version`, `--version`, and `--help` succeed from a standard-user terminal.
- [ ] The version matches the candidate and reports tree-sitter support for the full build.
- [ ] Explorer Properties shows OptScale Kompressor, Hystax, the expected file/product versions, and copyright.
- [ ] The executable starts on a clean VM without Rust, MinGW, or C++ build tools installed.
- [ ] Copy the executable into a directory with spaces and Unicode characters. Repeat `version` and a compression command.
- [ ] Inspect signing with `Get-AuthenticodeSignature $Exe` and `$Setup`. A signed release must report Valid and the expected signer. Record NotSigned for an intentionally unsigned test build.
- [ ] For a signed installer, inspect the installed executable, service wrapper, and uninstaller as well.

## 4. Native source build and regression tests

Run from the source checkout. Capture native-command exit codes after each command.

```powershell
Set-Location $Repo
rustc --version
cargo test --workspace --locked
Assert-Exit
cargo test --workspace --no-default-features --locked
Assert-Exit
cargo fmt --all --check
Assert-Exit
cargo clippy --workspace --all-targets --locked -- -D warnings
Assert-Exit
$env:RUSTDOCFLAGS = '-D warnings'
cargo doc --workspace --no-deps --locked
Assert-Exit
Remove-Item Env:RUSTDOCFLAGS
cargo build --release -p kompressor-cli --locked
Assert-Exit
Set-Location $Lab
```

Expected results are zero failures in each suite, formatting check, clippy, and rustdoc. Record ignored tests separately. Inspect test output for the Windows-only profile, ProgramData, signal, and file-lock cases.

Run the focused CLI regressions as well.

```powershell
Set-Location $Repo
cargo test -p kompressor-cli --bin kompressor wrap::configfile::tests
Assert-Exit
cargo test -p kompressor-cli --bin kompressor wrap::settings::tests
Assert-Exit
cargo test -p kompressor-cli --bin kompressor windows_batch_lookup_preserves_path_order_and_spaces
Assert-Exit
cargo test -p kompressor-cli --bin kompressor cancelling_an_attachment_restores_every_claimed_setting
Assert-Exit
Set-Location $Lab
```

Verify coverage for quoted TOML keys and multiline values, invalid provider-table injection, crashed/orphaned sessions, first-time liveness locks, lock errors, cancellation cleanup, and batch launcher resolution.

## 5. Compression baseline and determinism

Use the committed baseline without re-recording it during validation.

```powershell
& $Exe bench replay `
    --corpus "$Repo\fixtures\corpus.jsonl.gz" `
    --compare "$Repo\fixtures\baseline.json" `
    --check-determinism 2>&1 | Tee-Object "$Lab\baseline.txt"
Assert-Exit
foreach ($Mode in @('lossless', 'recoverable', 'aggressive')) {
    & $Exe bench replay --corpus "$Repo\fixtures\corpus.jsonl.gz" `
        --mode $Mode --check-determinism 2>&1 |
        Tee-Object "$Lab\determinism-$Mode.txt"
    Assert-Exit
}
```

- [ ] Recoverable baseline prints PASS and exits 0.
- [ ] Every compression category matches the committed baseline. Investigate every delta, including improvements that indicate platform-dependent output.
- [ ] All three modes pass determinism checks. The committed baseline comparison above applies to recoverable mode.
- [ ] Repeat using the native MSVC executable from section 4. Compare category metrics with the distributed GNU executable.
- [ ] Repeat from a working directory containing an unrelated `kompressor.toml`. A replay without `--config` must retain the baseline results.
- [ ] Set `KOMPRESSOR__ENGINE__MODE=aggressive` temporarily, repeat the recoverable baseline, then remove it. Replay must retain its explicit/default mode and baseline.
- [ ] Copy the lab config, change an engine option, and run replay with `--config` and `--json`. Confirm the reported engine options reflect the file, including when conflicting `KOMPRESSOR__ENGINE__*` variables are present.

## 6. CLI payload and persistent retrieval checks

Create a reproducible log.

```powershell
$InputFile = Join-Path $Lab 'payload.log'
$Payload = (0..299 | ForEach-Object {
    '2026-09-18 10:00:{0:D2} INFO [worker-{1}] processed job {2} in {2}ms' -f ($_ % 60), ($_ % 4), $_
}) -join "`n"
[IO.File]::WriteAllText($InputFile, $Payload, $Utf8)
& $Exe detect $InputFile --json
Assert-Exit
& $Exe compress $InputFile --config $Config --json --explain |
    Tee-Object "$Lab\compressed.json"
Assert-Exit
& $Exe stats summary --config $Config
Assert-Exit
```

- [ ] Log detection succeeds, output shrinks substantially, and recoverable output contains handles.
- [ ] Select a handle from the output and run `retrieve <handle> --config $Config`. Confirm the returned fragment matches the corresponding original text.
- [ ] Repeat retrieval in a fresh process and after restarting the server. SQLite-backed handles remain retrievable within their configured TTL.
- [ ] Test `--lines 1:2`, `--grep processed`, and `--pointer` on a JSON fragment. Results match the requested subset.
- [ ] Exercise unknown handles and malformed ranges. Errors are bounded and the process remains usable.
- [ ] Repeat compression with JSON, source code, CSV, prose, empty input, CRLF input, Unicode, and a large log. No crash or replacement characters appear.
- [ ] Test `--role user` and `--role system`. Protected content remains unchanged.
- [ ] Repeat identical inputs and compare the JSON content fields byte-for-byte. Avoid comparing timestamps or explanatory timing values.
- [ ] Use an unwritable refs location in a copy of the config. A recoverable result must retain its original content or report a clear storage error. It must never advertise unavailable fragments as successfully stored.

## 7. HTTP API and supervised shutdown

This offline test uses the hidden stdin shutdown interface exercised by `wrap`.

```powershell
$Start = New-Object System.Diagnostics.ProcessStartInfo
$Start.FileName = $Exe
$Start.Arguments = "serve --config `"$Config`" --listen 127.0.0.1:$Port --stop-on-stdin"
$Start.UseShellExecute = $false
$Start.RedirectStandardInput = $true
$Server = New-Object System.Diagnostics.Process
$Server.StartInfo = $Start
[void]$Server.Start()
try {
    $Ready = $false
    for ($Attempt = 0; $Attempt -lt 60; $Attempt++) {
        try {
            Invoke-RestMethod "$Base/healthz" -TimeoutSec 1 | Out-Null
            $Ready = $true
            break
        } catch { Start-Sleep -Milliseconds 250 }
    }
    Assert-True $Ready 'Server did not become healthy'
    Invoke-RestMethod "$Base/readyz" | Out-Null
    Invoke-WebRequest "$Base/metrics" -UseBasicParsing | Out-Null
    $Request = @{ messages = @(
        @{ role = 'user'; content = 'what failed?' },
        @{ role = 'tool'; content = $Payload }
    ) } | ConvertTo-Json -Depth 8
    $Reply = Invoke-RestMethod "$Base/v1/compress" -Method Post `
        -ContentType 'application/json' -Body ([Text.Encoding]::UTF8.GetBytes($Request))
    Assert-True ($Reply.messages[0].content -eq 'what failed?') 'User content changed'
    Assert-True ($Reply.messages[1].content.Length -lt $Payload.Length) 'Tool output did not shrink'
    $Reference = [regex]::Match($Reply.messages[1].content, 'k_[A-Za-z0-9_-]{12}').Value
    Assert-True ($Reference.Length -gt 0) 'No recoverable handle'
    $Retrieved = Invoke-RestMethod "$Base/v1/retrieve" -Method Post `
        -ContentType 'application/json' -Body (@{ref=$Reference} | ConvertTo-Json)
    Assert-True $Retrieved.found 'Stored handle did not resolve'
    Invoke-RestMethod "$Base/v1/stats/summary" | ConvertTo-Json -Depth 8
    $Tools = Invoke-WebRequest "$Base/mcp" -Method Post -UseBasicParsing `
        -ContentType 'application/json' -Headers @{Accept='application/json, text/event-stream'} `
        -Body '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
    Assert-True ($Tools.Content -match 'optscale_kompressor_retrieve') 'Retrieval tool missing'
    $Server.StandardInput.WriteLine('stop')
    $Server.StandardInput.Flush()
    Assert-True ($Server.WaitForExit(20000)) 'Supervised stop timed out'
    Assert-True ($Server.ExitCode -eq 0) 'Supervised stop failed'
} finally {
    if (-not $Server.HasExited) { $Server.Kill(); $Server.WaitForExit() }
    $Server.Dispose()
}
Assert-True (-not (Get-NetTCPConnection -State Listen -LocalPort $Port -ErrorAction SilentlyContinue)) 'Listener survived shutdown'
```

- [ ] Health, readiness, metrics, compression, retrieval, stats, and MCP checks pass.
- [ ] Send malformed JSON and invalid request shapes. They produce appropriate client errors and health remains available.
- [ ] Repeat with an occupied listener port. Startup fails with an actionable message and leaves no extra process.
- [ ] Repeat supervised startup, close stdin without writing `stop`, and confirm the process remains healthy. End that test process explicitly afterwards. EOF allows an adopted proxy to keep serving.
- [ ] Run interactive `serve --proxy --api --config <path>` in a separate terminal. Test Ctrl-C, Ctrl-Break, and closing the terminal window. The listener goes away and statistics remain readable.

## 8. Offline batch-launcher and argument checks

This fixture runs without any provider credentials. It exercises extension inference and spaces in both executable and argument paths.

```powershell
$AgentBin = Join-Path $Lab 'agent bin'
New-Item -ItemType Directory -Force $AgentBin | Out-Null
$Batch = "@echo off`r`necho %OPENAI_BASE_URL%>`"%~1`"`r`nexit /b 0`r`n"
[IO.File]::WriteAllText((Join-Path $AgentBin 'review-agent.cmd'), $Batch, [Text.Encoding]::ASCII)
$PreviousPath = $env:PATH
try {
    $env:PATH = "$AgentBin;$PreviousPath"
    $ResultFile = Join-Path $Lab 'agent result.txt'
    & $Exe wrap --config $Config --port $Port --env OPENAI_BASE_URL=v1 -- review-agent $ResultFile
    Assert-Exit
    Assert-True ((Get-Content $ResultFile -Raw).Trim() -eq "$Base/v1") 'Agent environment mismatch'
    Assert-True (-not (Get-NetTCPConnection -State Listen -LocalPort $Port -ErrorAction SilentlyContinue)) 'Owned proxy still listening'
} finally { $env:PATH = $PreviousPath }
```

- [ ] Repeat with a `.bat` launcher and with an explicit full `.cmd` path.
- [ ] Put two same-named batch launchers in different PATH directories. The earlier directory wins.
- [ ] Provide a same-named valid native executable. Native executable resolution wins before the batch fallback.
- [ ] Change the batch exit status to a nonzero value. `wrap` reports failure and still cleans up its owned proxy.
- [ ] Use a missing command. A useful error appears and no owned proxy remains.
- [ ] Repeat from cmd and Git Bash, using those shells' quoting syntax.
- [ ] Start an independent server first, then use `wrap --no-proxy` on its port. The server remains running when the agent exits.
- [ ] Run `wrap --no-proxy` on an unused port. It fails clearly.

## 9. Codex configuration editing without a paid request

Use a dedicated `CODEX_HOME` under `$Lab` and a dummy `codex.cmd` on a temporary PATH. The dummy can run `pause` to keep a session open. An empty dedicated home exercises the API-key configuration path without reading your normal login.

For each input below, save its original bytes, start `wrap codex --config <lab-config> --port <lab-port>`, inspect the file while the dummy waits, then let the dummy exit. Check that the file parses throughout and that the original bytes return afterwards.

| Input or event | Expected result |
|---|---|
| No config file | Managed config appears during the session and is removed after it |
| Existing comments, blank lines, CRLF | Original bytes restored |
| `model_provider = 'mine'` | Session uses the managed provider, original assignment returns |
| `"model_provider" = "mine"` | Quoted key is overridden without duplicate keys |
| Multiline string assigned to `model_provider` | Whole assignment is disabled and restored |
| Unrelated multiline instructions containing key-looking lines and `[example]` | Instruction content remains intact |
| Existing `[model_providers.other]` | Existing table remains intact |
| Existing `[model_providers.kompressor]` | Clear configuration error, original file preserved |
| Invalid TOML or unreadable config | Clear error before rewriting the config |
| Edit an unrelated setting while the dummy waits | Edit survives cleanup |
| Force-kill the wrapper, then launch a new wrapper | Stale managed settings are removed on final exit |
| Force-kill and remove only the test ownership sidecar, then relaunch | Orphaned proxy settings are not saved back as the original |
| Run `unwrap codex` after a forced kill | Original settings become usable again |

Restore PATH and CODEX_HOME after these checks. The dedicated home must be used by both wrap and unwrap.

## 10. Attached settings, locks, and concurrent sessions

Use a fresh project folder under `$Lab`. Run these checks with `wrap claude --attach --config <lab-config> --port <lab-port>`.

- [ ] First attachment creates `.claude/settings.local.json` and a liveness `.held` file beside the ownership metadata.
- [ ] Start a second attachment in the same project. Exiting one keeps the other session's settings active.
- [ ] Exit the last session normally with Ctrl-C. Original values return and this process's liveness file disappears.
- [ ] Close the attachment window. Settings are restored through cancellation cleanup.
- [ ] Repeat with Ctrl-Break, logoff, and reboot. Inspect settings, logs, and listeners after returning.
- [ ] Force-kill one attachment. A later session can reap its abandoned lock and recover the settings.
- [ ] Hold the edit lock with another process. A competing edit waits or reports a lock timeout, and preserves the configuration.
- [ ] Deny writes to the test project's settings directory. Attachment fails clearly and preserves existing content.
- [ ] Run `unwrap claude` during an attachment. The wrapper's eventual exit remains safe and clears its own lock.
- [ ] With two sessions sharing one proxy, end its starting session first. The peer continues to reach the proxy. Record that the current ownership model can leave that proxy running after the last adopted session exits, then stop it explicitly.
- [ ] Repeat with independent projects and ports. Their settings and ownership records remain independent.

## 11. Config discovery and Windows paths

Use disposable profile/config locations for precedence checks, and restore process environment variables between cases. Run `check-config` and inspect the resolved refs path to identify which file won.

- [ ] Explicit `--config` wins over discovery.
- [ ] `KOMPRESSOR_CONFIG` wins over a local `kompressor.toml` and the per-user config.
- [ ] Local `kompressor.toml` wins over the per-user config.
- [ ] Nonempty `XDG_CONFIG_HOME` points to its `kompressor\config.toml`.
- [ ] Empty `XDG_CONFIG_HOME` falls back to the user profile.
- [ ] With HOME unset in PowerShell/cmd, `%USERPROFILE%\.config\kompressor\config.toml` is found.
- [ ] With both HOME and USERPROFILE set on Windows, the profile-based config behavior matches the Windows unit tests.
- [ ] `CODEX_HOME` selects the dedicated Codex directory. With it unset, Codex config lookup uses the user profile.
- [ ] PowerShell UTF-8-with-BOM and UTF-8-without-BOM config files both load.
- [ ] TOML paths using forward slashes work with spaces and Unicode characters.
- [ ] `KOMPRESSOR__*` overrides affect normal commands, while replay remains isolated as tested in section 5.
- [ ] With all user/local overrides absent, `stats summary` locates the service config in ProgramData.
- [ ] Explicitly configured stats and refs paths are used consistently by compression, retrieval, stats, and wrapping.

## 12. Per-user installation

Run as a standard user on a fresh VM.

- [ ] Default installation requests no administrator credentials.
- [ ] Files land in `%LOCALAPPDATA%\Programs\kompressor` and the uninstall entry is under HKCU.
- [ ] A new terminal resolves `kompressor` through the user PATH.
- [ ] No system service is created and no proxy starts as an installation side effect.
- [ ] Help, version, CLI compression, and section 8 work using the installed executable.
- [ ] The getting-started guide opens and finish-page text is readable without clipping.
- [ ] A custom directory with spaces and Unicode characters works.
- [ ] A user PATH longer than 1024 characters retains all existing entries, including `%VARIABLE%` entries where supported.
- [ ] Reinstalling adds no duplicate install-directory PATH entries.
- [ ] Silent `setup.exe /S` completes and exposes a meaningful process exit code. Use `Start-Process -Wait -PassThru` to capture it.
- [ ] With the installed executable running, upgrade/uninstall reports the conflict. Retry works after stopping it.

## 13. System installation and permissions

Restore a fresh VM snapshot and select system service installation.

- [ ] UAC approval leads to a successful system install. Declining it gives a clear outcome without a partially running service.
- [ ] Files land in Program Files, the uninstall entry is under HKLM, and PATH is updated for the machine.
- [ ] A custom `/D=` value cannot redirect the service binary into a standard-user-writable directory.
- [ ] ProgramData contains valid config, refs/stats stores, CA directory, and logs.
- [ ] Config contains resolved absolute paths and no `@STATE@` placeholders.
- [ ] Service is automatic and runs as LocalService.
- [ ] Default API/proxy listener is loopback port 8817 and intercept listener is loopback port 8840.
- [ ] Standard users can run stats where permitted, and cannot modify service binaries, configuration, or CA private keys.
- [ ] Another administrator can upgrade the installation.
- [ ] A pre-planted state directory owned by a standard user is refused with an actionable error.
- [ ] A junction/reparse point in the state directory is refused and its target remains unchanged.
- [ ] A restricted or blocked PowerShell environment produces a clear result for config/PATH/ownership operations. The installer must not claim a healthy service when configuration or startup failed.

Useful inspection commands in an elevated terminal follow.

```powershell
Get-CimInstance Win32_Service -Filter "Name='kompressor'" |
    Select-Object Name, State, StartMode, StartName, PathName
Get-NetTCPConnection -State Listen -LocalPort 8817,8840
icacls "$env:ProgramData\kompressor"
icacls "$env:ProgramData\kompressor\ca"
& 'C:\Program Files\kompressor\kompressor.exe' check-config `
    --config "$env:ProgramData\kompressor\config.toml"
```

## 14. Service lifecycle and failure handling

- [ ] Stop-Service and Start-Service release and recreate listeners cleanly.
- [ ] Restart-Service preserves refs and recorded statistics.
- [ ] Reboot starts the service without an interactive login.
- [ ] Logging off an unrelated user leaves the service healthy.
- [ ] Kill the service's child `kompressor.exe` by its verified PID. WinSW restarts it within the configured recovery interval.
- [ ] Occupy 8817 before starting the service. The startup problem appears in logs and installer feedback. Freeing the port allows recovery.
- [ ] Introduce invalid TOML in the test service config, restart, then restore it. Failure and recovery are diagnosable.
- [ ] Issue requests while stopping the service. Shutdown completes within its timeout and the stats database remains readable.
- [ ] Repeat startup/shutdown at least ten times. There are no orphaned child processes or locked databases after stopping.
- [ ] Run a wrapped session against the service. Exiting the wrapper leaves the service running.

## 15. Upgrade, reinstall, downgrade, and removal

Perform separate sequences for per-user and service installations.

1. Install the previous version. Add a config comment, a custom valid setting, and test refs/stats data.
2. Record config bytes, a known retrieval handle, and statistics totals.
3. Upgrade to the candidate. Verify the mode and install directory remain consistent.
4. Verify original configuration and data survive, the known handle resolves within TTL, and the candidate version runs.
5. Reinstall the candidate. Repeat preservation and PATH checks.
6. Exercise the UI's explicit downgrade choice on a VM snapshot. Confirm its choice text, resulting version, and whether the older version can read the current config/data. Record compatibility failures.
7. Uninstall through Settings, then repeat from another snapshot using `uninstall.exe /S`.

- [ ] Switching from per-user to system mode requires uninstalling the existing per-user installation as described by the installer.
- [ ] Service upgrade stops/restarts its own service and reports executable-in-use failures clearly.
- [ ] Uninstall removes installed binaries, service registration, uninstall entry, and its PATH entry.
- [ ] Configuration, refs, statistics, CA, and logs are retained as documented.
- [ ] Unrelated PATH entries and files outside the installation remain intact.
- [ ] A later reinstall can use the retained configuration and data.
- [ ] Running from the retained test profile after uninstall produces an understandable executable-not-found outcome.

## 16. Real-agent integrations

These checks contact providers and may consume API credits or subscription usage. Use a dedicated test project containing the payload from section 6. Keep the live-provider config separate from the offline config and derive it from `packaging/macos/config.toml.in`, replacing `@STATE@` with an absolute test-state path.

| Scenario | Checks |
|---|---|
| Codex API key in environment | Managed provider config, HTTP path, successful response, stats, exact config restoration |
| Codex stored API key | Stored auth is used without requiring an extra environment key |
| Codex ChatGPT login | Command-line base URL override, subscription routing, successful HTTP/WebSocket turn, normal config preserved |
| Native Codex executable | Launch, interrupt behavior, cleanup |
| npm Codex `.cmd` launcher | PATH fallback, arguments, interrupt behavior, cleanup |
| Claude Code | Base URL forwarding, compression, recoverable fragments, stats |
| Claude attachment | Editor-started agent reads settings, cancellation restores them |
| Claude Desktop intercept | Configured proxy and CA enable successful API traffic and compression |

For each supported scenario, ask the agent to read the generated log and identify a known line. Then exercise a prompt that requires an elided detail and confirm successful retrieval where a fragment was actually removed. Record request counts and correctness alongside compression savings.

- [ ] A successful session sends requests through the intended proxy and returns the correct answer.
- [ ] API-key traffic stays on the API-key route even when an account header is present.
- [ ] Subscription traffic reaches the subscription upstream with the appropriate auth headers.
- [ ] A WebSocket session remains usable. Treat usage reporting according to the relay's documented limitations.
- [ ] An upstream error is surfaced, and the next valid turn can succeed.
- [ ] Repeat after a model change/fallback. The first request completes within configured deadlines.
- [ ] Repeated conversation prefixes remain stable and retained handles resolve across turns.
- [ ] For intercept tests, use the documented client-specific CA setup. Verify configured hosts are intercepted and unrelated hosts tunnel successfully.
- [ ] After agent exit or window close, owned proxy/config/MCP state is cleaned up as appropriate.

## 17. Hermetic provider and compression end-to-end suite

From the checkout with Docker Desktop using Linux containers, run the repository suite.

```powershell
Set-Location $Repo
docker compose -f e2e/compose.yaml up --build --abort-on-container-exit --exit-code-from runner
$SuiteExit = $LASTEXITCODE
docker compose -f e2e/compose.yaml logs --no-color | Out-File "$Lab\e2e.log" -Encoding utf8
docker compose -f e2e/compose.yaml down -v --remove-orphans
Assert-True ($SuiteExit -eq 0) 'Hermetic end-to-end suite failed'
Set-Location $Lab
```

Require the runner's `all good` result. This covers the Linux-container stack on the Windows host. Sections 3–16 supply native executable and installer coverage.

Review coverage for request encodings, provider routing, streaming, cross-turn compression, retention fallback, retrieval narrowing, missing refs, and cache-prefix behavior. Preserve failed-run logs before teardown.

## 18. Results and cleanup

Use a results table with these columns.

| Case | OS/shell/build | Result | Actual result and evidence | Issue |
|---|---|---|---|---|
| Example, recoverable baseline | Windows 11, PS 7, candidate SHA | PASS/FAIL/BLOCKED/N/A | Exit code, output path, category deltas | Link or identifier |

Before signing off, verify the candidate version and hashes in the record, all required gates, every failure's disposition, and all untested matrix entries. Retain baseline reports and failed-run logs.

Stop test servers and wrappers, restore temporary PATH/CODEX_HOME/config environment changes, and close the dedicated PowerShell session. Export evidence, then remove only the generated `$Lab` directory. Use VM snapshot restoration for installer/ACL/reboot cases. Remove client-specific CA trust added during intercept testing when that test environment is retired.
