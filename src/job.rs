use serde::{Deserialize, Serialize};

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

#[derive(Debug, Serialize, Deserialize)]
pub struct Chunk {
	id:  String,
	md5:  String,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Chunks {
	#[serde(alias = "chunk")]
	pub chunks: Vec<Chunk>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RendererInfos {
	#[serde(alias = "md5")]
	md5: String,
	#[serde(alias = "commandline")]
	commandline: String,
	#[serde(alias = "update_method")]
	updateMethod: String,
}

#[derive(Debug, Serialize, Deserialize)]
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
	md5: String,
	action: Option<String>,
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