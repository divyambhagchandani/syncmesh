//! Commands we send to the local mpv.
//!
//! This is a strict superset of [`syncmesh_core::MpvCommand`] — the core crate
//! only knows about commands the sync state machine can emit (pause/seek/speed).
//! The player also needs `LoadFile`, which is driven by the UI and never by
//! inbound peer events.

use std::path::PathBuf;

use syncmesh_core::MpvCommand as CoreMpvCommand;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MpvCommand {
    /// Toggle pause state. `true` = paused.
    Pause(bool),
    /// Absolute seek to the given position.
    Seek { media_pos_ms: u64 },
    /// Playback speed in centiunits — 100 == 1.00x.
    SetSpeed { speed_centi: u16 },
    /// Replace the currently loaded file.
    LoadFile { path: PathBuf },
}

impl From<CoreMpvCommand> for MpvCommand {
    fn from(c: CoreMpvCommand) -> Self {
        match c {
            CoreMpvCommand::Pause(p) => Self::Pause(p),
            CoreMpvCommand::Seek { media_pos_ms } => Self::Seek { media_pos_ms },
            CoreMpvCommand::SetSpeed { speed_centi } => Self::SetSpeed { speed_centi },
        }
    }
}

impl MpvCommand {
    /// Render to the JSON frame mpv expects, without the trailing newline.
    ///
    /// `request_id` is attached so the caller can correlate success/error
    /// replies. mpv echoes the `request_id` back on the response frame.
    pub(crate) fn to_json(&self, request_id: u64) -> serde_json::Value {
        use serde_json::json;
        let args: Vec<serde_json::Value> = match self {
            Self::Pause(p) => vec![json!("set_property"), json!("pause"), json!(*p)],
            Self::Seek { media_pos_ms } => {
                let secs = (*media_pos_ms as f64) / 1000.0;
                vec![json!("seek"), json!(secs), json!("absolute")]
            }
            Self::SetSpeed { speed_centi } => {
                let speed = f64::from(*speed_centi) / 100.0;
                vec![json!("set_property"), json!("speed"), json!(speed)]
            }
            Self::LoadFile { path } => vec![
                json!("loadfile"),
                json!(path.to_string_lossy().into_owned()),
                json!("replace"),
            ],
        };
        json!({ "command": args, "request_id": request_id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pause_true_renders_set_property() {
        let v = MpvCommand::Pause(true).to_json(7);
        assert_eq!(
            v,
            json!({"command": ["set_property", "pause", true], "request_id": 7})
        );
    }

    #[test]
    fn seek_converts_ms_to_seconds() {
        let v = MpvCommand::Seek { media_pos_ms: 12_500 }.to_json(1);
        assert_eq!(
            v,
            json!({"command": ["seek", 12.5, "absolute"], "request_id": 1})
        );
    }

    #[test]
    fn set_speed_converts_centi_to_float() {
        let v = MpvCommand::SetSpeed { speed_centi: 95 }.to_json(2);
        assert_eq!(
            v,
            json!({"command": ["set_property", "speed", 0.95], "request_id": 2})
        );
    }

    #[test]
    fn load_file_renders_replace() {
        let v = MpvCommand::LoadFile {
            path: "C:/videos/a b.mkv".into(),
        }
        .to_json(3);
        assert_eq!(
            v,
            json!({"command": ["loadfile", "C:/videos/a b.mkv", "replace"], "request_id": 3})
        );
    }

    #[test]
    fn from_core_preserves_variants() {
        assert_eq!(
            MpvCommand::from(CoreMpvCommand::Pause(false)),
            MpvCommand::Pause(false)
        );
        assert_eq!(
            MpvCommand::from(CoreMpvCommand::Seek { media_pos_ms: 42 }),
            MpvCommand::Seek { media_pos_ms: 42 }
        );
        assert_eq!(
            MpvCommand::from(CoreMpvCommand::SetSpeed { speed_centi: 105 }),
            MpvCommand::SetSpeed { speed_centi: 105 }
        );
    }
}
