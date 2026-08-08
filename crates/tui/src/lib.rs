//! Optional terminal UI with event streaming, cooperative cancellation, and
//! exact-action approval prompts.

use std::collections::VecDeque;
use std::io::{Stdout, Write, stdout};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::thread;
use std::time::Duration;

use agent_harness_core::{
    AgentRuntime, ApprovalDecision, ApprovalError, ApprovalProvider, ApprovalRequest, BoxFuture,
    CancellationToken, EventEnvelope, EventObserver, ReceiptStatus, RuntimeEvent, ThreadId,
};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::style::{Color, Print, ResetColor, SetForegroundColor};
use crossterm::terminal::{
    self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode,
};
use crossterm::{execute, queue};
use thiserror::Error;

#[derive(Clone, Debug)]
pub struct TuiConfig {
    pub title: String,
    pub refresh_interval: Duration,
    pub maximum_log_entries: usize,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            title: "Agent Harness".to_owned(),
            refresh_interval: Duration::from_millis(50),
            maximum_log_entries: 2_000,
        }
    }
}

#[derive(Debug, Error)]
pub enum TuiError {
    #[error("terminal I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub struct ApprovalPrompt {
    pub request: ApprovalRequest,
    response: SyncSender<ApprovalDecision>,
}

pub struct TuiApprovalProvider {
    sender: Sender<ApprovalPrompt>,
}

impl ApprovalProvider for TuiApprovalProvider {
    fn request<'a>(
        &'a self,
        request: ApprovalRequest,
    ) -> BoxFuture<'a, Result<ApprovalDecision, ApprovalError>> {
        Box::pin(async move {
            let (response, receiver) = mpsc::sync_channel(1);
            self.sender
                .send(ApprovalPrompt { request, response })
                .map_err(|_| ApprovalError("TUI approval channel was closed".to_owned()))?;
            receiver
                .recv()
                .map_err(|_| ApprovalError("TUI approval response was dropped".to_owned()))
        })
    }
}

#[must_use]
pub fn approval_channel() -> (Arc<TuiApprovalProvider>, Receiver<ApprovalPrompt>) {
    let (sender, receiver) = mpsc::channel();
    (Arc::new(TuiApprovalProvider { sender }), receiver)
}

pub struct ChannelEventObserver {
    sender: Sender<EventEnvelope>,
}

impl EventObserver for ChannelEventObserver {
    fn on_event(&self, event: &EventEnvelope) {
        let _ = self.sender.send(event.clone());
    }
}

#[must_use]
pub fn event_channel() -> (Arc<ChannelEventObserver>, Receiver<EventEnvelope>) {
    let (sender, receiver) = mpsc::channel();
    (Arc::new(ChannelEventObserver { sender }), receiver)
}

pub fn run(
    runtime: Arc<AgentRuntime>,
    thread_id: ThreadId,
    events: Receiver<EventEnvelope>,
    approvals: Receiver<ApprovalPrompt>,
    config: TuiConfig,
) -> Result<(), TuiError> {
    let mut terminal = TerminalSession::enter()?;
    let (turn_sender, turn_receiver) = mpsc::channel::<Result<String, String>>();
    let mut state = UiState::new(thread_id);
    let mut dirty = true;

    loop {
        dirty |= drain_events(&events, &mut state, config.maximum_log_entries);
        dirty |= drain_approvals(&approvals, &mut state);
        match turn_receiver.try_recv() {
            Ok(result) => {
                dirty = true;
                state.running = false;
                state.cancellation = None;
                match result {
                    Ok(content) => state.push_log(format!("turn complete: {content}")),
                    Err(error) => state.push_log(format!("turn failed: {error}")),
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) if state.running => {
                dirty = true;
                state.running = false;
            }
            Err(TryRecvError::Disconnected) => {}
        }
        if dirty {
            render(terminal.stdout(), &state, &config)?;
            dirty = false;
        }

        if !event::poll(config.refresh_interval)? {
            continue;
        }
        let key = match event::read()? {
            Event::Key(key) => key,
            Event::Resize(_, _) => {
                dirty = true;
                continue;
            }
            _ => continue,
        };
        dirty = true;
        if handle_key(
            key,
            &runtime,
            &turn_sender,
            &mut state,
            config.maximum_log_entries,
        ) {
            deny_pending(&mut state);
            if let Some(cancellation) = &state.cancellation {
                cancellation.cancel();
            }
            break;
        }
    }
    Ok(())
}

struct UiState {
    thread_id: ThreadId,
    input: String,
    logs: VecDeque<String>,
    running: bool,
    cancellation: Option<CancellationToken>,
    current_approval: Option<ApprovalPrompt>,
    queued_approvals: VecDeque<ApprovalPrompt>,
}

impl UiState {
    fn new(thread_id: ThreadId) -> Self {
        let mut state = Self {
            thread_id,
            input: String::new(),
            logs: VecDeque::new(),
            running: false,
            cancellation: None,
            current_approval: None,
            queued_approvals: VecDeque::new(),
        };
        state.push_log("Type a message. /cancel stops the active turn; /quit exits.".to_owned());
        state
    }

    fn push_log(&mut self, line: String) {
        self.logs.push_back(line);
    }
}

fn handle_key(
    key: KeyEvent,
    runtime: &Arc<AgentRuntime>,
    turn_sender: &Sender<Result<String, String>>,
    state: &mut UiState,
    maximum_logs: usize,
) -> bool {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return true;
    }
    if state.current_approval.is_some() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => resolve_approval(state, true),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                resolve_approval(state, false)
            }
            _ => {}
        }
        return false;
    }
    match key.code {
        KeyCode::Esc => return true,
        KeyCode::Char(character) => state.input.push(character),
        KeyCode::Backspace => {
            state.input.pop();
        }
        KeyCode::Enter => {
            let input = state.input.trim().to_owned();
            state.input.clear();
            match input.as_str() {
                "" => {}
                "/quit" | "/exit" => return true,
                "/cancel" => {
                    if let Some(cancellation) = &state.cancellation {
                        cancellation.cancel();
                        state.push_log("cancellation requested".to_owned());
                    }
                }
                _ if state.running => {
                    state.push_log("a turn is already running".to_owned());
                }
                _ => {
                    state.running = true;
                    let cancellation = CancellationToken::new();
                    state.cancellation = Some(cancellation.clone());
                    state.push_log(format!("you: {input}"));
                    let runtime = runtime.clone();
                    let thread_id = state.thread_id.clone();
                    let sender = turn_sender.clone();
                    thread::spawn(move || {
                        let result = block_on(runtime.run_turn_with_cancellation(
                            &thread_id,
                            input,
                            cancellation,
                        ))
                        .map(|outcome| outcome.content)
                        .map_err(|error| error.to_string());
                        let _ = sender.send(result);
                    });
                }
            }
        }
        _ => {}
    }
    while state.logs.len() > maximum_logs {
        state.logs.pop_front();
    }
    false
}

fn drain_events(
    events: &Receiver<EventEnvelope>,
    state: &mut UiState,
    maximum_logs: usize,
) -> bool {
    let mut changed = false;
    while let Ok(event) = events.try_recv() {
        if let Some(line) = format_event(&event.event) {
            state.push_log(line);
            changed = true;
        }
    }
    while state.logs.len() > maximum_logs {
        state.logs.pop_front();
    }
    changed
}

fn drain_approvals(approvals: &Receiver<ApprovalPrompt>, state: &mut UiState) -> bool {
    let mut changed = false;
    while let Ok(prompt) = approvals.try_recv() {
        changed = true;
        if state.current_approval.is_none() {
            state.current_approval = Some(prompt);
        } else {
            state.queued_approvals.push_back(prompt);
        }
    }
    changed
}

fn resolve_approval(state: &mut UiState, approve: bool) {
    if let Some(prompt) = state.current_approval.take() {
        let decision = if approve {
            state.push_log(format!("approved: {}", prompt.request.action.tool_name));
            ApprovalDecision::Approved {
                action: prompt.request.action,
            }
        } else {
            state.push_log(format!("denied: {}", prompt.request.action.tool_name));
            ApprovalDecision::Denied {
                reason: "denied in TUI".to_owned(),
            }
        };
        let _ = prompt.response.send(decision);
    }
    state.current_approval = state.queued_approvals.pop_front();
}

fn deny_pending(state: &mut UiState) {
    while state.current_approval.is_some() {
        resolve_approval(state, false);
    }
}

fn format_event(event: &RuntimeEvent) -> Option<String> {
    match event {
        RuntimeEvent::AssistantMessage {
            content,
            tool_calls,
            ..
        } => {
            if !content.is_empty() {
                Some(format!("assistant: {content}"))
            } else if !tool_calls.is_empty() {
                Some(format!(
                    "assistant requested: {}",
                    tool_calls
                        .iter()
                        .map(|call| call.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            } else {
                None
            }
        }
        RuntimeEvent::ToolPrepared { prepared } => Some(format!(
            "prepared {}: {}",
            prepared.action.tool_name, prepared.action.arguments
        )),
        RuntimeEvent::ToolReceiptRecorded { receipt } => Some(format!(
            "tool {} [{}]: {}",
            receipt.tool_name,
            match receipt.status {
                ReceiptStatus::Succeeded => "ok",
                ReceiptStatus::Failed => "failed",
                ReceiptStatus::Denied => "denied",
                ReceiptStatus::Unknown => "unknown",
            },
            receipt
                .output
                .as_ref()
                .map(ToString::to_string)
                .or_else(|| receipt.error.clone())
                .unwrap_or_default()
        )),
        RuntimeEvent::TurnCancelled => Some("turn cancelled".to_owned()),
        RuntimeEvent::TurnFailed { error } => Some(format!("turn failed: {error}")),
        _ => None,
    }
}

fn render(stdout: &mut Stdout, state: &UiState, config: &TuiConfig) -> Result<(), std::io::Error> {
    let (width, height) = terminal::size()?;
    let width = usize::from(width.max(20));
    let approval_height = usize::from(state.current_approval.is_some()) * 4;
    let log_height = usize::from(height).saturating_sub(4 + approval_height);
    let wrapped = state
        .logs
        .iter()
        .flat_map(|line| wrap_line(line, width.saturating_sub(2)))
        .collect::<Vec<_>>();
    let visible_start = wrapped.len().saturating_sub(log_height);

    queue!(
        stdout,
        Hide,
        MoveTo(0, 0),
        Clear(ClearType::All),
        SetForegroundColor(Color::Cyan),
        Print(format!("{}  [{}]\r\n", config.title, state.thread_id)),
        ResetColor
    )?;
    for line in wrapped.iter().skip(visible_start) {
        queue!(stdout, Print(line), Print("\r\n"))?;
    }
    if let Some(prompt) = &state.current_approval {
        queue!(
            stdout,
            SetForegroundColor(Color::Yellow),
            Print("\r\nApproval required [y/n]\r\n"),
            Print(format!("tool: {}\r\n", prompt.request.action.tool_name)),
            Print(format!("action: {}\r\n", prompt.request.action.arguments)),
            ResetColor
        )?;
    }
    let status = if state.running { "running" } else { "ready" };
    queue!(
        stdout,
        MoveTo(0, height.saturating_sub(2)),
        SetForegroundColor(Color::DarkGrey),
        Print(format!("status: {status}")),
        ResetColor,
        MoveTo(0, height.saturating_sub(1)),
        Print("> "),
        Print(&state.input),
        Show
    )?;
    stdout.flush()
}

fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for character in line.chars() {
        if character == '\n' || current.chars().count() >= width {
            lines.push(std::mem::take(&mut current));
            if character == '\n' {
                continue;
            }
        }
        current.push(character);
    }
    lines.push(current);
    lines
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Waker};
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => thread::yield_now(),
        }
    }
}

struct TerminalSession {
    stdout: Stdout,
}

impl TerminalSession {
    fn enter() -> Result<Self, std::io::Error> {
        enable_raw_mode()?;
        let mut stdout = stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, Hide) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        Ok(Self { stdout })
    }

    fn stdout(&mut self) -> &mut Stdout {
        &mut self.stdout
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = execute!(self.stdout, Show, LeaveAlternateScreen, ResetColor);
        let _ = disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use agent_harness_core::{CanonicalAction, EffectKind, RetrySafety, TurnId};
    use serde_json::json;

    use super::*;

    #[test]
    fn wrapping_is_deterministic() {
        assert_eq!(wrap_line("abcdef", 3), vec!["abc", "def"]);
        assert_eq!(wrap_line("a\nb", 3), vec!["a", "b"]);
    }

    #[test]
    fn approval_channel_returns_the_exact_action() {
        let (provider, receiver) = approval_channel();
        let action = CanonicalAction::new(
            "write_file",
            json!({ "path": "/workspace/file" }),
            EffectKind::WorkspaceWrite,
            RetrySafety::Idempotent,
        );
        let expected = action.clone();
        let handle = thread::spawn(move || {
            block_on(provider.request(ApprovalRequest {
                thread_id: ThreadId::new(),
                turn_id: TurnId::new(),
                call_id: agent_harness_core::CallId::new(),
                action,
                reason: "test".to_owned(),
            }))
            .unwrap()
        });
        let prompt = receiver.recv().unwrap();
        prompt
            .response
            .send(ApprovalDecision::Approved {
                action: prompt.request.action.clone(),
            })
            .unwrap();

        assert_eq!(
            handle.join().unwrap(),
            ApprovalDecision::Approved { action: expected }
        );
    }
}
