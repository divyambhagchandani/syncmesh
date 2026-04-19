//! Events flowing out of mpv, translated from the JSON IPC event stream.
//!
//! The subscribed property set is the one listed in plan decision 18:
//! `pause`, `time-pos`, `seeking`, `playback-restart`, `eof-reached`, `speed`,
//! `filename`, `duration`, `file-size`. Other properties arrive on the wire
//! (mpv is chatty) and are ignored here — callers only see the nine signals
//! the sync layer actually cares about.

/// A semantically-meaningful event from mpv. Every variant corresponds to one
/// of the property-changes we observe (decision 18).
#[derive(Debug, Clone, PartialEq)]
pub enum MpvEvent {
    /// `pause` property toggled.
    Pause(bool),
    /// `time-pos` property moved (in seconds). mpv emits this roughly every
    /// 100 ms during normal playback.
    TimePos(f64),
    /// mpv started a seek — the `time-pos` that follows should be treated as
    /// authoritative, but `time-pos` values seen *during* a seek can be
    /// misleading.
    Seeking,
    /// mpv finished the last seek / file load and playback is live again.
    PlaybackRestart,
    /// End of file reached on the current track.
    EofReached,
    /// Playback speed changed (1.0 == normal).
    Speed(f64),
    /// A new file was loaded. `name` is mpv's `filename` property (basename
    /// only).
    Filename(String),
    /// Duration of the currently loaded file, in seconds.
    Duration(f64),
    /// Size of the currently loaded file, in bytes.
    FileSize(u64),
    /// mpv exited (process death, not a property change). Terminal event on
    /// the stream; no further events follow.
    Shutdown,
}

/// Try to translate a single mpv event frame into an [`MpvEvent`].
///
/// `frame` must be a parsed JSON object; we tolerate unexpected / unknown
/// events by returning `None` instead of erroring so the IPC reader doesn't
/// die on a new mpv release that adds a new event type.
pub(crate) fn translate(frame: &serde_json::Value) -> Option<MpvEvent> {
    let obj = frame.as_object()?;
    let event = obj.get("event")?.as_str()?;
    match event {
        // property-change for an observed property
        "property-change" => translate_property_change(obj),
        "seek" => Some(MpvEvent::Seeking),
        "playback-restart" => Some(MpvEvent::PlaybackRestart),
        "end-file" => {
            // mpv's `end-file` carries a `reason`. We only surface EOF for
            // natural end-of-file; other reasons (error, quit, stop) are not
            // relevant to sync.
            let reason = obj.get("reason").and_then(|v| v.as_str()).unwrap_or("");
            if reason == "eof" {
                Some(MpvEvent::EofReached)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn translate_property_change(obj: &serde_json::Map<String, serde_json::Value>) -> Option<MpvEvent> {
    let name = obj.get("name")?.as_str()?;
    // `data` may be null when the property is currently unavailable (e.g. no
    // file loaded yet). Treat that as "no event" rather than synthesizing a
    // default value.
    let data = obj.get("data")?;
    if data.is_null() {
        return None;
    }
    match name {
        "pause" => data.as_bool().map(MpvEvent::Pause),
        "time-pos" => data.as_f64().map(MpvEvent::TimePos),
        "seeking" => {
            if data.as_bool().unwrap_or(false) {
                Some(MpvEvent::Seeking)
            } else {
                None
            }
        }
        "eof-reached" => {
            if data.as_bool().unwrap_or(false) {
                Some(MpvEvent::EofReached)
            } else {
                None
            }
        }
        "speed" => data.as_f64().map(MpvEvent::Speed),
        "filename" => data.as_str().map(|s| MpvEvent::Filename(s.to_owned())),
        "duration" => data.as_f64().map(MpvEvent::Duration),
        "file-size" => data.as_u64().map(MpvEvent::FileSize),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pause_true_is_translated() {
        let v = json!({"event": "property-change", "id": 1, "name": "pause", "data": true});
        assert_eq!(translate(&v), Some(MpvEvent::Pause(true)));
    }

    #[test]
    fn pause_false_is_translated() {
        let v = json!({"event": "property-change", "name": "pause", "data": false});
        assert_eq!(translate(&v), Some(MpvEvent::Pause(false)));
    }

    #[test]
    fn time_pos_is_translated_as_float() {
        let v = json!({"event": "property-change", "name": "time-pos", "data": 42.5});
        assert_eq!(translate(&v), Some(MpvEvent::TimePos(42.5)));
    }

    #[test]
    fn null_data_on_property_change_yields_none() {
        let v = json!({"event": "property-change", "name": "time-pos", "data": null});
        assert_eq!(translate(&v), None);
    }

    #[test]
    fn speed_translates_to_f64() {
        let v = json!({"event": "property-change", "name": "speed", "data": 1.05});
        assert_eq!(translate(&v), Some(MpvEvent::Speed(1.05)));
    }

    #[test]
    fn filename_translates_to_string() {
        let v = json!({"event": "property-change", "name": "filename", "data": "movie.mkv"});
        assert_eq!(
            translate(&v),
            Some(MpvEvent::Filename("movie.mkv".to_string()))
        );
    }

    #[test]
    fn duration_translates_to_f64() {
        let v = json!({"event": "property-change", "name": "duration", "data": 3600.0});
        assert_eq!(translate(&v), Some(MpvEvent::Duration(3600.0)));
    }

    #[test]
    fn file_size_translates_to_u64() {
        let v = json!({"event": "property-change", "name": "file-size", "data": 1_234_567_890u64});
        assert_eq!(translate(&v), Some(MpvEvent::FileSize(1_234_567_890)));
    }

    #[test]
    fn seeking_event_is_translated() {
        assert_eq!(
            translate(&json!({"event": "seek"})),
            Some(MpvEvent::Seeking)
        );
    }

    #[test]
    fn playback_restart_event_is_translated() {
        assert_eq!(
            translate(&json!({"event": "playback-restart"})),
            Some(MpvEvent::PlaybackRestart)
        );
    }

    #[test]
    fn end_file_eof_is_translated() {
        assert_eq!(
            translate(&json!({"event": "end-file", "reason": "eof"})),
            Some(MpvEvent::EofReached)
        );
    }

    #[test]
    fn end_file_with_other_reason_is_ignored() {
        assert_eq!(
            translate(&json!({"event": "end-file", "reason": "quit"})),
            None
        );
        assert_eq!(
            translate(&json!({"event": "end-file", "reason": "error"})),
            None
        );
    }

    #[test]
    fn unknown_property_yields_none() {
        let v = json!({"event": "property-change", "name": "volume", "data": 50});
        assert_eq!(translate(&v), None);
    }

    #[test]
    fn unknown_event_yields_none() {
        assert_eq!(translate(&json!({"event": "idle"})), None);
    }

    #[test]
    fn command_reply_yields_none() {
        // Command replies have request_id and no "event" field.
        let v = json!({"request_id": 1, "error": "success", "data": null});
        assert_eq!(translate(&v), None);
    }

    #[test]
    fn seeking_false_data_yields_none() {
        // `seeking` property flips to false when the seek completes; that's
        // conveyed by PlaybackRestart elsewhere, so we skip the false edge.
        let v = json!({"event": "property-change", "name": "seeking", "data": false});
        assert_eq!(translate(&v), None);
    }
}
