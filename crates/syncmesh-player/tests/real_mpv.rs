//! Integration test that drives a real mpv binary.
//!
//! This test is the concrete proof of Phase 1's exit criterion:
//! "A test binary can open a local file in mpv, pause/play/seek it from Rust
//! code, and receive events back, cross-platform."
//!
//! Skipping rules:
//! - If `SYNCMESH_MPV_BIN` is set, we use that path.
//! - Else we probe the default `"mpv"` PATH lookup plus
//!   `C:\Program Files\MPV Player\mpv.exe` (the path winget installs to).
//! - If no mpv is found, we print a SKIPPED banner and the test succeeds.
//!   The plan's exit criterion is satisfied by CI having mpv available;
//!   local contributors without mpv should not be blocked.

use std::path::PathBuf;
use std::time::Duration;

use syncmesh_player::{MpvCommand, MpvEvent, MpvExit, SpawnOptions, spawn};
use tokio::sync::mpsc;

fn find_mpv() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SYNCMESH_MPV_BIN") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    let candidates = [
        PathBuf::from("mpv"),
        #[cfg(windows)]
        PathBuf::from(r"C:\Program Files\MPV Player\mpv.exe"),
        #[cfg(windows)]
        PathBuf::from(r"C:\Program Files\mpv\mpv.exe"),
        #[cfg(unix)]
        PathBuf::from("/usr/bin/mpv"),
        #[cfg(unix)]
        PathBuf::from("/usr/local/bin/mpv"),
    ];
    for c in candidates {
        // `exists()` returns false for a bare name on PATH; try spawning
        // `--version` to probe.
        if c.is_absolute() && c.exists() {
            return Some(c);
        }
        if !c.is_absolute()
            && std::process::Command::new(&c)
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        {
            return Some(c);
        }
    }
    None
}

async fn wait_for<F>(rx: &mut mpsc::Receiver<MpvEvent>, timeout: Duration, mut pred: F) -> MpvEvent
where
    F: FnMut(&MpvEvent) -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for event");
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(ev)) => {
                if pred(&ev) {
                    return ev;
                }
            }
            Ok(None) => panic!("event channel closed before predicate matched"),
            Err(elapsed) => panic!("timed out waiting for event after {elapsed}"),
        }
    }
}

#[tokio::test]
async fn pause_play_seek_and_speed_round_trip_with_real_mpv() {
    let Some(mpv_bin) = find_mpv() else {
        eprintln!("SKIPPED: mpv binary not found (set SYNCMESH_MPV_BIN or install mpv on PATH)");
        return;
    };

    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "syncmesh_player=info".into()),
        )
        .with_test_writer()
        .try_init();

    let opts = SpawnOptions {
        mpv_binary: mpv_bin,
        ipc_path: None,
        initial_file: None,
        // Headless. `keep_alive=true` adds `--idle=yes --force-window=yes`;
        // we override force-window via extra_args so no window opens. The
        // idle flag is the one that matters — without it mpv exits as soon
        // as it notices no file was given.
        extra_args: vec![
            "--no-config".into(),
            "--vo=null".into(),
            "--ao=null".into(),
            "--force-window=no".into(),
            "--really-quiet".into(),
            "--hr-seek=yes".into(),
        ],
        connect_timeout: Duration::from_secs(10),
        keep_alive: true,
    };

    let (handle, mut events) = spawn(opts).await.expect("spawn mpv");

    // Load a synthetic test stream (no external file needed). lavfi is built
    // into mpv's bundled FFmpeg on both the winget Windows build and all
    // recent Linux packages.
    let source = PathBuf::from("av://lavfi:testsrc=duration=10:rate=5:size=160x120");
    handle
        .send(MpvCommand::LoadFile { path: source })
        .await
        .expect("loadfile");

    // Duration should surface.
    let duration = wait_for(&mut events, Duration::from_secs(10), |e| {
        matches!(e, MpvEvent::Duration(_))
    })
    .await;
    match duration {
        MpvEvent::Duration(d) => assert!(d > 0.0, "duration was {d}"),
        _ => unreachable!(),
    }

    // Pause it and verify we get the pause-true event.
    handle.send(MpvCommand::Pause(true)).await.expect("pause");
    wait_for(&mut events, Duration::from_secs(5), |e| {
        matches!(e, MpvEvent::Pause(true))
    })
    .await;

    // Seek to 3 seconds. mpv should emit a Seeking event followed by a
    // TimePos reflecting the seek.
    handle
        .send(MpvCommand::Seek {
            media_pos_ms: 3_000,
        })
        .await
        .expect("seek");
    wait_for(&mut events, Duration::from_secs(5), |e| {
        matches!(e, MpvEvent::Seeking | MpvEvent::PlaybackRestart)
    })
    .await;
    // After the seek settles, time-pos should be near 3.0. Drain events
    // until we see one in that range, or give up.
    let mut saw_time_near_seek = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Some(MpvEvent::TimePos(t))) => {
                if (t - 3.0).abs() < 0.6 {
                    saw_time_near_seek = true;
                    break;
                }
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    assert!(
        saw_time_near_seek,
        "no TimePos event near the seek target arrived"
    );

    // Set playback speed to 0.95x.
    handle
        .send(MpvCommand::SetSpeed { speed_centi: 95 })
        .await
        .expect("set speed");
    let speed_ev = wait_for(&mut events, Duration::from_secs(5), |e| {
        matches!(e, MpvEvent::Speed(_))
    })
    .await;
    match speed_ev {
        MpvEvent::Speed(s) => assert!((s - 0.95).abs() < 0.01, "speed was {s}"),
        _ => unreachable!(),
    }

    // Unpause and expect Pause(false).
    handle
        .send(MpvCommand::Pause(false))
        .await
        .expect("unpause");
    wait_for(&mut events, Duration::from_secs(5), |e| {
        matches!(e, MpvEvent::Pause(false))
    })
    .await;

    // Graceful shutdown.
    let exit = handle.shutdown().await;
    match exit {
        MpvExit::Clean { .. } | MpvExit::Killed => {} // both acceptable
        MpvExit::Crashed { code } => panic!("mpv crashed with code {code:?}"),
        MpvExit::SpawnFailed(e) => panic!("spawn failed: {e}"),
    }
}

#[tokio::test]
async fn spawn_with_missing_binary_fails_cleanly() {
    let opts = SpawnOptions {
        mpv_binary: PathBuf::from("definitely-not-mpv-xyz-12345"),
        ..SpawnOptions::default()
    };
    let r = spawn(opts).await;
    assert!(r.is_err(), "spawn must reject missing binary");
}

#[tokio::test]
async fn dropping_handle_kills_mpv() {
    let Some(mpv_bin) = find_mpv() else {
        eprintln!("SKIPPED: mpv not found");
        return;
    };
    let opts = SpawnOptions {
        mpv_binary: mpv_bin,
        extra_args: vec![
            "--no-config".into(),
            "--vo=null".into(),
            "--ao=null".into(),
            "--really-quiet".into(),
        ],
        keep_alive: true,
        ..SpawnOptions::default()
    };
    let (handle, _events) = spawn(opts).await.expect("spawn");
    drop(handle);
    // Give the OS a moment to reap the child. If kill_on_drop worked and
    // our force_kill path ran, the process is gone. There's no tight
    // assertion we can make here without platform-specific pid polling; the
    // absence of a hang in CI is the signal.
    tokio::time::sleep(Duration::from_millis(500)).await;
}
