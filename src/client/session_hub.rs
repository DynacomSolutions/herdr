use std::collections::HashSet;
use std::io::{self, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use interprocess::local_socket::traits::Stream as _;
use interprocess::TryClone as _;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use crate::ipc::LocalStream;
use crate::protocol::{
    ClientInputEvent, ClientKeybindings, ClientLaunchMode, ClientMessage, ClientMouseKind,
    FrameData, RenderEncoding, ServerMessage, SessionSidebarSummary, SessionSummary,
    MAX_FRAME_SIZE, MAX_RENDER_FRAME_SIZE,
};

use super::{ClientError, ClientLoopEvent};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteSessionDescriptor {
    pub(crate) name: String,
    pub(crate) running: bool,
}

pub(crate) trait SessionHubBackend {
    fn sessions(&self) -> &[RemoteSessionDescriptor];
    fn connect_client(&mut self, session: &str) -> io::Result<LocalStream>;
    fn focus_workspace(&self, session: &str, workspace_id: &str) -> io::Result<()>;
    fn create_session(&mut self, session: &str) -> io::Result<()>;
    fn rename_session(&mut self, session: &str, new_name: &str) -> io::Result<()>;
    fn close_session(&mut self, session: &str) -> io::Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HubStreamKind {
    App,
    Sidebar,
    Summary,
}

struct HubSession {
    name: String,
    running: bool,
    summary: Option<SessionSummary>,
    summary_generation: u64,
    summary_stream: Option<LocalStream>,
    sidebar_generation: u64,
    sidebar_stream: Option<LocalStream>,
    sidebar_frame: Option<FrameData>,
}

struct ActiveSession {
    name: String,
    generation: u64,
    stream: LocalStream,
    frame: Option<FrameData>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HubRowTarget {
    Session(String),
    SpaceRow {
        session: String,
        workspace_id: String,
        original_y: u16,
    },
    SpaceGapRow {
        session: String,
        original_y: u16,
    },
    NewSession,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HubRow {
    y: u16,
    target: HubRowTarget,
}

struct NewSessionModal {
    name: String,
    error: Option<String>,
}

enum SessionModal {
    Create(NewSessionModal),
    Rename {
        session: String,
        name: String,
        error: Option<String>,
    },
    ConfirmClose {
        session: String,
        error: Option<String>,
    },
}

struct SessionContextMenu {
    session: String,
    x: u16,
    y: u16,
    highlighted: usize,
}

struct HubState {
    sessions: Vec<HubSession>,
    collapsed: HashSet<String>,
    active: ActiveSession,
    next_generation: u64,
    modal: Option<SessionModal>,
    session_menu: Option<SessionContextMenu>,
    status: Option<String>,
    cols: u16,
    rows: u16,
    repaint: bool,
    full_repaint: bool,
    keybindings: ClientKeybindings,
    remote_image_paste_key: Option<(crossterm::event::KeyCode, KeyModifiers)>,
    server_overlay_session: Option<String>,
    server_overlay_seen: bool,
}

impl HubState {
    fn active_summary(&self) -> Option<&SessionSummary> {
        self.sessions
            .iter()
            .find(|session| session.name == self.active.name)
            .and_then(|session| session.summary.as_ref())
    }

    fn sidebar_layout(&self) -> Option<&SessionSidebarSummary> {
        self.active_summary()?.sidebar.as_ref()
    }

    fn sidebar_width(&self) -> u16 {
        self.sidebar_layout().map_or(0, |layout| layout.width)
    }

    fn rows(&self) -> Vec<HubRow> {
        let Some(layout) = self.sidebar_layout() else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        let mut y = layout.spaces_y.saturating_add(1);
        let new_session_y = layout
            .footer_y
            .min(layout.spaces_y.saturating_add(layout.spaces_height))
            .saturating_sub(1);
        for session in &self.sessions {
            if y >= new_session_y {
                break;
            }
            rows.push(HubRow {
                y,
                target: HubRowTarget::Session(session.name.clone()),
            });
            y = y.saturating_add(1);
            if self.collapsed.contains(&session.name) {
                continue;
            }
            if session.name == self.active.name {
                for (index, card) in layout.workspace_cards.iter().enumerate() {
                    for offset in 0..card.height {
                        if y >= new_session_y {
                            break;
                        }
                        rows.push(HubRow {
                            y,
                            target: HubRowTarget::SpaceRow {
                                session: session.name.clone(),
                                workspace_id: card.workspace_id.clone(),
                                original_y: card.y.saturating_add(offset),
                            },
                        });
                        y = y.saturating_add(1);
                    }
                    if let Some(next) = layout.workspace_cards.get(index + 1) {
                        for original_y in card.y.saturating_add(card.height)..next.y {
                            if y >= new_session_y {
                                break;
                            }
                            rows.push(HubRow {
                                y,
                                target: HubRowTarget::SpaceGapRow {
                                    session: session.name.clone(),
                                    original_y,
                                },
                            });
                            y = y.saturating_add(1);
                        }
                    }
                }
                continue;
            }
            if let Some(layout) = session
                .summary
                .as_ref()
                .and_then(|summary| summary.sidebar.as_ref())
            {
                for (index, card) in layout.workspace_cards.iter().enumerate() {
                    if y >= new_session_y {
                        break;
                    }
                    for offset in 0..card.height {
                        if y >= new_session_y {
                            break;
                        }
                        rows.push(HubRow {
                            y,
                            target: HubRowTarget::SpaceRow {
                                session: session.name.clone(),
                                workspace_id: card.workspace_id.clone(),
                                original_y: card.y.saturating_add(offset),
                            },
                        });
                        y = y.saturating_add(1);
                    }
                    if let Some(next) = layout.workspace_cards.get(index + 1) {
                        for original_y in card.y.saturating_add(card.height)..next.y {
                            if y >= new_session_y {
                                break;
                            }
                            rows.push(HubRow {
                                y,
                                target: HubRowTarget::SpaceGapRow {
                                    session: session.name.clone(),
                                    original_y,
                                },
                            });
                            y = y.saturating_add(1);
                        }
                    }
                }
            }
        }
        if new_session_y > layout.spaces_y && new_session_y < self.rows {
            rows.push(HubRow {
                y: new_session_y,
                target: HubRowTarget::NewSession,
            });
        }
        rows
    }

    fn session_mut(&mut self, name: &str) -> Option<&mut HubSession> {
        self.sessions
            .iter_mut()
            .find(|session| session.name == name)
    }

    fn session_frame(&self, name: &str) -> Option<&FrameData> {
        if name == self.active.name {
            return self.active.frame.as_ref();
        }
        self.sessions
            .iter()
            .find(|session| session.name == name)
            .and_then(|session| session.sidebar_frame.as_ref())
    }
}

pub(crate) fn run_remote_session_hub(
    backend: &mut impl SessionHubBackend,
    initial_session: &str,
    keybindings: ClientKeybindings,
) -> io::Result<()> {
    super::init_logging();
    let loaded_config = crate::config::Config::load();
    crate::terminal_modes::clear_host_mouse_reporting(&mut io::stdout())?;
    let mouse_capture = loaded_config.config.ui.mouse_capture;
    let remote_image_paste_key = match loaded_config.config.remote_image_paste_key() {
        Ok(key) => key,
        Err(diagnostic) => {
            tracing::warn!(%diagnostic, "local remote image paste key config diagnostic");
            None
        }
    };
    let (cols, rows, _, _, _) = super::initial_terminal_geometry(false);

    let mut descriptors = backend.sessions().to_vec();
    if !descriptors
        .iter()
        .any(|session| session.name == initial_session)
    {
        descriptors.push(RemoteSessionDescriptor {
            name: initial_session.to_owned(),
            running: false,
        });
    }
    descriptors.sort_by(
        |left, right| match (left.name.as_str(), right.name.as_str()) {
            (crate::session::DEFAULT_SESSION_NAME, crate::session::DEFAULT_SESSION_NAME) => {
                std::cmp::Ordering::Equal
            }
            (crate::session::DEFAULT_SESSION_NAME, _) => std::cmp::Ordering::Less,
            (_, crate::session::DEFAULT_SESSION_NAME) => std::cmp::Ordering::Greater,
            _ => left.name.cmp(&right.name),
        },
    );

    let mut next_generation = 1_u64;
    let active = connect_active(
        backend,
        initial_session,
        next_generation,
        cols,
        rows,
        None,
        &keybindings,
    )?;
    next_generation = next_generation.saturating_add(1);

    let terminal_guard = super::setup_terminal(mouse_capture).map_err(|err| {
        eprintln!("herdr: failed to set up terminal: {err}");
        err
    })?;
    let panic_resets_modify_other_keys = terminal_guard.reset_modify_other_keys;
    let panic_resets_host_color_scheme_reports = terminal_guard.reset_host_color_scheme_reports;
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        super::restore_terminal_state(
            panic_resets_modify_other_keys,
            panic_resets_host_color_scheme_reports,
        );
        original_hook(info);
    }));

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(io::Error::other)?;
    let should_quit = Arc::new(AtomicBool::new(false));
    let quit_flag = should_quit.clone();
    if let Err(err) = ctrlc::set_handler(move || {
        quit_flag.store(true, Ordering::Release);
    }) {
        tracing::warn!(%err, "failed to install session hub termination handler");
    }

    let sessions = descriptors
        .into_iter()
        .map(|descriptor| HubSession {
            name: descriptor.name,
            running: descriptor.running,
            summary: None,
            summary_generation: 0,
            summary_stream: None,
            sidebar_generation: 0,
            sidebar_stream: None,
            sidebar_frame: None,
        })
        .collect::<Vec<_>>();
    let collapsed = sessions
        .iter()
        .filter(|session| session.name != initial_session)
        .map(|session| session.name.clone())
        .collect();
    let mut state = HubState {
        sessions,
        collapsed,
        active,
        next_generation,
        modal: None,
        session_menu: None,
        status: None,
        cols,
        rows,
        repaint: true,
        full_repaint: true,
        keybindings,
        remote_image_paste_key,
        server_overlay_session: None,
        server_overlay_seen: false,
    };
    if let Some(session) = state.session_mut(initial_session) {
        session.running = true;
    }

    let result = rt.block_on(run_hub_loop(
        backend,
        &mut state,
        should_quit,
        mouse_capture,
        loaded_config.config.ui.redraw_on_focus_gained,
    ));

    drop(terminal_guard);
    rt.shutdown_timeout(Duration::from_millis(100));
    crate::logging::shutdown("client");
    result.map_err(io::Error::other)
}

async fn run_hub_loop(
    backend: &mut impl SessionHubBackend,
    state: &mut HubState,
    should_quit: Arc<AtomicBool>,
    mouse_capture: bool,
    redraw_on_focus_gained: bool,
) -> Result<(), ClientError> {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<ClientLoopEvent>(256);
    let host_mouse_capture_active = Arc::new(AtomicBool::new(mouse_capture));
    let host_sgr_pixels_active = Arc::new(AtomicBool::new(false));
    let reported_cell_size = Arc::new(std::sync::atomic::AtomicU64::new(0));

    let stdin_quit = should_quit.clone();
    let stdin_tx = event_tx.clone();
    let stdin_mouse_capture = host_mouse_capture_active.clone();
    let stdin_sgr_pixels = host_sgr_pixels_active.clone();
    let stdin_direct_response = Arc::new(std::sync::Mutex::new(
        super::direct_graphics::ResponseMatcher::default(),
    ));
    let stdin_direct_response_active = stdin_direct_response
        .lock()
        .map(|matcher| matcher.active_handle())
        .unwrap_or_default();
    std::thread::spawn(move || {
        super::input::stdin_reader_loop(
            stdin_tx,
            &stdin_quit,
            false,
            false,
            stdin_mouse_capture,
            stdin_sgr_pixels,
            stdin_direct_response,
            stdin_direct_response_active,
        );
    });

    let resize_quit = should_quit.clone();
    let resize_tx = event_tx.clone();
    let resize_cell_size = reported_cell_size.clone();
    let initial_cols = state.cols;
    let initial_rows = state.rows;
    std::thread::spawn(move || {
        super::resize_poll_loop(
            resize_tx,
            initial_cols,
            initial_rows,
            0,
            0,
            false,
            &resize_cell_size,
            &resize_quit,
        );
    });

    spawn_hub_reader(
        state
            .active
            .stream
            .try_clone()
            .map_err(ClientError::ConnectionFailed)?,
        event_tx.clone(),
        state.active.name.clone(),
        state.active.generation,
        HubStreamKind::App,
        should_quit.clone(),
    );

    let running_sessions = state
        .sessions
        .iter()
        .filter(|session| session.running)
        .map(|session| session.name.clone())
        .collect::<Vec<_>>();
    for session in running_sessions {
        if let Err(err) = connect_summary(backend, state, &session, &event_tx, should_quit.clone())
        {
            state.status = Some(format!("{session}: {err}"));
        }
    }

    let mut encoder = crate::protocol::render_ansi::BlitEncoder::new();
    render_hub(state, &mut encoder)?;

    while !should_quit.load(Ordering::Acquire) {
        let event = tokio::select! {
            event = event_rx.recv() => event.unwrap_or(ClientLoopEvent::Timer),
            _ = tokio::time::sleep(Duration::from_millis(100)) => ClientLoopEvent::Timer,
        };

        match event {
            ClientLoopEvent::StdinInput(data) => {
                if handle_local_input(backend, state, &event_tx, should_quit.clone(), data)? {
                    state.repaint = true;
                }
            }
            ClientLoopEvent::Resize(cols, rows, _, _) => {
                state.cols = cols;
                state.rows = rows;
                state.repaint = true;
                state.full_repaint = true;
                let message = ClientMessage::Resize {
                    cols,
                    rows,
                    cell_width_px: 0,
                    cell_height_px: 0,
                };
                super::write_to_server(&mut state.active.stream, &message)
                    .map_err(ClientError::ConnectionLost)?;
                for session in &mut state.sessions {
                    if let Some(stream) = session.sidebar_stream.as_mut() {
                        super::write_to_server(stream, &message)
                            .map_err(ClientError::ConnectionLost)?;
                    }
                }
            }
            ClientLoopEvent::HubServerMessage {
                session,
                generation,
                stream_kind,
                message,
            } => match stream_kind {
                HubStreamKind::App
                    if session == state.active.name && generation == state.active.generation =>
                {
                    handle_active_message(
                        state,
                        super::decompress_server_message(message)?,
                        &host_mouse_capture_active,
                        &host_sgr_pixels_active,
                        redraw_on_focus_gained,
                    )?;
                }
                HubStreamKind::Summary => {
                    let overlay_is_owned =
                        state.server_overlay_session.as_deref() == Some(session.as_str());
                    let overlay_was_seen = state.server_overlay_seen;
                    let mut received_overlay_active = None;
                    if let Some(hub_session) = state.session_mut(&session) {
                        if generation == hub_session.summary_generation {
                            if let ServerMessage::SessionSummary(summary) = message {
                                received_overlay_active = Some(summary.overlay_active);
                                hub_session.summary = Some(summary);
                                hub_session.running = true;
                                state.repaint = true;
                            }
                        }
                    }
                    if let Some(overlay_active) = received_overlay_active {
                        if overlay_is_owned && overlay_active {
                            state.server_overlay_seen = true;
                        } else if overlay_is_owned && overlay_was_seen && !overlay_active {
                            state.server_overlay_session = None;
                            state.server_overlay_seen = false;
                        } else if state.server_overlay_session.is_none()
                            && session == state.active.name
                            && overlay_active
                        {
                            state.server_overlay_session = Some(session);
                            state.server_overlay_seen = true;
                        }
                    }
                }
                HubStreamKind::Sidebar => {
                    if let Some(hub_session) = state.session_mut(&session) {
                        if generation == hub_session.sidebar_generation {
                            match super::decompress_server_message(message)? {
                                ServerMessage::Frame(frame) => {
                                    hub_session.sidebar_frame = Some(frame);
                                    state.repaint = true;
                                }
                                ServerMessage::ServerShutdown { .. } => {
                                    hub_session.sidebar_frame = None;
                                    hub_session.sidebar_stream = None;
                                    hub_session.running = false;
                                    state.repaint = true;
                                }
                                _ => {}
                            }
                        }
                    }
                }
                HubStreamKind::App => {}
            },
            ClientLoopEvent::HubServerDisconnected {
                session,
                generation,
                stream_kind,
            } => match stream_kind {
                HubStreamKind::App
                    if session == state.active.name && generation == state.active.generation =>
                {
                    state.active.frame = None;
                    state.status = Some(format!("session {session} disconnected"));
                    state.repaint = true;
                }
                HubStreamKind::Summary => {
                    if let Some(hub_session) = state.session_mut(&session) {
                        if generation == hub_session.summary_generation {
                            hub_session.summary_stream = None;
                            hub_session.running = false;
                            state.repaint = true;
                        }
                    }
                }
                HubStreamKind::Sidebar => {
                    if let Some(hub_session) = state.session_mut(&session) {
                        if generation == hub_session.sidebar_generation {
                            hub_session.sidebar_stream = None;
                            hub_session.sidebar_frame = None;
                            state.repaint = true;
                        }
                    }
                }
                HubStreamKind::App => {}
            },
            ClientLoopEvent::Timer => {}
            ClientLoopEvent::ServerMessage(_)
            | ClientLoopEvent::ServerDisconnected
            | ClientLoopEvent::PixelMouse(_, _)
            | ClientLoopEvent::DirectGraphicsResponse(_) => {}
        }

        if state.repaint {
            render_hub(state, &mut encoder)?;
        }
    }

    let _ = super::write_to_server(&mut state.active.stream, &ClientMessage::Detach);
    Ok(())
}

fn handle_active_message(
    state: &mut HubState,
    message: ServerMessage,
    host_mouse_capture_active: &Arc<AtomicBool>,
    host_sgr_pixels_active: &Arc<AtomicBool>,
    _redraw_on_focus_gained: bool,
) -> Result<(), ClientError> {
    match message {
        ServerMessage::Frame(frame) => {
            state.active.frame = Some(frame);
            state.repaint = true;
        }
        ServerMessage::Notify {
            kind,
            message,
            body,
        } => {
            let sound = crate::config::Config::load().config.ui.sound;
            super::handle_notify(kind, &message, body.as_deref(), &sound);
        }
        ServerMessage::Clipboard { data } => super::forward_clipboard(&data),
        ServerMessage::WindowTitle { title } => {
            let _ =
                crate::terminal_effects::write_window_title(&mut io::stdout(), title.as_deref());
        }
        ServerMessage::MouseCapture { enabled, .. } => {
            // The local hub owns a cell-width sidebar, so keep host mouse reports
            // in cell coordinates where the content offset can be translated.
            let next_sgr_pixels = false;
            super::set_mouse_capture(enabled, next_sgr_pixels)
                .map_err(ClientError::ConnectionFailed)?;
            host_mouse_capture_active.store(enabled, Ordering::Release);
            host_sgr_pixels_active.store(next_sgr_pixels, Ordering::Release);
        }
        ServerMessage::ServerShutdown { reason } => {
            state.status = Some(reason.unwrap_or_else(|| "session stopped".to_owned()));
            state.active.frame = None;
            state.repaint = true;
        }
        ServerMessage::ReloadSoundConfig
        | ServerMessage::KittyKeyboardReportAll { .. }
        | ServerMessage::PrefixInputSource { .. }
        | ServerMessage::Graphics { .. }
        | ServerMessage::GraphicsFile { .. }
        | ServerMessage::GraphicsTransmissionRetired { .. }
        | ServerMessage::TerminalBell { .. }
        | ServerMessage::Terminal(_)
        | ServerMessage::SessionSummary(_)
        | ServerMessage::Welcome { .. } => {}
        ServerMessage::CompressedFrame(_) => unreachable!("compressed frame normalised"),
    }
    Ok(())
}

fn handle_local_input(
    backend: &mut impl SessionHubBackend,
    state: &mut HubState,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    should_quit: Arc<AtomicBool>,
    data: Vec<u8>,
) -> Result<bool, ClientError> {
    let events = crate::raw_input::parse_raw_input_bytes_sync(&data);
    if state.modal.is_some() {
        return handle_modal_input(backend, state, event_tx, should_quit, events);
    }
    if state.session_menu.is_some() {
        return Ok(handle_session_menu_input(state, &events));
    }

    if let Some(session) = state.server_overlay_session.clone() {
        forward_input_to_session(state, &session, &data, &events)?;
        return Ok(false);
    }

    if let [crate::raw_input::RawInputEvent::Mouse(mouse)] = events.as_slice() {
        if mouse.column < state.sidebar_width() {
            let target = state
                .rows()
                .into_iter()
                .find(|row| row.y == mouse.row)
                .map(|row| row.target);
            match target {
                Some(HubRowTarget::SpaceRow {
                    session,
                    workspace_id,
                    original_y,
                }) => {
                    if session != state.active.name
                        && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                    {
                        switch_active(
                            backend,
                            state,
                            &session,
                            event_tx,
                            should_quit,
                            Some(&workspace_id),
                        )
                        .map_err(ClientError::ConnectionFailed)?;
                        return Ok(true);
                    }
                    let Some(kind) = ClientMouseKind::from_crossterm(mouse.kind) else {
                        return Ok(false);
                    };
                    let message = ClientMessage::InputEvents {
                        events: vec![ClientInputEvent::Mouse {
                            kind,
                            column: mouse.column,
                            row: original_y,
                            modifiers: mouse.modifiers.bits(),
                        }],
                    };
                    write_to_session(state, &session, &message)?;
                    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Right)) {
                        state.server_overlay_session = Some(session);
                        state.server_overlay_seen = false;
                    }
                    return Ok(false);
                }
                Some(HubRowTarget::SpaceGapRow {
                    session,
                    original_y,
                }) => {
                    let Some(kind) = ClientMouseKind::from_crossterm(mouse.kind) else {
                        return Ok(false);
                    };
                    write_to_session(
                        state,
                        &session,
                        &ClientMessage::InputEvents {
                            events: vec![ClientInputEvent::Mouse {
                                kind,
                                column: mouse.column,
                                row: original_y,
                                modifiers: mouse.modifiers.bits(),
                            }],
                        },
                    )?;
                    return Ok(false);
                }
                Some(HubRowTarget::Session(session))
                    if session != crate::session::DEFAULT_SESSION_NAME
                        && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Right)) =>
                {
                    state.session_menu = Some(SessionContextMenu {
                        session,
                        x: mouse.column,
                        y: mouse.row,
                        highlighted: 0,
                    });
                    return Ok(true);
                }
                Some(target) => {
                    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                        activate_row(backend, state, event_tx, should_quit, target)?;
                        return Ok(true);
                    }
                    return Ok(false);
                }
                None => {}
            }
        }

        let Some(kind) = ClientMouseKind::from_crossterm(mouse.kind) else {
            return Ok(false);
        };
        let message = ClientMessage::InputEvents {
            events: vec![ClientInputEvent::Mouse {
                kind,
                column: mouse.column,
                row: mouse.row,
                modifiers: mouse.modifiers.bits(),
            }],
        };
        super::write_to_server(&mut state.active.stream, &message)
            .map_err(ClientError::ConnectionLost)?;
        return Ok(false);
    }

    if super::should_bridge_clipboard_image_paste(&data, true, state.remote_image_paste_key) {
        if let Some(image) = crate::platform::read_clipboard_image() {
            if image.bytes.len() <= super::MAX_CLIPBOARD_IMAGE_PAYLOAD {
                let message = ClientMessage::ClipboardImage {
                    extension: image.extension.to_owned(),
                    data: image.bytes,
                };
                super::write_to_server(&mut state.active.stream, &message)
                    .map_err(ClientError::ConnectionLost)?;
                return Ok(false);
            }
            tracing::warn!(
                bytes = image.bytes.len(),
                max = super::MAX_CLIPBOARD_IMAGE_PAYLOAD,
                "local clipboard image is too large to bridge"
            );
            return Ok(false);
        }
    }
    if let Some(image) = super::read_image_file_from_terminal_drop(&data, true) {
        let message = ClientMessage::ClipboardImage {
            extension: image.extension.to_owned(),
            data: image.bytes,
        };
        super::write_to_server(&mut state.active.stream, &message)
            .map_err(ClientError::ConnectionLost)?;
        return Ok(false);
    }

    super::write_to_server(&mut state.active.stream, &ClientMessage::Input { data })
        .map_err(ClientError::ConnectionLost)?;
    Ok(false)
}

fn session_menu_rect(state: &HubState) -> Option<Rect> {
    let menu = state.session_menu.as_ref()?;
    let width = 16.min(state.cols.max(1));
    let height = 4.min(state.rows.max(1));
    Some(Rect::new(
        menu.x.min(state.cols.saturating_sub(width)),
        menu.y.min(state.rows.saturating_sub(height)),
        width,
        height,
    ))
}

fn handle_session_menu_input(
    state: &mut HubState,
    events: &[crate::raw_input::RawInputEvent],
) -> bool {
    let Some(event) = events.first() else {
        return false;
    };
    match event {
        crate::raw_input::RawInputEvent::Key(key)
            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
        {
            match key.code {
                KeyCode::Esc => state.session_menu = None,
                KeyCode::Up | KeyCode::Down => {
                    if let Some(menu) = state.session_menu.as_mut() {
                        menu.highlighted = usize::from(menu.highlighted == 0);
                    }
                }
                KeyCode::Enter => open_session_menu_action(state),
                _ => {}
            }
            true
        }
        crate::raw_input::RawInputEvent::Mouse(mouse)
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) =>
        {
            let selected = session_menu_rect(state).and_then(|rect| {
                let inner_y = rect.y.saturating_add(1);
                (mouse.column > rect.x
                    && mouse.column < rect.x.saturating_add(rect.width).saturating_sub(1)
                    && mouse.row >= inner_y
                    && mouse.row < inner_y.saturating_add(2))
                .then_some((mouse.row - inner_y) as usize)
            });
            if let Some(selected) = selected {
                if let Some(menu) = state.session_menu.as_mut() {
                    menu.highlighted = selected;
                }
                open_session_menu_action(state);
            } else {
                state.session_menu = None;
            }
            true
        }
        _ => true,
    }
}

fn open_session_menu_action(state: &mut HubState) {
    let Some(menu) = state.session_menu.take() else {
        return;
    };
    state.modal = Some(match menu.highlighted {
        0 => SessionModal::Rename {
            name: menu.session.clone(),
            session: menu.session,
            error: None,
        },
        _ => SessionModal::ConfirmClose {
            session: menu.session,
            error: None,
        },
    });
}

fn forward_input_to_session(
    state: &mut HubState,
    session: &str,
    data: &[u8],
    events: &[crate::raw_input::RawInputEvent],
) -> Result<(), ClientError> {
    if let [crate::raw_input::RawInputEvent::Mouse(mouse)] = events {
        let Some(kind) = ClientMouseKind::from_crossterm(mouse.kind) else {
            return Ok(());
        };
        return write_to_session(
            state,
            session,
            &ClientMessage::InputEvents {
                events: vec![ClientInputEvent::Mouse {
                    kind,
                    column: mouse.column,
                    row: mouse.row,
                    modifiers: mouse.modifiers.bits(),
                }],
            },
        );
    }
    write_to_session(
        state,
        session,
        &ClientMessage::Input {
            data: data.to_vec(),
        },
    )
}

fn write_to_session(
    state: &mut HubState,
    session: &str,
    message: &ClientMessage,
) -> Result<(), ClientError> {
    if session == state.active.name {
        return super::write_to_server(&mut state.active.stream, message)
            .map_err(ClientError::ConnectionLost);
    }
    let stream = state
        .session_mut(session)
        .and_then(|session| session.sidebar_stream.as_mut())
        .ok_or_else(|| {
            ClientError::ConnectionLost(io::Error::new(
                io::ErrorKind::NotConnected,
                format!("session {session} sidebar is not connected"),
            ))
        })?;
    super::write_to_server(stream, message).map_err(ClientError::ConnectionLost)
}

fn handle_modal_input(
    backend: &mut impl SessionHubBackend,
    state: &mut HubState,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    should_quit: Arc<AtomicBool>,
    events: Vec<crate::raw_input::RawInputEvent>,
) -> Result<bool, ClientError> {
    for event in events {
        let crate::raw_input::RawInputEvent::Key(key) = event else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        match key.code {
            KeyCode::Esc => {
                state.modal = None;
                return Ok(true);
            }
            KeyCode::Backspace => match state.modal.as_mut() {
                Some(SessionModal::Create(modal)) => {
                    modal.name.pop();
                    modal.error = None;
                }
                Some(SessionModal::Rename { name, error, .. }) => {
                    name.pop();
                    *error = None;
                }
                Some(SessionModal::ConfirmClose { .. }) | None => {}
            },
            KeyCode::Enter => {
                submit_session_modal(backend, state, event_tx, should_quit.clone())?;
                if state.modal.is_none() {
                    return Ok(true);
                }
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                match state.modal.as_mut() {
                    Some(SessionModal::Create(modal)) if modal.name.len() < 64 => {
                        modal.name.push(ch);
                        modal.error = None;
                    }
                    Some(SessionModal::Rename { name, error, .. }) if name.len() < 64 => {
                        name.push(ch);
                        *error = None;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    Ok(true)
}

fn submit_session_modal(
    backend: &mut impl SessionHubBackend,
    state: &mut HubState,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    should_quit: Arc<AtomicBool>,
) -> Result<(), ClientError> {
    match state.modal.as_ref() {
        Some(SessionModal::Create(modal)) => {
            let requested = modal.name.trim().to_owned();
            create_named_session(backend, state, event_tx, should_quit, requested)
        }
        Some(SessionModal::Rename { session, name, .. }) => {
            let session = session.clone();
            let requested = name.trim().to_owned();
            rename_named_session(backend, state, event_tx, should_quit, &session, requested)
        }
        Some(SessionModal::ConfirmClose { session, .. }) => {
            let session = session.clone();
            close_named_session(backend, state, event_tx, should_quit, &session)
        }
        None => Ok(()),
    }
}

fn validated_new_session_name(state: &mut HubState, requested: &str) -> Option<String> {
    let session = match crate::session::parse_target_name(requested) {
        Ok(Some(session)) => session,
        Ok(None) => {
            set_modal_error(state, "use a named session, not default");
            return None;
        }
        Err(error) => {
            set_modal_error(state, error);
            return None;
        }
    };
    if state.sessions.iter().any(|item| item.name == session) {
        set_modal_error(state, format!("session {session} already exists"));
        return None;
    }
    Some(session)
}

fn create_named_session(
    backend: &mut impl SessionHubBackend,
    state: &mut HubState,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    should_quit: Arc<AtomicBool>,
    requested: String,
) -> Result<(), ClientError> {
    let Some(session) = validated_new_session_name(state, &requested) else {
        return Ok(());
    };
    if let Err(error) = backend.create_session(&session) {
        set_modal_error(state, error.to_string());
        return Ok(());
    }
    state.sessions.push(HubSession {
        name: session.clone(),
        running: true,
        summary: None,
        summary_generation: 0,
        summary_stream: None,
        sidebar_generation: 0,
        sidebar_stream: None,
        sidebar_frame: None,
    });
    state
        .sessions
        .sort_by(|left, right| left.name.cmp(&right.name));
    connect_summary(backend, state, &session, event_tx, should_quit.clone())
        .map_err(ClientError::ConnectionFailed)?;
    switch_active(backend, state, &session, event_tx, should_quit, None)
        .map_err(ClientError::ConnectionFailed)?;
    state.modal = None;
    state.status = Some(format!("created session {session}"));
    Ok(())
}

fn rename_named_session(
    backend: &mut impl SessionHubBackend,
    state: &mut HubState,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    should_quit: Arc<AtomicBool>,
    session: &str,
    requested: String,
) -> Result<(), ClientError> {
    if requested == session {
        state.modal = None;
        return Ok(());
    }
    let Some(new_name) = validated_new_session_name(state, &requested) else {
        return Ok(());
    };
    detach_session_observers(state, session);
    if let Err(error) = backend.rename_session(session, &new_name) {
        set_modal_error(state, error.to_string());
        return Ok(());
    }
    let was_active = state.active.name == session;
    let was_collapsed = state.collapsed.remove(session);
    if let Some(item) = state.session_mut(session) {
        item.name = new_name.clone();
        item.running = true;
        item.summary = None;
        item.summary_generation = 0;
        item.sidebar_generation = 0;
    }
    if was_collapsed {
        state.collapsed.insert(new_name.clone());
    }
    state
        .sessions
        .sort_by(|left, right| left.name.cmp(&right.name));
    connect_summary(backend, state, &new_name, event_tx, should_quit.clone())
        .map_err(ClientError::ConnectionFailed)?;
    if was_active {
        switch_active(backend, state, &new_name, event_tx, should_quit, None)
            .map_err(ClientError::ConnectionFailed)?;
    } else if !was_collapsed {
        connect_sidebar(backend, state, &new_name, event_tx, should_quit)
            .map_err(ClientError::ConnectionFailed)?;
    }
    state.modal = None;
    state.status = Some(format!("renamed session {session} to {new_name}"));
    Ok(())
}

fn close_named_session(
    backend: &mut impl SessionHubBackend,
    state: &mut HubState,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    should_quit: Arc<AtomicBool>,
    session: &str,
) -> Result<(), ClientError> {
    state.collapsed.insert(session.to_owned());
    if state.active.name == session {
        let fallback = state
            .sessions
            .iter()
            .find(|item| item.name != session)
            .map(|item| item.name.clone())
            .ok_or_else(|| ClientError::ConnectionLost(io::Error::other("no fallback session")))?;
        switch_active(backend, state, &fallback, event_tx, should_quit, None)
            .map_err(ClientError::ConnectionFailed)?;
    }
    detach_session_observers(state, session);
    if let Err(error) = backend.close_session(session) {
        set_modal_error(state, error.to_string());
        return Ok(());
    }
    state.sessions.retain(|item| item.name != session);
    state.collapsed.remove(session);
    state.modal = None;
    state.status = Some(format!("closed session {session}"));
    Ok(())
}

fn detach_session_observers(state: &mut HubState, session: &str) {
    if let Some(item) = state.session_mut(session) {
        if let Some(stream) = item.summary_stream.as_mut() {
            let _ = super::write_to_server(stream, &ClientMessage::Detach);
        }
        if let Some(stream) = item.sidebar_stream.as_mut() {
            let _ = super::write_to_server(stream, &ClientMessage::Detach);
        }
        item.summary_stream = None;
        item.sidebar_stream = None;
        item.sidebar_frame = None;
    }
}

fn activate_row(
    backend: &mut impl SessionHubBackend,
    state: &mut HubState,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    should_quit: Arc<AtomicBool>,
    target: HubRowTarget,
) -> Result<(), ClientError> {
    match target {
        HubRowTarget::Session(session) => {
            if state.collapsed.remove(&session) {
                let needs_connection = state
                    .sessions
                    .iter()
                    .find(|item| item.name == session)
                    .is_some_and(|item| item.summary_stream.is_none());
                if needs_connection {
                    connect_summary(backend, state, &session, event_tx, should_quit.clone())
                        .map_err(ClientError::ConnectionFailed)?;
                }
                if session != state.active.name {
                    connect_sidebar(backend, state, &session, event_tx, should_quit)
                        .map_err(ClientError::ConnectionFailed)?;
                }
                return Ok(());
            }
            let needs_connection = state
                .sessions
                .iter()
                .find(|item| item.name == session)
                .is_some_and(|item| item.summary_stream.is_none());
            if needs_connection {
                connect_summary(backend, state, &session, event_tx, should_quit)
                    .map_err(ClientError::ConnectionFailed)?;
            } else {
                state.collapsed.insert(session.clone());
                if let Some(hub_session) = state.session_mut(&session) {
                    if let Some(stream) = hub_session.sidebar_stream.as_mut() {
                        let _ = super::write_to_server(stream, &ClientMessage::Detach);
                    }
                    hub_session.sidebar_stream = None;
                    hub_session.sidebar_frame = None;
                    hub_session.sidebar_generation = 0;
                }
            }
        }
        HubRowTarget::SpaceRow { .. } => {}
        HubRowTarget::SpaceGapRow { .. } => {}
        HubRowTarget::NewSession => {
            state.modal = Some(SessionModal::Create(NewSessionModal {
                name: String::new(),
                error: None,
            }));
        }
    }
    Ok(())
}

fn set_modal_error(state: &mut HubState, error: impl Into<String>) {
    let error = Some(error.into());
    match state.modal.as_mut() {
        Some(SessionModal::Create(modal)) => modal.error = error,
        Some(SessionModal::Rename { error: target, .. })
        | Some(SessionModal::ConfirmClose { error: target, .. }) => *target = error,
        None => {}
    }
}

fn connect_summary(
    backend: &mut impl SessionHubBackend,
    state: &mut HubState,
    session: &str,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    should_quit: Arc<AtomicBool>,
) -> io::Result<()> {
    if state
        .sessions
        .iter()
        .find(|item| item.name == session)
        .is_some_and(|item| item.summary_stream.is_some())
    {
        return Ok(());
    }
    let generation = state.next_generation;
    state.next_generation = state.next_generation.saturating_add(1);
    let mut stream = backend.connect_client(session)?;
    super::do_handshake_with_options(
        &mut stream,
        1,
        1,
        0,
        0,
        RenderEncoding::SemanticFrame,
        state.keybindings.clone(),
        ClientLaunchMode::SessionSummary,
        super::REMOTE_HANDSHAKE_READ_TIMEOUT,
    )
    .map_err(io::Error::other)?;
    let read_stream = stream.try_clone()?;
    spawn_hub_reader(
        read_stream,
        event_tx.clone(),
        session.to_owned(),
        generation,
        HubStreamKind::Summary,
        should_quit,
    );
    if let Some(hub_session) = state.session_mut(session) {
        hub_session.running = true;
        hub_session.summary_generation = generation;
        hub_session.summary_stream = Some(stream);
    }
    Ok(())
}

fn connect_sidebar(
    backend: &mut impl SessionHubBackend,
    state: &mut HubState,
    session: &str,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    should_quit: Arc<AtomicBool>,
) -> io::Result<()> {
    if session == state.active.name
        || state
            .sessions
            .iter()
            .find(|item| item.name == session)
            .is_some_and(|item| item.sidebar_stream.is_some())
    {
        return Ok(());
    }
    let generation = state.next_generation;
    state.next_generation = state.next_generation.saturating_add(1);
    let mut stream = backend.connect_client(session)?;
    super::do_handshake_with_options(
        &mut stream,
        state.cols,
        state.rows,
        0,
        0,
        RenderEncoding::SemanticFrame,
        state.keybindings.clone(),
        ClientLaunchMode::AppSidebar,
        super::REMOTE_HANDSHAKE_READ_TIMEOUT,
    )
    .map_err(io::Error::other)?;
    let read_stream = stream.try_clone()?;
    spawn_hub_reader(
        read_stream,
        event_tx.clone(),
        session.to_owned(),
        generation,
        HubStreamKind::Sidebar,
        should_quit,
    );
    if let Some(hub_session) = state.session_mut(session) {
        hub_session.running = true;
        hub_session.sidebar_generation = generation;
        hub_session.sidebar_stream = Some(stream);
        hub_session.sidebar_frame = None;
    }
    Ok(())
}

fn switch_active(
    backend: &mut impl SessionHubBackend,
    state: &mut HubState,
    session: &str,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    should_quit: Arc<AtomicBool>,
    workspace_id: Option<&str>,
) -> io::Result<()> {
    if let Some(target) = state.session_mut(session) {
        if let Some(stream) = target.sidebar_stream.as_mut() {
            let _ = super::write_to_server(stream, &ClientMessage::Detach);
        }
        target.sidebar_stream = None;
        target.sidebar_frame = None;
        target.sidebar_generation = 0;
    }
    let generation = state.next_generation;
    state.next_generation = state.next_generation.saturating_add(1);
    let active = connect_active(
        backend,
        session,
        generation,
        state.cols,
        state.rows,
        workspace_id,
        &state.keybindings,
    )?;
    spawn_hub_reader(
        active.stream.try_clone()?,
        event_tx.clone(),
        session.to_owned(),
        generation,
        HubStreamKind::App,
        should_quit.clone(),
    );
    let mut previous = std::mem::replace(&mut state.active, active);
    let previous_name = previous.name.clone();
    let _ = super::write_to_server(&mut previous.stream, &ClientMessage::Detach);
    if previous_name != session
        && !state.collapsed.contains(&previous_name)
        && state.sessions.iter().any(|item| item.name == previous_name)
    {
        connect_sidebar(backend, state, &previous_name, event_tx, should_quit)?;
    }
    state.repaint = true;
    Ok(())
}

fn connect_active(
    backend: &mut impl SessionHubBackend,
    session: &str,
    generation: u64,
    cols: u16,
    rows: u16,
    workspace_id: Option<&str>,
    keybindings: &ClientKeybindings,
) -> io::Result<ActiveSession> {
    if let Some(workspace_id) = workspace_id {
        backend.focus_workspace(session, workspace_id)?;
    }
    let mut stream = backend.connect_client(session)?;
    super::do_handshake_with_options(
        &mut stream,
        cols,
        rows,
        0,
        0,
        RenderEncoding::SemanticFrame,
        keybindings.clone(),
        ClientLaunchMode::App,
        super::REMOTE_HANDSHAKE_READ_TIMEOUT,
    )
    .map_err(io::Error::other)?;
    stream.set_nonblocking(false)?;
    Ok(ActiveSession {
        name: session.to_owned(),
        generation,
        stream,
        frame: None,
    })
}

fn spawn_hub_reader(
    mut stream: LocalStream,
    event_tx: tokio::sync::mpsc::Sender<ClientLoopEvent>,
    session: String,
    generation: u64,
    stream_kind: HubStreamKind,
    should_quit: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let _ = stream.set_nonblocking(false);
        let max_frame_size = match stream_kind {
            HubStreamKind::App | HubStreamKind::Sidebar => MAX_RENDER_FRAME_SIZE,
            HubStreamKind::Summary => MAX_FRAME_SIZE,
        };
        while !should_quit.load(Ordering::Acquire) {
            match crate::protocol::read_message(&mut stream, max_frame_size) {
                Ok(message) => {
                    if event_tx
                        .blocking_send(ClientLoopEvent::HubServerMessage {
                            session: session.clone(),
                            generation,
                            stream_kind,
                            message,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                Err(crate::protocol::FramingError::UnexpectedEof) => break,
                Err(error) => {
                    tracing::warn!(%error, %session, ?stream_kind, "session hub stream failed");
                    break;
                }
            }
        }
        let _ = event_tx.blocking_send(ClientLoopEvent::HubServerDisconnected {
            session,
            generation,
            stream_kind,
        });
    });
}

fn render_hub(
    state: &mut HubState,
    encoder: &mut crate::protocol::render_ansi::BlitEncoder,
) -> Result<(), ClientError> {
    let frame = compose_frame(state);
    let encoded = encoder.encode(&frame, state.full_repaint);
    let mut stdout = io::stdout();
    super::write_encoded_frame_with_graphics(&mut stdout, &encoded.bytes, &[])
        .map_err(ClientError::ConnectionFailed)?;
    stdout.flush().map_err(ClientError::ConnectionFailed)?;
    encoder.commit(frame, encoded);
    state.repaint = false;
    state.full_repaint = false;
    Ok(())
}

fn compose_frame(state: &HubState) -> FrameData {
    if let Some(session) = state.server_overlay_session.as_deref() {
        if let Some(frame) = state.session_frame(session) {
            return frame.clone();
        }
    }
    let Some(original) = state.active.frame.as_ref() else {
        return FrameData::from_ratatui_buffer_with_hyperlinks(
            &Buffer::empty(Rect::new(0, 0, state.cols, state.rows)),
            None,
            &[],
        );
    };
    let mut frame = original.clone();
    let Some(layout) = state.sidebar_layout() else {
        return frame;
    };
    let sidebar_width = layout.width.min(frame.width);
    let body_start = layout.spaces_y.saturating_add(1);
    let body_end = layout.footer_y.min(frame.height);
    let blank = original
        .cells
        .get(body_start as usize * original.width as usize)
        .cloned()
        .or_else(|| original.cells.first().cloned())
        .unwrap_or_else(|| crate::protocol::CellData {
            symbol: " ".to_owned(),
            fg: 0,
            bg: 0,
            modifier: 0,
            skip: false,
            hyperlink: None,
        });
    let heading = original
        .cells
        .get(layout.spaces_y as usize * original.width as usize + 1)
        .cloned()
        .unwrap_or_else(|| blank.clone());

    for y in body_start..body_end {
        fill_frame_row(&mut frame, y, sidebar_width.saturating_sub(1), &blank);
    }

    for row in state.rows() {
        match row.target {
            HubRowTarget::Session(name) => {
                let marker = if state.collapsed.contains(&name) {
                    "▶"
                } else {
                    "▼"
                };
                write_frame_text(
                    &mut frame,
                    0,
                    row.y,
                    sidebar_width.saturating_sub(1),
                    &format!(" {marker} {name}"),
                    &heading,
                );
            }
            HubRowTarget::SpaceRow {
                session,
                workspace_id: _,
                original_y,
            } => {
                if let Some(source) = state.session_frame(&session) {
                    copy_frame_row(
                        source,
                        &mut frame,
                        original_y,
                        row.y,
                        sidebar_width.saturating_sub(1),
                    );
                }
            }
            HubRowTarget::SpaceGapRow {
                session,
                original_y,
            } => {
                if let Some(source) = state.session_frame(&session) {
                    copy_frame_row(
                        source,
                        &mut frame,
                        original_y,
                        row.y,
                        sidebar_width.saturating_sub(1),
                    );
                }
            }
            HubRowTarget::NewSession => write_frame_text(
                &mut frame,
                1,
                row.y,
                sidebar_width.saturating_sub(2),
                "+ new named session",
                &heading,
            ),
        }
    }

    if state.session_menu.is_some() {
        overlay_session_context_menu(&mut frame, state);
    }
    if let Some(modal) = &state.modal {
        overlay_session_modal(&mut frame, modal);
    }
    frame
}

fn fill_frame_row(frame: &mut FrameData, y: u16, width: u16, template: &crate::protocol::CellData) {
    for x in 0..width {
        let index = y as usize * frame.width as usize + x as usize;
        if let Some(cell) = frame.cells.get_mut(index) {
            *cell = template.clone();
            cell.symbol = " ".to_owned();
            cell.hyperlink = None;
        }
    }
}

fn write_frame_text(
    frame: &mut FrameData,
    x: u16,
    y: u16,
    width: u16,
    text: &str,
    template: &crate::protocol::CellData,
) {
    for (offset, symbol) in text.chars().take(width as usize).enumerate() {
        let index = y as usize * frame.width as usize + x as usize + offset;
        if let Some(cell) = frame.cells.get_mut(index) {
            *cell = template.clone();
            cell.symbol = symbol.to_string();
            cell.hyperlink = None;
        }
    }
}

fn copy_frame_row(
    source: &FrameData,
    target: &mut FrameData,
    source_y: u16,
    target_y: u16,
    width: u16,
) {
    for x in 0..width.min(source.width).min(target.width) {
        let source_index = source_y as usize * source.width as usize + x as usize;
        let target_index = target_y as usize * target.width as usize + x as usize;
        if let (Some(source), Some(target)) = (
            source.cells.get(source_index),
            target.cells.get_mut(target_index),
        ) {
            *target = source.clone();
        }
    }
}

fn overlay_session_context_menu(frame: &mut FrameData, state: &HubState) {
    let Some(menu) = state.session_menu.as_ref() else {
        return;
    };
    let Some(rect) = session_menu_rect(state) else {
        return;
    };
    let area = Rect::new(0, 0, frame.width, frame.height);
    let mut overlay = Buffer::empty(area);
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().fg(Color::White).bg(Color::Black))
        .render(rect, &mut overlay);
    for (index, label) in ["Rename", "Close"].iter().enumerate() {
        let style = if index == menu.highlighted {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default().fg(Color::White).bg(Color::Black)
        };
        Paragraph::new(format!(" {label}")).style(style).render(
            Rect::new(rect.x + 1, rect.y + 1 + index as u16, rect.width - 2, 1),
            &mut overlay,
        );
    }
    blit_overlay(frame, &overlay, rect);
}

fn overlay_session_modal(frame: &mut FrameData, modal: &SessionModal) {
    let (title, input, message, error) = match modal {
        SessionModal::Create(modal) => (
            " New named session ",
            Some(modal.name.as_str()),
            "Enter create  Esc cancel".to_owned(),
            modal.error.as_deref(),
        ),
        SessionModal::Rename {
            session,
            name,
            error,
        } => (
            " Rename named session ",
            Some(name.as_str()),
            format!("Renaming {session} stops and restores its saved state"),
            error.as_deref(),
        ),
        SessionModal::ConfirmClose { session, error } => (
            " Close named session ",
            None,
            format!("Close {session}? Enter confirms; Esc cancels"),
            error.as_deref(),
        ),
    };
    let width = frame.width.saturating_sub(4).clamp(20, 52);
    let height = if error.is_some() { 7 } else { 6 };
    let rect = Rect::new(
        frame.width.saturating_sub(width) / 2,
        frame.height.saturating_sub(height) / 2,
        width,
        height.min(frame.height),
    );
    let area = Rect::new(0, 0, frame.width, frame.height);
    let mut overlay = Buffer::empty(area);
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().fg(Color::White).bg(Color::Black))
        .render(rect, &mut overlay);
    if let Some(input) = input {
        Paragraph::new(format!("Name: {input}_"))
            .style(Style::default().fg(Color::White).bg(Color::Black))
            .render(
                Rect::new(
                    rect.x.saturating_add(2),
                    rect.y.saturating_add(2),
                    rect.width.saturating_sub(4),
                    1,
                ),
                &mut overlay,
            );
    }
    Paragraph::new(clip(&message, rect.width.saturating_sub(4) as usize))
        .style(Style::default().fg(Color::DarkGray).bg(Color::Black))
        .render(
            Rect::new(
                rect.x.saturating_add(2),
                rect.y.saturating_add(4),
                rect.width.saturating_sub(4),
                1,
            ),
            &mut overlay,
        );
    if let Some(error) = error {
        Paragraph::new(clip(error, rect.width.saturating_sub(4) as usize))
            .style(Style::default().fg(Color::Red).bg(Color::Black))
            .render(
                Rect::new(
                    rect.x.saturating_add(2),
                    rect.y.saturating_add(3),
                    rect.width.saturating_sub(4),
                    1,
                ),
                &mut overlay,
            );
    }
    blit_overlay(frame, &overlay, rect);
    frame.cursor = input.map(|input| crate::protocol::CursorState {
        x: rect
            .x
            .saturating_add(8)
            .saturating_add(input.chars().count() as u16)
            .min(rect.x.saturating_add(rect.width.saturating_sub(2))),
        y: rect.y.saturating_add(2),
        visible: true,
        shape: 6,
    });
}

fn blit_overlay(frame: &mut FrameData, overlay: &Buffer, rect: Rect) {
    let overlay_frame = FrameData::from_ratatui_buffer_with_hyperlinks(overlay, None, &[]);
    for y in rect.y..rect.y.saturating_add(rect.height) {
        for x in rect.x..rect.x.saturating_add(rect.width) {
            let index = y as usize * frame.width as usize + x as usize;
            if let (Some(source), Some(target)) =
                (overlay_frame.cells.get(index), frame.cells.get_mut(index))
            {
                *target = source.clone();
            }
        }
    }
}

fn clip(value: &str, width: usize) -> String {
    value.chars().take(width).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labelled_frame(width: u16, height: u16) -> FrameData {
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        for y in 0..height {
            for x in 0..width {
                let symbol = y.to_string();
                buffer[(x, y)].set_symbol(&symbol);
            }
        }
        FrameData::from_ratatui_buffer(&buffer, None)
    }

    #[test]
    fn moving_sidebar_rows_preserves_standard_content_columns() {
        let source = labelled_frame(8, 6);
        let mut composed = source.clone();
        let blank = source.cells[0].clone();

        fill_frame_row(&mut composed, 2, 3, &blank);
        copy_frame_row(&source, &mut composed, 1, 2, 3);

        for x in 0..3 {
            let x = x as usize;
            assert_eq!(composed.cells[2 * 8 + x], source.cells[8 + x]);
        }
        for y in 0..6 {
            for x in 3..8 {
                let index = (y * 8 + x) as usize;
                assert_eq!(composed.cells[index], source.cells[index]);
            }
        }
    }
}
