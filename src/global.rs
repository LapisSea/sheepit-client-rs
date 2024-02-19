#[derive(Debug, Clone, Eq, PartialEq)]
pub enum QuitState {
	Running,
	Quitting,
	QuittingNow,
}

#[derive(Debug, Clone)]
pub struct Task {
	pub name: String,
	pub progress: f32,
}

impl PartialEq for Task {
	fn eq(&self, other: &Self) -> bool {
		return self.name == other.name;
	}
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkerState {
	pub quitState: QuitState,
	pub tasks: Vec<Task>,
	pub log: String,
}

impl WorkerState {
	pub fn new() -> Self {
		Self {
			quitState: QuitState::Running,
			tasks: vec![],
			log: "".to_string(),
		}
	}
}