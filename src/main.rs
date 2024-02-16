#![allow(non_snake_case)]
#![allow(unused_imports, unused_variables, dead_code)]

mod conf;
mod HwInfo;
mod Work;
mod ServerConf;
mod net;
mod job;
mod fmd5;
mod files;
#[macro_use]
mod utils;
mod defs;
mod process;

use std::{env};
use std::borrow::Cow;
use std::cmp::{max, min};
use std::fmt::{Display, format};
use std::iter::zip;
use std::ops::{Deref};
use std::path::{Path, PathBuf};
use std::process::{ExitCode, ExitStatus, Stdio};
use std::ptr::write;
use std::rc::Rc;
use std::sync::{Arc, LockResult, Mutex};
use std::time::Duration;
use futures_util::TryFutureExt;
use indoc::indoc;
use reqwest::{Client};
use reqwest::cookie::{Jar};
use serde::{Deserialize, ser, Serialize};
use tokio::fs;
use tokio::time::Instant;
use crate::conf::{ComputeMethod, ClientConfig};
use crate::ServerConf::ConfError;
use rand::Rng;
use rc_zip_tokio::{ArchiveHandle, HasCursor};
use rc_zip_tokio::rc_zip::chrono::format;
use rc_zip_tokio::rc_zip::error::Error;
use reqwest::multipart::{Form, Part};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::task::JoinHandle;
use crate::job::{Chunk, ClientErrorType, Job, JobInfo, JobRequestStatus, JobResponse};
use crate::net::{Req, RequestEndPoint, TransferStats};
use crate::utils::*;

#[derive(Debug, Serialize, Deserialize)]
struct ServerConfig {
	publicKey: Option<String>,
	speedTestUrls: Vec<net::SpeedTestTarget>,
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

pub trait ClientStateMut {
	fn stop(&self) -> ResultJMsg;
	fn pause(&self) -> ResultJMsg;
	fn reportDownload(&self, stat: TransferStats) -> ResultJMsg;
}

impl ClientStateMut for Mutex<ClientState> {
	fn stop(&self) -> ResultJMsg { self.access(|m| m.shouldRun = false) }
	fn pause(&self) -> ResultJMsg { self.access(|m| m.paused = true) }
	fn reportDownload(&self, stat: TransferStats) -> ResultJMsg { self.access(|m| m.downloadStats += stat) }
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

enum AppLoopAction {
	Continue,
	CreateNewSession,
	FatalStop(String),
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

async fn start() -> ResultJMsg {
	println!("###Start");
	let hwInfo = Work::spawn(HwInfo::collectInfo());
	let mut clientConf = conf::read(&mut env::args().skip(1).map(|x| x.into()))?.make()?;
	
	
	let mut cleanTask = Some({
		let path = (*clientConf.workPath).to_owned();
		Work::spawn(async move { files::cleanWorkingDir(path.as_path()).await })
	});
	
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
	
	let hwInfo = awaitStrErr!(hwInfo)?;
	
	println!("{:#?}", hwInfo);
	println!("{:#?}", clientConf);
	
	let httpClient = Arc::new(httpClient);
	
	let mut attempts = 1;
	
	'loginLoop:
	loop {
		let server = Arc::new(ServerConnection {
			httpClient: httpClient.clone(),
			serverConf: tryConnect(httpClient.clone().as_ref(), &mut clientConf, &hwInfo).await?,
			clientConf: clientConf.clone(),
			hwInfo: hwInfo.clone(),
			hashCache: Default::default(),
		});
		
		let state = Arc::new(Mutex::new(ClientState {
			shouldRun: true,
			paused: false,
			downloadStats: Default::default(),
		}));
		
		if let Err(err) = init(state.clone(), server.clone()).await {
			println!("Failed to init: {err}");
			state.stop()?;
		}
		
		if let Some(cleanTask) = cleanTask {
			awaitStrErr!(cleanTask).map_err(|e| format!("Failed to clean working dir because: {e}"))?;
		}
		cleanTask = None;
		
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
					state.stop()?;
					if attempts == 3 {
						break;
					}
					attempts += 1;
					tSleep!(5);
					continue 'loginLoop;
				}
				AppLoopAction::FatalStop(fatal) => {
					state.stop()?;
					println!("A fatal error has occurred. Shutting down because:\n\t{fatal}");
				}
			}
		}
		
		cleanup(state, server.as_ref()).await;
		return Ok(());
	}
}

async fn tryConnect(httpClient: &Client, conf: &mut ClientConfig, hwInfo: &HwInfo::HwInfo) -> ResultMsg<ServerConfig> {
	let mut attempt: u32 = 1;
	loop {
		match ServerConf::fetchNew(httpClient, conf, hwInfo).await {
			Ok(sConf) => {
				if let Some(pk) = &sConf.publicKey {
					conf.password = pk.as_str().into();
				}
				println!("Server config loaded");
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
	clientConf: ClientConfig,
	hwInfo: HwInfo::HwInfo,
	hashCache: fmd5::HashCache,
}

macro_rules! servReq {
    ($server:expr, $endpoint:ident,$method:ident) => {
        $server.serverConf.$endpoint.$method(&$server.httpClient, &$server.clientConf.hostname)
    };
}

impl ServerConnection {
	pub fn effectiveCores(&self) -> u16 {
		let maxCores = self.hwInfo.cores;
		if let Some(maxCoresConf) = self.clientConf.maxCpuCores {
			max(if maxCores == 1 { 1 } else { 2 }, min(maxCoresConf, maxCores))
		} else {
			maxCores
		}
	}
	
	async fn requestJob(&self, state: Arc<Mutex<ClientState>>) -> ResultMsg<JobResponse> {
		println!("Requesting job");
		
		let cores = self.effectiveCores();
		
		let maxMemory = {
			let gig = 1024 * 1024;
			let freeMemory = max(HwInfo::getSystemFreeMemory(), gig) - gig;
			min(self.clientConf.maxMemory.unwrap_or(u64::MAX), freeMemory)
		};
		
		let (downloadRate, uploadRate) = state.access(|state| {
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
	
	async fn downloadBinary(&self, job: &JobInfo) -> ResultMsg<TransferStats> {
		let md5 = job.rendererInfo.md5.as_str();
		let req = servReq!(self,downloadBinary,get).query(&[("job", &job.id)]);
		
		net::downloadFile(&self.clientConf.rendererArchivePath(job), req, md5, self.hashCache.clone()).await
	}
	
	async fn downloadChunk(&self, chunk: Chunk) -> ResultMsg<TransferStats> {
		let md5 = chunk.md5.as_ref();
		let req = servReq!(self,downloadChunk,get).query(&[("chunk", &chunk.id)]);
		
		net::downloadFile(&self.clientConf.chunkPath(&chunk), req, md5, self.hashCache.clone()).await
	}
	
	async fn keepMeAlive(&self, state: ClientState) -> bool {
		println!("keepMeAlive start");
		let res = servReq!(self,keepMeAlive,get).query(&[
			("paused", state.paused)
		]).send().await;
		match res {
			Ok(ok) => {
				println!("keepMeAlive ok");
				true
			}
			Err(err) => {
				eprintln!("keepMeAlive error: {err}");
				false
			}
		}
	}
	
	async fn logout(&self) -> ResultJMsg {
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
	
	async fn error(&self, status: ClientErrorType, log: String) -> ResultJMsg {
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

async fn init(state: Arc<Mutex<ClientState>>, server: Arc<ServerConnection>) -> ResultJMsg {
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
			} else {
				tSleep!(60);
			}
		}
	});
	
	Ok(())
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

async fn preprocessJob(server: &ServerConnection, job: JobResponse) -> ResultMsg<JobResult> {
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
	
	Ok(job.renderTask.map(|t| JobResult::NewJob(Box::new((t, server).into()))).unwrap_or(JobResult::JustWaitFixed(1)))
}

async fn execute_job(state: Arc<Mutex<ClientState>>, server: Arc<ServerConnection>, job: Arc<Job>) -> ResultJMsg {
	println!("Working on job \"{}\"", job.info.name);
	downloadJobFiles(state, server.clone(), job.clone()).await?;
	println!("Preparing work directory");
	prepareWorkingDirectory(server.clone(), job.clone()).await?;
	
	let res = render(job.clone(), server.clone()).await;
	
	let scenePath = server.clientConf.scenePath(&job.info);
	swait!(files::deleteDirDeep(scenePath.as_ref()), "Failed to delete scene directory")?;
	// swait!(tokio::fs::remove_file(server.clientConf.workPath.join("pre_load_script.py")), "Failed to delete script").warn();
	// swait!(tokio::fs::remove_file(server.clientConf.workPath.join("post_load_script.py")), "Failed to delete script").warn();
	
	res
}

async fn render(job: Arc<Job>, server: Arc<ServerConnection>) -> ResultJMsg {
	
	let mut command=process::createBlenderCommand(job.clone(),server.clone()).await?;
	command.stdout(Stdio::piped());
	command.kill_on_drop(true);
	
	println!("{:#?}", command);
	
	let mut blenderProc = command.spawn().map_err(|e| e.to_string())?;
	
	let stdin=BufReader::new(blenderProc.stdout.take().ok_or("Failed to acquire stdout")?);
	let stdinReader:JoinHandle<ResultJMsg>=Work::spawn(async move {
		let mut lines =stdin.lines();
		loop {
			let l=swait!(lines.next_line(),"failed to read std line")?;
			if l.is_none() {break;}
			let l=l.unwrap_or_default();
			
			println!("BLENDER: {l}");
			
		}
		
		Ok(())
	});
	
	
	let status=swait!(blenderProc.wait(),"failed to wait for blender")? ;
	if !status.success() { return Err(format!("Blender crashed with status {status}")); }
	
	// loop {
	// 	tokio::time::sleep(Duration::from_millis(500)).await;
	//
	// 	match blenderProc.try_wait() {
	// 		Ok(Some(status)) => {
	// 			break;
	// 		}
	// 		Ok(None) => println!("timeout, process is still alive"),
	// 		Err(e) => { return Err(format!("Error while monitoring process: {e}")); }
	// 	}
	// }
	
	Err("I don't wanna work".to_string())
	// Ok(())
}

async fn prepareWorkingDirectory(server: Arc<ServerConnection>, job: Arc<Job>) -> ResultJMsg {
	{
		let zipLoc = server.clientConf.rendererArchivePath(&job.info);
		let extractLoc = server.clientConf.rendererPath(&job.info);
		if !swait!(tokio::fs::try_exists(extractLoc.join("rend.exe")), "").is_ok_and(|r| r) {
			if let Err(e) = files::deleteDirDeep(&extractLoc).await {
				println!("Note: tried deleting {} but got: {e}", extractLoc.to_string_lossy())
			}
			files::unzip(zipLoc.as_path(), extractLoc.as_path()).await.map_err(|e| e.to_string())?;
		}
	}
	
	{
		let scenePath = server.clientConf.scenePath(&job.info);
		
		let mut size = 0;
		for path in job.info.archiveChunks.iter().map(|c| server.clientConf.chunkPath(c)) {
			size += fs::metadata(path).await.map_err(|e| e.to_string())?.len();
		}
		
		let mut mem = vec![];
		mem.reserve_exact(size as usize);
		
		{
			for chunkPath in job.info.archiveChunks.iter().map(|c| server.clientConf.chunkPath(c)) {
				let mut ch = spwait!(fs::File::open(&chunkPath), "Could not open",chunkPath)?;
				spwait!(tokio::io::copy(&mut ch, &mut mem), "Could not read",chunkPath)?;
			}
		}
		swait!(files::unzipMem(mem, scenePath.as_path()), "Failed to unzip scene")?;
	}
	Ok(())
}

async fn downloadJobFiles(state: Arc<Mutex<ClientState>>, server: Arc<ServerConnection>, job: Arc<Job>) -> ResultJMsg {
	async fn downloadScene(server: Arc<ServerConnection>, job: Arc<Job>) -> ResultMsg<TransferStats> {
		let mut jobs = vec![];
		
		for chunk in job.info.archiveChunks.iter().cloned() {
			let server = server.clone();
			jobs.push(Work::spawn(async move { server.downloadChunk(chunk).await }));
		}
		
		let mut stats = TransferStats::default();
		
		for job in jobs {
			stats += awaitStrErr!(job)?;
		}
		
		Ok(stats)
	}
	
	async fn downloadExecutable(server: Arc<ServerConnection>, job: Arc<Job>) -> ResultMsg<TransferStats> {
		server.downloadBinary(&job.info).await
	}
	
	let ds = Work::spawn(downloadScene(server.clone(), job.clone()));
	let de = Work::spawn(downloadExecutable(server.clone(), job.clone()));
	
	let mut fail = None;
	let mut stats = TransferStats::default();
	match awaitStrErr!(ds) {
		Ok(s) => { stats += s; }
		Err(e) => { fail = Some(e); }
	}
	match awaitStrErr!(de) {
		Ok(s) => { stats += s; }
		Err(e) => { fail = Some(e); }
	}
	
	if let Some(err) = fail {
		return Err(err);
	}
	
	state.reportDownload(stats)?;
	Ok(())
}

async fn cleanup(state: Arc<Mutex<ClientState>>, server: &ServerConnection) {
	let cleanTask = {
		let path = (*server.clientConf.workPath).to_owned();
		Work::spawn(async move { files::cleanWorkingDir(path.as_path()).await })
	};
	
	match server.logout().await {
		Ok(_) => {}
		Err(_) => { println!("Failed to logout"); }
	}
	
	let res = cleanTask.await;
	if let Ok(Err(e)) = res {
		println!("Failed to clean work dir: {e}");
	} else {
		println!("Cleaned up!");
	}
}
