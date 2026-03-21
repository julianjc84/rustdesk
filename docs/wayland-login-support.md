# Wayland Login Screen - Remote Desktop Support

## Problem

When RustDesk's service connects to a host at a **Wayland login screen** (GDM/SDDM/LightDM), the connection is rejected with "Login screen using Wayland is not supported." This is because Wayland's security model prevents screen capture of the greeter without portal approval, and nobody is physically present to approve.

## Solution

Instead of capturing the login screen (architecturally impossible on Wayland without compositor integration), we **bypass the greeter entirely**: show a login form in the remote client, authenticate via PAM, then trigger the display manager to start a desktop session via temporary autologin.

## Display Manager + Session Combinations

| DM Greeter | User Desktop | Behavior | Status | Transition Phase |
|---|---|---|---|---|
| **X11 → X11** | SDDM/LightDM → X11 desktop | RustDesk captures login screen directly via XCB | Existing behavior, works | Legacy |
| **X11 → Wayland** | SDDM/LightDM → Wayland desktop | Captures X11 login screen, user logs in normally. Minor: stale `--cm` process for DM user lingers (dormant, no CPU impact) | Works, minor cleanup issue | **Current transition** |
| **Wayland → Wayland** | GDM → GNOME/KDE Wayland | PAM authentication → temporary autologin config → DM restart → full desktop session | **New (this PR)** | Future standard |
| **Wayland → X11** | GDM → X11 desktop | PAM authentication → temporary autologin config → DM starts X11 session | Should work (untested) | Uncommon |
| **No DM** | TTY → sway/compositor | No greeter involved, user starts compositor manually | Not affected | Niche |

> **Transition note:** As Linux distros move from X11 to Wayland, display managers are transitioning their greeters:
> - **GDM**: Wayland-only greeter since v50 (2024) — **our fix is needed now**
> - **SDDM**: Experimental Wayland greeter (opt-in) — our fix is future-ready
> - **LightDM**: Wayland support depends on greeter implementation — our fix is future-ready
>
> The X11→Wayland row represents the current transition phase where most distros are today. The Wayland→Wayland row is the future standard that our changes enable.

## Architecture

### Flow

```
1. Remote client connects to RustDesk service (root) on Wayland login screen
2. Server detects is_login_screen_wayland() == true
3. Server does NOT reject — activates headless login flow
4. LinuxHeadlessHandle::try_start_desktop() called with empty credentials
5. Returns "session not ready, password empty" message
6. Client shows username + password dialog (existing Flutter dialog)
7. User enters credentials, clicks Login
8. Server receives LoginRequest with os_login filled
9. try_start_wayland_session() authenticates via PAM
10. Writes signal file: /tmp/rustdesk-wayland-session-request
11. Returns "session not ready" — client retries
12. Service daemon (start_os_service loop) reads signal file
13. Detects display manager (GDM/SDDM/LightDM)
14. Backs up DM config → writes temporary autologin → restarts DM
15. DM auto-logs in the user with full systemd infrastructure
16. Restores original DM config after 10 seconds
17. Service loop detects new user session on seat0
18. Spawns --server subprocess for the user's desktop
19. Client retries → connection succeeds → normal screen capture
```

### Why Signal File Architecture?

The session activation (stopping/restarting the display manager) **must** happen in the `--service` daemon, not in the `--server` connection handler. This is because:

- The `--service` daemon is a systemd service — it survives DM restarts
- The `--server` subprocess runs under the DM's session — it dies when the DM restarts
- If the `--server` tried to restart the DM, it would kill itself mid-operation

The signal file (`/tmp/rustdesk-wayland-session-request`) bridges the two:
- Written by the `--server` (connection handler) after PAM authentication
- Read by the `--service` (daemon) which safely handles the DM restart

### Security Model

1. **Service runs as root** — authorized at install via systemd
2. **PAM validates credentials** — same authentication as typing into the login screen
3. **Only after PAM succeeds** does any DM config modification happen
4. **Autologin config is temporary** — restored after 10 seconds
5. **Wrong password** → PAM rejects → nothing is modified, user can retry

### Display Manager Autologin Configs

Each DM uses a different config format. All follow the same pattern: backup → write autologin → restart DM → restore.

**GDM** (`/etc/gdm/custom.conf` or `/etc/gdm3/custom.conf`):
```ini
[daemon]
AutomaticLoginEnable=true
AutomaticLogin=username
```

**SDDM** (`/etc/sddm.conf.d/rustdesk-autologin.conf` — drop-in, deleted after):
```ini
[Autologin]
User=username
Session=
```

**LightDM** (`/etc/lightdm/lightdm.conf`):
```ini
[Seat:*]
autologin-user=username
```

## Files Changed

| File | Change |
|------|--------|
| `src/server/connection.rs` | Remove Wayland login rejection; allow headless handle for Wayland login screens |
| `src/platform/linux_desktop_manager.rs` | PAM authentication, signal file read/write, session exec resolution, DM detection, autologin config helpers |
| `src/platform/linux.rs` | `activate_wayland_session()` — DM autologin + restart; service loop signal handling; skip server spawn for DM greeter users |
| `src/client.rs` | Map `LOGIN_SCREEN_WAYLAND` error to `session-login-password` dialog |
| `libs/hbb_common/src/platform/linux.rs` | Add `gdm-greeter` and `lightdm` to `is_gdm_user()` |
| `CLAUDE.md` | GCC 15+ build workaround note |

## Known Issues

1. **Reconnection gap**: Client gets "server terminated" during DM restart transition. Auto-reconnect may fail; manual reconnect works. The gap is ~5-10 seconds while the DM restarts and the new session starts.

2. **Stale --cm process (X11→Wayland transition)**: When SDDM runs an X11 greeter and the user logs into a Wayland desktop, the `--cm` process spawned for the DM user lingers. It's dormant (no CPU impact) but not cleaned up until reboot. This is a pre-existing issue in the `--server`/`--cm` lifecycle management.

3. **Session selection**: The PAM login dialog only has username + password fields. The user cannot choose which desktop session to launch — it uses the last-selected session from AccountsService. Future work: add a session dropdown to the Flutter dialog.

4. **GDM-specific autologin**: Only GDM autologin has been tested. SDDM and LightDM autologin configs are implemented but untested (these DMs currently use X11 greeters where direct capture works).

5. **Non-systemd distros**: The implementation relies on `systemctl` for DM management and `loginctl`/logind for session detection. Non-systemd distros (Void, Artix, Gentoo OpenRC) are not supported.

## Testing

1. **GDM Wayland login screen**: Connect remotely → should see username/password dialog → enter credentials → desktop session starts → remote control works
2. **Wrong password**: Enter bad credentials → PAM rejects → dialog re-shows
3. **SDDM X11 login screen**: Connect remotely → should see login screen directly (X11 capture) → type credentials into visible login screen
4. **Normal sessions unaffected**: Connecting to an already-logged-in desktop works as before
5. **Safety net**: If autologin fails, DM is already running (just shows greeter again)
