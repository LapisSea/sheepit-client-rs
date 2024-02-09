use std::env;
use std::fmt::{Display, Formatter};
use std::sync::Arc;
use reqwest::{Client, IntoUrl, Response};
use serde::{Deserialize, Serialize};

use crate::{HwInfo, LongSummaryStatistics, RequestEndPoint, ServerConfig, SpeedTestTarget};
use crate::conf::Config;
use crate::net::{fromXml, postForm};
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
		self.url.clone().map(|url| SpeedTestTarget {
			url,
			speedtest: Default::default(),
			ping: LongSummaryStatistics {
				count: 0,
				sum: 0,
				min: usize::MAX,
				max: usize::MIN,
			},
		})
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

pub async fn fetchNew(httpClient: &Client, conf: &Config, hwInfo: &HwInfo::HwInfo) -> Result<ServerConfig, ConfError> {
	let args = &[
		("login", conf.login.clone()),
		("password", conf.password.clone()),
		("cpu_family", hwInfo.familyId.to_string().into()),
		("cpu_model", hwInfo.modelId.to_string().into()),
		("cpu_model_name", hwInfo.cpuName.clone().into()),
		("cpu_cores", hwInfo.cores.to_string().into()),
		("os", env::consts::OS.into()),
		("os_version", hwInfo.osName.clone().into()),
		("ram", hwInfo.totalSystemMemory.to_string().into()),
		("bits", if cfg!(target_pointer_width = "64") { "64bit".into() } else { "32bit".into() }),
		("version", "7.23353.0".into()),
		("hostname", format!("{}", hwInfo.hostName).into()),
		("ui", "GuiText".into()),
		("extras", "".into()),
		("headless", conf.headless.to_string().into()),
		("hwid", hwInfo.hwId.clone().into()),
	];
	let url = format!("{}{}", conf.hostname, SERVER_CONF_ENDPOINT);
	// println!("{:#?}", args);
	
	let xml = postForm(httpClient, url, args, Some("text/xml")).await
		.map_err(|e| ConfError::IOError(e.to_string()))?;
	// println!("Got back:\n{xml}");
	
	let conf: ServerConfigBuild = fromXml(xml.as_str())
		.map_err(|e| ConfError::IOError(format!("Server configuration is malformed:\n\t{}", e)))?;
	
	conf.make()
}