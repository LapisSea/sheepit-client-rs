use std::fmt::Display;
use std::future::Future;
use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use anyhow::anyhow;
use tokio::task::JoinHandle;
use crate::Work;
use crate::Work::TOKIO_RT;

pub type ResultMsg<T> = anyhow::Result<T>;
pub type ResultJMsg = ResultMsg<()>;

#[macro_export]
macro_rules! tSleepMs {
    ($millis:expr) => {
        tokio::time::sleep(Duration::from_millis($millis as u64)).await;
    };
}

#[macro_export]
macro_rules! tSleep {
    ($seconds:expr) => {
        tokio::time::sleep(Duration::from_secs($seconds as u64)).await;
    };
}
#[macro_export]
macro_rules! tSleepMin {
    ($seconds:expr) => {
        tokio::time::sleep(Duration::from_secs(($seconds as u64)*60)).await;
    };
}
#[macro_export]
macro_rules! tSleepMinRandRange {
    ($range:expr) => {
		let mins={
			let mut rng = rand::thread_rng();
			rng.gen_range($range)
		};
		tSleepMin!(mins);
    };
}
#[macro_export]
macro_rules! tSleepRandRange {
    ($range:expr) => {
		let mins={
			let mut rng = rand::thread_rng();
			rng.gen_range($range)
		};
		tSleep!(mins);
    };
}

#[macro_export]
macro_rules! awaitStrErr {
    ($future:expr) => {
	    $future.await.map_err(|e|anyhow!("Failed to execute"))?
    };
}

#[macro_export]
macro_rules! swait {
    ($future:expr,$msg:expr) => {
	 	$future.await.map_err(|e| anyhow!("{} because: {e}",$msg))
    };
}

#[macro_export]
macro_rules! spwait {
    ($future:expr,$msg:expr,$path:expr) => {
	 	$future.await.map_err(|e| anyhow!("{} {} because: {}",$msg, &$path.to_string_lossy(),e))
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

pub type ArcMut<T> =Arc<Mutex<T>>;

pub trait MutRes<T> {
	fn access<R>(&self, get: impl FnOnce(&mut T) -> R) -> ResultMsg<R>;
}

impl<T> MutRes<T> for Mutex<T> {
	fn access<R>(&self, get: impl FnOnce(&mut T) -> R) -> ResultMsg<R> {
		match self.lock() {
			Ok(mut v) => { Ok(get(v.deref_mut())) }
			Err(err) => {
				Err(anyhow!("Something has gone very wrong... {err}"))
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

#[macro_export]
macro_rules! deferAsync {
	($e:expr) => {
		let _defer = defer::defer(|| {
			let handle=Work::spawn(async move {
				$e
			});
			while !handle.is_finished() {
				std::thread::sleep(Duration::from_millis(1));
			}
		});
	};
}