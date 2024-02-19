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
mod serverCon;
mod tui;
mod global;

use std::{env};
use std::borrow::Cow;
use std::cmp::{max, min};
use std::fmt::{Display, format};
use std::io::Read;
use std::iter::zip;
use std::ops::{Deref};
use std::path::{Path, PathBuf};
use std::process::{ExitCode, ExitStatus, Stdio};
use std::ptr::write;
use std::rc::Rc;
use std::sync::{Arc, LockResult, Mutex};
use std::time::Duration;
use crossterm::terminal::disable_raw_mode;
use defer::defer;
use futures_util::future::err;
use futures_util::TryFutureExt;
use indoc::indoc;
use reqwest::{Client, RequestBuilder, Url};
use reqwest::cookie::{Jar};
use serde::{Deserialize, ser, Serialize};
use tokio::fs;
use tokio::time::Instant;
use crate::conf::{ComputeMethod, ClientConfig};
use crate::ServerConf::ConfError;
use rand::Rng;
use regex::Regex;
use reqwest::multipart::{Form, Part};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio::sync::{mpsc, mpsc::Receiver, MutexGuard, TryLockError};
use tokio::task::JoinHandle;
use crate::defs::XML_CONTENT_O;
use crate::job::{Chunk, ClientErrorType, Job, JobInfo, JobRequestStatus, JobResponse};
use crate::net::{bodyText, fromXml, Req, RequestEndPoint, requireContentType, TransferStats};
use crate::process::{BlenderVersionStr, BlendProcessInfo, ProcStatus};
use crate::serverCon::ServerConnection;
use crate::utils::*;

#[derive(Debug, Serialize, Deserialize)]
pub struct ServerConfig {
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
	let e = tui::runUi();
	match e {
		Ok(O) => {}
		Err(e) => {
			println!("{e}");
		}
	}
	ExitCode::SUCCESS
	
	// let res = Work::block(start()).map_err(|e| format!("Aborting client because:\n{e}"));
	// match res {
	// 	Ok(_) => { ExitCode::SUCCESS }
	// 	Err(errMsg) => {
	// 		eprintln!("{errMsg}");
	// 		ExitCode::FAILURE
	// 	}
	// }
}

async fn start() -> ResultJMsg {
	println!("###Start");
	let hwInfo = Work::spawn(HwInfo::collectInfo());
	let mut clientConf = conf::read(&mut env::args().skip(1).map(|x| x.into()))?.make()?;
	
	
	let mut cleanTask = Some({
		let path = (*clientConf.workPath).to_owned();
		Work::spawn(async move { files::cleanWorkingDir(path.as_path()).await })
	});
	
	let httpClient = Client::builder()
		.connect_timeout(Duration::from_secs(30))
		.timeout(Duration::from_secs(300))
		.cookie_provider(Arc::new(Jar::default()))
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
		let (uploadSender, uploadReceiver) = mpsc::channel(2);
		let server = Arc::new(ServerConnection {
			httpClient: httpClient.clone(),
			serverConf: net::tryConnectToServer(httpClient.clone().as_ref(), &mut clientConf, &hwInfo).await?,
			clientConf: clientConf.clone(),
			hwInfo: hwInfo.clone(),
			hashCache: Default::default(),
			uploadSender,
		});
		
		let state = Arc::new(Mutex::new(ClientState {
			shouldRun: true,
			paused: false,
			downloadStats: Default::default(),
		}));
		
		let tasks = match init(state.clone(), server.clone(), uploadReceiver).await {
			Ok(t) => { t }
			Err(err) => {
				println!("Failed to init: {err}");
				state.stop()?;
				vec![]
			}
		};
		
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
					let _ = stopAndBlockTasks(server, tasks).await;
					tSleep!(5);
					continue 'loginLoop;
				}
				AppLoopAction::FatalStop(fatal) => {
					state.stop()?;
					println!("A fatal error has occurred. Shutting down because:\n\t{fatal}");
				}
			}
		}
		
		let _ = stopAndBlockTasks(server.clone(), tasks).await;
		cleanup(state, server.as_ref()).await;
		return Ok(());
	}
}

async fn stopAndBlockTasks(server: Arc<ServerConnection>, tasks: Vec<JoinHandle<()>>) {
	let _ = server.uploadSender.send(FrameUploadMessage::End).await;
	for x in tasks { let _ = x.await; }
}

async fn init(state: ArcMut<ClientState>, server: Arc<ServerConnection>, mut uploadReceiver: Receiver<FrameUploadMessage>) -> ResultMsg<Vec<JoinHandle<()>>> {
	let keepAlive = {
		let state = state.clone();
		let server = server.clone();
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
				let toSleep = keepAlivePeriod.checked_sub(time).unwrap_or(Duration::ZERO).min(Duration::from_secs(2));
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
			println!("Ended keepalive worker");
		})
	};
	
	let uploader = {
		Work::spawn(async move {
			while let Some(res) = uploadReceiver.recv().await {
				match res {
					FrameUploadMessage::Frame(frame) => {
						uploadFrame(server.httpClient.as_ref(), &frame).await.warn();
					}
					FrameUploadMessage::End => {
						break;
					}
				}
			}
			println!("Ended uploader worker");
		})
	};
	
	Ok(vec![keepAlive, uploader])
}

async fn uploadFrame(httpClient: &Client, frame: &RenderResult) -> ResultJMsg {
	async fn doUpload(httpClient: &Client, frame: &RenderResult) -> ResultJMsg {
		let url = Url::parse(&frame.validationUrl).map_err(|e| e.to_string())?;
		let queryParts = url.query_pairs();
		
		let mut builder = httpClient.post(url)
			.query(&[
				("rendertime", frame.renderTime.as_secs().to_string()),
				("memoryused", frame.memoryUsed.to_string()),
			]);
		
		if frame.speedSamplesRendered > 0f32 {
			builder = builder.query(&[
				("speedsamples", frame.speedSamplesRendered.to_string()),
			]);
		}
		let path = frame.output.as_path();
		let ext = path.extension().map(|s| s.to_string_lossy().to_string()).unwrap_or("png".to_string());
		
		let fileLen = fs::metadata(path).await.map_err(|e| "Could not open file {e}")?.len();
		let file = fs::File::open(path).await.map_err(|e| "Could not open file {e}")?;
		
		builder = builder.multipart(Form::new()
			.part(
				"file",
				Part::stream_with_length(file, fileLen)
					.mime_str(&format!("image/{ext}")).unwrap()
					.file_name(path.file_name().unwrap_or_default().to_string_lossy().to_string()),
			));
		
		let response = net::send(&frame.validationUrl, builder).await?;
		// println!("{:#?}", response);
		
		requireContentType(defs::XML_CONTENT_O, &response)?;
		let xml = bodyText(&frame.validationUrl, response).await?;
		#[derive(Deserialize)]
		struct JobValidation {
			status: i32,
		}
		let status = fromXml::<JobValidation>(xml.as_str())?.status;
		if status != 0 {
			return Err(format!("Got back validation code {status}"));//TODO handle codes properly
		}
		
		Ok(())
	}
	defer!({
		std::fs::remove_file(&frame.output).map_err(|e| format!("Failed to delete {} because: {e}", &frame.output.to_string_lossy())).warn();
		if let Some(f)=&frame.outputPreview{
			std::fs::remove_file(&f).map_err(|e| format!("Failed to delete {} because: {e}", &f.to_string_lossy())).warn();
		}
	});
	
	println!("Uploading {:#?}", frame);
	
	let mut attempt = 1;
	let mut timeToSleep = 22;
	
	loop {
		if let Err(msg) = swait!(doUpload(httpClient, frame), "Failed to upload frame") {
			if attempt == 3 {
				return Err(msg);
			}
			println!("{msg} (attempt {attempt})");
		} else { return Ok(()); }
		
		println!("Failed to upload frame. trying in {timeToSleep} seconds");
		tSleep!(timeToSleep);
		attempt += 1;
		timeToSleep *= 2;
	}
}

async fn run(state: ArcMut<ClientState>, server: Arc<ServerConnection>) -> AppLoopAction {
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

async fn execute_job(state: ArcMut<ClientState>, server: Arc<ServerConnection>, job: Arc<Job>) -> ResultJMsg {
	println!("Working on job \"{}\"", job.info.name);
	downloadJobFiles(state, server.clone(), job.clone()).await?;
	println!("Preparing work directory");
	prepareWorkingDirectory(server.clone(), job.clone()).await?;
	let serverC = server.clone();
	let jobc = job.clone();
	deferAsync!({
		let scenePath = serverC.clientConf.scenePath(&jobc.info);
		swait!(files::deleteDirDeep(&scenePath), "Failed to delete scene directory").warn();
	});
	
	match render(job.clone(), server.clone()).await {
		Ok(res) => {
			if job.info.synchronousUpload {
				uploadFrame(server.httpClient.as_ref(), &res).await?;
			} else {
				server.uploadSender.send(FrameUploadMessage::Frame(res)).await.map_err(|e| e.to_string())?;
			}
		}
		Err(err) => {
			println!("{:?}", err);
		}
	}
	
	Ok(())
}


enum FrameUploadMessage {
	Frame(RenderResult),
	End,
}

#[derive(Debug)]
struct RenderResult {
	validationUrl: String,
	output: PathBuf,
	outputPreview: Option<PathBuf>,
	memoryUsed: u64,
	renderTime: Duration,
	speedSamplesRendered: f32,
}

#[derive(Debug)]
enum RenderError {
	Unknown(String),
	NoOutputFile,
	RendererCrashed,
	RendererMissingLibraries,
	ValidationFailed,
}

async fn render(job: Arc<Job>, server: Arc<ServerConnection>) -> Result<RenderResult, RenderError> {
	let (mut command, files) = process::createBlenderCommand(job.clone(), server.clone()).await.map_err(RenderError::Unknown)?;
	defer!({
		for file in files{
			std::fs::remove_file(&file).map_err(|e|format!("Failed to delete script {} because: {e}",file.to_string_lossy())).warn();
		}
	});
	
	let mut blenderProc = command.spawn().map_err(|e| RenderError::Unknown(e.to_string()))?;
	let procInfo = Arc::new(Mutex::new(BlendProcessInfo::default()));
	
	fn scanOutput<T: AsyncRead + Unpin + Send + Sync + 'static>(procInfo: ArcMut<BlendProcessInfo>, stream: T) -> JoinHandle<ResultJMsg> {
		Work::spawn(async move {
			let mut lines = BufReader::new(stream).lines();
			loop {
				let l = swait!(lines.next_line(),"failed to read output line")?;
				if l.is_none() { break; }
				let l = l.unwrap_or_default();
				
				let exited = procInfo.access(|procInfo| {
					process::observeBlendStdout(procInfo, l);
					procInfo.status != ProcStatus::Running
				})?;
				if exited { break; }
			}
			
			Ok(())
		})
	}
	
	blenderProc.stdout
		.take()
		.map(|s| scanOutput(procInfo.clone(), s))
		.ok_or(RenderError::Unknown("Failed to acquire stdout".to_string()))?;
	
	blenderProc.stderr
		.take()
		.map(|s| scanOutput(procInfo.clone(), s))
		.ok_or(RenderError::Unknown("Failed to acquire stderr".to_string()))?;
	
	let memoryWatch: JoinHandle<ResultJMsg> = {
		let pid = blenderProc.id().ok_or(RenderError::Unknown("Failed to get process PID".to_string()))?;
		let procInfo = procInfo.clone();
		Work::spawn(async move {
			loop {
				tSleepMs!(200);
				let mem = HwInfo::getProcessWorkingSet(pid);
				let exited = procInfo.access(|procInfo| {
					if procInfo.status == ProcStatus::Running {
						if let Some(mem) = mem.warn() {
							procInfo.memUsage = mem;
							procInfo.maxMemUsage = max(procInfo.maxMemUsage, mem);
						}
						true
					} else { false }
				})?;
				if exited { break; }
			}
			Ok(())
		})
	};
	
	let blenderProc = Arc::new(tokio::sync::Mutex::new(blenderProc));
	
	let maxRenderTimeTrigger: JoinHandle<ResultJMsg> = {
		let timeout = server.clientConf.maxRenderTime;
		let blenderProc = blenderProc.clone();
		let procInfo = procInfo.clone();
		Work::spawn(async move {
			let timeout = match timeout {
				None => { return Ok(()); }
				Some(t) => { t }
			};
			let timeout = timeout.checked_add(Duration::from_secs(2)).unwrap_or(timeout);
			
			tokio::time::sleep(timeout).await;
			
			let shouldKill = procInfo.access(|procInfo| {
				if procInfo.status == ProcStatus::Running {
					procInfo.status = ProcStatus::TookTooLong;
					true
				} else {
					false
				}
			})?;
			if shouldKill {
				let mut blenderProc = blenderProc.lock().await;
				blenderProc.kill().await.map_err(|e| format!("Failed to kill blender because: {e}"))
			} else { Ok(()) }
		})
	};
	
	loop {
		let mut blenderProc = match blenderProc.try_lock() {
			Ok(l) => { l }
			Err(e) => {
				continue;
			}
		};
		match blenderProc.try_wait() {
			Ok(Some(status)) => {
				if !status.success() {
					// procInfo.access(|i| i.status = ProcStatus::Crashed);TODO signal crash
					return Err(RenderError::RendererCrashed);
				}
				break;
			}
			Ok(None) => {}//Alive
			Err(e) => { return Err(RenderError::Unknown(format!("There was en error pooling blender process status: {e}"))); }
		}
	}
	
	let procInfo = procInfo.access(|i| {
		i.status = ProcStatus::ExitedOk;
		i.clone()
	}).map_err(RenderError::Unknown)?;
	
	maxRenderTimeTrigger.abort();
	
	println!("{:#?}", procInfo);
	
	let mut outputFiles = vec![];
	let nameStart = format!("{}_{}.", job.info.id, job.info.frameNumber);
	let mut files = swait!(fs::read_dir(&server.clientConf.workPath), "Failed to read work directory").map_err(RenderError::Unknown)?;
	while let Ok(Some(file)) = files.next_entry().await {
		let name = file.file_name();
		if !name.to_string_lossy().starts_with(&nameStart) { continue; }
		outputFiles.push(file.path());
		break;
	}
	
	let (output, outputPreview) = match outputFiles.as_slice() {
		[] => { return Err(RenderError::NoOutputFile); }
		[output] => {
			(output.clone(), None)
		}
		[output1, output2] => {
			if &output1.extension().unwrap_or_default().to_string_lossy() != "exr" {
				(output1.clone(), Some(output2.clone()))
			} else {
				(output2.clone(), Some(output1.clone()))
			}
		}
		_ => {
			files::cleanWorkingDir(&server.clientConf.workPath).await.warn();
			return Err(RenderError::NoOutputFile);
		}
	};
	
	Ok(RenderResult {
		renderTime: procInfo.compositStart.ok_or(RenderError::Unknown("No composit start encountered".to_string()))?
			.duration_since(procInfo.renderStart.ok_or(RenderError::Unknown("No render start encountered".to_string()))?),
		memoryUsed: procInfo.maxMemUsage,
		output,
		outputPreview,
		validationUrl: job.info.validationUrl.clone(),
		speedSamplesRendered: procInfo.speedSamplesRendered,
	})
}

async fn prepareWorkingDirectory(server: Arc<ServerConnection>, job: Arc<Job>) -> ResultJMsg {
	{
		let zipLoc = server.clientConf.rendererArchivePath(&job.info);
		let extractLoc = server.clientConf.rendererPath(&job.info);
		if !swait!(tokio::fs::try_exists(extractLoc.join("rend.exe")), "").is_ok_and(|r| r) {
			if let Err(e) = files::deleteDirDeep(&extractLoc).await {
				println!("Note: tried deleting {} but got: {e}", extractLoc.to_string_lossy())
			}
			files::unzip(zipLoc.as_path(), extractLoc.as_path(), None).await.map_err(|e| format!("Failed to unzip renderer because: {e}"))?;
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
		let password = job.info.password.as_deref();
		files::unzipMem(mem, scenePath.as_path(), password).map_err(|e| format!("Failed to unzip scene because: {e}"))?;
	}
	Ok(())
}

async fn downloadJobFiles(state: ArcMut<ClientState>, server: Arc<ServerConnection>, job: Arc<Job>) -> ResultJMsg {
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

async fn cleanup(state: ArcMut<ClientState>, server: &ServerConnection) {
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
