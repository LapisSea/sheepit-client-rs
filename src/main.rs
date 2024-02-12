#![allow(non_snake_case)]
#![allow(unused_imports, unused_variables, dead_code)]

mod conf;
mod HwInfo;
mod Work;
mod ServerConf;
mod net;
mod job;

use std::{env};
use std::borrow::Cow;
use std::cmp::{max, min};
use std::ops::{Deref};
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use reqwest::{Client};
use reqwest::cookie::{Jar};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::time::Instant;
use crate::conf::{ComputeMethod, Config};
use crate::ServerConf::ConfError;
use rand::Rng;
use reqwest::multipart::{Form, Part};
use crate::job::{Chunk, ClientErrorType, Job, JobRequestStatus, JobResponse};
use crate::net::{RequestEndPoint, TransferStats};

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
macro_rules! tSleepRandRange {
    ($range:expr) => {
		let mins={
			let mut rng = rand::thread_rng();
			rng.gen_range($range)
		};
		tSleep!(mins);
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


#[derive(Debug, Clone)]
struct ClientState {
	shouldRun: bool,
	paused: bool,
	downloadStats: TransferStats,
}

fn modState(state: Arc<Mutex<ClientState>>, modf: impl FnOnce(&mut ClientState)) {
	let mut state = match state.lock() {
		Ok(g) => { g }
		Err(err) => {
			eprintln!("Something has gone very wrong... {err}");
			return;
		}
	};
	modf(&mut state);
}

fn getFromState<T>(state: Arc<Mutex<ClientState>>, modf: impl FnOnce(&mut ClientState) -> T) -> Result<T, String> {
	let mut state = match state.lock() {
		Ok(g) => { g }
		Err(err) => {
			return Err(format!("Something has gone very wrong... {err}"));
		}
	};
	Ok(modf(&mut state))
}

macro_rules! setState {
    ($state:expr, $field:ident,$val:expr) => {
        modState($state.clone(), |mds|mds.$field=$val)
    };
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
	
	let hwInfo = hwInfo.await.map_err(|_| "Failed to execute HwInfo::collectInfo".to_string())??;
	
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
			downloadStats: Default::default(),
		};
		
		let server = Arc::new(server);
		let state = Arc::new(Mutex::new(state));
		
		keepAliveLoop(server.clone(), state.clone());
		
		if let Err(err) = init(state.clone(), &server).await {
			println!("Failed to init: {err}");
			setState!(state, shouldRun, false);
		}
		
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
			
			
			match run(state.clone(), server.clone()).await {
				AppLoopAction::Continue => {
					attempts = 1;
					continue;
				}
				AppLoopAction::CreateNewSession => {
					setState!(state, shouldRun, false);
					if attempts == 3 {
						break;
					}
					attempts += 1;
					tSleep!(5);
					continue 'loginLoop;
				}
				AppLoopAction::FatalStop(fatal) => {
					setState!(state, shouldRun, false);
					println!("A fatal error has occurred. Shutting down because:\n\t{fatal}");
				}
			}
		}
		
		cleanup(state, server.as_ref()).await;
		return Ok(());
	}
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
	let mut attempt: u32 = 1;
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
				let att = min(attempt, 9);
				let time = att * att * 20;
				let timeMsg = if attempt >= 10 {
					"Some time..".to_string()
				} else {
					format!("{time} seconds")
				};
				println!("Failed to establish connection to server on attempt {attempt}. Trying again in {timeMsg}.\n\tReason: {e}");
				if attempt == 10 {
					println!("We may be here for a while")
				}
				if attempt >= 10 {
					tSleepRandRange!(time/2..=time);
				} else {
					tSleep!(time);
				}
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


macro_rules! servReq {
    ($server:expr, $endpoint:ident,$method:ident) => {
        $server.serverConf.$endpoint.$method(&$server.httpClient, &$server.clientConf.hostname)
    };
}

impl ServerConnection {
	async fn requestJob(&self, state: Arc<Mutex<ClientState>>) -> Result<JobResponse, String> {
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
		
		let (downloadRate, uploadRate) = getFromState(state, |state| {
			(state.downloadStats.toRate(), "0")
		})?;
		
		let res = servReq!(self,requestJob,post).query(&[
			("computemethod", match self.clientConf.computeMethod {
				ComputeMethod::CpuGpu => { "0" }
				ComputeMethod::Cpu => { "1" }
				ComputeMethod::Gpu => { "2" }
			}.to_string()),
			("network_dl", downloadRate.to_string()),
			("network_up", uploadRate.to_string()),
			("cpu_cores", cores.to_string()),
			("ram_max", maxMemory.to_string()),
			("rendertime_max", "0".to_string()),//TODO
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
	
	async fn downloadBinary(&self, job: Arc<Job>) -> Result<TransferStats, String> {
		let binPath = self.clientConf.binCachePath.deref();
		let rendererMd5 = job.info.rendererInfo.md5.as_str();
		let jobId = job.info.id.to_owned();
		
		let fileName = binPath.join(format!("{}.zip", rendererMd5));
		
		let req = servReq!(self,downloadBinary,get).query(&[("job", jobId)]);
		
		net::downloadFile(fileName.as_ref(), req, rendererMd5).await
	}
	
	async fn downloadChunk(&self, chunk: Chunk) -> Result<TransferStats, String> {
		let id = chunk.id.clone();
		let fileName = self.clientConf.workPath.join(format!("{}.wool", id));
		let fileName = fileName.as_path();
		
		let req = servReq!(self,downloadChunk,get).query(&[("chunk", id.as_str())]);
		
		let stats = net::downloadFile(fileName, req, chunk.md5.as_ref()).await?;
		println!("Downloaded {}", fileName.to_string_lossy());
		Ok(stats)
	}
	
	async fn keepMeAlive(&self, state: ClientState) -> bool {
		println!("keepMeAlive start");
		let res = servReq!(self,keepMeAlive,post).query(&[
			("paused", state.paused)
		]).send().await;
		if res.is_ok() {
			println!("keepMeAlive ok");
		}
		res.is_ok()
	}
	
	async fn logout(&self) -> Result<(), String> {
		println!("Logging out");
		let res = servReq!(self,logout,get).send().await.map(|_| ());
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

async fn init(state: Arc<Mutex<ClientState>>, server: &ServerConnection) -> Result<(), String> {
	Ok(())
}

enum AppLoopAction {
	Continue,
	CreateNewSession,
	FatalStop(String),
}

async fn run(state: Arc<Mutex<ClientState>>, server: Arc<ServerConnection>) -> AppLoopAction {
	let job = match server.requestJob(state.clone()).await {
		Ok(job) => { job }
		Err(err) => {
			println!("Failed to request job! Reason:\n\t{}", err);
			tSleepMinRandRange!(15..=30);
			return AppLoopAction::Continue;
		}
	};
	
	let res = match preprocessJob(server.as_ref(), job).await {
		Ok(res) => { res }
		Err(err) => { return AppLoopAction::FatalStop(err); }
	};
	match res {
		JobResult::CreateNewSession => { AppLoopAction::CreateNewSession }
		JobResult::NewJob(job) => {
			match execute_job(state.clone(), server, job.into()).await {
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

async fn downloadScene(server: Arc<ServerConnection>, job: Arc<Job>) -> Result<TransferStats, String> {
	let mut jobs = vec![];
	
	for chunk in job.info.archiveChunks.iter().cloned() {
		let server = server.clone();
		jobs.push(Work::spawn(async move { server.downloadChunk(chunk).await }));
	}
	
	let mut stats = TransferStats::default();
	
	for job in jobs {
		stats += job.await.map_err(|e| "failed to exec task")??;
	}
	
	Ok(stats)
}

async fn downloadExecutable(server: Arc<ServerConnection>, job: Arc<Job>) -> Result<TransferStats, String> {
	server.downloadBinary(job).await
}

enum JobResult {
	CreateNewSession,
	NewJob(Box<Job>),
	JustWait { min: u32, max: u32 },
}

impl JobResult {
	fn JustWaitFixed(secs: u32) -> JobResult {
		JobResult::JustWait { min: secs, max: secs }
	}
}

async fn preprocessJob(server: &ServerConnection, job: JobResponse) -> Result<JobResult, String> {
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
	
	Ok(job.renderTask.map(|t| JobResult::NewJob(Box::new(t.into()))).unwrap_or(JobResult::JustWaitFixed(1)))
}


async fn execute_job(state: Arc<Mutex<ClientState>>, server: Arc<ServerConnection>, job: Arc<Job>) -> Result<(), String> {
	println!("Working on job {}", job.info.name);
	
	downloadJobFiles(state, server, job).await?;
	
	tSleep!(5);
	Err("I don't wanna work".to_string())
}

async fn downloadJobFiles(state: Arc<Mutex<ClientState>>, server: Arc<ServerConnection>, job: Arc<Job>) -> Result<(), String> {
	let ds = Work::spawn(downloadScene(server.clone(), job.clone()));
	let de = Work::spawn(downloadExecutable(server.clone(), job.clone()));
	
	let mut fail = None;
	let mut stats = TransferStats::default();
	match ds.await.map_err(|_| "failed to exec task")? {
		Ok(s) => { stats += s; }
		Err(e) => { fail = Some(e); }
	}
	match de.await.map_err(|_| "failed to exec task")? {
		Ok(s) => { stats += s; }
		Err(e) => { fail = Some(e); }
	}
	
	if let Some(err) = fail {
		return Err(err);
	}
	
	modState(state, |s| s.downloadStats += stats);
	Ok(())
}

async fn cleanup(state: Arc<Mutex<ClientState>>, server: &ServerConnection) {
	match server.logout().await {
		Ok(_) => {}
		Err(_) => { println!("Failed to logout"); }
	}
}
