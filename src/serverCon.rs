use std::borrow::Cow;
use std::cmp::{max, min};
use std::ops::Deref;
use std::path::Path;
use std::sync::{Arc, Mutex};
use reqwest::Client;
use reqwest::multipart::{Form, Part};
use crate::conf::{ClientConfig, ComputeMethod};
use crate::{ClientState, fmd5, FrameUploadMessage, HwInfo, net, RenderResult, ServerConfig};
use crate::job::{Chunk, ClientErrorType, JobInfo, JobResponse};
use crate::net::TransferStats;
use crate::utils::{MutRes, ResultJMsg, ResultMsg};

pub struct ServerConnection {
	pub httpClient: Arc<Client>,
	pub serverConf: ServerConfig,
	pub clientConf: ClientConfig,
	pub hwInfo: HwInfo::HwInfo,
	pub hashCache: fmd5::HashCache,
	pub uploadSender: tokio::sync::mpsc::Sender<FrameUploadMessage>,
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
	
	pub async fn requestJob(&self, state: Arc<Mutex<ClientState>>) -> ResultMsg<JobResponse> {
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
			("rendertime_max", self.clientConf.maxRenderTime
				.map(|t| t.as_secs()).unwrap_or_default().to_string()),
		]).send().await?.xml::<JobResponse>().await;
		match &res {
			Ok(res) => {
				if let Some(ref name) = res.renderTask.as_ref().map(|t| t.name.as_str()) {
					println!("Requested job: {name}");
				}
			}
			Err(err) => {
				println!("Failed to request job: {err}");
			}
		}
		res
	}
	
	pub(crate) async fn downloadBinary(&self, job: &JobInfo) -> ResultMsg<TransferStats> {
		let md5 = job.rendererInfo.md5.as_str();
		let req = servReq!(self,downloadBinary,get).query(&[("job", &job.id)]);
		
		net::downloadFile(&self.clientConf.rendererArchivePath(job), req, md5, self.hashCache.clone()).await
	}
	
	pub async fn downloadChunk(&self, chunk: Chunk) -> ResultMsg<TransferStats> {
		let md5 = chunk.md5.as_ref();
		let req = servReq!(self,downloadChunk,get).query(&[("chunk", &chunk.id)]);
		
		net::downloadFile(&self.clientConf.chunkPath(&chunk), req, md5, self.hashCache.clone()).await
	}
	
	pub async fn keepMeAlive(&self, state: ClientState) -> bool {
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
	
	pub async fn logout(&self) -> ResultJMsg {
		println!("Logging out");
		let res = servReq!(self,logout,get).send().await.map(|_| ());
		if res.is_ok() {
			println!("Logged out");
		}
		res
	}
	
	pub fn speedTestAnswer(&self) {
		todo!()
	}
	
	pub async fn error(&self, status: ClientErrorType, log: String) -> ResultJMsg {
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