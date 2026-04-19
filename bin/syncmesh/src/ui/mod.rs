//! TUI entry point: `run_ui` drives the render loop + input dispatch.
//!
//! Owns nothing synchronized; reads a `watch::Receiver<RoomSnapshot>`
//! produced by the event loop, and pushes user-intent events back through
//! an `mpsc::Sender<UiEvent>`. That one-way split keeps mutation centralized
//! in the event loop while the UI is free to re-render at its own cadence.

use std::io::Stdout;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{Event as CtEvent, EventStream, KeyEventKind};
use crossterm::{execute, terminal};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::{mpsc, watch};
use tokio::time::{MissedTickBehavior, interval};
use tracing::debug;

pub mod input;
pub mod keybinds;
pub mod layout;

use keybinds::{KeyAction, Mode, translate};
use layout::UiState;
use syncmesh_core::RoomSnapshot;

/// User-intent events the UI emits for the event loop to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiEvent {
    ToggleReady,
    TogglePauseRelay,
    SubmitChat(String),
    ToggleOverride,
    CopyTicket,
    Quit,
}

/// Tell the UI task what extra context it needs that isn't in `RoomSnapshot`.
#[derive(Debug, Clone)]
pub struct UiContext {
    /// The ticket string for this room, used when the user presses `c` to
    /// copy. `None` when we joined an existing room (ours isn't meaningful).
    pub ticket: Option<String>,
}

type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Lines PgUp/PgDn advance the chat scroll. Kept small; user can hold.
const CHAT_SCROLL_STEP: u16 = 5;

/// Enter raw mode + alternate screen, then run the render + input loop until
/// `Quit` is emitted or the snapshot channel closes.
pub async fn run_ui(
    mut snapshots: watch::Receiver<RoomSnapshot>,
    events: mpsc::Sender<UiEvent>,
    ctx: UiContext,
) -> Result<()> {
    let mut terminal = enter_tui()?;
    let result = render_loop(&mut terminal, &mut snapshots, &events, &ctx).await;
    leave_tui(&mut terminal);
    result
}

fn enter_tui() -> Result<Tui> {
    terminal::enable_raw_mode().context("enable raw mode")?;
    let mut stdout = std::io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).context("build terminal")
}

fn leave_tui(terminal: &mut Tui) {
    terminal::disable_raw_mode().ok();
    execute!(terminal.backend_mut(), terminal::LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();
}

async fn render_loop(
    terminal: &mut Tui,
    snapshots: &mut watch::Receiver<RoomSnapshot>,
    events: &mpsc::Sender<UiEvent>,
    ctx: &UiContext,
) -> Result<()> {
    let mut ui = UiState::new();
    let mut key_stream = EventStream::new();
    // Re-draw at 10 FPS even without signal, so RTT/time-pos tickers age.
    let mut ticker = interval(Duration::from_millis(100));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        {
            let snap = snapshots.borrow().clone();
            terminal.draw(|f| layout::render(f, &snap, &ui))?;
        }

        tokio::select! {
            changed = snapshots.changed() => {
                if changed.is_err() {
                    debug!("snapshot channel closed; exiting UI");
                    break;
                }
            }
            _ = ticker.tick() => {}
            ev = key_stream.next() => {
                match ev {
                    Some(Ok(CtEvent::Key(k))) if k.kind == KeyEventKind::Press => {
                        let action = translate(ui.mode, k);
                        if let Some(outcome) = handle_key(&mut ui, action, ctx) {
                            if events.send(outcome.clone()).await.is_err() {
                                break;
                            }
                            if outcome == UiEvent::Quit {
                                break;
                            }
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        debug!(error = %e, "crossterm read error; exiting UI");
                        break;
                    }
                    None => break,
                }
            }
        }
    }

    Ok(())
}

fn handle_key(ui: &mut UiState, action: KeyAction, ctx: &UiContext) -> Option<UiEvent> {
    match action {
        KeyAction::Ignore => None,
        KeyAction::SetMode(mode) => {
            ui.mode = mode;
            ui.flash = None;
            None
        }
        KeyAction::ChatAppend(c) => {
            ui.input.append(c);
            None
        }
        KeyAction::ChatBackspace => {
            ui.input.backspace();
            None
        }
        KeyAction::ChatWordDelete => {
            ui.input.word_delete();
            None
        }
        KeyAction::ChatSubmit => {
            let text = ui.input.take();
            ui.mode = Mode::Normal;
            text.map(UiEvent::SubmitChat)
        }
        KeyAction::ChatScrollUp => {
            ui.chat_follow = false;
            ui.chat_scroll = ui.chat_scroll.saturating_add(CHAT_SCROLL_STEP);
            None
        }
        KeyAction::ChatScrollDown => {
            ui.chat_scroll = ui.chat_scroll.saturating_sub(CHAT_SCROLL_STEP);
            if ui.chat_scroll == 0 {
                ui.chat_follow = true;
            }
            None
        }
        KeyAction::ChatScrollBottom => {
            ui.chat_scroll = 0;
            ui.chat_follow = true;
            None
        }
        KeyAction::ToggleDebug => {
            ui.debug_visible = !ui.debug_visible;
            None
        }
        KeyAction::Emit(UiEvent::CopyTicket) => {
            let Some(ticket) = ctx.ticket.as_deref() else {
                ui.flash = Some("no ticket to copy".into());
                return None;
            };
            #[cfg(feature = "clipboard")]
            {
                match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(ticket.to_string())) {
                    Ok(()) => ui.flash = Some("ticket copied".into()),
                    Err(_) => ui.flash = Some(format!("clipboard unavailable; ticket: {ticket}")),
                }
            }
            #[cfg(not(feature = "clipboard"))]
            {
                ui.flash = Some(format!("ticket: {ticket}"));
            }
            Some(UiEvent::CopyTicket)
        }
        KeyAction::Emit(ev) => Some(ev),
    }
}
