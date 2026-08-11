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
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use crate::api::schema::AgentStatus;
use crate::ipc::LocalStream;
use crate::protocol::{
    ClientInputEvent, ClientKeybindings, ClientLaunchMode, ClientMessage, ClientMouseKind,
    FrameData, RenderEncoding, ServerMessage, SessionSummary, MAX_FRAME_SIZE,
    MAX_RENDER_FRAME_SIZE,
};

use super::{ClientError, ClientLoopEvent};

const MAX_SIDEBAR_WIDTH: u16 = 30;
const MIN_CONTENT_WIDTH: u16 = 20;

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HubStreamKind {
    App,
    Summary,
}

struct HubSession {
    name: String,
    running: bool,
    summary: Option<SessionSummary>,
    summary_generation: u64,
    summary_stream: Option<LocalStream>,
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
    Workspace {
        session: String,
        workspace_id: String,
    },
    NewSession,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HubRow {
    y: u16,
    target: HubRowTarget,
}

#[derive(Default)]
struct NewSessionModal {
    name: String,
    error: Option<String>,
}

struct HubState {
    sessions: Vec<HubSession>,
    collapsed: HashSet<String>,
    active: ActiveSession,
    next_generation: u64,
    modal: Option<NewSessionModal>,
    status: Option<String>,
    cols: u16,
    rows: u16,
    repaint: bool,
    full_repaint: bool,
    keybindings: ClientKeybindings,
    remote_image_paste_key: Option<(crossterm::event::KeyCode, KeyModifiers)>,
}

impl HubState {
    fn sidebar_width(&self) -> u16 {
        sidebar_width(self.cols)
    }

    fn rows(&self) -> Vec<HubRow> {
        let mut rows = Vec::new();
        let mut y = 1_u16;
        for session in &self.sessions {
            if y >= self.rows.saturating_sub(2) {
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
            if let Some(summary) = &session.summary {
                for workspace in &summary.workspaces {
                    if y >= self.rows.saturating_sub(2) {
                        break;
                    }
                    rows.push(HubRow {
                        y,
                        target: HubRowTarget::Workspace {
                            session: session.name.clone(),
                            workspace_id: workspace.workspace_id.clone(),
                        },
                    });
                    y = y.saturating_add(1);
                }
            }
        }
        if y < self.rows {
            rows.push(HubRow {
                y,
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
    let (cols, rows, _, _) = super::initial_terminal_geometry(false);

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
        })
        .collect::<Vec<_>>();
    let mut state = HubState {
        sessions,
        collapsed: HashSet::new(),
        active,
        next_generation,
        modal: None,
        status: None,
        cols,
        rows,
        repaint: true,
        full_repaint: true,
        keybindings,
        remote_image_paste_key,
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
    let reported_cell_size = Arc::new(std::sync::atomic::AtomicU64::new(0));

    let stdin_quit = should_quit.clone();
    let stdin_tx = event_tx.clone();
    let stdin_mouse_capture = host_mouse_capture_active.clone();
    std::thread::spawn(move || {
        super::input::stdin_reader_loop(stdin_tx, &stdin_quit, false, false, stdin_mouse_capture);
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
                    cols: content_width(cols),
                    rows,
                    cell_width_px: 0,
                    cell_height_px: 0,
                };
                super::write_to_server(&mut state.active.stream, &message)
                    .map_err(ClientError::ConnectionLost)?;
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
                        redraw_on_focus_gained,
                    )?;
                }
                HubStreamKind::Summary => {
                    if let Some(hub_session) = state.session_mut(&session) {
                        if generation == hub_session.summary_generation {
                            if let ServerMessage::SessionSummary(summary) = message {
                                hub_session.summary = Some(summary);
                                hub_session.running = true;
                                state.repaint = true;
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
                HubStreamKind::App => {}
            },
            ClientLoopEvent::Timer => {}
            ClientLoopEvent::ServerMessage(_) | ClientLoopEvent::ServerDisconnected => {}
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
        ServerMessage::WindowTitle { title } => super::write_window_title(title.as_deref()),
        ServerMessage::MouseCapture { enabled, .. } => {
            super::set_mouse_capture(enabled).map_err(ClientError::ConnectionFailed)?;
            host_mouse_capture_active.store(enabled, Ordering::Release);
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

    if let [crate::raw_input::RawInputEvent::Mouse(mouse)] = events.as_slice() {
        if mouse.column < state.sidebar_width() {
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                let target = state
                    .rows()
                    .into_iter()
                    .find(|row| row.y == mouse.row)
                    .map(|row| row.target);
                if let Some(target) = target {
                    activate_row(backend, state, event_tx, should_quit, target)?;
                    return Ok(true);
                }
            }
            return Ok(false);
        }

        let Some(kind) = ClientMouseKind::from_crossterm(mouse.kind) else {
            return Ok(false);
        };
        let message = ClientMessage::InputEvents {
            events: vec![ClientInputEvent::Mouse {
                kind,
                column: mouse.column.saturating_sub(state.sidebar_width()),
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
            KeyCode::Backspace => {
                if let Some(modal) = state.modal.as_mut() {
                    modal.name.pop();
                    modal.error = None;
                }
            }
            KeyCode::Enter => {
                let requested = state
                    .modal
                    .as_ref()
                    .map(|modal| modal.name.trim().to_owned())
                    .unwrap_or_default();
                let session = match crate::session::parse_target_name(&requested) {
                    Ok(Some(session)) => session,
                    Ok(None) => {
                        set_modal_error(state, "use a named session, not default");
                        continue;
                    }
                    Err(error) => {
                        set_modal_error(state, error);
                        continue;
                    }
                };
                if state.sessions.iter().any(|item| item.name == session) {
                    set_modal_error(state, format!("session {session} already exists"));
                    continue;
                }
                if let Err(error) = backend.create_session(&session) {
                    set_modal_error(state, error.to_string());
                    continue;
                }
                state.sessions.push(HubSession {
                    name: session.clone(),
                    running: true,
                    summary: None,
                    summary_generation: 0,
                    summary_stream: None,
                });
                state
                    .sessions
                    .sort_by(|left, right| left.name.cmp(&right.name));
                connect_summary(backend, state, &session, event_tx, should_quit.clone())
                    .map_err(ClientError::ConnectionFailed)?;
                switch_active(
                    backend,
                    state,
                    &session,
                    event_tx,
                    should_quit.clone(),
                    None,
                )
                .map_err(ClientError::ConnectionFailed)?;
                state.modal = None;
                state.status = Some(format!("created session {session}"));
                return Ok(true);
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(modal) = state.modal.as_mut() {
                    if modal.name.len() < 64 {
                        modal.name.push(ch);
                        modal.error = None;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(true)
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
                    connect_summary(backend, state, &session, event_tx, should_quit)
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
                state.collapsed.insert(session);
            }
        }
        HubRowTarget::Workspace {
            session,
            workspace_id,
        } => {
            if state.active.name != session {
                switch_active(
                    backend,
                    state,
                    &session,
                    event_tx,
                    should_quit,
                    Some(&workspace_id),
                )
                .map_err(ClientError::ConnectionFailed)?;
            } else {
                backend
                    .focus_workspace(&session, &workspace_id)
                    .map_err(ClientError::ConnectionFailed)?;
            }
            state.status = None;
        }
        HubRowTarget::NewSession => {
            state.modal = Some(NewSessionModal::default());
        }
    }
    Ok(())
}

fn set_modal_error(state: &mut HubState, error: impl Into<String>) {
    if let Some(modal) = state.modal.as_mut() {
        modal.error = Some(error.into());
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

fn switch_active(
    backend: &mut impl SessionHubBackend,
    state: &mut HubState,
    session: &str,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    should_quit: Arc<AtomicBool>,
    workspace_id: Option<&str>,
) -> io::Result<()> {
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
        should_quit,
    );
    let mut previous = std::mem::replace(&mut state.active, active);
    let _ = super::write_to_server(&mut previous.stream, &ClientMessage::Detach);
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
        content_width(cols),
        rows,
        0,
        0,
        RenderEncoding::SemanticFrame,
        keybindings.clone(),
        ClientLaunchMode::AppEmbedded,
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
            HubStreamKind::App => MAX_RENDER_FRAME_SIZE,
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
    let area = Rect::new(0, 0, state.cols, state.rows);
    let sidebar_width = state.sidebar_width();
    let mut buffer = Buffer::empty(area);
    let heading = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    buffer.set_string(1, 0, "sessions", heading);
    if sidebar_width > 0 {
        let divider_x = sidebar_width.saturating_sub(1);
        for y in 0..state.rows {
            buffer[(divider_x, y)]
                .set_symbol("│")
                .set_style(Style::default().fg(Color::DarkGray));
        }
    }

    let rows = state.rows();
    for row in &rows {
        match &row.target {
            HubRowTarget::Session(name) => {
                let session = state.sessions.iter().find(|item| &item.name == name);
                let expanded = !state.collapsed.contains(name);
                let active = state.active.name == *name;
                let status = session
                    .and_then(|item| item.summary.as_ref())
                    .map(aggregate_status)
                    .unwrap_or(AgentStatus::Unknown);
                let marker = if expanded { "▼" } else { "▶" };
                let dot = status_symbol(status);
                let text = format!(" {marker} {name} {dot}");
                let style = if active {
                    Style::default()
                        .fg(Color::White)
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                fill_row(&mut buffer, row.y, sidebar_width, style);
                buffer.set_string(0, row.y, clip(&text, sidebar_width as usize), style);
            }
            HubRowTarget::Workspace {
                session,
                workspace_id,
            } => {
                let workspace = state
                    .sessions
                    .iter()
                    .find(|item| item.name == *session)
                    .and_then(|item| item.summary.as_ref())
                    .and_then(|summary| {
                        summary
                            .workspaces
                            .iter()
                            .find(|workspace| workspace.workspace_id == *workspace_id)
                    });
                if let Some(workspace) = workspace {
                    let focused = state.active.name == *session && workspace.focused;
                    let text = format!(
                        "   └─ {} {}",
                        workspace.label,
                        status_symbol(workspace.agent_status)
                    );
                    let style = if focused {
                        Style::default().fg(Color::Black).bg(Color::Cyan)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    fill_row(&mut buffer, row.y, sidebar_width, style);
                    buffer.set_string(0, row.y, clip(&text, sidebar_width as usize), style);
                }
            }
            HubRowTarget::NewSession => {
                buffer.set_string(
                    1,
                    row.y,
                    "+ new named session",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                );
            }
        }
    }

    if let Some(status) = &state.status {
        let y = state.rows.saturating_sub(1);
        buffer.set_string(
            1,
            y,
            clip(status, sidebar_width.saturating_sub(2) as usize),
            Style::default().fg(Color::Yellow),
        );
    }

    let mut frame = FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]);
    if let Some(content) = &state.active.frame {
        let content_width = state.cols.saturating_sub(sidebar_width).min(content.width);
        let content_height = state.rows.min(content.height);
        for y in 0..content_height {
            for x in 0..content_width {
                let source = y as usize * content.width as usize + x as usize;
                let target = y as usize * state.cols as usize + (sidebar_width + x) as usize;
                if let (Some(source), Some(target)) =
                    (content.cells.get(source), frame.cells.get_mut(target))
                {
                    *target = source.clone();
                }
            }
        }
        frame.hyperlinks = content.hyperlinks.clone();
        frame.cursor = content
            .cursor
            .as_ref()
            .map(|cursor| crate::protocol::CursorState {
                x: cursor.x.saturating_add(sidebar_width),
                y: cursor.y,
                visible: cursor.visible,
                shape: cursor.shape,
            });
    }

    if let Some(modal) = &state.modal {
        overlay_new_session_modal(&mut frame, modal);
    }
    frame.graphics.clear();
    frame
}

fn overlay_new_session_modal(frame: &mut FrameData, modal: &NewSessionModal) {
    let width = frame.width.saturating_sub(4).clamp(20, 52);
    let height = if modal.error.is_some() { 7 } else { 6 };
    let rect = Rect::new(
        frame.width.saturating_sub(width) / 2,
        frame.height.saturating_sub(height) / 2,
        width,
        height.min(frame.height),
    );
    let area = Rect::new(0, 0, frame.width, frame.height);
    let mut overlay = Buffer::empty(area);
    Block::default()
        .title(" New named session ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().fg(Color::White).bg(Color::Black))
        .render(rect, &mut overlay);
    let input = format!("Name: {}_", modal.name);
    Paragraph::new(input)
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
    Paragraph::new("Enter create  Esc cancel")
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
    if let Some(error) = &modal.error {
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
    let overlay_frame = FrameData::from_ratatui_buffer_with_hyperlinks(&overlay, None, &[]);
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
    frame.cursor = Some(crate::protocol::CursorState {
        x: rect
            .x
            .saturating_add(8)
            .saturating_add(modal.name.chars().count() as u16)
            .min(rect.x.saturating_add(rect.width.saturating_sub(2))),
        y: rect.y.saturating_add(2),
        visible: true,
        shape: 6,
    });
}

fn fill_row(buffer: &mut Buffer, y: u16, width: u16, style: Style) {
    for x in 0..width.saturating_sub(1) {
        buffer[(x, y)].set_symbol(" ").set_style(style);
    }
}

fn aggregate_status(summary: &SessionSummary) -> AgentStatus {
    summary
        .workspaces
        .iter()
        .map(|workspace| workspace.agent_status)
        .max_by_key(|status| match status {
            AgentStatus::Blocked => 5,
            AgentStatus::Done => 4,
            AgentStatus::Working => 3,
            AgentStatus::Idle => 2,
            AgentStatus::Unknown => 1,
        })
        .unwrap_or(AgentStatus::Unknown)
}

fn status_symbol(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Blocked => "!",
        AgentStatus::Done => "●",
        AgentStatus::Working => "◆",
        AgentStatus::Idle => "·",
        AgentStatus::Unknown => "?",
    }
}

fn sidebar_width(cols: u16) -> u16 {
    cols.saturating_sub(MIN_CONTENT_WIDTH)
        .min(MAX_SIDEBAR_WIDTH)
        .max((cols / 3).min(MAX_SIDEBAR_WIDTH))
}

fn content_width(cols: u16) -> u16 {
    cols.saturating_sub(sidebar_width(cols)).max(1)
}

fn clip(value: &str, width: usize) -> String {
    value.chars().take(width).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrow_terminals_keep_content_visible() {
        assert_eq!(sidebar_width(30), 10);
        assert_eq!(content_width(30), 20);
        assert_eq!(sidebar_width(120), 30);
        assert_eq!(content_width(120), 90);
    }

    #[test]
    fn aggregate_status_prioritises_attention() {
        let summary = SessionSummary {
            workspaces: vec![
                crate::protocol::SessionWorkspaceSummary {
                    workspace_id: "1".into(),
                    label: "one".into(),
                    focused: true,
                    agent_status: AgentStatus::Working,
                },
                crate::protocol::SessionWorkspaceSummary {
                    workspace_id: "2".into(),
                    label: "two".into(),
                    focused: false,
                    agent_status: AgentStatus::Blocked,
                },
            ],
        };
        assert_eq!(aggregate_status(&summary), AgentStatus::Blocked);
    }
}
