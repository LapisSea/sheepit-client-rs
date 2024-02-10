#![allow(non_snake_case)]
#![warn(unused_imports)]
#![warn(dead_code)]

mod conf;
mod HwInfo;
mod Work;
mod ServerConf;
mod net;
mod job;

use std::{env, io, thread};
use std::cmp::{max, min};
use std::fmt::{Display, Formatter};
use std::future::Future;
use std::io::stdout;
use std::ops::{Deref, DerefMut};
use std::process::ExitCode;
use std::sync::{Arc, LockResult, Mutex};
use std::time::Duration;
use reqwest::{Client, RequestBuilder, Url};
use reqwest::cookie::{CookieStore, Jar};
use serde::{de, Deserialize, Serialize};
use serde_json::from_str;
use tokio::time::Instant;
use crate::conf::{ComputeMethod, Config};
use crate::ServerConf::ConfError;
use tokio::sync::mpsc;
use crate::job::JobResponse;

pub const BASE_URL: &str = "https://client.sheepit-renderfarm.com";


macro_rules! tSleep {
    ($seconds:expr) => {
        tokio::time::sleep(Duration::from_secs($seconds)).await;
    };
}


#[derive(Debug, Serialize, Deserialize)]
pub struct RequestEndPoint {
	path: String,
	maxPeriod: u32,
}

impl RequestEndPoint {
	async fn postRequestQueryParm<S: AsRef<str> + Display, ARGS: Serialize + ?Sized>(&self, client: &Client, doamin: S, query: &ARGS, contentType: Option<&str>) -> Result<String, String> {
		let path = self.path.as_str();
		Self::send(path, client.post(format!("{doamin}{path}")).query(query), contentType).await
	}
	async fn getRequest<S: AsRef<str> + Display>(&self, client: &Client, doamin: S, contentType: Option<&str>) -> Result<String, String> {
		let path = self.path.as_str();
		Self::send(path, client.get(format!("{doamin}{path}")), contentType).await
	}
	
	async fn send(path: &str, res: RequestBuilder, contentType: Option<&str>) -> Result<String, String> {
		let res = res.send().await
			.map_err(|e| format!("Failed to get {} because: {}", path, e))?;
		net::requireContentType(contentType, &res)?;
		res.text().await.map_err(|e| format!("Failed to get body of {} because: {}", path, e))
	}
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
	
	
	let serverConf = tryConnect(&httpClient, &clientConf, &hwInfo).await?;
	println!("{:#?}", serverConf);
	
	if let Some(ref pk) = serverConf.publicKey {
		clientConf.password = pk.clone().into();
	}
	
	let server = ServerConnection {
		httpClient: httpClient.into(),
		serverConf,
		clientConf,
		hwInfo,
	};
	
	// let (sender, mut reciever) = mpsc::channel(32);
	
	let state = ClientState {
		shouldRun: true,
		paused: false,
	};
	
	let server = Arc::new(server);
	let state = Arc::new(Mutex::new(state));
	
	keepAliveLoop(server.clone(), state.clone());
	keepAliveLoop(server.clone(), state.clone());
	
	init(state.clone(), &server).await;
	
	
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
		run(state.clone(), server).await;
		tSleep!(1);
	}
	
	let wait = 40;
	for i in 0..wait {
		println!("Waiting... {}", wait - i);
		tSleep!(1);
	}
	cleanup(state, server).await;
	
	
	Ok(())
}

fn durf(d: Duration) -> String {
	format!("{:.3}ms", d.as_millis() as f64 / 1000f64)
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

async fn tryConnect(httpClient: &Client, conf: &Config, hwInfo: &HwInfo::HwInfo) -> Result<ServerConfig, String> {
	let mut attempt = 1;
	let maxAttempts = 5;
	loop {
		match ServerConf::fetchNew(httpClient, conf, hwInfo).await {
			Ok(sConf) => { return Ok(sConf); }
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
	httpClient: Box<Client>,
	serverConf: ServerConfig,
	clientConf: Config,
	hwInfo: HwInfo::HwInfo,
}


impl ServerConnection {
	async fn requestJob(&self) -> Result<job::JobResponse, String> {
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
		
		let res = self.serverConf.requestJob.postRequestQueryParm(&self.httpClient, self.clientConf.hostname.as_ref(), &[
			("computemethod", match self.clientConf.computeMethod {
				ComputeMethod::CpuGpu => { "0" }
				ComputeMethod::Cpu => { "1" }
				ComputeMethod::Gpu => { "2" }
			}),
			("network_dl", "0"),//TODO
			("network_up", "0"),//TODO
			("cpu_cores", cores.to_string().as_str()),
			("ram_max", maxMemory.to_string().as_str()),
			("rendertime_max", "-1"),//TODO
		], net::XML_CONTENT_O).await?;
		println!("Requested job");
		net::fromXml(res.as_str())
	}
	
	fn downloadBinary(&self) {
		todo!()
	}
	
	fn downloadChunk(&self) {
		todo!()
	}
	
	async fn keepMeAlive(&self, state: ClientState) -> bool {
		println!("keepMeAlive start");
		let res = self.serverConf.logout.postRequestQueryParm(&self.httpClient, self.clientConf.hostname.as_ref(), &[
			("paused", state.paused)
		], None).await;
		if res.is_ok() {
			println!("keepMeAlive ok");
		}
		res.is_ok()
	}
	
	async fn logout(&self) -> Result<(), String> {
		println!("Logging out");
		let res = self.serverConf.logout.getRequest(&self.httpClient, self.clientConf.hostname.as_ref(), None).await.map(|_| ());
		if res.is_ok() {
			println!("Logged out");
		}
		res
	}
	
	fn speedTestAnswer(&self) {
		todo!()
	}
}

#[derive(Debug, Clone)]
struct ClientState {
	shouldRun: bool,
	paused: bool,
}

async fn init(state: Arc<Mutex<ClientState>>, server: &ServerConnection) {}

async fn run(state: Arc<Mutex<ClientState>>, server: &ServerConnection) {
	match server.requestJob().await {
		Ok(ayy) => { println!("{:#?}", ayy); }
		Err(err) => {
			println!("{}", err);
		}
	}
	
	let mut state = state.lock().unwrap();
	state.shouldRun = false;
}

async fn cleanup(state: Arc<Mutex<ClientState>>, server: &ServerConnection) {
	match server.logout().await {
		Ok(_) => {}
		Err(_) => { println!("Failed to logout"); }
	}
}
