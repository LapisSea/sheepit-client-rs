use std::{
	io::{self, Stdout},
	time::Duration,
};
use std::cmp::{max, min};
use std::ops::Deref;
use std::sync::Mutex;

use anyhow::{anyhow, Context, Error, Result};
use crossterm::{
	event::{self, Event, KeyCode},
	execute,
	terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use rand::RngCore;
use ratatui::{prelude::*, widgets::*, widgets};
use ratatui::layout::Offset;
use ratatui::style::palette::tailwind;
use ratatui::symbols::border;
use ratatui::widgets::block::Title;
use serde::de::Unexpected::Option;
use tokio::time::Instant;
use crate::global;
use crate::global::{QuitState, WorkerState};
use crate::utils::{ArcMut, MutRes, ResultJMsg, ResultMsg};

type Term = Terminal<CrosstermBackend<Stdout>>;

const MAX_TASKS: u16 = 10;
const QUIT_KEY: char = 'c';

fn setup_terminal() -> Result<Term> {
	let mut stdout = io::stdout();
	enable_raw_mode().context("failed to enable raw mode")?;
	execute!(stdout, EnterAlternateScreen).context("unable to enter alternate screen")?;
	Terminal::new(CrosstermBackend::new(stdout)).context("creating terminal failed")
}

fn restore_terminal(terminal: &mut Term) -> Result<()> {
	disable_raw_mode().context("failed to disable raw mode")?;
	execute!(terminal.backend_mut(), LeaveAlternateScreen).context("unable to switch to main screen")?;
	terminal.show_cursor().context("unable to show cursor")
}

pub fn runUi(state: &Mutex<WorkerState>) -> Result<()> {
	let mut terminal = setup_terminal().context("setup failed")?;
	let res = run(&mut terminal, state).context("app loop failed");
	restore_terminal(&mut terminal).context("restore terminal failed")?;
	res
}

struct UIState {
	spinId: u8,
	task_scroll: u16,
	log_scroll: u16,
	lastCheck: Instant,
}

fn run(terminal: &mut Term, state: &Mutex<WorkerState>) -> ResultJMsg {
	let uiState = &mut UIState { spinId: 0, task_scroll: 0, log_scroll: 0, lastCheck: Instant::now() };
	
	let mut lastState = None;
	let mut lastRefresh = Instant::now();
	loop {
		let cmd = poolInputs(state, uiState)?;
		match cmd {
			UILoop::Sleep => {}
			UILoop::Normal | UILoop::Force => {
				let (shouldQuit, stateChange) = lastState.as_ref().map(|l| {
					state.access(|s| (matches!(s.quitState, QuitState::Quit), s != l))
				}).unwrap_or(Ok((false, true)))?;
				if shouldQuit {
					return Ok(());
				}
				if stateChange ||
					matches!(cmd, UILoop::Force) ||
					lastRefresh.elapsed() > Duration::from_secs(2)
				{
					if stateChange { lastState = Some(state.access(|s| s.clone())?); }
					lastRefresh = Instant::now();
					terminal.draw(|f| {
						if let Err(e) = render_app(f, state, uiState) {
							f.render_widget(Text::from(e.to_string()).fg(Color::Red), f.size());
						}
					})?;
				}
				uiState.lastCheck = Instant::now();
			}
			UILoop::Quit => {
				return Ok(());
			}
		}
	}
}

fn render_app(frame: &mut Frame, state: &Mutex<WorkerState>, uiState: &mut UIState) -> ResultJMsg {
	let state = state.lock().map_err(|e| anyhow!("State corrupted: {e}"))?;
	let scrollEnd = uiState.task_scroll + MAX_TASKS - 1;
	let off = state.tasks.len() as i32 - scrollEnd as i32;
	if off < 0 {
		uiState.task_scroll = max(0, uiState.task_scroll as i32 + off) as u16;
	}
	
	let runStr = match state.quitState {
		QuitState::Running => { "Running" }
		QuitState::Quitting => { "Quitting after job. (quit again to cancel job)" }
		QuitState::QuittingNow => { "Quitting now..." }
		QuitState::Quit => { "Closing..." }
	};
	
	let spin = r"|/-\";
	let sid = (uiState.spinId + 1) % (spin.len() as u8);
	uiState.spinId = sid;
	
	let border = Block::bordered()
		.title(format!(" Sheepit Client (press 'Ctrl + {}' to quit) ", QUIT_KEY.to_uppercase()))
		.title_bottom(Line::from(format!("State: {runStr} {} ", spin.chars().nth(uiState.spinId as usize).unwrap())).left_aligned())
		.title_alignment(Alignment::Center)
		.border_type(BorderType::Rounded);
	
	frame.render_widget(border, frame.size());
	
	let area = frame.size().inner(&Margin::new(1, 1));
	
	let layout =
		Layout::vertical([Constraint::Length(MAX_TASKS), Constraint::Fill(2)])
			.split(area)
			.to_vec();
	
	let mut area = layout[0];
	area.x += 1;
	area.width -= 1;
	
	for (i, t) in state.tasks.iter().skip(uiState.task_scroll as usize).enumerate().take(area.height as usize) {
		let mut area = area;
		area.y += i as u16;
		area.height = 1;
		area.width -= 1;
		frame.render_widget(
			Gauge::default()
				.label(t.name.as_str())
				.gauge_style(tailwind::BLUE.c800)
				.percent((t.progress * 100.0) as u16),
			area,
		);
	}
	
	// let items = state.tasks.iter().flat_map(|t| [Line::from(t.name.as_str()),Line::raw("")]).collect::<Vec<_>>();
	// let paragraph = Paragraph::new(items).scroll((uiState.task_scroll, 0));
	render_widgetScroll(frame, Line::raw(""), area, state.tasks.len() * 2, uiState.task_scroll);
	
	let area = layout[1];
	
	frame.render_widget(Block::new().title_top("Log: ").borders(Borders::TOP), area);
	
	let mut area = area;
	area.y += 1;
	area.height = area.height.saturating_sub(1);
	area.x += 1;
	area.width -= 1;
	
	let wrapWidth = area.width.saturating_sub(1) as usize;
	
	let lines = state.log.as_str().lines().flat_map(|line| {
		let mut wrap = vec![];
		let mut rest = line;
		if rest.is_empty() { wrap.push(rest); }
		while !rest.is_empty() {
			let len = min(wrapWidth, rest.len());
			wrap.push(&rest[0..len]);
			rest = &rest[len..];
		}
		wrap
	}).map(|l| Line {
		spans: vec![Span::raw(l)],
		style: Default::default(),
		alignment: None,
	}).collect::<Vec<_>>();
	
	
	let count = lines.iter().map(|l| ceiling_divide(l.width(), area.width.saturating_sub(1) as usize)).sum::<usize>();
	let scroll = count.saturating_sub((uiState.log_scroll + area.height) as usize);
	let paragraph = Paragraph::new(lines).scroll((scroll as u16, 0));
	render_widgetScroll(frame, paragraph, area, count, scroll);
	Ok(())
}

fn ceiling_divide(dividend: usize, divisor: usize) -> usize {
	if divisor <= 1 { return dividend; }
	(dividend + divisor - 1) / divisor
}

fn render_widgetScroll<W: Widget, S: Into<usize>, S2: Into<usize>>(frame: &mut Frame, widget: W, area: Rect, widgetHeight: S, scroll: S2) {
	let height = widgetHeight.into();
	let showScroll = height > area.height as usize;
	let mut contentArea = area;
	if showScroll { contentArea.width -= 1; }
	frame.render_widget(widget, contentArea);
	
	if showScroll {
		let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
		frame.render_stateful_widget(
			scrollbar,
			area,
			&mut ScrollbarState::new(max(0, height as i64 - area.height as i64) as usize).position(scroll.into()),
		);
	}
}

enum UILoop {
	Normal,
	Force,
	Quit,
	Sleep,
}

fn poolInputs(state: &Mutex<WorkerState>, uiState: &mut UIState) -> Result<UILoop> {
	if !event::poll(Duration::from_millis(5)).context("event poll failed")? {
		if uiState.lastCheck.elapsed() > Duration::from_millis(100) {
			return Ok(UILoop::Normal);
		}
		return Ok(UILoop::Sleep);
	}
	
	match event::read().context("event read failed")? {
		Event::FocusGained => {}
		Event::FocusLost => {}
		Event::Key(mut key) => {
			if key.kind == KeyEventKind::Press {
				if let KeyCode::Char(c) = key.code {
					key.code = KeyCode::Char(c.to_ascii_lowercase());
				}
				
				match key.code {
					KeyCode::Char(QUIT_KEY) => {
						if key.modifiers == KeyModifiers::CONTROL {
							state.access(|state| {
								match state.quitState {
									QuitState::Running => {
										state.quitState = QuitState::Quitting;
									}
									QuitState::Quitting => {
										state.quitState = QuitState::QuittingNow;
									}
									QuitState::QuittingNow | QuitState::Quit => {}
								}
							})?;
						}
					}
					KeyCode::Char('1') => {
						if uiState.task_scroll > 0 {
							uiState.task_scroll -= 1;
							return Ok(UILoop::Force);
						}
					}
					KeyCode::Char('2') => {
						uiState.task_scroll += 1;
						return Ok(UILoop::Force);
					}
					_ => {}
				}
			}
		}
		Event::Mouse(_) => {}
		Event::Paste(_) => {}
		Event::Resize(_, _) => { return Ok(UILoop::Force); }
	}
	Ok(UILoop::Normal)
}
