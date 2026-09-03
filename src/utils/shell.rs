// Standard
use std::path::PathBuf;

/// Resolve a launcher command to an absolute path.
///
/// The resolution logic is:
///
/// 1. If `command_path` is `Some` and the path **exists**, return it
///    directly.
/// 2. If `command_path` is `Some` but the path **does not exist** and
///    looks like a path (contains `/` on Unix, or `\` or `/` on Windows),
///    bail — the user gave an explicit path that is wrong.
/// 3. If `command_path` is `Some` but the path **does not exist** and
///    looks like a bare command name, fall back to a `PATH` lookup via
///    `which::which`.
/// 4. If `command_path` is `None`, fall back to `PATH` lookup using
///    `default_command`.
/// 5. On Windows, if the initial resolution returns a non-PE binary
///    (e.g. a Linux ELF in WSL), retry with a `.exe` suffix appended
///    to the command name so the launcher picks up the Windows variant.
///
/// This lets users set `command_path` to a bare name like `"claude"` and
/// still have the launcher succeed as long as that name is on `PATH` —
/// the same behaviour as the default (unset) path.
pub fn resolve_shell_command(
    command_path: &Option<String>,
    default_command: &str,
) -> anyhow::Result<PathBuf> {
    let Some(explicit) = command_path else {
        return which::which(default_command)
            .map_err(|_| anyhow::anyhow!("'{default_command}' not found on PATH"));
    };

    let p = PathBuf::from(explicit);
    if p.exists() {
        return Ok(p);
    }

    // On Windows, backslash and forward slash are both path separators.
    // On Unix, only forward slash is.
    let looks_like_path = if cfg!(windows) {
        explicit.contains('\\') || explicit.contains('/')
    } else {
        explicit.contains('/')
    };

    if looks_like_path {
        anyhow::bail!("explicit path '{}' does not exist", p.display());
    }

    let result = which::which(explicit);

    #[cfg(windows)]
    {
        // In WSL, `which` may resolve to a Linux ELF binary when we're
        // running a Windows process (granite-cli.exe).  Such a binary will
        // fail at spawn time with error 193 ("not a valid Win32
        // application").  Retry with a `.exe` suffix to pick up the
        // Windows variant on PATH.
        if let Ok(ref path) = result {
            if !path.to_string_lossy().to_lowercase().ends_with(".exe") {
                let exe_name = format!("{}.exe", explicit);
                if let Ok(exe_path) = which::which(&exe_name) {
                    return Ok(exe_path);
                }
            }
        }
    }

    result.map_err(|_| {
        anyhow::anyhow!(
            "command '{explicit}' not found on PATH (explicit path did not exist either)"
        )
    })
}
