use std::borrow::Cow;
use std::fmt::Display;
use std::fs;
use std::fs::File;
use std::io::{Bytes, Read};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use mime::Mime;
use reqwest::{Body, Client, IntoUrl, multipart, RequestBuilder, Response};
use reqwest::multipart::Part;
use serde::{de, Deserialize, Serialize};
use crate::net;

pub const XML_CONTENT: &str = "text/xml";
pub const XML_CONTENT_O: Option<&str> = Some(XML_CONTENT);

#[derive(Debug, Serialize, Deserialize)]
pub struct RequestEndPoint {
	pub path: String,
	pub maxPeriod: u32,
}

impl RequestEndPoint {
	pub fn get<'a, S: AsRef<str> + Display>(&self, client: &Client, domain: S) -> ReqBuild {
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
	path: String,
	req: RequestBuilder,
}

pub enum VFile {
	Str { pretendPath: Box<Path>, data: Box<str> },
	Real(Box<Path>),
}

impl VFile {
	fn read(&self) -> Result<Vec<u8>, String> {
		match self {
			VFile::Str { data, .. } => { todo!() }
			VFile::Real(p) => { todo!() }
		}
	}
	fn path(&self) -> &Box<Path> {
		match self {
			VFile::Str { pretendPath, .. } => { pretendPath }
			VFile::Real(p) => { p }
		}
	}
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
}

pub async fn send<U: Display>(url: U, res: RequestBuilder) -> Result<Response, String> {
	res.send().await
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