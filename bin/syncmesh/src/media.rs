//! Coalesce mpv's per-property media events into a `MediaId`.
//!
//! mpv reports `filename`, `duration`, and `file-size` as independent
//! property-change events. The sync layer wants a single `MediaId` tuple, so
//! we buffer the three fields and emit exactly one `MediaId` per distinct
//! file — once all three are known and at least one has changed since the
//! last emission.
//!
//! A fresh `filename` resets the duration and size: we're loading a new file
//! and the stale duration/size should not leak into the next `MediaId`.

use syncmesh_core::MediaId;

#[derive(Debug, Default)]
pub struct MediaCollector {
    filename: Option<String>,
    duration_s: Option<u32>,
    file_size: Option<u64>,
    last_emitted: Option<MediaId>,
}

impl MediaCollector {
    pub const fn new() -> Self {
        Self {
            filename: None,
            duration_s: None,
            file_size: None,
            last_emitted: None,
        }
    }

    /// Record a new filename (basename). If it differs from the current one,
    /// clear duration + size because they belong to the old file.
    pub fn on_filename(&mut self, name: &str) {
        let lower = name.to_lowercase();
        if self.filename.as_deref() != Some(&lower) {
            self.filename = Some(lower);
            self.duration_s = None;
            self.file_size = None;
        }
    }

    pub fn on_duration_secs(&mut self, secs: f64) {
        // Round to nearest whole second, per MediaId's contract. Clamp to
        // [0, u32::MAX] to avoid a silent wraparound on absurd values.
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let s = secs.max(0.0).round().min(f64::from(u32::MAX)) as u32;
        self.duration_s = Some(s);
    }

    pub fn on_file_size(&mut self, bytes: u64) {
        self.file_size = Some(bytes);
    }

    /// If all three fields are populated and differ from the last emission,
    /// return the new `MediaId` and remember it. Otherwise return `None`.
    pub fn take_if_changed(&mut self) -> Option<MediaId> {
        let (filename, duration_s, size_bytes) =
            match (&self.filename, self.duration_s, self.file_size) {
                (Some(f), Some(d), Some(s)) => (f.clone(), d, s),
                _ => return None,
            };
        let candidate = MediaId {
            filename_lower: filename,
            size_bytes,
            duration_s,
        };
        if Some(&candidate) == self.last_emitted.as_ref() {
            return None;
        }
        self.last_emitted = Some(candidate.clone());
        Some(candidate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_once_all_three_fields_present() {
        let mut c = MediaCollector::new();
        assert!(c.take_if_changed().is_none());
        c.on_filename("Movie.MKV");
        assert!(c.take_if_changed().is_none());
        c.on_duration_secs(3_600.4);
        assert!(c.take_if_changed().is_none());
        c.on_file_size(1_000_000);
        let m = c.take_if_changed().expect("complete tuple must emit");
        assert_eq!(m.filename_lower, "movie.mkv");
        assert_eq!(m.duration_s, 3_600);
        assert_eq!(m.size_bytes, 1_000_000);
    }

    #[test]
    fn does_not_re_emit_unchanged_tuple() {
        let mut c = MediaCollector::new();
        c.on_filename("a.mkv");
        c.on_duration_secs(10.0);
        c.on_file_size(100);
        assert!(c.take_if_changed().is_some());
        assert!(c.take_if_changed().is_none());
    }

    #[test]
    fn new_filename_clears_duration_and_size() {
        let mut c = MediaCollector::new();
        c.on_filename("a.mkv");
        c.on_duration_secs(10.0);
        c.on_file_size(100);
        assert!(c.take_if_changed().is_some());

        // New file: filename changes first. Until duration+size re-arrive we
        // should not emit the stale tuple under the new filename.
        c.on_filename("b.mkv");
        assert!(c.take_if_changed().is_none());
        c.on_duration_secs(20.0);
        assert!(c.take_if_changed().is_none());
        c.on_file_size(200);
        let m = c
            .take_if_changed()
            .expect("second file emits once complete");
        assert_eq!(m.filename_lower, "b.mkv");
        assert_eq!(m.duration_s, 20);
        assert_eq!(m.size_bytes, 200);
    }

    #[test]
    fn filename_lowercases_and_rejects_duplicate_same_name() {
        let mut c = MediaCollector::new();
        c.on_filename("FILE.MKV");
        c.on_duration_secs(1.0);
        c.on_file_size(1);
        let m = c.take_if_changed().unwrap();
        assert_eq!(m.filename_lower, "file.mkv");

        // Re-report the same name in mixed case: no reset, still no re-emit.
        c.on_filename("file.mkv");
        assert!(c.take_if_changed().is_none());
    }

    #[test]
    fn negative_duration_clamps_to_zero() {
        let mut c = MediaCollector::new();
        c.on_filename("a.mkv");
        c.on_duration_secs(-5.0);
        c.on_file_size(1);
        let m = c.take_if_changed().unwrap();
        assert_eq!(m.duration_s, 0);
    }

    #[test]
    fn duration_rounds_to_nearest_second() {
        let mut c = MediaCollector::new();
        c.on_filename("a.mkv");
        c.on_duration_secs(59.6);
        c.on_file_size(1);
        let m = c.take_if_changed().unwrap();
        assert_eq!(m.duration_s, 60);
    }
}
