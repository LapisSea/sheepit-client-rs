use std::cmp::min;
use std::fmt::Display;
use std::fs::File;
use std::io;
use std::ops::{Add, AddAssign};
use std::path::{Path};
use std::time::Duration;
use anyhow::{anyhow, Context};
use futures_util::StreamExt;
use md5::Digest;
use reqwest::{Client, IntoUrl, RequestBuilder, Response, Url};
use serde::{de, Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::Instant;
use crate::conf::ClientConfig;
use crate::fmd5::{checkMd5, computeFileHash, HashCache};
use crate::defs::*;
use crate::{HwInfo, ServerConf, ServerConfig, tSleep, tSleepRandRange};
use crate::ServerConf::ConfError;
use crate::utils::ResultMsg;
use rand::Rng;

#[derive(Debug, Serialize, Deserialize)]
pub struct LongSummaryStatistics {
	count: usize,
	sum: usize,
	min: usize,
	max: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SpeedTestTarget {
	url: String,
	speedtest: Duration,
	ping: LongSummaryStatistics,
}

impl SpeedTestTarget {
	pub fn new(url: String) -> Self {
		Self {
			url,
			speedtest: Default::default(),
			ping: LongSummaryStatistics {
				count: 0,
				sum: 0,
				min: usize::MAX,
				max: usize::MIN,
			},
		}
	}
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RequestEndPoint {
	pub path: String,
	pub maxPeriod: u32,
}

impl RequestEndPoint {
	pub fn get<S: AsRef<str> + Display>(&self, client: &Client, domain: S) -> ReqBuild {
		let path = self.makeUrl(domain);
		ReqBuild {
			path: path.clone(),
			req: client.get(path),
		}
	}
	
	pub fn makeUrl<S: Display>(&self, domain: S) -> String {
		format!("{domain}{}", self.path.as_str())
	}
	
	pub fn post<S: AsRef<str> + Display>(&self, client: &Client, domain: S) -> ReqBuild {
		let path = self.makeUrl(domain);
		ReqBuild {
			path: path.clone(),
			req: client.post(path),
		}
	}
	
	pub async fn postRequestQueryParm<
		S: AsRef<str> + Display,
		ARGS: Serialize + ?Sized
	>(&self, client: &Client, doamin: S, query: &ARGS) -> ResultMsg<Req> {
		self.post(client, doamin).query(query).send().await
	}
}

pub struct Req {
	path: Box<str>,
	res: Response,
}

pub struct ReqBuild {
	pub path: String,
	pub req: RequestBuilder,
}

impl ReqBuild {
	pub fn query<ARGS: Serialize + ?Sized>(self, args: &ARGS) -> Self {
		Self {
			path: self.path,
			req: self.req.query(args),
		}
	}
	
	pub async fn send(self) -> ResultMsg<Req> {
		Ok(Req {
			path: self.path.clone().into(),
			res: send::<&str>(self.path.as_ref(), self.req).await?,
		})
	}
}

impl Req {
	pub async fn text(self) -> ResultMsg<String> {
		bodyText(self.path, self.res).await
	}
	
	pub async fn xml<'de, T: de::Deserialize<'de>>(self) -> ResultMsg<T> {
		requireContentType(XML_CONTENT_O, &self.res)?;
		let xml = bodyText(self.path, self.res).await?;
		fromXml(xml.as_str())
	}
	
	pub fn response(self) -> ResultMsg<Response> {
		let res = self.res;
		res.error_for_status_ref()?;
		Ok(res)
	}
}

pub async fn send<U: Display>(url: U, res: RequestBuilder) -> ResultMsg<Response> {
	res.send().await.and_then(|res| res.error_for_status())
		.map_err(|e| anyhow!("Could not get an ok response from {url} because: {e}"))
}

pub async fn bodyText<U: Display>(url: U, res: Response) -> ResultMsg<String> {
	res.text().await.map_err(|e| anyhow!("Failed to get body of {url} because: {e}"))
}

pub async fn postRequestForm<
	T: Serialize + ?Sized,
	U: IntoUrl + Display + Clone
>(client: &Client, url: U, data: &T, requiredContentType: Option<&str>) -> ResultMsg<String> {
	let resp = send(&url, client.post(url.clone()).form(data)).await?;
	requireContentType(requiredContentType, &resp)?;
	bodyText(url, resp).await
}

pub fn requireContentType(requiredContentType: Option<&str>, resp: &Response) -> ResultMsg<()> {
	let validContentType = match requiredContentType {
		None => { true }//No required type. Anything goes
		Some(requiredContentType) => {
			match getContentTypeStr(resp) {
				None => { false }//Type required but none is provided
				Some(typ) => {
					typ.split(';').map(|s| s.trim())
						.any(|s| s.eq(requiredContentType))
				}
			}
		}
	};
	
	if !validContentType {
		return Err(anyhow!(
			"Could not get server config because the response is of invalid type.\n\tRequires {} But got {}",
			requiredContentType.unwrap_or("none"),
			match getContentTypeStr(resp) {
				None => { "".to_string() }
				Some(s) => { format!(": {}", s) }
			}
		));
	}
	Ok(())
}

fn getContentTypeStr(resp: &Response) -> Option<&str> {
	resp.headers()
		.get(reqwest::header::CONTENT_TYPE)
		.and_then(|v| v.to_str().ok())
}

pub fn fromXml<'de, T: de::Deserialize<'de>>(xml: &str) -> ResultMsg<T> {
	let mut body = xml.trim();
	if body.starts_with("<?xml") {
		let mark = "?>";
		body = &body[body.find(mark).unwrap_or(0) + mark.len()..].trim();
	}
	serde_xml_rs::from_str(body).map_err(|e| anyhow!("Could not parse {} because: {}\nXML Text:\n{}", std::any::type_name::<T>(), e, xml))
}

pub fn toXml<T: serde::Serialize>(val: &T) -> ResultMsg<String> {
	serde_xml_rs::to_string(val)
		.map(|s| format!("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n{s}"))
		.map_err(|e| anyhow!("Could not serialize {} because: {}", std::any::type_name::<T>(), e))
}


#[derive(Debug, Copy, Clone, Default)]
pub struct TransferStats {
	bytes: u64,
	time: Duration,
}

impl TransferStats {
	pub fn toRate(&self) -> u64 {
		self.bytes.checked_div(self.time.as_secs()).unwrap_or_default()
	}
}

impl Add for TransferStats {
	type Output = TransferStats;
	
	fn add(self, rhs: Self) -> Self::Output {
		let bytes = match self.bytes.checked_add(rhs.bytes) {
			None => { return if self.bytes > rhs.bytes { rhs } else { self }; }
			Some(b) => { b }
		};
		let time = match self.time.checked_add(rhs.time) {
			None => { return if self.bytes > rhs.bytes { rhs } else { self }; }
			Some(b) => { b }
		};
		TransferStats { bytes, time }
	}
}

impl AddAssign for TransferStats {
	fn add_assign(&mut self, rhs: Self) { *self = (*self) + rhs; }
}

pub async fn downloadFile(path: &Path, req: ReqBuild, md5Check: &str, hashCache: HashCache) -> ResultMsg<TransferStats> {
	let pathStr = path.to_string_lossy();
	if tokio::fs::try_exists(path).await.context(format!("Could not check existence of {pathStr}"))? {
		println!("Checking validity of {pathStr}...");
		let hash = computeFileHash(path, hashCache).await
			.map_err(|e| anyhow!("Could not read file \"{}\" because: {e}", pathStr));
		
		match hash.and_then(|hash| checkMd5(md5Check, pathStr.as_ref(), hash)) {
			Ok(_) => {
				println!("{} already exists, skipping download", pathStr);
				return Ok(Default::default());
			}
			Err(msg) => {
				println!("{msg}. Re-downloading...");
				tokio::fs::remove_file(path).await
					.map_err(|e| anyhow!("Could not delete file \"{}\" because: {e}", pathStr))?;
			}
		}
	}
	
	if let Some(parent) = path.parent() {
		tokio::fs::create_dir_all(parent).await.context(format!("Could not create directory: {pathStr}"))?;
	}
	
	let resp = req.send().await?.response()?;
	let url = resp.url().to_owned();
	let mut stream = resp.bytes_stream();
	
	let mut file = tokio::fs::File::create(path).await
		.context(format!("Could not create file \"{pathStr}\""))?;
	
	let mut bytesDownloaded = 0;
	let start = Instant::now();
	
	let mut context = md5::Context::new();
	while let Some(item) = StreamExt::next(&mut stream).await {
		let item = item.context(format!("Could not download \"{url}\""))?;
		// println!("chunk len {}", item.len());
		bytesDownloaded += item.len() as u64;
		let ch = item.as_ref();
		context.consume(ch);
		file.write_all(ch).await.context(format!("Could not write to file: {}", pathStr))?;
	}
	
	let end = Instant::now();
	let computed = context.compute();
	checkMd5(md5Check, url, computed)?;
	
	println!("\"{pathStr}\" downloaded!");
	
	Ok(start.checked_duration_since(end).map(|time| TransferStats { bytes: bytesDownloaded, time }).unwrap_or_default())
}


pub async fn tryConnectToServer(httpClient: &Client, conf: &mut ClientConfig, hwInfo: &HwInfo::HwInfo) -> ResultMsg<ServerConfig> {
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
					return Err(anyhow!("Failed to establish connection to server due to fatal error: {code}"));
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