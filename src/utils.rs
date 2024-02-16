use std::fmt::Display;
use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;
use crate::Work;
use crate::Work::TOKIO_RT;

pub type ResultMsg<T> = Result<T, String>;
pub type ResultJMsg = ResultMsg<()>;

macro_rules! tSleep {
    ($seconds:expr) => {
        tokio::time::sleep(Duration::from_secs($seconds as u64)).await;
    };
}
macro_rules! tSleepMin {
    ($seconds:expr) => {
        tokio::time::sleep(Duration::from_secs(($seconds as u64)*60)).await;
    };
}
macro_rules! tSleepMinRandRange {
    ($range:expr) => {
		let mins={
			let mut rng = rand::thread_rng();
			rng.gen_range($range)
		};
		tSleepMin!(mins);
    };
}
macro_rules! tSleepRandRange {
    ($range:expr) => {
		let mins={
			let mut rng = rand::thread_rng();
			rng.gen_range($range)
		};
		tSleep!(mins);
    };
}

macro_rules! awaitStrErr {
    ($future:expr) => {
	    $future.await.map_err(|e|"Failed to execute".to_string())?
    };
}

macro_rules! swait {
    ($future:expr,$msg:expr) => {
	 	$future.await.map_err(|e| format!("{} because: {e}",$msg))
    };
}

macro_rules! spwait {
    ($future:expr,$msg:expr,$path:expr) => {
	 	$future.await.map_err(|e| format!("{} {} because: {}",$msg, &$path.to_string_lossy(),e))
    };
}
pub trait Warn<T> {
	fn warn(self) -> Option<T>;
}

impl<T, E: Display> Warn<T> for Result<T, E> {
	fn warn(self) -> Option<T> {
		match self {
			Ok(r) => { Some(r) }
			Err(err) => {
				println!("Warn: {err}");
				None
			}
		}
	}
}

pub trait MutRes<T> {
	fn access<R>(&self, get: impl FnOnce(&mut T) -> R) -> ResultMsg<R>;
}

impl<T> MutRes<T> for Mutex<T> {
	fn access<R>(&self, get: impl FnOnce(&mut T) -> R) -> ResultMsg<R> {
		match self.lock() {
			Ok(mut v) => { Ok(get(v.deref_mut())) }
			Err(err) => {
				Err(format!("Something has gone very wrong... {err}"))
			}
		}
	}
}

#[cfg(target_os = "windows")]
pub fn blendExe() -> String { "rend.exe".to_string() }

#[cfg(target_os = "linux")]
pub fn blendExe() -> String { "rend.exe".to_string() }

#[cfg(target_os = "macos")]
pub fn blendExe() -> String {
	Path::new("Blender").join("Blender.app").join("Contents").join("MacOS").join("Blender")
		.to_string_lossy().to_string()
}