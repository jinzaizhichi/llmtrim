//! `llmtrim setup` — the one-command bootstrap. llmtrim is *only* a MITM proxy, so
//! integration is purely at the environment level: it ensures the local CA, then sets
//! `HTTPS_PROXY` + `NODE_EXTRA_CA_CERTS` for the user (POSIX: a managed shell-profile
//! block; Windows: `HKCU\Environment`) so every newly-launched tool routes through the
//! interceptor and trusts the CA — **no IDE settings touched, no sudo** — enables
//! run-at-login, and starts the daemon.
//!
//! Best-effort and idempotent: a step that fails warns and the rest proceeds.

use std::net::{Ipv4Addr, TcpListener};
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::ui::{self, Tone};

const BEGIN: &str = "# >>> llmtrim >>>";
const END: &str = "# <<< llmtrim <<<";

/// Default interceptor port; the scan for a free port starts here.
const DEFAULT_PORT: u16 = 8787;

/// First loopback port that actually binds, scanning `start..=start+span`. A successful bind
/// (immediately dropped) proves the port is usable *right now*; because we accept only `Ok`,
/// this also skips Windows reserved/excluded ranges, which fail the bind with `PermissionDenied`
/// rather than `AddrInUse`. Probes `127.0.0.1` to match exactly what `serve` binds. `None` if the
/// whole window is unusable.
fn first_free_port(start: u16, span: u16) -> Option<u16> {
    (start..=start.saturating_add(span))
        .find(|&p| TcpListener::bind((Ipv4Addr::LOCALHOST, p)).is_ok())
}

pub fn run(requested: Option<u16>) -> Result<()> {
    let color = ui::color_stdout();

    // 0. Resolve the port *once*, here, before anything is wired. The port is a contract
    //    between three parties that must agree: the profile's HTTPS_PROXY (clients), the
    //    autostart entry (`serve --port N` at login), and the daemon that binds it. Picking
    //    it lazily in `serve` would desync the clients — so we choose a port that actually
    //    binds now and feed that single value into all three below.
    let start = requested.unwrap_or(DEFAULT_PORT);
    let port = first_free_port(start, 64)
        .with_context(|| format!("no free port in {start}..={}", start.saturating_add(64)))?;
    if port != start {
        println!(
            "{}",
            ui::note(color, &format!("Port {start} busy — using {port}."))
        );
    }

    // Steps are collected as checklist rows and rendered as one summary panel at the
    // end; soft failures become `⚠` rows instead of stderr asides, so the user sees
    // one coherent report.
    let mut rows: Vec<(&str, String, String)> = Vec::new();

    // 1. Local CA (generated on first run, name-constrained to LLM domains).
    crate::serve::ensure_ca()?;
    let ca = crate::serve::ca_cert_path()?.to_string_lossy().to_string();
    let proxy = format!("http://127.0.0.1:{port}");
    rows.push((ui::OK, "Local CA".into(), ca.clone()));

    // 2. Route + trust at the environment level.
    //
    // POSIX: a managed block in the shell rc file (`export …`).
    // Windows: the *user environment* in `HKCU\Environment`, NOT a shell profile — a profile
    //   only helps PowerShell, and ExecutionPolicy can stop it loading entirely (the silent
    //   "no traffic" trap). The registry is read by every process at launch (PS5, pwsh7, Git
    //   Bash, cmd, GUI apps alike), independent of any profile running.
    #[cfg(windows)]
    {
        set_user_env(&proxy, &ca)?;
        rows.push((
            ui::OK,
            "Environment".into(),
            "HKCU\\Environment — HTTPS_PROXY + CA trust".into(),
        ));
        // Upgrade path: drop any legacy managed block a previous version wrote to the
        // PowerShell profile, so a dead (possibly ExecutionPolicy-blocked) block isn't
        // left behind.
        if let Ok(Some(path)) = remove_profile_block() {
            rows.push((
                ui::OK,
                "Profile".into(),
                format!("legacy env block removed from {}", path.display()),
            ));
        }
        // Tell Explorer to re-read the environment so freshly-launched terminals/editors
        // inherit it without a logout (a raw registry write alone is invisible to running
        // processes).
        broadcast_env_change();
    }
    #[cfg(not(windows))]
    let manual_env = match write_profile_block(&proxy, &ca)? {
        Some(path) => {
            rows.push((
                ui::OK,
                "Profile".into(),
                format!("{} — HTTPS_PROXY + CA trust", path.display()),
            ));
            false
        }
        None => {
            rows.push((
                ui::NOTE,
                "Profile".into(),
                "no shell profile found — set the env yourself (below)".into(),
            ));
            true
        }
    };

    // 3. Run at login (systemd / launchd / Windows, via auto-launch).
    match crate::autostart::configure(true, port) {
        Ok(()) => rows.push((ui::OK, "Autostart".into(), "runs at login".into())),
        Err(e) => rows.push((ui::WARN, "Autostart".into(), format!("not enabled: {e}"))),
    }

    // 4. (Re)start the interceptor. Stop any existing daemon first so re-running `setup`
    //    after an update actually goes live — otherwise the old process keeps serving the
    //    old binary until a manual restart (the silent-stale-update trap).
    let _ = crate::daemon::stop();
    let daemon_ok = match crate::daemon::spawn_detached(port) {
        Ok(pid) => {
            rows.push((
                ui::OK,
                "Interceptor".into(),
                format!("running · pid {pid} · port {port}"),
            ));
            true
        }
        Err(e) => {
            rows.push((ui::WARN, "Interceptor".into(), format!("not started: {e}")));
            false
        }
    };

    print!(
        "{}",
        ui::panel(color, "llmtrim setup", &ui::kv_rows(color, &rows))
    );

    // On Windows the env is written to the registry above, never manually.
    #[cfg(not(windows))]
    if manual_env {
        println!();
        println!("Export these in your shell yourself:");
        println!("    export HTTPS_PROXY={proxy}");
        println!("    export NODE_EXTRA_CA_CERTS={ca}");
    }

    // The env only reaches *future* processes — already-running tools (editors, Claude
    // Code, open terminals) keep their old environment until relaunched. Spell that
    // out: it's the #1 "why don't I see any traffic?" confusion.
    let check = if cfg!(windows) {
        "echo $env:HTTPS_PROXY"
    } else {
        "echo $HTTPS_PROXY"
    };
    println!();
    if daemon_ok {
        println!(
            "{}",
            ui::paint(color, Tone::Bold, "Done — the interceptor is running.")
        );
    } else {
        println!(
            "{}",
            ui::warn(
                color,
                "Setup finished, but the interceptor is not running — see above."
            )
        );
    }
    println!(
        "Only programs started after this pick up the proxy env; already-running\n\
         tools (your editor, Claude Code, open terminals) keep their old environment\n\
         until relaunched. To route one through llmtrim:"
    );
    println!();
    let new_shell = if cfg!(windows) {
        "open a new terminal (any shell — the env is set for your whole user)"
    } else {
        "open a new terminal (or re-source your shell profile)"
    };
    println!("  1. {new_shell}");
    println!("  2. verify it took:  {check}  →  {proxy}");
    println!("  3. launch your tool from that shell");
    println!();
    println!(
        "  {}  llmtrim status",
        ui::paint(color, Tone::Dim, "watch savings")
    );
    #[cfg(windows)]
    println!(
        "{}",
        ui::note(
            color,
            &format!(
                "For GUI apps that pin their own trust store, trust the CA system-wide: \
                 certutil -addstore -user Root \"{ca}\" — or see llmtrim ca."
            )
        )
    );
    #[cfg(not(windows))]
    println!(
        "{}",
        ui::note(
            color,
            "GUI apps that ignore the shell env need the CA trusted system-wide — see llmtrim ca."
        )
    );
    Ok(())
}

/// `llmtrim uninstall` — the transparent inverse of `setup`: stop the daemon, disable
/// autostart, strip the shell-profile block, and remove the CA + state (and, unless told
/// otherwise, the binary itself). Best-effort: a failed step becomes a `⚠` row and the
/// rest proceeds; every action lands in the summary panel, nothing is silent.
pub fn uninstall(purge: bool, keep_binary: bool) -> Result<()> {
    let color = ui::color_stdout();
    let mut rows: Vec<(&str, String, String)> = Vec::new();

    // 1. Stop the running daemon.
    match crate::daemon::stop() {
        Ok(Some(pid)) => rows.push((ui::OK, "Interceptor".into(), format!("stopped (pid {pid})"))),
        Ok(None) => rows.push((
            ui::NOTE,
            "Interceptor".into(),
            "no daemon was running".into(),
        )),
        Err(e) => rows.push((
            ui::WARN,
            "Interceptor".into(),
            format!("could not stop: {e}"),
        )),
    }

    // 2. Disable run-at-login (matched by app name, so the port is irrelevant here).
    match crate::autostart::configure(false, 8787) {
        Ok(()) => rows.push((ui::OK, "Autostart".into(), "disabled".into())),
        Err(e) => rows.push((ui::WARN, "Autostart".into(), format!("not changed: {e}"))),
    }

    // 3. Remove the interceptor env. Windows: the `HKCU\Environment` values (plus any legacy
    //    profile block a prior version left). POSIX: the managed block in the shell rc file.
    #[cfg(windows)]
    {
        match clear_user_env() {
            Ok(true) => rows.push((
                ui::OK,
                "Environment".into(),
                "interceptor env removed from HKCU\\Environment".into(),
            )),
            Ok(false) => rows.push((
                ui::NOTE,
                "Environment".into(),
                "no interceptor env to remove".into(),
            )),
            Err(e) => rows.push((ui::WARN, "Environment".into(), format!("not cleaned: {e}"))),
        }
        if let Ok(Some(path)) = remove_profile_block() {
            rows.push((
                ui::OK,
                "Profile".into(),
                format!("legacy env block removed from {}", path.display()),
            ));
        }
        // Refresh Explorer's environment so new processes stop seeing the removed values.
        broadcast_env_change();
    }
    #[cfg(not(windows))]
    match remove_profile_block() {
        Ok(Some(path)) => rows.push((
            ui::OK,
            "Profile".into(),
            format!("env block removed from {}", path.display()),
        )),
        Ok(None) => rows.push((ui::NOTE, "Profile".into(), "no env block to remove".into())),
        Err(e) => rows.push((ui::WARN, "Profile".into(), format!("not cleaned: {e}"))),
    }

    // 4. Remove the CA + daemon state (~/.llmtrim).
    let home = crate::daemon::home_dir()?;
    if home.exists() {
        match std::fs::remove_dir_all(&home) {
            Ok(()) => rows.push((
                ui::OK,
                "State".into(),
                format!("removed {} (CA, key, daemon state)", home.display()),
            )),
            Err(e) => rows.push((
                ui::WARN,
                "State".into(),
                format!("could not remove {}: {e}", home.display()),
            )),
        }
    } else {
        rows.push((
            ui::NOTE,
            "State".into(),
            "no state directory to remove".into(),
        ));
    }

    // 5. The savings ledger — kept by default (it's your history), removed with --purge.
    match crate::tracking::db_path() {
        Ok(db) if db.exists() && purge => {
            std::fs::remove_file(&db).ok();
            rows.push((ui::OK, "Ledger".into(), format!("removed {}", db.display())));
        }
        Ok(db) if db.exists() => {
            rows.push((
                ui::NOTE,
                "Ledger".into(),
                format!("kept {} (use --purge to remove)", db.display()),
            ));
        }
        _ => {}
    }

    // 6. The binary itself (Unix can unlink a running executable; Windows can't).
    if keep_binary {
        rows.push((ui::NOTE, "Binary".into(), "kept".into()));
    } else if let Ok(exe) = std::env::current_exe() {
        #[cfg(unix)]
        {
            std::fs::remove_file(&exe).ok();
            rows.push((
                ui::OK,
                "Binary".into(),
                format!("removed {}", exe.display()),
            ));
        }
        #[cfg(not(unix))]
        {
            rows.push((
                ui::NOTE,
                "Binary".into(),
                format!("remove manually: {}", exe.display()),
            ));
        }
    }

    print!(
        "{}",
        ui::panel(color, "llmtrim uninstall", &ui::kv_rows(color, &rows))
    );
    println!();
    println!(
        "{}",
        ui::paint(
            color,
            Tone::Bold,
            "Done. Open a new shell so the environment changes take effect."
        )
    );
    println!(
        "{}",
        ui::note(
            color,
            "If you trusted the CA system-wide manually, remove it from your OS trust store."
        )
    );
    Ok(())
}

/// Strip the llmtrim managed block from the shell profile, if present.
fn remove_profile_block() -> Result<Option<PathBuf>> {
    let Some((path, _)) = profile_target() else {
        return Ok(None);
    };
    let Ok(existing) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    if !existing.contains(BEGIN) {
        return Ok(None);
    }
    std::fs::write(&path, strip_block(&existing))
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(Some(path))
}

/// Is the interceptor env still wired up? Used to warn that stopping the daemon while
/// `HTTPS_PROXY` still points at it will break the client's HTTPS. Windows reads the
/// `HKCU\Environment` value; POSIX checks the shell-profile block.
pub fn profile_has_block() -> bool {
    #[cfg(windows)]
    {
        user_env_has_proxy()
    }
    #[cfg(not(windows))]
    {
        profile_target()
            .and_then(|(p, _)| std::fs::read_to_string(p).ok())
            .map(|t| t.contains(BEGIN))
            .unwrap_or(false)
    }
}

// ── Windows user environment (`HKCU\Environment`) ───────────────────────────────
// On Windows the proxy env lives in the registry, not a shell profile: it's inherited by
// every process at launch (PS5, pwsh7, Git Bash, cmd, GUI apps) and survives an
// ExecutionPolicy that would block a profile from running. Only processes started after
// the write see it — that's why setup still says "open a new terminal".

/// The three values llmtrim manages in the user environment.
#[cfg(windows)]
const ENV_KEYS: [&str; 3] = ["HTTPS_PROXY", "HTTP_PROXY", "NODE_EXTRA_CA_CERTS"];

/// Open `HKCU\Environment` for read+write (created if somehow absent).
#[cfg(windows)]
fn user_env_key() -> Result<winreg::RegKey> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (env, _) = hkcu
        .create_subkey_with_flags("Environment", KEY_READ | KEY_WRITE)
        .context("failed to open HKCU\\Environment")?;
    Ok(env)
}

/// Set `HTTPS_PROXY`/`HTTP_PROXY`/`NODE_EXTRA_CA_CERTS` in the user environment.
#[cfg(windows)]
fn set_user_env(proxy: &str, ca: &str) -> Result<()> {
    set_env_in(&user_env_key()?, proxy, ca)
}

/// Delete the managed values from the user environment. Returns true if anything was
/// removed. Missing values are not an error (idempotent uninstall).
#[cfg(windows)]
fn clear_user_env() -> Result<bool> {
    clear_env_in(&user_env_key()?)
}

/// Does the user environment's `HTTPS_PROXY` point at a local llmtrim interceptor?
#[cfg(windows)]
fn user_env_has_proxy() -> bool {
    user_env_key().is_ok_and(|env| has_proxy_in(&env))
}

/// Broadcast `WM_SETTINGCHANGE("Environment")` so Explorer (and through it, newly-launched
/// terminals, editors, and GUI apps) re-reads `HKCU\Environment` without a logout — a raw
/// registry write alone is invisible until then (`setx` sends the same message). The call
/// needs `SendMessageTimeout`, which is `unsafe` FFI this crate forbids
/// (`unsafe_code = "forbid"`), so shell out to PowerShell with a one-shot P/Invoke.
/// Best-effort: a failure just means "open a new shell" still applies; never breaks setup.
#[cfg(windows)]
fn broadcast_env_change() {
    // HWND_BROADCAST = 0xffff, WM_SETTINGCHANGE = 0x1A, SMTO_ABORTIFHUNG = 0x2, 5 s timeout.
    // (Keep this comment outside the PS string: the string is one line, so an inline `#`
    // would comment out the rest of it and silently no-op the broadcast.)
    const PS: &str = "\
        $sig = '[DllImport(\"user32.dll\", SetLastError=true, CharSet=CharSet.Auto)]\
        public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint Msg, UIntPtr wParam, \
        string lParam, uint fuFlags, uint uTimeout, out UIntPtr lpdwResult);';\
        $t = Add-Type -MemberDefinition $sig -Name NativeMethods -Namespace Win32 -PassThru;\
        $r = [UIntPtr]::Zero;\
        [void]$t::SendMessageTimeout([IntPtr]0xffff, 0x1A, [UIntPtr]::Zero, 'Environment', 0x2, 5000, [ref]$r)";
    let _ = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", PS])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

// The registry mechanics, taking the key as a seam so tests can exercise them against a
// throwaway subkey instead of the real `HKCU\Environment`.

#[cfg(windows)]
fn set_env_in(env: &winreg::RegKey, proxy: &str, ca: &str) -> Result<()> {
    env.set_value("HTTPS_PROXY", &proxy)
        .context("failed to set HTTPS_PROXY")?;
    env.set_value("HTTP_PROXY", &proxy)
        .context("failed to set HTTP_PROXY")?;
    env.set_value("NODE_EXTRA_CA_CERTS", &ca)
        .context("failed to set NODE_EXTRA_CA_CERTS")?;
    Ok(())
}

#[cfg(windows)]
fn clear_env_in(env: &winreg::RegKey) -> Result<bool> {
    let mut removed = false;
    for key in ENV_KEYS {
        match env.delete_value(key) {
            Ok(()) => removed = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("failed to delete {key}")),
        }
    }
    Ok(removed)
}

#[cfg(windows)]
fn has_proxy_in(env: &winreg::RegKey) -> bool {
    env.get_value::<String, _>("HTTPS_PROXY")
        .is_ok_and(|v| v.contains("127.0.0.1"))
}

/// Which shell dialect the profile uses, so the managed block is written in its native syntax.
/// Each variant is constructed on only one platform (`Posix` off-Windows, `PowerShell` on
/// Windows), yet both arms of `env_block` are compiled and unit-tested everywhere so the
/// formatting is verifiable on either OS — hence the unconditional `allow(dead_code)`.
#[allow(dead_code)]
#[derive(Clone, Copy)]
enum Syntax {
    Posix,
    PowerShell,
}

/// The profile file to write the managed env block into, and the syntax it uses. Unix: the
/// `$SHELL` rc file (`export`). Windows: the current-user PowerShell profile (`$env:`).
fn profile_target() -> Option<(PathBuf, Syntax)> {
    #[cfg(not(windows))]
    {
        let home = std::env::var("HOME").ok()?;
        let shell = std::env::var("SHELL").unwrap_or_default();
        let file = if shell.ends_with("zsh") {
            ".zshrc"
        } else if shell.ends_with("bash") {
            ".bashrc"
        } else {
            ".profile"
        };
        Some((PathBuf::from(home).join(file), Syntax::Posix))
    }
    #[cfg(windows)]
    {
        powershell_profile().map(|p| (p, Syntax::PowerShell))
    }
}

/// Resolve `$PROFILE.CurrentUserAllHosts` (handles PowerShell 5 vs 7 and a redirected/OneDrive
/// `Documents`), falling back to the conventional location if PowerShell can't be queried.
#[cfg(windows)]
fn powershell_profile() -> Option<PathBuf> {
    if let Ok(out) = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", "$PROFILE.CurrentUserAllHosts"])
        .output()
    {
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    let up = std::env::var("USERPROFILE").ok()?;
    Some(
        PathBuf::from(up)
            .join("Documents")
            .join("PowerShell")
            .join("profile.ps1"),
    )
}

/// The managed env block, in the profile's native syntax. Both variants are unit-tested on
/// every platform; on Windows the live env path is the registry, so this is test-only there.
#[allow(dead_code)]
fn env_block(proxy: &str, ca: &str, syntax: Syntax) -> String {
    match syntax {
        Syntax::Posix => format!(
            "{BEGIN}\n\
             export HTTPS_PROXY=\"{proxy}\"\n\
             export HTTP_PROXY=\"{proxy}\"\n\
             export NODE_EXTRA_CA_CERTS=\"{ca}\"\n\
             {END}\n"
        ),
        Syntax::PowerShell => format!(
            "{BEGIN}\n\
             $env:HTTPS_PROXY = \"{proxy}\"\n\
             $env:HTTP_PROXY = \"{proxy}\"\n\
             $env:NODE_EXTRA_CA_CERTS = \"{ca}\"\n\
             {END}\n"
        ),
    }
}

/// Replace (or append) the llmtrim managed block in the shell profile. Idempotent — a
/// re-run updates the existing block rather than stacking duplicates. POSIX-only: on
/// Windows the env lives in the registry, so `run()` never calls this there.
#[allow(dead_code)]
fn write_profile_block(proxy: &str, ca: &str) -> Result<Option<PathBuf>> {
    let Some((path, syntax)) = profile_target() else {
        return Ok(None);
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent); // the PowerShell profile dir may not exist yet
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut base = strip_block(&existing);
    if !base.is_empty() && !base.ends_with('\n') {
        base.push('\n');
    }
    let block = env_block(proxy, ca, syntax);
    std::fs::write(&path, format!("{base}{block}"))
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(Some(path))
}

/// Remove any existing llmtrim managed block (between the markers, inclusive).
fn strip_block(s: &str) -> String {
    let mut out = String::new();
    let mut skip = false;
    for line in s.lines() {
        match line.trim() {
            BEGIN => skip = true,
            END => skip = false,
            _ if !skip => {
                out.push_str(line);
                out.push('\n');
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_block_removes_managed_section_only() {
        let input = format!("keep1\n{BEGIN}\nexport X=1\n{END}\nkeep2\n");
        let out = strip_block(&input);
        assert_eq!(out, "keep1\nkeep2\n");
    }

    #[test]
    fn strip_block_is_noop_without_markers() {
        assert_eq!(strip_block("a\nb\n"), "a\nb\n");
    }

    #[test]
    fn env_block_posix_uses_export() {
        let b = env_block("http://127.0.0.1:8787", "/home/u/ca.pem", Syntax::Posix);
        assert!(b.contains("export HTTPS_PROXY=\"http://127.0.0.1:8787\""));
        assert!(b.contains("export NODE_EXTRA_CA_CERTS=\"/home/u/ca.pem\""));
        assert!(b.starts_with(BEGIN) && b.trim_end().ends_with(END));
    }

    #[test]
    fn env_block_powershell_uses_env_assignment() {
        let b = env_block(
            "http://127.0.0.1:8787",
            "C:\\Users\\u\\ca.pem",
            Syntax::PowerShell,
        );
        assert!(b.contains("$env:HTTPS_PROXY = \"http://127.0.0.1:8787\""));
        assert!(b.contains("$env:NODE_EXTRA_CA_CERTS = \"C:\\Users\\u\\ca.pem\""));
        assert!(!b.contains("export ")); // no posix syntax leaked in
    }

    #[test]
    fn strip_block_reverses_powershell_block() {
        let withblock = format!("keep\n{}", env_block("p", "c", Syntax::PowerShell));
        assert_eq!(strip_block(&withblock), "keep\n");
    }

    // Exercise the registry set/has/clear cycle against a throwaway subkey under HKCU so
    // the real `HKCU\Environment` is never touched. The process's own PID keys the scratch
    // path so concurrent test runs don't collide.
    #[cfg(windows)]
    #[test]
    fn registry_env_set_has_clear_roundtrip() {
        use winreg::RegKey;
        use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};

        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let scratch = format!("Software\\llmtrim-test-{}", std::process::id());
        let (env, _) = hkcu
            .create_subkey_with_flags(&scratch, KEY_READ | KEY_WRITE)
            .expect("create scratch key");

        assert!(!has_proxy_in(&env), "fresh key has no proxy");
        assert!(
            !clear_env_in(&env).expect("clear on empty key"),
            "nothing to clear yet"
        );

        set_env_in(&env, "http://127.0.0.1:18784", "C:\\Users\\u\\ca.pem").expect("set env");
        assert!(has_proxy_in(&env), "proxy set");
        assert_eq!(
            env.get_value::<String, _>("NODE_EXTRA_CA_CERTS")
                .expect("read CA value"),
            "C:\\Users\\u\\ca.pem"
        );

        assert!(
            clear_env_in(&env).expect("clear set values"),
            "values removed"
        );
        assert!(!has_proxy_in(&env), "proxy gone after clear");

        // Tidy up the scratch key.
        hkcu.delete_subkey_all(&scratch).ok();
    }

    #[test]
    fn first_free_port_rejects_occupied_accepts_free() {
        // Hold a real port open → occupied. Scanning just that port (span 0) finds nothing,
        // proving a bound port is rejected (this is the bug we hit: 8787 held by VS Code).
        let held = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind ephemeral");
        let taken = held.local_addr().expect("local_addr").port();
        assert_eq!(
            first_free_port(taken, 0),
            None,
            "occupied port not rejected"
        );

        // Release it; the same port is now bindable and the probe returns it.
        drop(held);
        assert_eq!(
            first_free_port(taken, 0),
            Some(taken),
            "free port not accepted"
        );
    }
}
