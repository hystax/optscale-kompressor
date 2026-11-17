# OptScale Kompressor on Windows

Install it, connect the agents and the desktop apps, check it works. The
Windows counterpart of [`MACOS.md`](MACOS.md); [`CODEX.md`](CODEX.md),
[`CLAUDE-CODE.md`](CLAUDE-CODE.md) and [`CLAUDE-DESKTOP.md`](CLAUDE-DESKTOP.md)
explain the agent side. Commands are for PowerShell.

| Port | Used by |
|---|---|
| `127.0.0.1:8817` | everything pointed at a base URL: Claude Code, Codex CLI, the Codex app |
| `127.0.0.1:8840` | Claude Desktop only: an `HTTPS_PROXY` that decrypts `api.anthropic.com` with a local CA |

## 1. Install it

Run `kompressor-<version>-x86_64-setup.exe`, from a
[release](https://github.com/hystax/optscale-kompressor/releases) that carries it
next to the bare `kompressor-<version>-x86_64-windows.exe` (1.5.19 and earlier
do not), or built by `make win-installer` on Linux with Docker
([`QUICKSTART.md`](QUICKSTART.md) §9). It needs 64-bit Windows 10 or later. For
an unsigned build SmartScreen shows *Windows protected your PC*: **More info →
Run anyway**.

The first page asks how to install:

* **As a system service** (the default; the box is ticked), with administrator
  rights: `C:\Program Files\kompressor`, on the system `PATH`, a ready
  configuration in `%ProgramData%\kompressor\config.toml`, and the service
  started now and with Windows. Choose this for Claude Desktop and the Codex app:
  they need a proxy that is always there. See [The service](#the-service).
* **For you only** (untick the box), with no administrator rights:
  `%LOCALAPPDATA%\Programs\kompressor`, on your `PATH`. No configuration is
  written and nothing is started: `wrap` starts a proxy per session, and you
  write the configuration yourself (§2).

An existing install keeps its mode; switching means uninstalling first, and
setup refuses the service while a per-user install is present.

Open a **new** PowerShell window afterwards so it sees the new `PATH`:

```powershell
kompressor --version
```

Silently: `setup.exe /S` installs for the current user (a silent install never
asks for elevation); `setup.exe /S /SYSTEM`, from an elevated prompt, installs
the service.

**Or build it** from a checkout, with [rustup](https://rustup.rs) (Rust 1.95+,
MSVC toolchain) and the *Desktop development with C++* build tools, and continue
as for *For you only*:

```powershell
cargo install --path crates/kompressor-cli --locked
```

## 2. Configure it (for you only)

Skip this with the service: it has its own configuration, and `wrap` uses the
service when it is running.

Without a file, the stores go to a relative `./data/` in whatever directory you
start from. Write one from the packaged template, which carries the Claude and
Codex settings and the Claude Desktop listener. From a checkout it is
`packaging\macos\config.toml.in`; the per-user install does not ship it, so
otherwise this fetches it:

```powershell
$state = "$env:LOCALAPPDATA\kompressor".Replace('\', '/')
$conf = "$env:USERPROFILE\.config\kompressor\config.toml"
New-Item -ItemType Directory -Force (Split-Path $conf), $state | Out-Null
if (-not (Test-Path $conf)) {
    $template = 'packaging\macos\config.toml.in'
    $text = if (Test-Path $template) { Get-Content $template -Raw } else {
        (Invoke-WebRequest -UseBasicParsing 'https://raw.githubusercontent.com/hystax/optscale-kompressor/main/packaging/macos/config.toml.in').Content
    }
    [IO.File]::WriteAllText($conf, $text.Replace('@STATE@', $state))
}
```

Paths in the file are absolute and use forward slashes: `~` and `%VARS%` are
not expanded, and in a TOML `"…"` string a backslash is an escape. The state
lives in your profile because the stores hold the content the engine elided.

```powershell
kompressor check-config
```

shows what is in force; `path = "./data/refs.db"` means no file was read.

## 3. Wrap an agent

```powershell
kompressor wrap codex
```

```powershell
kompressor wrap claude
```

`wrap` uses the proxy already on `127.0.0.1:8817` (such as the service) or starts
one, points the agent at it, runs it, and puts everything back when it exits. A
proxy it starts logs to `%USERPROFILE%\.kompressor\logs\proxy-8817.log`.

* **Codex** with a ChatGPT login gets `-c openai_base_url=…` on its command
  line. With an API key, a block is added to `%USERPROFILE%\.codex\config.toml`
  (or `%CODEX_HOME%`) for the session; `kompressor unwrap codex` removes it if a
  session was killed.
* **Claude Code** gets `ANTHROPIC_BASE_URL` in its environment.

`wrap` runs the agent's `.exe` — `codex.exe` and `claude.exe` from their native
installers — and falls back to an npm `.cmd` launcher. `kompressor wrap --list`
shows every agent it knows.

Without `wrap`, for every session: Claude Code reads a user environment variable,

```powershell
[Environment]::SetEnvironmentVariable('ANTHROPIC_BASE_URL', 'http://127.0.0.1:8817', 'User')
```

and Codex under a ChatGPT login takes the flag from a function in your
PowerShell profile (`notepad $PROFILE`) — not from `config.toml`, see §6:

```powershell
function codex { codex.exe -c 'openai_base_url="http://127.0.0.1:8817/v1"' @args }
```

An API-key login uses the provider block in [`CODEX.md`](CODEX.md) instead.

## 4. Check it

```powershell
curl.exe -sf http://127.0.0.1:8817/healthz
```

```powershell
kompressor stats summary
```

`stats` reads the service's configuration when you have none of your own. The
log of a proxy `wrap` started, then the service's:

```powershell
Select-String '"message":"compressed"' "$env:USERPROFILE\.kompressor\logs\proxy-8817.log" | Select-Object -Last 5
```

```powershell
Select-String '"message":"compressed"' "$env:ProgramData\kompressor\logs\kompressor-service.err.log" | Select-Object -Last 5
```

`"message":"compressed"` with `saved_tokens` means traffic is going through.
Use `curl.exe`, not `curl`: in Windows PowerShell 5.1 `curl` is
`Invoke-WebRequest`.

## 5. Claude Desktop

The **Code tab** of Claude Desktop ignores `ANTHROPIC_BASE_URL`, so it reaches
the proxy through the intercept listener on `127.0.0.1:8840`: an `HTTPS_PROXY`
that decrypts `api.anthropic.com` with a certificate authority (CA) of its own
and tunnels every other host untouched. Desktop keeps its subscription login.
[`CLAUDE-DESKTOP.md`](CLAUDE-DESKTOP.md) explains why and what each setting
does. The packaged configuration — the service's, or yours from §2 — already
enables the listener; the CA is generated on its first start, as `ca.pem` (the
certificate to trust) and `ca.key` (never copy it).

**a) A `ca.pem` Desktop can read.** The service keeps its CA in
`%ProgramData%\kompressor\ca`, which only administrators, SYSTEM and the service
can open, so copy the certificate out from an **administrator** PowerShell:

```powershell
Copy-Item "$env:ProgramData\kompressor\ca\ca.pem" "$env:USERPROFILE\.claude\kompressor-ca.pem"
```

If it is not there yet, `Restart-Service kompressor` creates it. With the
per-user install, use `%LOCALAPPDATA%\kompressor\ca\ca.pem` in place, after
`kompressor serve --proxy --api` has started once — and keep that proxy running
(§7) while Desktop is open.

**b) Point Claude at the listener.** Merge three entries into the `env` object
of `%USERPROFILE%\.claude\settings.json`, keeping everything else. This backs
the file up, removes an `ANTHROPIC_BASE_URL` Desktop would override anyway, and
writes UTF-8 without a byte-order mark, which Claude does not accept:

```powershell
$f  = "$env:USERPROFILE\.claude\settings.json"
$ca = "$env:USERPROFILE\.claude\kompressor-ca.pem"      # per-user install: "$env:LOCALAPPDATA\kompressor\ca\ca.pem"
New-Item -ItemType Directory -Force (Split-Path $f) | Out-Null
$s = if (Test-Path $f) { Copy-Item $f "$f.bak" -Force; Get-Content $f -Raw | ConvertFrom-Json } else { [pscustomobject]@{} }
if (-not $s.env) { $s | Add-Member env ([pscustomobject]@{}) }
$add = @{ HTTPS_PROXY = 'http://127.0.0.1:8840'; NODE_EXTRA_CA_CERTS = $ca; CLAUDE_CODE_TETHER_LIVE = '0' }
foreach ($k in $add.Keys) { $s.env | Add-Member -Force $k $add[$k] }
$s.env.PSObject.Properties.Remove('ANTHROPIC_BASE_URL')
[IO.File]::WriteAllText($f, ($s | ConvertTo-Json -Depth 32))
```

`CLAUDE_CODE_TETHER_LIVE=0` turns off server-held message threads, without which
old tool output never reaches the proxy again. It is a version-specific switch;
recheck it after Desktop upgrades.

**c)** Quit Desktop from its **tray icon** — closing the window leaves it
running — reopen it, and start a **new** Code session.

**d) Check the route** without a paid call; HTTP **401** is the expected answer:

```powershell
curl.exe -s --proxy http://127.0.0.1:8840 --cacert "$env:USERPROFILE\.claude\kompressor-ca.pem" https://api.anthropic.com/v1/messages -H "content-type: application/json" -H "anthropic-version: 2023-06-01" -d "{}" -o NUL -w "HTTP %{http_code}`n"
```

Then use Desktop and look for `"message":"compressed"` in the log (§4). The
service logs `intercepting` with the CA's path and fingerprint when the listener
starts.

Only the Code tab is covered; regular Claude chat is not intercepted. To undo:
restore `settings.json.bak` (or remove the three entries), restart Desktop and
delete `kompressor-ca.pem`. This path was validated on Linux; it has not been
measured on Windows yet.

## 6. Codex desktop app

The Codex app has no base-URL field of its own. Under a ChatGPT plan it reads
`%USERPROFILE%\.codex\config.toml` (or `%CODEX_HOME%`), the same file as the
CLI. One line at the very top, before any `[section]`:

```toml
openai_base_url = "http://127.0.0.1:8817/backend-api/codex"
```

That targets the `codex-desktop-chatgpt` mount, which forwards to
`chatgpt.com/backend-api/codex` with your own login passed through. Restart the
app and look for `"message":"compressed"` in the log (§4); silence means the app
is not honouring the key. Two caveats:

* **Not yet measured for the app**, on Windows or elsewhere: everything in
  [`CODEX.md`](CODEX.md) was measured against codex-cli.
* **The CLI reads this line too.** From codex 0.164, with `openai_base_url` in
  the file under a ChatGPT login, the CLI sends an API key instead of the plan's
  token and every request fails with `401 … Missing scopes:
  api.responses.write`. If you see that — in the CLI or the app — remove the
  line, and use `wrap` or the profile function (§3) for the CLI.

An API-key login needs no line here: `wrap` writes a provider block for the CLI.

The **ChatGPT** desktop app (chat, not Codex) has no base-URL setting and is not
intercepted, so it cannot be routed.

## 7. Running the proxy without the service

With the per-user install, the desktop apps need a proxy that outlives a `wrap`
session. Keep one open in a terminal, with the configuration from §2:

```powershell
kompressor serve --proxy --api
```

Ctrl-C, Ctrl-Break or closing the window stops it after it saves its
statistics; a client still pointed at it then fails every request. If the
desktop apps are what you use, the service is the simpler choice.

## The service

`kompressor serve --proxy --api --config "%ProgramData%\kompressor\config.toml"`
under [WinSW](https://github.com/winsw/winsw), as the service `kompressor`
(*OptScale Kompressor*). It runs as `LocalService`, starts with Windows, restarts
10 seconds after a failure, and listens on `127.0.0.1:8817` and, for Claude
Desktop, `127.0.0.1:8840`.

```powershell
Get-Service kompressor
```

```powershell
Get-NetTCPConnection -State Listen -LocalPort 8817,8840
```

```powershell
Restart-Service kompressor
```

Restarting and stopping need an administrator PowerShell. The configuration,
`%ProgramData%\kompressor\config.toml`, is written by setup from the same
template as macOS, only when absent, so an upgrade keeps your edits. It can be
changed by administrators and by the service account, which needs write access
to the state folder it sits in; ordinary users can read it but not change it.
Restart the service to apply an edit, and check it with:

```powershell
kompressor check-config --config "$env:ProgramData\kompressor\config.toml"
```

`%ProgramData%\kompressor` also holds the stores, the CA (`ca\`, closed to
ordinary users) and the logs: `logs\kompressor-service.err.log` is kompressor's
own log, `kompressor-service.wrapper.log` WinSW's.

If setup reports that the service *started but is not serving*, port 8817 is
taken — usually by a `wrap` or `serve` you started yourself. Stop it and the
service recovers within seconds.

## Where the config is read from

First match wins:

| | |
|---|---|
| 1 | `%KOMPRESSOR_CONFIG%` |
| 2 | `.\kompressor.toml` in the working directory |
| 3 | `%XDG_CONFIG_HOME%\kompressor\config.toml`, else `%USERPROFILE%\.config\kompressor\config.toml` |

`KOMPRESSOR__*` environment variables override whichever file is read. The
service's file is not on the list; `kompressor stats` falls back to it when
none of these exists.

## Upgrading and removing

Run the newer installer: it upgrades in the same mode and folder and keeps
configuration and data. Exit running `wrap` and `serve` sessions first.

Uninstall *OptScale Kompressor* from **Settings → Apps → Installed apps**, or
silently with `uninstall.exe /S` from the install folder (an administrator
prompt for the service). Configuration and data are kept; delete them yourself
if you want: `%USERPROFILE%\.config\kompressor`, `%LOCALAPPDATA%\kompressor` and
`%USERPROFILE%\.kompressor` for your own runs, `%ProgramData%\kompressor` for
the service. Undo the client settings of §3, §5 and §6 as well: a client left
pointed at a proxy that is gone fails every request.

A build from a checkout: `cargo install … --force` to upgrade,
`cargo uninstall kompressor-cli` to remove.

## When something is wrong

| Symptom | Fix |
|---|---|
| `kompressor` is not recognised | open a **new** PowerShell window; setup changed `PATH` |
| setup: the service *started but is not serving* | port 8817 is in use by your own `wrap` / `serve`; stop it |
| `Copy-Item … ca.pem`: access denied | run it from an administrator PowerShell |
| Desktop: requests fail after setup | the proxy on 8840 is not running, or `HTTPS_PROXY` names 8817 instead of 8840 |
| `TLS handshake failed` in the log | `alpn` in `[proxy.intercept]` must contain `h2`; then check the `NODE_EXTRA_CA_CERTS` path |
| Desktop: 404 mid-session | the configuration lacks the `/api` mount; use the packaged one |
| Codex: `401 … Missing scopes: api.responses.write` | `openai_base_url` in `config.toml` under a ChatGPT login; see §6 |
| Codex: `Missing environment variable: OPENAI_API_KEY` | `model_provider` set under a ChatGPT login; remove it |
| nothing compressed, no log lines | the client is not routed: check the base URL or `HTTPS_PROXY` it actually reads |

## Where things are

| | for you only | as a service |
|---|---|---|
| binary | `%LOCALAPPDATA%\Programs\kompressor\kompressor.exe` | `C:\Program Files\kompressor\kompressor.exe` |
| config | `%USERPROFILE%\.config\kompressor\config.toml` | `%ProgramData%\kompressor\config.toml` |
| state | `%LOCALAPPDATA%\kompressor\` | `%ProgramData%\kompressor\` |
| CA | `%LOCALAPPDATA%\kompressor\ca\` | `%ProgramData%\kompressor\ca\` (administrators only) |
| logs | `%USERPROFILE%\.kompressor\logs\` | `%ProgramData%\kompressor\logs\` |
