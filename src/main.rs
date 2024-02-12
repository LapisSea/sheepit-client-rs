#![allow(non_snake_case)]
#![warn(unused_imports)]
#![warn(dead_code)]

mod conf;
mod HwInfo;
mod Work;
mod ServerConf;
mod net;
mod job;

use std::{env, io};
use std::borrow::Cow;
use std::cmp::{max, min};
use std::fmt::{Display, Formatter};
use std::fs::File;
use std::future::Future;
use std::io::{Read, stdout};
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::{Arc, LockResult, Mutex};
use std::time::Duration;
use reqwest::{Body, Client, RequestBuilder, Url};
use reqwest::cookie::{CookieStore, Jar};
use serde::{de, Deserialize, Serialize};
use serde_json::from_str;
use tokio::fs;
use tokio::time::Instant;
use crate::conf::{ComputeMethod, Config};
use crate::ServerConf::ConfError;
use tokio::sync::mpsc;
use rand::{random, Rng};
use reqwest::multipart::{Form, Part};
use crate::job::{ClientErrorType, Job, JobRequestStatus, JobResponse};
use crate::net::{Req, RequestEndPoint, VFile};

pub const BASE_URL: &str = "https://client.sheepit-renderfarm.com";

pub const CLIENT_VERSION: &str = "7.23353.0";

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

#[derive(Debug, Serialize, Deserialize)]
struct LongSummaryStatistics {
	count: usize,
	sum: usize,
	min: usize,
	max: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct SpeedTestTarget {
	url: String,
	speedtest: Duration,
	ping: LongSummaryStatistics,
}

#[derive(Debug, Serialize, Deserialize)]
struct ServerConfig {
	publicKey: Option<String>,
	speedTestUrls: Vec<SpeedTestTarget>,
	requestJob: RequestEndPoint,
	downloadBinary: RequestEndPoint,
	downloadChunk: RequestEndPoint,
	error: RequestEndPoint,
	keepMeAlive: RequestEndPoint,
	logout: RequestEndPoint,
	speedTestAnswer: RequestEndPoint,
}


fn main() -> ExitCode {
	let res = Work::block(start()).map_err(|e| format!("Aborting client because:\n{e}"));
	match res {
		Ok(_) => {
			ExitCode::SUCCESS
		}
		Err(errMsg) => {
			eprintln!("{errMsg}");
			ExitCode::FAILURE
		}
	}
}

async fn start() -> Result<(), String> {
	println!("###Start");
	let hwInfo = Work::spawn(HwInfo::collectInfo());
	let mut clientConf = conf::read(&mut env::args().skip(1).map(|x| x.into()))?.make()?;
	
	let cookieJar = Arc::new(Jar::default());
	let httpClient = Client::builder()
		.connect_timeout(Duration::from_secs(30))
		.timeout(Duration::from_secs(300))
		.cookie_provider(cookieJar.clone())
		.user_agent(format!(
			"Rust{}",
			rustc_version::version()
				.map(|v| format!("/{}.{}.{}", v.major, v.minor, v.patch))
				.unwrap_or_default()
		))
		.build().map_err(|e| format!("Failed to create http client: {e}"))?;
	
	let hwInfo = hwInfo.await.map_err(|e| "Failed to execute HwInfo::collectInfo".to_string())??;
	
	println!("{:#?}", hwInfo);
	println!("{:#?}", clientConf);
	
	let httpClient = Arc::new(httpClient);
	
	let mut attempts = 1;
	
	'loginLoop:
	loop {
		let serverConf = tryConnect(httpClient.clone().as_ref(), &mut clientConf, &hwInfo).await?;
		//println!("{:#?}", serverConf);
		println!("Server config loaded");
		
		let server = ServerConnection {
			httpClient: httpClient.clone(),
			serverConf,
			clientConf: clientConf.clone(),
			hwInfo: hwInfo.clone(),
		};
		
		let state = ClientState {
			shouldRun: true,
			paused: false,
		};
		
		let server = Arc::new(server);
		let state = Arc::new(Mutex::new(state));
		
		keepAliveLoop(server.clone(), state.clone());
		
		if let Err(err) = init(state.clone(), &server).await {
			println!("Failed to init: {err}");
			stop(state.clone());
		}
		
		let server = server.as_ref();
		loop {
			let paused;
			{
				let state = state.lock().unwrap();
				if !state.shouldRun { break; }
				paused = state.paused;
			}
			if paused {
				tSleep!(1);
				continue;
			}
			
			
			match run(state.clone(), server).await {
				AppLoopAction::Continue => {
					attempts = 1;
					continue;
				}
				AppLoopAction::CreateNewSession => {
					stop(state.clone());
					if attempts == 3 {
						break;
					}
					attempts += 1;
					tSleep!(5);
					continue 'loginLoop;
				}
				AppLoopAction::FatalStop(fatal) => {
					let mut state = state.lock().unwrap();
					state.shouldRun = false;
					println!("A fatal error has occoured. Shutting down because:\n\t{fatal}");
				}
			}
		}
		
		cleanup(state, server).await;
		return Ok(());
	}
}

fn durf(d: Duration) -> String {
	format!("{:.3}ms", d.as_millis() as f64 / 1000f64)
}

fn stop(state: Arc<Mutex<ClientState>>) {
	let mut state = state.lock().unwrap();
	state.shouldRun = false;
}

fn pause(state: Arc<Mutex<ClientState>>) {
	let mut state = state.lock().unwrap();
	state.paused = true;
}

fn keepAliveLoop(server: Arc<ServerConnection>, state: Arc<Mutex<ClientState>>) {
	Work::spawn(async move {
		let state = state.deref();
		let keepAlivePeriod = Duration::from_secs(15 * 60);
		let mut lastPoke = Instant::now() - (keepAlivePeriod * 2);
		loop {
			let state = {
				match state.lock() {
					Ok(state) => { (*state).clone() }
					Err(_) => { break; }
				}
			};
			
			if !state.shouldRun { break; }
			let time = Instant::now() - lastPoke;
			let toSleep = keepAlivePeriod.checked_sub(time).unwrap_or(Duration::ZERO) / 2;
			if toSleep > Duration::ZERO {
				tokio::time::sleep(toSleep).await;
				continue;
			}
			
			let server = server.clone();
			if server.keepMeAlive(state).await {
				lastPoke = Instant::now();
			}
		}
	});
}

async fn tryConnect(httpClient: &Client, conf: &mut Config, hwInfo: &HwInfo::HwInfo) -> Result<ServerConfig, String> {
	let mut attempt = 1;
	let maxAttempts = 5;
	loop {
		match ServerConf::fetchNew(httpClient, conf, hwInfo).await {
			Ok(sConf) => {
				if let Some(pk) = &sConf.publicKey {
					conf.password = pk.as_str().into();
				}
				return Ok(sConf);
			}
			Err(e) => {
				if let ConfError::FatalStatus(code) = &e {
					return Err(format!("Failed to establish connection to server due to fatal error: {code}"));
				}
				if attempt == maxAttempts {
					return Err("Failed to establish connection to server".into());
				}
				println!("Failed to connect on attempt {attempt}/{maxAttempts}. Reason:\n\t{e}");
				tSleep!(attempt*10);
				attempt += 1;
			}
		}
	}
}

struct ServerConnection {
	httpClient: Arc<Client>,
	serverConf: ServerConfig,
	clientConf: Config,
	hwInfo: HwInfo::HwInfo,
}


impl ServerConnection {
	async fn requestJob(&self) -> Result<JobResponse, String> {
		println!("Requesting job");
		
		let cores = {
			let maxCores = self.hwInfo.cores;
			if let Some(maxCoresConf) = self.clientConf.maxCpuCores {
				max(if maxCores == 1 { 1 } else { 2 }, min(maxCoresConf, maxCores))
			} else {
				maxCores
			}
		};
		
		let maxMemory = {
			let gig = 1024 * 1024;
			let freeMemory = max(HwInfo::getSystemFreeMemory(), gig) - gig;
			min(self.clientConf.maxMemory.unwrap_or(u64::MAX), freeMemory)
		};
		
		let res = self.serverConf.requestJob.post(&self.httpClient, self.clientConf.hostname.as_ref()).query(&[
			("computemethod", match self.clientConf.computeMethod {
				ComputeMethod::CpuGpu => { "0" }
				ComputeMethod::Cpu => { "1" }
				ComputeMethod::Gpu => { "2" }
			}),
			("network_dl", "6969"),//TODO
			("network_up", "6969"),//TODO
			("cpu_cores", cores.to_string().as_str()),
			("ram_max", maxMemory.to_string().as_str()),
			("rendertime_max", "0"),//TODO
		]).send().await?.xml::<JobResponse>().await;
		match &res {
			Ok(res) => {
				println!("Requested job\n{:#?}", res);
			}
			Err(err) => {
				println!("Failed to request job: {err}");
			}
		}
		res
	}
	
	fn downloadBinary(&self) {
		todo!()
	}
	
	fn downloadChunk(&self) {
		todo!()
	}
	
	async fn keepMeAlive(&self, state: ClientState) -> bool {
		println!("keepMeAlive start");
		let res = self.serverConf.keepMeAlive.post(&self.httpClient, self.clientConf.hostname.as_ref()).query(&[
			("paused", state.paused)
		]).send().await;
		if res.is_ok() {
			println!("keepMeAlive ok");
		}
		res.is_ok()
	}
	
	async fn logout(&self) -> Result<(), String> {
		println!("Logging out");
		let res = self.serverConf.logout.get(&self.httpClient, self.clientConf.hostname.as_ref()).send().await.map(|_| ());
		if res.is_ok() {
			println!("Logged out");
		}
		res
	}
	
	fn speedTestAnswer(&self) {
		todo!()
	}
	
	async fn error(&self, status: ClientErrorType, log: String) -> Result<(), String> {
		let path = Path::new("/tmp/err.txt");
		let ext = path.extension().unwrap().to_string_lossy();
		
		let file = Cow::Owned(log.as_bytes().to_vec());
		let file = Part::bytes(file);
		let url = self.serverConf.error.makeUrl(&self.clientConf.hostname);
		let url = url.as_str();
		let form = Form::new()
			.part(
				path.file_name().unwrap_or_default().to_string_lossy(),
				file
					.mime_str(match ext.deref() {
						"txt" => { mime::TEXT_PLAIN }
						_ => { return Err(format!("Could not find mime type of: {ext}")); }
					}.as_ref()).unwrap()
					.file_name(path.to_string_lossy().to_string()),
			);
		let res = self.httpClient.post(url).multipart(form);
		let res = res.query(&[
			("type", (status as u32).to_string())
		]);
		let _ = net::send(url, res).await?;
		Ok(())
	}
}

#[derive(Debug, Clone)]
struct ClientState {
	shouldRun: bool,
	paused: bool,
}

async fn init(state: Arc<Mutex<ClientState>>, server: &ServerConnection) -> Result<(), String> {
// 	server.error(ClientErrorType::OK, r#"
// ====================================================================================================
// LapisSea  /  LapisSea  /  windows  /  SheepIt v7.23353.0
// AMD Ryzen 9 5900X 12-Core Processor              x3  6.6 GB RAM
// CUDA_NVIDIA GeForce RTX 3070_0000:2b:00_OptiX   NVIDIA GeForce RTX 3070   8.0 GB VRAM
// ====================================================================================================
// Project ::: No project allocated.
// ERROR Type :: OK
// ====================================================================================================
//
// 10-02 10:03:14 (info) OS: windows 10 build 19045 amd64
// 10-02 10:03:14 (info)     CFG: version:                   7.23353.0-2-ge27fc30
//     CFG: configFilePath:            null
//     CFG: workingDirectory:          H:\sheepit\sheepit
//     CFG: sharedDownloadsDirectory:  null
//     CFG: storageDirectory:          H:\sheepit\sheepit_binary_cache
//     CFG: userHasSpecifiedACacheDir: true
//     CFG: static_exeDirName:         exe
//     CFG: login:                     LapisSea
//     CFG: proxy:
//     CFG: maxUploadingJob:           3
//     CFG: nbCores:                   3
//     CFG: maxAllowedMemory:          6877319
//     CFG: maxRenderTime:             1800
//     CFG: priority:                  19
//     CFG: computeMethod:             GPU
//     CFG: GPUDevice:                 GPUDevice [type=OPTIX, model='NVIDIA GeForce RTX 3070', memory=8589410304, id=CUDA_NVIDIA GeForce RTX 3070_0000:2b:00_OptiX, driverVersion=551.23]
//     CFG: detectGPUs:                true
//     CFG: printLog:                  false
//     CFG: requestTime:               null
//     CFG: shutdownTime:              -1
//     CFG: shutdownMode:              soft
//     CFG: extras:
//     CFG: autoSignIn:                true
//     CFG: useSysTray:                true
//     CFG: headless:                  true
//     CFG: UIType:                    swing
//     CFG: hostname:                  LapisSea
//     CFG: theme:                     dark
// 10-02 10:03:14 (info) FSHealth: FilesystemHealthCheck started
// 10-02 10:03:14 (debug) Sending error to server (type: OK)
// "#.to_string()).await?;
// 	tSleep!(4);
	Ok(())
}

enum AppLoopAction {
	Continue,
	CreateNewSession,
	FatalStop(String),
}

async fn run(state: Arc<Mutex<ClientState>>, server: &ServerConnection) -> AppLoopAction {
	let job = match server.requestJob().await {
		Ok(job) => { job }
		Err(err) => {
			println!("Failed to request job! Reason:\n\t{}", err);
			tSleepMinRandRange!(15..=30);
			return AppLoopAction::Continue;
		}
	};
	
	let res = match processJob(state.clone(), server, job).await {
		Ok(res) => { res }
		Err(err) => { return AppLoopAction::FatalStop(err); }
	};
	match res {
		JobResult::CreateNewSession => { AppLoopAction::CreateNewSession }
		JobResult::NewJob(job) => {
			match work(state.clone(), server, job).await {
				Ok(_) => { AppLoopAction::Continue }
				Err(err) => { AppLoopAction::FatalStop(err) }
			}
		}
		JobResult::JustWait { min, max } => {
			if min == max {
				if min > 0 { tSleep!(min); }
			} else {
				tSleepMinRandRange!(min..=max);
			}
			AppLoopAction::Continue
		}
	}
}

async fn work(state: Arc<Mutex<ClientState>>, server: &ServerConnection, job: Job) -> Result<(), String> {
	println!("Working on {:#?}", job);
	tSleep!(5);
	Err("I don't wanna work".to_string())
}

enum JobResult {
	CreateNewSession,
	NewJob(Job),
	JustWait { min: u32, max: u32 },
}

impl JobResult {
	fn JustWaitFixed(secs: u32) -> JobResult {
		JobResult::JustWait { min: secs, max: secs }
	}
}

async fn processJob(state: Arc<Mutex<ClientState>>, server: &ServerConnection, job: JobResponse) -> Result<JobResult, String> {
	if let Some(fileMD5s) = &job.fileMD5s {
		for path in fileMD5s.iter()
			.filter(|f| f.action.clone().map(|a| a.eq("delete")).unwrap_or(false))
			.map(|f| f.md5.as_str())
			.filter(|h| !h.is_empty())
			.flat_map(|h| [
				server.clientConf.binCachePath.join(Path::new(h)),
				server.clientConf.workPath.join(Path::new(h))
			]) {
			if let Err(err) = fs::remove_file(&path).await {
				let s = path.as_os_str();
				println!("Failed to delete {} because: {err}", s.to_string_lossy().deref());
			}
		}
	}
	if let Some(stat) = &job.sessionStats {
		println!("User status: {:#?}", stat);
	}
	match &job.status() {
		JobRequestStatus::Ok | JobRequestStatus::Unknown => {}
		JobRequestStatus::Nojob => {
			println!("There is no job right now. Sleeping for a bit...");
			return Ok(JobResult::JustWaitFixed(2 * 60));
		}
		JobRequestStatus::ErrorNoRenderingRight => { return Err("Client has no right to render!".to_string()); }
		JobRequestStatus::ErrorDeadSession => { return Ok(JobResult::CreateNewSession); }
		JobRequestStatus::ErrorSessionDisabled => { return Err("The session is disabled!".to_string()); }
		JobRequestStatus::ErrorSessionDisabledDenoisingNotSupported => { return Err("Deonising is not supported!".to_string()); }
		JobRequestStatus::ErrorInternalError => {
			println!("The server had an error! Sleeping for a bit...");
			return Ok(JobResult::JustWaitFixed(5 * 60));
		}
		JobRequestStatus::ErrorRendererNotAvailable => { return Err("The session is disabled!".to_string()); }
		JobRequestStatus::ServerInMaintenance => {
			println!("The server is in maintenance! Sleeping for a bit...");
			return Ok(JobResult::JustWait { min: 20 * 60, max: 30 * 60 });
		}
		JobRequestStatus::ServerOverloaded => {
			println!("The server is overloaded! Sleeping for a bit...");
			return Ok(JobResult::JustWait { min: 10 * 60, max: 30 * 60 });
		}
	}
	
	Ok(job.renderTask.map(|t| JobResult::NewJob(t.into())).unwrap_or(JobResult::JustWaitFixed(1)))
}

async fn cleanup(state: Arc<Mutex<ClientState>>, server: &ServerConnection) {
	match server.logout().await {
		Ok(_) => {}
		Err(_) => { println!("Failed to logout"); }
	}
}
