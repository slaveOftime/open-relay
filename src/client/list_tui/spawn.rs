// Spawn helpers: the platform-specific terminal launchers
// (spawn_session_terminal on windows / macos / unix), geometry and
// arrangement (arrange_window, windows_screen_geometry, WindowRect),
// bash / powershell / wsl command builders, and the marker file used
// to detect whether a session has an attached terminal already.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::constants::INACTIVE_LOG_TAIL_LINES;

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[cfg(any(windows, test))]
pub fn arrange_window(
    work: WindowRect,
    anchor: WindowRect,
    size: (u16, u16),
    slot: usize,
) -> WindowRect {
    let cell_width =
        ((size.0 as u32).saturating_mul(9).saturating_add(32)).clamp(480, work.width.max(1));
    let cell_height =
        ((size.1 as u32).saturating_mul(19).saturating_add(48)).clamp(320, work.height.max(1));
    let columns = (work.width / cell_width.max(1)).max(1) as usize;
    let rows = (work.height / cell_height.max(1)).max(1) as usize;
    let cells = columns.saturating_mul(rows).max(1);
    let index = slot % cells;
    let x = work.x + ((index % columns) as u32 * cell_width) as i32;
    let y = work.y + ((index / columns) as u32 * cell_height) as i32;
    let fallback_x = (anchor.x + 28 * slot as i32).clamp(
        work.x,
        work.x + work.width.saturating_sub(cell_width) as i32,
    );
    let fallback_y = (anchor.y + 28 * slot as i32).clamp(
        work.y,
        work.y + work.height.saturating_sub(cell_height) as i32,
    );
    WindowRect {
        x: if cells > 1 { x } else { fallback_x },
        y: if cells > 1 { y } else { fallback_y },
        width: cell_width,
        height: cell_height,
    }
}

pub fn terminal_marker(id: &str) -> PathBuf {
    std::env::temp_dir().join(format!("oly-list-{id}-{}.open", uuid::Uuid::new_v4()))
}

pub fn shell_command(
    executable: &str,
    args: &[String],
    marker: Option<&Path>,
    powershell: bool,
) -> String {
    if powershell {
        let command = std::iter::once(executable.to_string())
            .chain(args.iter().cloned())
            .map(|value| format!("'{}'", value.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(" ");
        match marker {
            Some(marker) => format!(
                "$m='{}'; New-Item -ItemType File -Force $m | Out-Null; try {{ & {command} }} finally {{ Remove-Item -Force $m -ErrorAction SilentlyContinue }}",
                marker.display().to_string().replace('\'', "''")
            ),
            None => format!("& {command}"),
        }
    } else {
        let command = std::iter::once(executable.to_string())
            .chain(args.iter().cloned())
            .map(shell_quote)
            .collect::<Vec<_>>()
            .join(" ");
        match marker {
            Some(marker) => format!(
                "m={}; touch \"$m\"; trap 'rm -f \"$m\"' EXIT; {command}",
                shell_quote(marker.display().to_string())
            ),
            None => command,
        }
    }
}

pub fn session_command(
    id: &str,
    node: Option<&str>,
    attach: bool,
) -> io::Result<(String, Vec<String>)> {
    let executable = std::env::current_exe()?.to_string_lossy().into_owned();
    let mut args = vec![
        if attach { "attach" } else { "logs" }.to_string(),
        id.to_string(),
    ];
    if !attach {
        args.push("--keep-color".to_string());
        args.extend(["--tail".to_string(), INACTIVE_LOG_TAIL_LINES.to_string()]);
    }
    if let Some(node) = node {
        args.extend(["--node".to_string(), node.to_string()]);
    }
    Ok((executable, args))
}

#[cfg(windows)]
pub fn powershell_encoded_command(script: &str) -> String {
    use base64::Engine as _;

    let utf16 = script
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    base64::engine::general_purpose::STANDARD.encode(utf16)
}

#[cfg(windows)]
pub fn spawn_session_terminal(
    id: &str,
    node: Option<&str>,
    size: (u16, u16),
    slot: usize,
    attach: bool,
    marker: Option<&Path>,
) -> io::Result<()> {
    let (executable, attach_args) = session_command(id, node, attach)?;
    let (work, anchor) = windows_screen_geometry();
    let rect = arrange_window(work, anchor, size, slot);
    let mut command = Command::new("wt.exe");
    command.args([
        "-w",
        "new",
        "--pos",
        &format!("{},{}", rect.x, rect.y),
        "--size",
        &format!("{},{}", size.0, size.1),
        "--title",
        &format!("oly · {id}"),
    ]);
    let script = shell_command(&executable, &attach_args, marker, true);
    let encoded_script = powershell_encoded_command(&script);
    command.arg("powershell.exe");
    command.arg("-NoProfile");
    if !attach {
        command.arg("-NoExit");
    }
    command.args(["-EncodedCommand", &encoded_script]);
    command.spawn().map(|_| ())
}

#[cfg(windows)]
pub fn windows_screen_geometry() -> (WindowRect, WindowRect) {
    use std::mem::size_of;
    use windows_sys::Win32::{
        Foundation::RECT,
        Graphics::Gdi::{
            GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
        },
        UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowRect},
    };

    unsafe {
        let window = GetForegroundWindow();
        let mut current = RECT::default();
        let _ = GetWindowRect(window, &mut current);
        let monitor = MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(monitor, &mut info);
        let convert = |rect: RECT| WindowRect {
            x: rect.left,
            y: rect.top,
            width: (rect.right - rect.left).max(1) as u32,
            height: (rect.bottom - rect.top).max(1) as u32,
        };
        (convert(info.rcWork), convert(current))
    }
}

#[cfg(target_os = "macos")]
pub fn spawn_session_terminal(
    id: &str,
    node: Option<&str>,
    size: (u16, u16),
    slot: usize,
    attach: bool,
    marker: Option<&Path>,
) -> io::Result<()> {
    let (executable, args) = session_command(id, node, attach)?;
    let mut shell = shell_command(&executable, &args, marker, false);
    if !attach {
        shell.push_str("; printf '\\nPress Enter to close…'; read _");
    }
    let script = format!(
        "tell application \"Terminal\" to do script \"{}\"",
        shell.replace('"', "\\\"")
    );
    let _ = (size, slot);
    Command::new("osascript")
        .args(["-e", &script])
        .spawn()
        .map(|_| ())
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn spawn_session_terminal(
    id: &str,
    node: Option<&str>,
    size: (u16, u16),
    slot: usize,
    attach: bool,
    marker: Option<&Path>,
) -> io::Result<()> {
    let (executable, args) = session_command(id, node, attach)?;
    let geometry = format!(
        "{}x{}+{}+{}",
        size.0,
        size.1,
        24 + slot * 28,
        24 + slot * 28
    );
    let terminal = ["xterm", "gnome-terminal", "konsole", "alacritty", "kitty"]
        .into_iter()
        .find(|candidate| which::which(candidate).is_ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no supported terminal emulator found (xterm, gnome-terminal, konsole, alacritty, or kitty)",
            )
        })?;
    let mut shell = shell_command(&executable, &args, marker, false);
    if !attach {
        shell.push_str("; printf '\\nPress Enter to close…'; read _");
    }
    let title = format!("oly · {id}");
    let mut command = Command::new(terminal);
    match terminal {
        "xterm" => {
            command.args([
                "-geometry",
                &geometry,
                "-T",
                &title,
                "-e",
                "sh",
                "-c",
                &shell,
            ]);
        }
        "gnome-terminal" => {
            command.args([
                "--title",
                &title,
                &format!("--geometry={geometry}"),
                "--",
                "sh",
                "-c",
                &shell,
            ]);
        }
        "konsole" => {
            command.args(["--title", &title, "-e", "sh", "-c", &shell]);
        }
        "alacritty" => {
            command.args([
                "--title",
                &title,
                "--dimensions",
                &size.0.to_string(),
                &size.1.to_string(),
                "-e",
                "sh",
                "-c",
                &shell,
            ]);
        }
        "kitty" => {
            command.args(["--title", &title, "sh", "-c", &shell]);
        }
        _ => unreachable!(),
    }
    command.spawn().map(|_| ())
}

pub fn shell_quote(value: String) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
