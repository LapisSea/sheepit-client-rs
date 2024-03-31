use std::env;
use std::fmt::{Display, Formatter};
use std::sync::Arc;
use anyhow::anyhow;
use reqwest::{Client, IntoUrl, Response, Url};
use serde::{Deserialize, Serialize};

use crate::{HwInfo, log, net, RequestEndPoint, ServerConfig};
use crate::conf::ClientConfig;
use crate::defs::*;
use crate::net::*;
use crate::ServerConf::StatusCodes::Unknown;

const SERVER_CONF_ENDPOINT: &str = "/server/config.php";


#[derive(Deserialize, Debug)]
struct RequestEndPointBuild {
	#[serde(alias = "type")]
	eType: String,
	path: String,
	#[serde(alias = "max-period")]
	maxPeriod: Option<u32>,
}

#[derive(Deserialize, Debug)]
struct SpeedTestTargetBuild {
	url: Option<String>,
}

impl SpeedTestTargetBuild {
	fn make(&self) -> Option<SpeedTestTarget> {
		self.url.clone().map(SpeedTestTarget::new)
	}
}

pub enum StatusCodes {
	Ok,
	NoVersion,
	ClientTooOld,
	AuthFailure,
	SessionExpired,
	MissingParm,
	Unknown,
}

impl From<i32> for StatusCodes {
	fn from(num: i32) -> Self {
		match num {
			0 => StatusCodes::Ok,
			100 => StatusCodes::NoVersion,
			101 => StatusCodes::ClientTooOld,
			102 => StatusCodes::AuthFailure,
			103 => StatusCodes::SessionExpired,
			104 => StatusCodes::MissingParm,
			_ => Unknown,
		}
	}
}

impl Display for StatusCodes {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		match *self {
			StatusCodes::Ok => { write!(f, "Ok") }
			StatusCodes::NoVersion => { write!(f, "No version given (100)") }
			StatusCodes::ClientTooOld => { write!(f, "Client is too old (101)") }
			StatusCodes::AuthFailure => { write!(f, "Authentication failure (102)") }
			StatusCodes::SessionExpired => { write!(f, "WebSession has expired (103)") }
			StatusCodes::MissingParm => { write!(f, "Missing parameter (104)") }
			Unknown => { write!(f, "Unknown code") }
		}
	}
}

pub enum ConfError {
	BadStatus(StatusCodes),
	FatalStatus(StatusCodes),
	IOError(String),
}

impl Display for ConfError {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		match self {
			ConfError::BadStatus(code) => { write!(f, "{code}")?; }
			ConfError::FatalStatus(code) => { write!(f, "{code}")?; }
			ConfError::IOError(msg) => { write!(f, "{msg}")?; }
		}
		Ok(())
	}
}

#[derive(Deserialize, Debug)]
struct ServerConfigBuild {
	status: i32,
	publickey: Option<String>,
	#[serde(alias = "request")]
	requestEndPoints: Option<Vec<RequestEndPointBuild>>,
	#[serde(alias = "speedtest")]
	speedTestTargets: Option<Vec<SpeedTestTargetBuild>>,
}

impl ServerConfigBuild {
	fn getEndpoint(&self, name: &str) -> Result<RequestEndPoint, ConfError> {
		if let Some(data) = &self.requestEndPoints {
			if let Some(e) = data.iter().find(|e| e.eType.eq(name)) {
				return Ok(RequestEndPoint {
					path: e.path.clone(),
					maxPeriod: e.maxPeriod.unwrap_or(0),
				});
			}
		}
		Err(ConfError::IOError(format!("Could not find endpoint for: {name}")))
	}
	fn make(&self) -> Result<ServerConfig, ConfError> {
		let code = self.status.into();
		match code {
			StatusCodes::Ok => {}
			StatusCodes::NoVersion | StatusCodes::ClientTooOld | StatusCodes::MissingParm | StatusCodes::AuthFailure => {
				return Err(ConfError::FatalStatus(code));
			}
			StatusCodes::SessionExpired | Unknown => {
				return Err(ConfError::BadStatus(code));
			}
		}
		
		Ok(ServerConfig {
			publicKey: self.publickey.clone(),
			speedTestUrls: match &self.speedTestTargets {
				None => { vec![] }
				Some(data) => { data.iter().filter_map(SpeedTestTargetBuild::make).collect() }
			},
			requestJob: self.getEndpoint("request-job")?,
			downloadBinary: self.getEndpoint("download-binary")?,
			downloadChunk: self.getEndpoint("download-chunk")?,
			error: self.getEndpoint("error")?,
			keepMeAlive: self.getEndpoint("keepmealive")?,
			logout: self.getEndpoint("logout")?,
			speedTestAnswer: self.getEndpoint("speedtest-answer")?,
		})
	}
}

pub async fn fetchNew(httpClient: &Client, conf: &ClientConfig, hwInfo: &HwInfo::HwInfo) -> Result<ServerConfig, ConfError> {
	let args: &[(&str, &str)] = &[
		("login", &conf.login),
		("password", &conf.password),
		("cpu_family", &hwInfo.familyId.to_string()),
		("cpu_model", &hwInfo.modelId.to_string()),
		("cpu_model_name", &hwInfo.cpuName),
		("cpu_cores", &hwInfo.cores.to_string()),
		("os", env::consts::OS),
		("os_version", &hwInfo.osName),
		("ram", &hwInfo.totalSystemMemory.to_string()),
		("bits", if cfg!(target_pointer_width = "64") { "64bit" } else { "32bit" }),
		("version", CLIENT_VERSION),
		("hostname", &hwInfo.hostName),
		("ui", "GuiText"),
		("extras", ""),
		("headless", &conf.headless.to_string()),
		("hwid", &hwInfo.hwId),
	];
	let mut url = conf.hostname.clone();
	{
		let mut urlBuild = url.path_segments_mut()
			.map_err(|e| ConfError::IOError("Illegal hostname".to_string()))?;
		SERVER_CONF_ENDPOINT.split('/').filter(|s| !s.is_empty())
			.for_each(|p| { urlBuild.push(p); });
	}
	
	let xml = postRequestForm(httpClient, url, args, XML_CONTENT_O).await
		.map_err(|e| ConfError::IOError(e.to_string()))?;
	// log!("Got back:\n{xml}");
	
	let conf: ServerConfigBuild = fromXml(xml.as_str())
		.map_err(|e| ConfError::IOError(format!("Server configuration is malformed:\n\t{}", e)))?;
	
	conf.make()
}