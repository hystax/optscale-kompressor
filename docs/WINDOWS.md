# OptScale Kompressor on Windows

Install it, configure it, wrap an agent, check it works. The Windows counterpart
of [`MACOS.md`](MACOS.md); [`CODEX.md`](CODEX.md) and
[`CLAUDE-CODE.md`](CLAUDE-CODE.md) cover the agent side. Commands are for
PowerShell; cmd behaves the same.

## 1. Install it

Run `kompressor-<version>-x86_64-setup.exe`, built by `make win-installer`
([`QUICKSTART.md`](QUICKSTART.md)). It needs 64-bit Windows 10 or later. For an
unsigned build SmartScreen shows *Windows protected your PC*: **More info → Run
anyway**.

The first page asks how to install:

* **For you only** (the default), with no administrator rights:
  `%LOCALAPPDATA%\Programs\kompressor`, on your `PATH`. Nothing is started; you
  run it yourself.
* **As a system service**, with administrator rights: `C:\Program Files\kompressor`,
  on the system `PATH`, configured in `%ProgramData%\kompressor\config.toml`,
  and started now and with Windows. See [The service](#the-service).

Open a **new** PowerShell window afterwards so it sees the new `PATH`:

```powershell
kompressor --version
```

Silently: `setup.exe /S` for the current user, `setup.exe /S /SYSTEM` from an
elevated prompt for the service.

**Or build it** from a checkout, with [rustup](https://rustup.rs) (Rust 1.95+,
MSVC toolchain) and the *Desktop development with C++* build tools:

```powershell
cargo install --path crates/kompressor-cli --locked
```

## 2. Configure it

For the `wrap` and `serve` you run yourself; the service has its own. Without a
file, the stores go to a relative `./data/` in whatever directory you start
from, so write one from the macOS template, which carries the Codex and Claude
Code upstreams:

```powershell
$state = "$env:LOCALAPPDATA\kompressor".Replace('\', '/')
$conf = "$env:USERPROFILE\.config\kompressor\config.toml"
New-Item -ItemType Directory -Force (Split-Path $conf), $state | Out-Null
if (-not (Test-Path $conf)) {
    $text = (Get-Content packaging\macos\config.toml.in -Raw).Replace('@STATE@', $state)
    [IO.File]::WriteAllText($conf, $text)
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

`wrap` starts a proxy on `127.0.0.1:8817` (or uses one already there, such as
the service), points the agent at it, runs it, and puts everything back when it
exits. A proxy it starts logs to
`%USERPROFILE%\.kompressor\logs\proxy-8817.log`; the service logs to
`%ProgramData%\kompressor\logs\`.

* **Codex** with a ChatGPT login gets `-c openai_base_url=…` on its command
  line. With an API key, a block is added to `%USERPROFILE%\.codex\config.toml`
  (or `%CODEX_HOME%`) for the session.
* **Claude Code** gets `ANTHROPIC_BASE_URL` in its environment.

`wrap` runs the agent's `.exe`: `codex.exe` and `claude.exe` from their native
installers.

## 4. Check it

```powershell
kompressor stats summary
```

```powershell
Select-String '"message":"compressed"' $env:USERPROFILE\.kompressor\logs\proxy-8817.log | Select-Object -Last 5
```

## Running the proxy without wrap

For a client `wrap` does not launch, such as the Codex desktop app or Claude
Desktop, use the service, or keep a proxy open in a terminal, with the
configuration from §2, which names the upstreams it forwards to:

```powershell
kompressor serve --proxy --api
```

Point the client at it as [`MACOS.md`](MACOS.md) §4 and
[`CLAUDE-DESKTOP.md`](CLAUDE-DESKTOP.md) describe. Ctrl-C, Ctrl-Break or closing
the window stops it after it saves its statistics.

## The service

`kompressor serve --proxy --api` under [WinSW](https://github.com/winsw/winsw),
as the service `kompressor` (*OptScale Kompressor*). It runs as `LocalService`,
starts with Windows, restarts after a failure, and listens on `127.0.0.1:8817`;
the default configuration also enables the Claude Desktop intercept on
`127.0.0.1:8840` ([`CLAUDE-DESKTOP.md`](CLAUDE-DESKTOP.md)).

```powershell
Get-Service kompressor
```

```powershell
Restart-Service kompressor
```

Restarting and stopping need an administrator PowerShell. The configuration,
`%ProgramData%\kompressor\config.toml`, can be changed only by administrators;
restart the service to apply an edit, and check it with:

```powershell
kompressor check-config --config "$env:ProgramData\kompressor\config.toml"
```

`%ProgramData%\kompressor` also holds the stores, the CA and the logs
(`logs\kompressor-service.*.log`).

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
silently with `uninstall.exe /S` from the install folder. Configuration and
data are kept; delete them yourself if you want:
`%USERPROFILE%\.config\kompressor`, `%LOCALAPPDATA%\kompressor` and
`%USERPROFILE%\.kompressor` for your own runs, `%ProgramData%\kompressor` for
the service.

A build from a checkout: `cargo install … --force` to upgrade,
`cargo uninstall kompressor-cli` to remove.

## Where things are

| | for you only | as a service |
|---|---|---|
| binary | `%LOCALAPPDATA%\Programs\kompressor\kompressor.exe` | `C:\Program Files\kompressor\kompressor.exe` |
| config | `%USERPROFILE%\.config\kompressor\config.toml` | `%ProgramData%\kompressor\config.toml` |
| state | `%LOCALAPPDATA%\kompressor\` | `%ProgramData%\kompressor\` |
| logs | `%USERPROFILE%\.kompressor\logs\` | `%ProgramData%\kompressor\logs\` |
