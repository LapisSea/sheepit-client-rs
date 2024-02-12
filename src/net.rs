use std::fmt::Display;
use std::fs::File;
use std::ops::{Add, AddAssign};
use std::path::{Path};
use std::time::Duration;
use futures_util::StreamExt;
use reqwest::{Client, IntoUrl, RequestBuilder, Response};
use serde::{de, Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::time::Instant;

pub const XML_CONTENT: &str = "text/xml";
pub const XML_CONTENT_O: Option<&str> = Some(XML_CONTENT);

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
	>(&self, client: &Client, doamin: S, query: &ARGS) -> Result<Req, String> {
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
	
	pub async fn send(&self) -> Result<Req, String> {
		Ok(Req {
			path: self.path.clone().into(),
			res: send::<&str>(self.path.as_ref(), self.req.try_clone().unwrap()).await?,
		})
	}
}

impl Req {
	pub async fn text(self) -> Result<String, String> {
		bodyText(self.path, self.res).await
	}
	
	pub async fn xml<'de, T: de::Deserialize<'de>>(self) -> Result<T, String> {
		requireContentType(XML_CONTENT_O, &self.res)?;
		let xml = bodyText(self.path, self.res).await?;
		fromXml(xml.as_str())
	}
	
	pub fn response(self) -> Result<Response, String> {
		let res = self.res;
		res.error_for_status_ref().map_err(|e| e.to_string())?;
		Ok(res)
	}
}

pub async fn send<U: Display>(url: U, res: RequestBuilder) -> Result<Response, String> {
	res.send().await.and_then(|res| res.error_for_status())
		.map_err(|e| format!("Could not get an ok response from {url} because: {e}"))
}

async fn bodyText<U: Display>(url: U, res: Response) -> Result<String, String> {
	res.text().await.map_err(|e| format!("Failed to get body of {url} because: {e}"))
}

pub async fn postRequestForm<
	T: Serialize + ?Sized,
	U: IntoUrl + Display + Clone
>(client: &Client, url: U, data: &T, requiredContentType: Option<&str>) -> Result<String, String> {
	let resp = send(&url, client.post(url.clone()).form(data)).await?;
	requireContentType(requiredContentType, &resp)?;
	bodyText(url, resp).await
}

pub fn requireContentType(requiredContentType: Option<&str>, resp: &Response) -> Result<(), String> {
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
		return Err(format!(
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

pub fn fromXml<'de, T: de::Deserialize<'de>>(xml: &str) -> Result<T, String> {
	let mut body = xml.trim();
	if body.starts_with("<?xml") {
		let mark = "?>";
		body = &body[body.find(mark).unwrap_or(0) + mark.len()..].trim();
	}
	serde_xml_rs::from_str(body).map_err(|e| format!("Could not parse {} because: {}\nXML Text:\n{}", std::any::type_name::<T>(), e, xml))
}

pub fn toXml<T: serde::Serialize>(val: &T) -> Result<String, String> {
	serde_xml_rs::to_string(val)
		.map(|s| format!("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n{s}"))
		.map_err(|e| format!("Could not serialize {} because: {}", std::any::type_name::<T>(), e))
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


pub async fn doesFileExist(path: &Path) -> std::io::Result<bool> {
	let mut exists = tokio::fs::try_exists(path).await?;
	if exists {
		let metadata = tokio::fs::metadata(path).await?;
		exists = metadata.len() > 0;
	}
	Ok(exists)
}

pub async fn downloadFile(path: &Path, req: ReqBuild, md5Check: &str) -> Result<TransferStats, String> {
	if doesFileExist(path).await.map_err(|e| format!("Could not check existence of {} because: {}", path.to_string_lossy(), e))? {
		return Ok(Default::default());
	}
	
	if let Some(parent) = path.parent() {
		tokio::fs::create_dir_all(parent).await.map_err(|e| format!("Could not create directory: {} because {e}", parent.to_string_lossy()))?;
	}
	
	let resp = req.send().await?.response()?;
	let url = resp.url().to_owned();
	let mut stream = resp.bytes_stream();
	
	let mut file = tokio::fs::File::from(File::create(path)
		.map_err(|e| format!("Could not create file \"{}\" because: {e}", path.to_string_lossy()))?);
	
	let mut bytes = 0;
	let start = Instant::now();
	
	let mut context = md5::Context::new();
	while let Some(item) = StreamExt::next(&mut stream).await {
		let item = item.map_err(|e| format!("Could not download \"{url}\" because: {e}"))?;
		// println!("chunk len {}", item.len());
		bytes += item.len() as u64;
		let ch = item.as_ref();
		context.consume(ch);
		file.write(ch).await.map_err(|e| format!("Could not write to file: {}", path.to_string_lossy()))?;
	}
	
	let end = Instant::now();
	
	let md5s = format!("{:x}", context.compute());
	if !md5s.as_str().eq(md5Check) {
		println!("{md5s}\n{md5Check}");
		return Err(format!("Hashes do not match for {}", url));
	}
	
	Ok(start.checked_duration_since(end).map(|time| TransferStats { bytes, time }).unwrap_or_default())
}