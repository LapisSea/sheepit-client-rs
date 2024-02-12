use std::ops::Deref;
use std::path::Path;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use crate::BASE_URL;

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
pub enum ComputeMethod {
	CpuGpu,
	Cpu,
	Gpu,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
pub struct Config {
	pub login: Box<str>,
	pub password: Box<str>,
	pub hostname: Box<str>,
	pub workPath: Box<Path>,
	pub binCachePath: Box<Path>,
	pub headless: bool,
	pub computeMethod: ComputeMethod,
	pub maxCpuCores: Option<u16>,
	pub maxMemory: Option<u64>,
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
}

impl ConfigBuild {
	pub fn make(self) -> Result<Config, String> {
		fn req<T>(val: Option<T>, err: &str) -> Result<T, String> {
			match val {
				None => Err(err.into()),
				Some(value) => Ok(value),
			}
		}
		Ok(Config {
			login: req(self.login, "The -login <username> is required")?,
			password: req(self.password, "The -password <password> is required")?,
			hostname: self.hostname.unwrap_or(BASE_URL.into()),
			workPath: req(self.workPath, "The -cache-dir <folder path> is required")?,
			binCachePath: req(self.binCachePath, "The -cache-dir <folder path> is required")?,
			headless: self.headless.unwrap_or(false),
			computeMethod: self.computeMethod.unwrap_or(ComputeMethod::Cpu),
			maxCpuCores: self.maxCpuCores,
			maxMemory: self.maxMemory,
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
			_ => { return Err(format!("Unrecognised option: \"{part}\"")); }
		}
	}
	
	Ok(res)
}