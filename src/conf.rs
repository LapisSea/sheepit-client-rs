use std::fmt::{Display, format, Formatter, Pointer, write};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::time::Duration;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use crate::defs::BASE_URL;
use crate::job::{Chunk, Job, JobInfo};
use crate::utils;
use crate::utils::ResultMsg;

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
pub enum ComputeMethod {
	CpuGpu,
	Cpu,
	Gpu,
}

impl Display for ComputeMethod {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		match self {
			ComputeMethod::CpuGpu => write!(f, "CPU_GPU"),
			ComputeMethod::Cpu => write!(f, "CPU"),
			ComputeMethod::Gpu => write!(f, "GPU"),
		}
	}
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
pub struct ClientConfig {
	pub login: Box<str>,
	pub password: Box<str>,
	pub hostname: Box<str>,
	pub workPath: Box<Path>,
	pub binCachePath: Box<Path>,
	pub headless: bool,
	pub computeMethod: ComputeMethod,
	pub maxCpuCores: Option<u16>,
	pub maxMemory: Option<u64>,
	pub maxRenderTime: Option<Duration>,
}

impl ClientConfig {
	pub fn rendererArchivePath(&self, job: &JobInfo) -> PathBuf {
		self.binCachePath.join(format!("{}.zip", &job.rendererInfo.md5))
	}
	pub fn rendererPath(&self, job: &JobInfo) -> PathBuf {
		self.workPath.join(&job.rendererInfo.md5)
	}
	
	pub fn rendererPathExecutable(&self, job: &JobInfo) -> PathBuf {
		self.rendererPath(job).join(utils::blendExe())
	}
	pub fn scenePath(&self, job: &JobInfo) -> PathBuf {
		self.workPath.join(&job.id)
	}
	pub fn scenePathBlend(&self, job: &JobInfo) -> PathBuf {
		self.workPath.join(&job.id).join(&job.path)
	}
	pub fn chunkPath(&self, chunk: &Chunk) -> PathBuf {
		self.workPath.join(format!("{}.wool", chunk.id))
	}
	pub fn tmpDir(&self) -> PathBuf {
		self.workPath.join("temp")
	}
}

#[derive(Default)]
pub struct ConfigBuild {
	login: Option<Box<str>>,
	password: Option<Box<str>>,
	hostname: Option<Box<str>>,
	workPath: Option<Box<Path>>,
	binCachePath: Option<Box<Path>>,
	headless: Option<bool>,
	pub computeMethod: Option<ComputeMethod>,
	pub maxCpuCores: Option<u16>,
	pub maxMemory: Option<u64>,
	pub maxRenderTime: Option<Duration>,
}

impl ConfigBuild {
	pub fn make(self) -> ResultMsg<ClientConfig> {
		fn req<T>(val: Option<T>, err: &str) -> Result<T, String> {
			match val {
				None => Err(err.into()),
				Some(value) => Ok(value),
			}
		}
		Ok(ClientConfig {
			login: req(self.login, "The -login <username> is required")?,
			password: req(self.password, "The -password <password> is required")?,
			hostname: self.hostname.unwrap_or(BASE_URL.into()),
			workPath: req(self.workPath, "The -cache-dir <folder path> is required")?
				.canonicalize().map_err(|e| format!("Failed to turn path to absolute{e}"))?.into(),
			binCachePath: req(self.binCachePath, "The -cache-dir <folder path> is required")?
				.canonicalize().map_err(|e| format!("Failed to turn path to absolute{e}"))?.into(),
			headless: self.headless.unwrap_or(false),
			computeMethod: self.computeMethod.unwrap_or(ComputeMethod::Cpu),
			maxCpuCores: self.maxCpuCores,
			maxMemory: self.maxMemory,
			maxRenderTime: self.maxRenderTime,
		})
	}
}


pub(crate) fn read(args: &mut dyn Iterator<Item=Box<str>>) -> Result<ConfigBuild, String> {
	fn requireNextO(args: &mut dyn Iterator<Item=Box<str>>) -> Result<Option<Box<str>>, String> {
		requireNext(args).map(Some)
	}
	fn requireNext(args: &mut dyn Iterator<Item=Box<str>>) -> Result<Box<str>, String> {
		match args.next() {
			None => Err("Not enough arguments".into()),
			Some(value) => Ok(value.deref().into()),
		}
	}
	
	let mut res = ConfigBuild::default();
	
	loop {
		let part;
		if let Some(v) = args.next() {
			part = v.to_lowercase();
		} else { break; }
		
		match part.as_str() {
			"-login" => { res.login = requireNextO(args)?; }
			"-password" => { res.password = requireNextO(args)?; }
			"-cache-dir" => {
				let val = requireNext(args)?;
				let val = Path::new(val.as_ref());
				res.workPath = Some(val.join(Path::new("sheepit")).into());
				res.binCachePath = Some(val.join(Path::new("sheepit_binary_cache")).into());
			}
			"-headless" => {
				let str = requireNext(args)?.to_lowercase();
				res.headless = Some(match str.as_str() {
					"true" => { true }
					"false" => { false }
					_ => { return Err("-headless can be only true or false".into()); }
				});
			}
			"-hostname" => {
				let str = requireNext(args)?;
				let url = match Url::parse(str.as_ref()) {
					Ok(u) => { u }
					Err(err) => {
						return Err(format!("Malformed -hostname: \"{err}\""));
					}
				};
				res.hostname = Some(url.to_string().into());
			}
			"-computemethod" => {
				let str = requireNext(args)?.to_lowercase();
				res.computeMethod = Some(match str.as_str() {
					"cpu" => { ComputeMethod::Cpu }
					"gpu" => { ComputeMethod::Gpu }
					"cpu_gpu" | "cpugpu" => { ComputeMethod::CpuGpu }
					_ => { return Err(format!("Unknown compute method: \"{str}\", can only be CPU, GPU, CPU_GPU")); }
				});
			}
			"-cores" => {
				let str = requireNext(args)?;
				let cores = str.parse().map_err(|e| format!("-cores must be a positive number but is: \"{str}\""))?;
				if cores == 0 { return Err("-cores must be greater than 0".to_string()); }
				res.maxCpuCores = Some(cores);
			}
			"-memory" => {
				let str = requireNext(args)?;
				let mem = str.parse().map_err(|e| format!("-memory must be a positive number but is: \"{str}\""))?;
				if mem == 0 { return Err("-memory must be greater than 0".to_string()); }
				res.maxMemory = Some(mem);
			}
			"-max-render-time" => {
				let str = requireNext(args)?;
				let time = str.parse::<u64>().map_err(|e| format!("-max-render-time <time> must be a positive number but is: \"{str}\""))?;
				let str = requireNext(args)?;
				let unitMul = match str.as_ref() {
					"s" | "sec" => { 1 }
					"m" | "min" => { 60 }
					"h" | "hours" => { 60 * 60 }
					_ => { return Err(format!("-max-render-time <time> <time-unit> must be one of [s, sec, m, min, h, hours] but is {str}")); }
				};
				
				res.maxRenderTime = Some(Duration::from_secs(time * unitMul))
			}
			_ => { return Err(format!("Unrecognised option: \"{part}\"")); }
		}
	}
	
	Ok(res)
}