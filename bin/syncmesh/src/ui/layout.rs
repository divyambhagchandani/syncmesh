//! Widget tree and render logic for the syncmesh TUI.
//!
//! The layout in words (decision 19):
//! ```text
//! ┌ status bar ─────────────────────────────────────────┐
//! │ peers │ chat                                         │
//! │       │                                              │
//! ├ input ──────────────────────────────────────────────┤
//! │ keyhints                                             │
//! └─────────────────────────────────────────────────────┘
//! ```
//! The function `render` takes a snapshot + UI-local state and writes
//! widgets onto the provided frame. It is deliberately pure (no I/O, no
//! async) so tests can drive it against a `TestBackend`.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use syncmesh_core::{ReadyState, RoomSnapshot};

use super::input::ChatInput;
use super::keybinds::Mode;

/// UI-local state not owned by `RoomSnapshot`: input mode, input buffer,
/// ephemeral flash messages, chat scroll offset.
///
/// `chat_scroll` is the number of lines the user has scrolled *up* from the
/// bottom of the chat pane. 0 means pinned to the newest message; higher
/// values reveal older messages. Follow mode (`chat_follow`) keeps the user
/// pinned to the bottom automatically when new messages arrive; any `PgUp`
/// disables it, `End` / `PgDn`-past-bottom re-enables it.
#[derive(Debug, Default)]
pub struct UiState {
    pub mode: Mode,
    pub input: ChatInput,
    pub flash: Option<String>,
    pub chat_scroll: u16,
    pub chat_follow: bool,
    /// Observability pane (Ctrl-D). Hidden by default; visible when the
    /// user is tuning drift tiers or hunting mystery RTT spikes.
    pub debug_visible: bool,
}

impl UiState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            chat_follow: true,
            ..Self::default()
        }
    }
}

pub fn render(frame: &mut Frame<'_>, snapshot: &RoomSnapshot, ui: &UiState) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // status bar
            Constraint::Min(3),    // body
            Constraint::Length(1), // input line
            Constraint::Length(1), // keyhints
        ])
        .split(area);

    render_status_bar(frame, rows[0], snapshot);

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(32), Constraint::Min(10)])
        .split(rows[1]);
    render_peers(frame, body[0], snapshot);
    render_chat(frame, body[1], snapshot, ui);

    render_input(frame, rows[2], ui);
    render_keyhints(frame, rows[3], ui);

    if ui.debug_visible {
        render_debug_overlay(frame, area, snapshot);
    }
    if ui.mode == Mode::Help {
        render_help_overlay(frame, area);
    }
}

fn render_debug_overlay(frame: &mut Frame<'_>, area: Rect, snap: &RoomSnapshot) {
    // Bottom-right, ~36 cols × (peers + 3) rows. Clamped to the available
    // area so small terminals don't render off-screen.
    let h = u16::try_from(snap.peers.len())
        .unwrap_or(u16::MAX)
        .saturating_add(3);
    let h = h.min(area.height.saturating_sub(2)).max(3);
    let w = 42u16.min(area.width.saturating_sub(2));
    let x = area.right().saturating_sub(w + 1);
    let y = area.bottom().saturating_sub(h + 2); // leave room for input + keyhints
    let popup = Rect::new(x, y, w, h);

    let mut lines: Vec<Line> = Vec::with_capacity(snap.peers.len() + 2);
    lines.push(Line::from(Span::styled(
        format!(
            "peers: {}   ready: {}   override: {}",
            snap.peers.len() + 1,
            match snap.ready_state {
                ReadyState::AllReady => "all",
                ReadyState::Pending => "pending",
            },
            if snap.override_enabled { "on" } else { "off" },
        ),
        Style::default().fg(Color::Cyan),
    )));
    lines.push(Line::from(Span::raw(
        "──────────────────────────────────────",
    )));
    for peer in &snap.peers {
        let rtt = peer
            .rtt_ms
            .map_or("  --".to_string(), |m| format!("{m:>4}"));
        let drift = peer
            .drift_ms
            .map_or("     ".to_string(), |d| format!("{d:+5}"));
        let nick: String = peer.nickname.chars().take(16).collect();
        lines.push(Line::from(Span::raw(format!(
            "{nick:<16}  rtt {rtt}ms  drift {drift}ms"
        ))));
    }

    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title("debug (Ctrl-D)")
        .borders(Borders::ALL);
    let para = Paragraph::new(lines).block(block);
    frame.render_widget(para, popup);
}

fn render_status_bar(frame: &mut Frame<'_>, area: Rect, snap: &RoomSnapshot) {
    let file = snap
        .local
        .media
        .as_ref()
        .map_or("no media", |m| m.filename_lower.as_str());
    let pos = format_ms(snap.local.playback.media_pos_ms);
    let dur = snap.local.media.as_ref().map_or("--:--".to_string(), |m| {
        format_secs(u64::from(m.duration_s))
    });
    let state = if snap.local.playback.paused {
        "paused"
    } else {
        "playing"
    };
    let peer_count = snap.peers.len() + 1;
    let gate = match snap.ready_state {
        ReadyState::AllReady => "all ready",
        ReadyState::Pending => "waiting",
    };
    let text = format!("syncmesh · {file} · {pos} / {dur} · {state} · {peer_count} peers · {gate}");
    let paragraph = Paragraph::new(text).style(Style::default().add_modifier(Modifier::BOLD));
    frame.render_widget(paragraph, area);
}

fn render_peers(frame: &mut Frame<'_>, area: Rect, snap: &RoomSnapshot) {
    let mut items: Vec<ListItem> = Vec::with_capacity(1 + snap.peers.len());

    // Local peer first.
    items.push(ListItem::new(Line::from(vec![
        Span::styled(
            ready_glyph(snap.local.ready),
            Style::default().fg(if snap.local.ready {
                Color::Green
            } else {
                Color::Yellow
            }),
        ),
        Span::raw(" "),
        Span::styled(
            format!("{} (me)", snap.local.nickname),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ])));
    items.push(ListItem::new(Line::from(Span::styled(
        "    --     --ms",
        Style::default().fg(Color::DarkGray),
    ))));

    for peer in &snap.peers {
        items.push(ListItem::new(Line::from(vec![
            Span::styled(
                ready_glyph(peer.ready),
                Style::default().fg(if peer.ready {
                    Color::Green
                } else {
                    Color::Yellow
                }),
            ),
            Span::raw(" "),
            Span::raw(peer.nickname.clone()),
        ])));
        let rtt = peer.rtt_ms.map_or("--ms".to_string(), |m| format!("{m}ms"));
        let drift = peer
            .drift_ms
            .map_or("     ".to_string(), |d| format!("{d:+5}ms"));
        items.push(ListItem::new(Line::from(Span::styled(
            format!("    {rtt:>4}  {drift}"),
            Style::default().fg(Color::DarkGray),
        ))));
    }

    let block = Block::default().borders(Borders::RIGHT).title("Peers");
    let list = List::new(items).block(block);
    frame.render_widget(list, area);
}

fn render_chat(frame: &mut Frame<'_>, area: Rect, snap: &RoomSnapshot, ui: &UiState) {
    let lines: Vec<Line> = snap
        .chat
        .iter()
        .map(|m| {
            let who = if m.origin == snap.local.node {
                "you".to_string()
            } else {
                // Prefer nickname for known peers; fall back to hex abbreviation.
                snap.peers
                    .iter()
                    .find(|p| p.node == m.origin)
                    .map_or_else(|| format!("{:?}", m.origin), |p| p.nickname.clone())
            };
            Line::from(vec![
                Span::styled(format!("{who:>10}: "), Style::default().fg(Color::Cyan)),
                Span::raw(m.text.clone()),
            ])
        })
        .collect();

    // Translate the "lines scrolled up from bottom" convention stored in
    // `ui.chat_scroll` into the top-anchored offset ratatui wants. If there
    // are more messages than fit, follow-mode or a non-zero chat_scroll keeps
    // the bottom visible; if they all fit we pass 0.
    let max_offset = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_sub(area.height);
    let scroll_from_top = if ui.chat_follow {
        max_offset
    } else {
        max_offset.saturating_sub(ui.chat_scroll)
    };

    let block = Block::default().borders(Borders::NONE).title("Chat");
    let para = Paragraph::new(lines)
        .block(block)
        .scroll((scroll_from_top, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

fn render_input(frame: &mut Frame<'_>, area: Rect, ui: &UiState) {
    let (prefix, text) = match ui.mode {
        Mode::Chat => ("> ", ui.input.as_str()),
        Mode::Normal => {
            if let Some(flash) = ui.flash.as_deref() {
                ("! ", flash)
            } else {
                ("  ", "")
            }
        }
        Mode::Help => ("  ", ""),
    };
    let style = if ui.mode == Mode::Chat {
        Style::default().fg(Color::White)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let para = Paragraph::new(Line::from(vec![
        Span::styled(prefix, style),
        Span::raw(text),
    ]));
    frame.render_widget(para, area);
}

fn render_keyhints(frame: &mut Frame<'_>, area: Rect, ui: &UiState) {
    let hints = match ui.mode {
        Mode::Normal => {
            "r ready   c copy   space pause   / chat   tab override   ctrl-d debug   ? help   q quit"
        }
        Mode::Chat => "enter send   esc cancel   ctrl-w delete word",
        Mode::Help => "press any key to close",
    };
    let para = Paragraph::new(hints).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(para, area);
}

fn render_help_overlay(frame: &mut Frame<'_>, area: Rect) {
    let popup = centered_rect(60, 40, area);
    let text = vec![
        Line::from("syncmesh keybindings"),
        Line::from(""),
        Line::from("  r         toggle ready"),
        Line::from("  c         copy room ticket to clipboard"),
        Line::from("  space     toggle pause for the mesh"),
        Line::from("  /         enter chat mode"),
        Line::from("  tab       toggle ready-gate override"),
        Line::from("  ctrl-d    toggle debug pane (RTT / drift)"),
        Line::from("  ?         show this help"),
        Line::from("  q         quit"),
        Line::from(""),
        Line::from("  chat mode: enter to send, esc to cancel, ctrl-w word delete"),
    ];
    frame.render_widget(Clear, popup);
    let block = Block::default().title("help").borders(Borders::ALL);
    let para = Paragraph::new(text).block(block);
    frame.render_widget(para, popup);
}

fn centered_rect(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn ready_glyph(ready: bool) -> &'static str {
    if ready { "●" } else { "○" }
}

fn format_ms(ms: u64) -> String {
    format_secs(ms / 1000)
}

fn format_secs(total: u64) -> String {
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use syncmesh_core::{ChatMessage, LocalSnapshot, NodeId, PeerSnapshot, PlaybackState};

    fn make_snapshot(nickname: &str, with_peers: bool, with_chat: bool) -> RoomSnapshot {
        let local = LocalSnapshot {
            node: NodeId::from_bytes([1u8; 32]),
            nickname: nickname.into(),
            ready: true,
            playback: PlaybackState {
                media_pos_ms: 125_000,
                paused: false,
                speed_centi: 100,
            },
            media: None,
        };
        let peers = if with_peers {
            vec![PeerSnapshot {
                node: NodeId::from_bytes([2u8; 32]),
                nickname: "alice".into(),
                ready: false,
                rtt_ms: Some(42),
                drift_ms: Some(-30),
                media_match: None,
            }]
        } else {
            Vec::new()
        };
        let chat = if with_chat {
            vec![ChatMessage {
                origin: NodeId::from_bytes([2u8; 32]),
                origin_ts_ms: 0,
                text: "hi there".into(),
            }]
        } else {
            Vec::new()
        };
        RoomSnapshot {
            local,
            peers,
            chat,
            ready_state: ReadyState::Pending,
            override_enabled: false,
        }
    }

    fn render_to_buffer(snap: &RoomSnapshot, ui: &UiState) -> String {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| super::render(f, snap, ui)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn status_bar_contains_nickname_and_peer_count() {
        let snap = make_snapshot("me", true, false);
        let ui = UiState::new();
        let text = render_to_buffer(&snap, &ui);
        assert!(text.contains("syncmesh"));
        assert!(text.contains("2 peers"));
    }

    #[test]
    fn peer_pane_shows_nickname() {
        let snap = make_snapshot("me", true, false);
        let ui = UiState::new();
        let text = render_to_buffer(&snap, &ui);
        assert!(text.contains("alice"), "expected alice in:\n{text}");
        assert!(text.contains("me (me)"), "expected local row in:\n{text}");
    }

    #[test]
    fn chat_pane_renders_messages() {
        let snap = make_snapshot("me", true, true);
        let ui = UiState::new();
        let text = render_to_buffer(&snap, &ui);
        assert!(text.contains("hi there"), "expected chat text in:\n{text}");
        assert!(text.contains("alice"));
    }

    #[test]
    fn help_overlay_appears_in_help_mode() {
        let snap = make_snapshot("me", false, false);
        let mut ui = UiState::new();
        ui.mode = Mode::Help;
        let text = render_to_buffer(&snap, &ui);
        assert!(text.contains("syncmesh keybindings"));
        assert!(text.contains("toggle ready"));
    }

    #[test]
    fn chat_mode_shows_input_prompt() {
        let snap = make_snapshot("me", false, false);
        let mut ui = UiState::new();
        ui.mode = Mode::Chat;
        for c in "hello".chars() {
            ui.input.append(c);
        }
        let text = render_to_buffer(&snap, &ui);
        assert!(text.contains("> hello"), "expected > hello in:\n{text}");
        assert!(text.contains("enter send"));
    }

    #[test]
    fn debug_overlay_renders_when_visible() {
        let snap = make_snapshot("me", true, false);
        let mut ui = UiState::new();
        ui.debug_visible = true;
        let text = render_to_buffer(&snap, &ui);
        assert!(
            text.contains("debug (Ctrl-D)"),
            "expected debug overlay title in:\n{text}"
        );
        // Peer row shows the rtt we synthesized (42).
        assert!(
            text.contains("42"),
            "expected rtt=42 in debug pane:\n{text}"
        );
        assert!(
            text.contains("alice"),
            "expected peer nickname in debug pane:\n{text}"
        );
    }

    #[test]
    fn debug_overlay_hidden_by_default() {
        let snap = make_snapshot("me", true, false);
        let ui = UiState::new();
        let text = render_to_buffer(&snap, &ui);
        assert!(!text.contains("debug (Ctrl-D)"));
    }

    #[test]
    fn large_terminal_size_does_not_panic() {
        let snap = make_snapshot("me", true, true);
        let ui = UiState::new();
        let backend = TestBackend::new(200, 60);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| super::render(f, &snap, &ui)).unwrap();
    }

    #[test]
    fn small_terminal_size_does_not_panic() {
        let snap = make_snapshot("me", true, true);
        let ui = UiState::new();
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| super::render(f, &snap, &ui)).unwrap();
    }

    #[test]
    fn format_ms_handles_hours() {
        assert_eq!(format_ms(1000), "00:01");
        assert_eq!(format_ms(61_000), "01:01");
        assert_eq!(format_ms(3_601_000), "01:00:01");
    }
}
