//! X11 connection setup shared by the hotkey and the overlay.
//!
//! WHY this is not just `RustConnection::connect(None)`: x11rb only tries
//! the filesystem socket `/tmp/.X11-unix/Xn` and TCP. GDM/GNOME XWayland
//! commonly listens ONLY on the abstract socket (`\0/tmp/.X11-unix/Xn`)
//! and creates no socket file at all, so the plain connect fails there.
//! The hotkey carried this fallback privately while the overlay used the
//! plain connect, which is why the pill silently vanished on exactly the
//! sessions where the hotkey still worked.

use anyhow::{Context, Result, bail};
use x11rb::rust_connection::{DefaultStream, RustConnection};
use x11rb_protocol::xauth;

pub fn connect_x11() -> Result<(RustConnection, usize)> {
    // Try DISPLAY first via x11rb's native connect (filesystem socket + TCP).
    if let Ok(res) = RustConnection::connect(None) {
        return Ok(res);
    }

    // x11rb only tries the filesystem socket /tmp/.X11-unix/Xn and TCP
    // localhost:6000+n. GDM/GNOME XWayland often listens ONLY on the
    // abstract socket (\0/tmp/.X11-unix/Xn) with no filesystem socket
    // file. Scan for display numbers and try abstract sockets.
    let display_num = std::env::var("DISPLAY")
        .ok()
        .and_then(|d| {
            // DISPLAY format: [protocol/][host]:display[.screen]
            // Abstract sockets are local-only; skip remote hosts.
            // Strip everything up to and including the last ':'.
            let after = d.rsplit_once(':')?.1;
            after.split('.').next().map(|s| s.to_string())
        });

    let mut candidates = Vec::new();
    if let Some(n) = &display_num {
        candidates.push(n.clone());
    }
    // Also scan for filesystem sockets. X11 sockets live in /tmp/.X11-unix/
    // on most systems, but some configurations use $XDG_RUNTIME_DIR/X11-unix/.
    let socket_dirs: Vec<String> = [
        Some("/tmp/.X11-unix".to_string()),
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(|d| std::path::PathBuf::from(d).join("X11-unix").to_string_lossy().into_owned()),
    ]
    .into_iter()
    .flatten()
    .collect();
    for dir in &socket_dirs {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let s = name.to_string_lossy();
                if let Some(num) = s.strip_prefix('X') {
                    if !candidates.contains(&num.to_string()) {
                        candidates.push(num.to_string());
                    }
                }
            }
        }
    }
    for n in ["0", "1", "2"] {
        if !candidates.iter().any(|c| c == n) {
            candidates.push(n.to_string());
        }
    }

    for num in &candidates {
        if let Ok(res) = connect_abstract(num) {
            return Ok(res);
        }
        // Also try x11rb's native connect for this display number.
        if let Ok(res) = RustConnection::connect(Some(&format!(":{num}"))) {
            return Ok(res);
        }
    }

    RustConnection::connect(None).context("cannot connect to X11: is DISPLAY set?")
}

/// Connect to an X11 display via the abstract Unix socket
/// (\0/tmp/.X11-unix/Xn). This is the only socket type GDM/GNOME
/// XWayland often provides — no filesystem socket file exists.
pub(crate) fn connect_abstract(display_num: &str) -> Result<(RustConnection, usize)> {
    use std::os::unix::io::FromRawFd;
    use std::os::unix::net::UnixStream;

    let path = format!("\0/tmp/.X11-unix/X{display_num}");
    let display: u16 = display_num.parse().unwrap_or(0);
    // Parse the screen number from DISPLAY (e.g. ":1.0" → screen 0).
    // Default to 0 when absent — most single-monitor and XWayland setups
    // only expose screen 0.
    let screen = std::env::var("DISPLAY")
        .ok()
        .and_then(|d| {
            d.rsplit_once(':')
                .and_then(|(_, after)| after.split('.').nth(1))
                .and_then(|s| s.parse::<usize>().ok())
        })
        .unwrap_or(0);

    // Create a Unix socket and connect to the abstract namespace address.
    let fd = unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .context("cannot create Unix socket for abstract X11 connection");
        }
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as u16;
        let bytes = path.as_bytes();
        // sun_path is 108 bytes; abstract socket starts with \0.
        if bytes.len() >= addr.sun_path.len() {
            libc::close(fd);
            bail!("abstract socket path too long: {path}");
        }
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            addr.sun_path.as_mut_ptr() as *mut u8,
            bytes.len(),
        );
        let addr_len = (std::mem::size_of::<u16>() + bytes.len()) as libc::socklen_t;
        if libc::connect(fd, &addr as *const _ as *const libc::sockaddr, addr_len) < 0 {
            let err = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(err).context(format!(
                "cannot connect to abstract X11 socket {path}"
            ));
        }
        fd
    };

    // Wrap the raw fd in a UnixStream, then into x11rb's DefaultStream.
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    let (stream, (family, address)) = DefaultStream::from_unix_stream(stream)
        .context("cannot wrap abstract X11 socket as DefaultStream")?;

    // Get auth info from XAUTHORITY (or ~/.Xauthority).
    let (auth_name, auth_data) = xauth::get_auth(family, &address, display)
        .unwrap_or(None)
        .unwrap_or_else(|| (Vec::new(), Vec::new()));

    let conn = RustConnection::connect_to_stream_with_auth_info(
        stream, screen, auth_name, auth_data,
    )
    .context("cannot complete X11 handshake on abstract socket")?;

    Ok((conn, screen))
}
