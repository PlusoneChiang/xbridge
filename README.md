# xbridge

xbridge - 唸作Cross-Bridge  
WINE / Proton 環境下的 Discord Rich Presence 橋接器。

xbridge 以 Windows 服務形式運行在 WINE prefix 中，將遊戲的 Discord IPC 流量透過 Unix domain socket 轉發至宿主機的 Discord 客戶端。同時內建自動發現機制——即使遊戲本身不支援 Discord RPC，也能偵測正在運行的遊戲並自動設定 Rich Presence。

---

*[English](#english)*

---

## 運作原理

```
┌────────────── WINE / Proton ──────────────┐    ┌──── 宿主機 (Linux/macOS) ──┐
│                                            │    │                            │
│  [遊戲]                                   │    │                            │
│    │  \\.\pipe\discord-ipc-0               │    │                            │
│    ▼                                       │    │                            │
│  ┌──────────────────────────────────┐      │    │                            │
│  │        xbridge Gateway           │      │    │                            │
│  │                                  │      │    │                            │
│  │  Named Pipe ←→ 狀態機 ←→ ────────┼──────┼────┼→ discord-ipc-0            │
│  │               (Frame 解析)       │      │    │   (Unix Domain Socket)     │
│  └──────────┬───────────────────────┘      │    │                            │
│             │ 事件                          │    │  [Discord 客戶端]          │
│  ┌──────────┴───────┐                      │    │                            │
│  │ 行程掃描器        │                     │    │                            │
│  │ (自動發現)        │                     │    │                            │
│  └──────────────────┘                      │    │                            │
└────────────────────────────────────────────┘    └────────────────────────────┘
```

**Gateway** 擁有單一 Discord IPC 連線（slot 0），作為協議感知代理：

- **Idle** — 建立 Named Pipe，等待事件
- **Discovery** — 偵測到遊戲行程，自動設定 Rich Presence
- **GameConnected** — 遊戲連上 Pipe，雙向轉發 frame

狀態切換為即時——當遊戲內建 RPC 斷線時，Discovery 立即接手，無需等待。

**行程掃描器**比對正在運行的行程與 Discord 的[可偵測應用程式清單](https://discord.com/api/v9/applications/detectable)，並將事件回報給 Gateway。

## 安裝

### 前置需求

- 宿主機上有運行中的 Discord 客戶端（Linux 或 macOS）
- WINE 或 Proton（Steam Play）

### 快速開始（Proton）

```bash
# 下載最新版本
wget https://github.com/PlusoneChiang/xbridge/releases/latest/download/xbridge.exe

# 安裝至 Steam 遊戲的 WINE prefix
WINEPREFIX=~/.steam/steam/steamapps/compatdata/<APPID>/pfx \
  wine xbridge.exe --install
```

此指令會將 `xbridge.exe` 複製到 `C:\windows\`、下載可偵測應用程式清單、註冊 Windows 服務（自動啟動）並啟動。

### 快速開始（WINE）

```bash
WINEPREFIX=~/my-prefix wine xbridge.exe --install
```

### 管理指令

| 指令 | 說明 |
|------|------|
| `wine xbridge.exe --install` | 安裝服務 + 下載清單、啟動 |
| `wine xbridge.exe --uninstall` | 停止服務、移除所有檔案 |
| `wine xbridge.exe --enable` | 設為自動啟動並啟動 |
| `wine xbridge.exe --disable` | 停止服務並停用自動啟動 |

### 前景模式

不安裝服務直接運行（適用於除錯）：

```bash
wine xbridge.exe --run
```

## IPC 路徑解析

xbridge 依以下優先順序尋找宿主機 Discord IPC socket 目錄：

| 優先順序 | 來源 | 說明 |
|----------|------|------|
| 1 | 環境變數 `DISCORD_IPC_PATH` | 適用於 Linux——WINE 會傳遞宿主機環境變數 |
| 2 | 登錄檔 `HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment\DISCORD_IPC_PATH` | 適用於 macOS——WINE 服務無法繼承 POSIX 環境變數 |
| 3 | 環境變數 `XDG_RUNTIME_DIR` | Linux fallback（通常為 `/run/user/<uid>`） |

在 **Linux + Proton** 環境下，fallback 鏈通常會自動解析（`XDG_RUNTIME_DIR` 會被傳遞）。在 **macOS** 上，需要在安裝前將 IPC 路徑寫入登錄檔：

```bash
wine reg add "HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment" \
  /v DISCORD_IPC_PATH /t REG_SZ /d "$TMPDIR" /f
```

## 可偵測應用程式清單

安裝時會下載 Discord 可偵測應用程式清單，用於自動發現。GitHub Actions 工作流程每月自動更新：

- **排程**：每月 1 日 00:00 UTC
- **手動**：可透過 `workflow_dispatch` 觸發
- **完整性**：下載時進行 SHA-256 雜湊驗證；服務啟動時檢查雜湊值，過期則重新下載

## 從原始碼建置

### 需求

- [Rust](https://rustup.rs/)（stable）
- `x86_64-pc-windows-gnu` target
- MinGW-w64 交叉編譯器

### 環境設定（macOS）

```bash
rustup target add x86_64-pc-windows-gnu
brew install mingw-w64
```

### 環境設定（Ubuntu / Debian）

```bash
rustup target add x86_64-pc-windows-gnu
sudo apt-get install gcc-mingw-w64-x86-64
```

### 建置

```bash
cargo build --release
```

預設 target 為 `x86_64-pc-windows-gnu`（設定於 `.cargo/config.toml`）。輸出二進位檔位於 `target/x86_64-pc-windows-gnu/release/xbridge.exe`。

Release profile 針對檔案大小最佳化（`opt-level = "z"`、LTO、strip、panic = abort）。

---

<a id="english"></a>

## English

xbridge — pronounced "Cross-Bridge".  
Discord Rich Presence bridge for WINE / Proton.

xbridge runs inside a WINE prefix as a Windows service, forwarding Discord IPC traffic from games to the host Discord client via Unix domain socket. It also provides auto-discovery — detecting running games and setting Rich Presence even when the game doesn't have native Discord RPC support.

### How it works

```
┌────────────── WINE / Proton ──────────────┐    ┌──── Host (Linux/macOS) ────┐
│                                            │    │                            │
│  [Game]                                    │    │                            │
│    │  \\.\pipe\discord-ipc-0               │    │                            │
│    ▼                                       │    │                            │
│  ┌──────────────────────────────────┐      │    │                            │
│  │        xbridge Gateway           │      │    │                            │
│  │                                  │      │    │                            │
│  │  Named Pipe ←→ State Machine ←→ ─┼──────┼────┼→ discord-ipc-0            │
│  │               (Frame Parser)     │      │    │   (Unix Domain Socket)     │
│  └──────────┬───────────────────────┘      │    │                            │
│             │ events                       │    │  [Discord Client]          │
│  ┌──────────┴───────┐                      │    │                            │
│  │ Process Scanner   │                     │    │                            │
│  │ (Auto-Discovery)  │                     │    │                            │
│  └──────────────────┘                      │    │                            │
└────────────────────────────────────────────┘    └────────────────────────────┘
```

**Gateway** owns a single Discord IPC connection (slot 0) and acts as a protocol-aware proxy:

- **Idle** — Named Pipe created, waiting for events
- **Discovery** — Game process detected, Rich Presence set automatically
- **GameConnected** — Game opened the pipe; frames forwarded bidirectionally

State transitions are immediate — when a game's in-app RPC disconnects, Discovery resumes without delay.

**Process Scanner** matches running processes against Discord's [detectable application list](https://discord.com/api/v9/applications/detectable) and reports events to the Gateway.

### Installation

#### Prerequisites

- A running Discord client on the host (Linux or macOS)
- WINE or Proton (Steam Play)

#### Quick start (Proton)

```bash
# Download the latest release
wget https://github.com/PlusoneChiang/xbridge/releases/latest/download/xbridge.exe

# Install into a Steam game's WINE prefix
WINEPREFIX=~/.steam/steam/steamapps/compatdata/<APPID>/pfx \
  wine xbridge.exe --install
```

This copies `xbridge.exe` to `C:\windows\`, downloads the detectable app list, registers a Windows service (auto-start), and starts it.

#### Quick start (WINE)

```bash
WINEPREFIX=~/my-prefix wine xbridge.exe --install
```

#### Management commands

| Command | Description |
|---------|-------------|
| `wine xbridge.exe --install` | Install service + detectable list, start |
| `wine xbridge.exe --uninstall` | Stop service, remove all files |
| `wine xbridge.exe --enable` | Set service to auto-start and start |
| `wine xbridge.exe --disable` | Stop service and disable auto-start |

#### Foreground mode

Run without installing as a service (useful for debugging):

```bash
wine xbridge.exe --run
```

### IPC Path Resolution

xbridge locates the host Discord IPC socket directory in this order:

| Priority | Source | Description |
|----------|--------|-------------|
| 1 | `DISCORD_IPC_PATH` env | Works on Linux — WINE passes host env vars through |
| 2 | Registry `HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment\DISCORD_IPC_PATH` | For macOS — WINE services cannot inherit POSIX env vars |
| 3 | `XDG_RUNTIME_DIR` env | Linux fallback (typically `/run/user/<uid>`) |

On **Linux with Proton**, the fallback chain usually resolves automatically (`XDG_RUNTIME_DIR` is passed through). On **macOS**, you need to write the IPC path to the registry before installing:

```bash
wine reg add "HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment" \
  /v DISCORD_IPC_PATH /t REG_SZ /d "$TMPDIR" /f
```

### Detectable App List

The list of Discord-detectable applications is bundled at install time and used for auto-discovery. A GitHub Actions workflow updates the list monthly:

- **Schedule**: 1st of each month at 00:00 UTC
- **Manual**: Can be triggered via `workflow_dispatch`
- **Integrity**: SHA-256 hash verified on download; the service checks the hash at startup and re-downloads if stale

### Building from source

#### Requirements

- [Rust](https://rustup.rs/) (stable)
- `x86_64-pc-windows-gnu` target
- MinGW-w64 cross-compiler

#### Setup (macOS)

```bash
rustup target add x86_64-pc-windows-gnu
brew install mingw-w64
```

#### Setup (Ubuntu / Debian)

```bash
rustup target add x86_64-pc-windows-gnu
sudo apt-get install gcc-mingw-w64-x86-64
```

#### Build

```bash
cargo build --release
```

The default target is `x86_64-pc-windows-gnu` (configured in `.cargo/config.toml`). The output binary is at `target/x86_64-pc-windows-gnu/release/xbridge.exe`.

The release profile is optimized for size (`opt-level = "z"`, LTO, strip, panic = abort).

## License

[GPL-3.0](LICENSE)
