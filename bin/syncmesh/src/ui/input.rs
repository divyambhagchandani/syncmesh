//! Chat input state: buffer + cursor operations.
//!
//! Intentionally tiny — no history, no multi-line, no IME. The TUI is a
//! single-line input; everything more sophisticated is out of scope for v1
//! (decision 19). All mutation happens through explicit methods so keybind
//! translation stays pure.

#[derive(Debug, Default, Clone)]
pub struct ChatInput {
    buffer: String,
}

impl ChatInput {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn append(&mut self, c: char) {
        self.buffer.push(c);
    }

    /// Remove the last character from the buffer. No-op on empty.
    pub fn backspace(&mut self) {
        self.buffer.pop();
    }

    /// Delete the trailing word — trailing whitespace followed by the final
    /// run of non-whitespace. Matches readline's Ctrl-W.
    pub fn word_delete(&mut self) {
        while self.buffer.ends_with(' ') {
            self.buffer.pop();
        }
        while let Some(ch) = self.buffer.chars().last() {
            if ch.is_whitespace() {
                break;
            }
            self.buffer.pop();
        }
    }

    /// Take the current contents, leaving the buffer empty. Trims trailing
    /// whitespace; returns `None` if the trimmed text is empty so callers
    /// don't submit blank chat messages.
    pub fn take(&mut self) -> Option<String> {
        let trimmed = self.buffer.trim().to_string();
        self.buffer.clear();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.buffer
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_backspace() {
        let mut i = ChatInput::new();
        i.append('h');
        i.append('i');
        assert_eq!(i.as_str(), "hi");
        i.backspace();
        assert_eq!(i.as_str(), "h");
    }

    #[test]
    fn backspace_on_empty_is_noop() {
        let mut i = ChatInput::new();
        i.backspace();
        assert_eq!(i.as_str(), "");
    }

    #[test]
    fn word_delete_trims_trailing_word() {
        let mut i = ChatInput::new();
        for c in "hello world".chars() {
            i.append(c);
        }
        i.word_delete();
        assert_eq!(i.as_str(), "hello ");
    }

    #[test]
    fn word_delete_trims_trailing_whitespace_first() {
        let mut i = ChatInput::new();
        for c in "hello   ".chars() {
            i.append(c);
        }
        i.word_delete();
        assert_eq!(i.as_str(), "");
    }

    #[test]
    fn word_delete_on_empty_is_noop() {
        let mut i = ChatInput::new();
        i.word_delete();
        assert!(i.is_empty());
    }

    #[test]
    fn take_returns_trimmed_and_clears() {
        let mut i = ChatInput::new();
        for c in "  hello  ".chars() {
            i.append(c);
        }
        assert_eq!(i.take(), Some("hello".into()));
        assert!(i.is_empty());
    }

    #[test]
    fn take_on_whitespace_only_returns_none() {
        let mut i = ChatInput::new();
        for c in "    ".chars() {
            i.append(c);
        }
        assert!(i.take().is_none());
        assert!(i.is_empty());
    }
}
