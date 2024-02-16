use std::collections::HashMap;
use std::fmt::Display;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use md5::Digest;
use tokio::io::AsyncReadExt;

#[derive(Eq, PartialEq)]
struct HashNode {
	modTime: SystemTime,
	size: u64,
}

#[derive(Clone, Default)]
pub struct HashCache {
	data: Arc<Mutex<HashMap<Box<Path>, (HashNode, Digest)>>>,
}


pub async fn computeFileHash(path: &Path, cache: HashCache) -> io::Result<Digest> {
	let meta = tokio::fs::metadata(path).await?;
	let fileNode = meta.modified().map(|modTime| HashNode { modTime, size: meta.len() });
	if let Ok(fileNode) = &fileNode {
		if let Some(hash) =
			cache.data.lock().unwrap()
				.get(path)
				.filter(|n| n.0.eq(fileNode))
				.map(|e| e.1) {
			return Ok(hash);
		}
	}
	
	let mut context = md5::Context::new();
	let mut file = tokio::fs::File::open(path).await?;
	
	let mut buffer = [0; 8192];
	loop {
		let bytes_read = file.read(&mut buffer).await?;
		if bytes_read == 0 {
			break;
		}
		context.consume(&buffer[..bytes_read]);
	}
	
	let hash = context.compute();
	if let Ok(fileNode) = fileNode {
		cache.data.lock().unwrap()
			.insert(path.into(), (fileNode, hash));
	}
	Ok(hash)
}

pub fn checkMd5<T: Display>(md5Check: &str, owner: T, computed: Digest) -> Result<(), String> {
	let md5s = format!("{:x}", computed);
	if md5s.as_str().eq(md5Check) { return Ok(()); }
	// println!("{md5s}\n{md5Check}");
	Err(format!("Hashes do not match for {}", owner))
}