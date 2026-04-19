//! Diagnostic: spawn mpv and try to connect, with stderr captured so we can
//! see what mpv is doing. Not a test — just a focused repro.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

#[tokio::main]
async fn main() {
    let mpv_bin = PathBuf::from(r"C:\Program Files\MPV Player\mpv.exe");
    let pipe = format!(r"\\.\pipe\syncmesh-dbg-{}", std::process::id());
    eprintln!("spawning mpv with ipc = {pipe}");

    let mut cmd = Command::new(&mpv_bin);
    cmd.arg(format!("--input-ipc-server={pipe}"))
        .arg("--no-config")
        .arg("--idle=yes")
        .arg("--force-window=no")
        .arg("--vo=null")
        .arg("--ao=null")
        .arg("--msg-level=all=info")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);

    let mut child = cmd.spawn().expect("spawn mpv");

    // Poll-connect.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut attempt = 0;
    let path = PathBuf::from(&pipe);
    loop {
        attempt += 1;
        match tokio::net::windows::named_pipe::ClientOptions::new().open(&path) {
            Ok(_) => {
                eprintln!("CONNECTED on attempt {attempt}");
                break;
            }
            Err(e) => {
                eprintln!("attempt {attempt}: {e:?}");
                if tokio::time::Instant::now() >= deadline {
                    eprintln!("GIVING UP");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}
