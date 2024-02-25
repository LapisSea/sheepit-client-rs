use std::sync::Arc;
use std::sync::Mutex;
use lazy_static::lazy_static;
use crate::global;
use crate::utils::{ArcMut, MutRes};

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum QuitState {
	Running,
	Quitting,
	QuittingNow,
	Quit,
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

lazy_static! {
	static ref STATE: Mutex<WorkerState> = Mutex::new(WorkerState {
		quitState: QuitState::Running,
		tasks: vec![],
		log: "".to_string(),
	});
}

pub fn state() -> &'static Mutex<WorkerState> {
	&global::STATE
}

pub fn log(str: &str) {
	let _ = state().access(|state| {
		state.log += str;
	});
}

#[macro_export]
macro_rules! log {
    () => {
	    //crate::global::log("\n");
	    println!();
    };
    ($($arg:tt)*) => {{
	    //let mut str=format!($($arg)*);
	    //str+="\n";
	    //crate::global::log(str.as_str());
	    println!($($arg)*);
    }};
}