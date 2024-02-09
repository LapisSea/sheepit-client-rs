use std::fmt::Display;
use reqwest::{Client, IntoUrl, Response};
use serde::{de, Serialize};

pub async fn postForm<
	T: Serialize + ?Sized,
	U: IntoUrl + Display + Clone
>(client: &Client, url: U, data: &T, requiredContentType: Option<&str>) -> Result<String, String> {
	let resp = client.post(url.clone()).form(data).send().await
		.map_err(|e| format!("Could not get an ok response from {url} because: {e}"))?;
	
	
	let validContentType = match requiredContentType {
		None => { true }//No required type. Anything goes
		Some(requiredContentType) => {
			match getContentTypeStr(&resp) {
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
			match getContentTypeStr(&resp) {
				None => { "".to_string() }
				Some(s) => { format!(": {}", s) }
			}
		));
	}
	
	resp.text().await
		.map_err(|e| format!("Could not get server config body because: {}", e))
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
	serde_xml_rs::from_str(body).map_err(|e| format!("Could not parse {} because: {}", std::any::type_name::<T>(), e))
}

pub fn toXml<T: serde::Serialize>(val: &T) -> Result<String, String> {
	serde_xml_rs::to_string(val)
		.map(|s| format!("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n{s}"))
		.map_err(|e| format!("Could not serialize {} because: {}", std::any::type_name::<T>(), e))
}