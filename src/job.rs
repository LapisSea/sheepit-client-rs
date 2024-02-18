use serde::{Deserialize, Serialize};
use crate::conf::{ComputeMethod, ClientConfig};
use crate::ServerConf::StatusCodes;
use crate::ServerConnection;

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionStats {
	#[serde(alias = "credits_session")]
	pub pointsEarnedOnSession: i32,
	#[serde(alias = "credits_total")]
	pub pointsEarnedByUser: i64,
	#[serde(alias = "frame_remaining")]
	pub remainingFrames: i32,
	#[serde(alias = "waiting_project")]
	pub waitingProjects: i32,
	#[serde(alias = "connected_machine")]
	pub connectedMachines: i32,
	#[serde(alias = "renderable_project")]
	pub renderableProjects: Option<i32>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Chunk {
	pub id: String,
	pub md5: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Chunks {
	#[serde(alias = "chunk")]
	pub chunks: Vec<Chunk>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RendererInfos {
	#[serde(alias = "md5")]
	pub md5: String,
	#[serde(alias = "commandline")]
	pub commandline: String,
	#[serde(alias = "update_method")]
	pub updateMethod: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RenderTask {
	#[serde(alias = "id")]
	pub id: String,
	#[serde(alias = "use_gpu")]
	pub useGpu: i32,
	#[serde(alias = "chunks")]
	pub chunks: Chunks,
	#[serde(alias = "path")]
	pub path: String,
	#[serde(alias = "frame")]
	pub frame: String,
	#[serde(alias = "synchronous_upload")]
	pub synchronousUpload: String,
	#[serde(alias = "validation_url")]
	pub validationUrl: String,
	#[serde(alias = "name")]
	pub name: String,
	#[serde(alias = "password")]
	pub password: String,
	#[serde(alias = "renderer")]
	pub rendererInfos: RendererInfos,
	#[serde(alias = "script")]
	pub script: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FileMD5 {
	pub md5: String,
	pub action: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JobResponse {
	pub status: i32,
	#[serde(alias = "stats")]
	pub sessionStats: Option<SessionStats>,
	#[serde(alias = "job")]
	pub renderTask: Option<RenderTask>,
	#[serde(alias = "file")]
	pub fileMD5s: Option<Vec<FileMD5>>,
}

impl JobResponse {
	pub fn status(&self) -> JobRequestStatus {
		self.status.into()
	}
}

pub enum JobRequestStatus {
	Ok = 0,
	Unknown = 999,
	Nojob = 200,
	ErrorNoRenderingRight = 201,
	ErrorDeadSession = 202,
	ErrorSessionDisabled = 203,
	ErrorSessionDisabledDenoisingNotSupported = 208,
	ErrorInternalError = 204,
	ErrorRendererNotAvailable = 205,
	ServerInMaintenance = 206,
	ServerOverloaded = 207,
}

impl From<i32> for JobRequestStatus {
	fn from(v: i32) -> Self {
		match v {
			x if x == JobRequestStatus::Ok as i32 => JobRequestStatus::Ok,
			x if x == JobRequestStatus::Nojob as i32 => JobRequestStatus::Nojob,
			x if x == JobRequestStatus::ErrorNoRenderingRight as i32 => JobRequestStatus::ErrorNoRenderingRight,
			x if x == JobRequestStatus::ErrorDeadSession as i32 => JobRequestStatus::ErrorDeadSession,
			x if x == JobRequestStatus::ErrorSessionDisabled as i32 => JobRequestStatus::ErrorSessionDisabled,
			x if x == JobRequestStatus::ErrorSessionDisabledDenoisingNotSupported as i32 => JobRequestStatus::ErrorSessionDisabledDenoisingNotSupported,
			x if x == JobRequestStatus::ErrorInternalError as i32 => JobRequestStatus::ErrorInternalError,
			x if x == JobRequestStatus::ErrorRendererNotAvailable as i32 => JobRequestStatus::ErrorRendererNotAvailable,
			x if x == JobRequestStatus::ServerInMaintenance as i32 => JobRequestStatus::ServerInMaintenance,
			x if x == JobRequestStatus::ServerOverloaded as i32 => JobRequestStatus::ServerOverloaded,
			_ => JobRequestStatus::Unknown,
		}
	}
}

#[derive(Debug)]
pub struct RenderProcess {}

#[derive(Debug)]
pub struct JobInfo {
	pub id: String,
	pub frameNumber: String,
	pub path: String,
	pub useGPU: bool,
	pub validationUrl: String,
	pub script: String,
	pub archiveChunks: Vec<Chunk>,
	pub name: String,
	pub password: Option<Vec<u8>>,
	pub synchronousUpload: bool,
	pub rendererInfo: RendererInfos,
}

#[derive(Debug)]
pub struct JobConfig {
	pub computeMethod: ComputeMethod,
	pub cores: u16,
}

#[derive(Debug)]
pub struct Job {
	pub info: JobInfo,
	pub conf: JobConfig,
}

impl From<(RenderTask, &ServerConnection)> for Job {
	fn from((t, s): (RenderTask, &ServerConnection)) -> Self {
		let info = JobInfo {
			id: t.id,
			frameNumber: t.frame,
			path: t.path.replace('/', std::path::MAIN_SEPARATOR.to_string().as_str()),
			useGPU: t.useGpu == 1,
			validationUrl: urlencoding::decode(&t.validationUrl).map(|e| e.to_string()).unwrap_or(t.validationUrl),
			script: t.script,
			archiveChunks: t.chunks.chunks,
			name: t.name,
			password: Some(t.password).filter(|p|! p.is_empty()).map(|p| p.as_bytes().to_owned()),
			synchronousUpload: t.synchronousUpload == "1",
			rendererInfo: t.rendererInfos,
		};
		
		Job {
			info,
			conf: JobConfig {
				computeMethod: ComputeMethod::Cpu,
				cores: s.effectiveCores(),
			},
		}
	}
}


pub enum ClientErrorType {
	OK = 0,
	UNKNOWN = 99,
}