use std::collections::HashSet;
use std::ffi::OsStr;
use std::io;
use std::io::{Cursor, ErrorKind, Read};
use std::mem::MaybeUninit;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::fs;
use tokio::io::{AsyncWriteExt, Join};
use tokio::task::JoinHandle;
use zip::read::ZipFile;
use zip::ZipArchive;
use crate::utils::{MutRes, Warn};
use crate::Work;

pub async fn cleanWorkingDir(path: &Path) -> io::Result<()> {
	if !fs::metadata(path).await?.is_dir() { return Ok(()); }
	
	let mut dir = fs::read_dir(path).await?;
	let mut tasks: Vec<JoinHandle<io::Result<()>>> = vec![];
	
	while let Some(entry) = dir.next_entry().await? {
		let path = entry.path().clone();
		if !fs::metadata(&path).await?.is_dir() {
			if match path.extension() {
				Some(ext) => { ext.to_string_lossy() != "wool" }
				None => { true }
			} {
				tasks.push(Work::spawn(async move { fs::remove_file(&path).await }));
			}
		} else {
			tasks.push(Work::spawn(async move { deleteDirDeep(path.as_path()).await }));
		}
	}
	
	for task in tasks {
		task.await??;
	}
	
	Ok(())
}

pub async fn deleteDirDeep(path: &Path) -> io::Result<()> {
	println!("deleting {}", &path.to_string_lossy());
	
	let mut folders = vec![];
	let mut tasks: Vec<JoinHandle<io::Result<()>>> = vec![];
	let mut buff = vec![path.to_owned()];
	
	while let Some(path) = buff.pop() {
		cleanupTasks(&mut tasks).await?;
		
		let mut files = vec![];
		
		if fs::metadata(path.clone()).await?.is_dir() {
			folders.push(path.clone());
			
			let mut dir = fs::read_dir(path).await?;
			while let Some(entry) = dir.next_entry().await? {
				buff.push(entry.path());
			}
		} else {
			files.push(path.deref().to_owned());
		}
		
		if !files.is_empty() {
			let task = Work::spawn(async move {
				for path in files {
					// println!("deleting {}", path.to_string_lossy());
					fs::remove_file(path.as_path()).await?;
				}
				Ok(())
			});
			tasks.push(task);
		}
	}
	for task in tasks {
		task.await??;
	}
	for folder in folders.iter().rev() {
		// println!("deleting {}", folder.to_string_lossy());
		fs::remove_dir(folder.as_path()).await?;
	}
	
	Ok(())
}

async fn cleanupTasks(tasks: &mut Vec<JoinHandle<io::Result<()>>>) -> io::Result<()> {
	let mut start = 0;
	'outer:
	loop {
		let st = start;
		for i in st..tasks.len() {
			if let Some(task) = tasks.get(i) {
				if task.is_finished() {
					tasks.remove(i).await??;
					start = i;
					continue 'outer;
				}
			}
		}
		break;
	}
	Ok(())
}

pub fn unzipMem(zipFile: Vec<u8>, destFolder: &Path, password: Option<&[u8]>) -> io::Result<()> {
	std::fs::create_dir_all(&destFolder)?;
	println!("Unzipping from {} bytes", zipFile.len());
	let mut data = Cursor::new(zipFile.as_slice());
	let mut archive = ZipArchive::new(&mut data)?;
	let password = password.map(|p| p.to_owned());
	unzipWorker(&mut archive, password, destFolder, Default::default(), 1, 0)?;
	println!("Unzipped  from {} bytes", zipFile.len());
	Ok(())
}

pub async fn unzip(zipFile: &Path, destFolder: &Path, password: Option<&[u8]>) -> io::Result<()> {
	fs::create_dir_all(destFolder).await?;
	println!("Unzipping {}", &zipFile.to_string_lossy());
	let mut tasks: Vec<JoinHandle<io::Result<()>>> = vec![];
	let workerCount = 8;
	let folders: Arc<Mutex<HashSet<PathBuf>>> = Default::default();
	for i in 0..workerCount {
		let zipFile = zipFile.to_owned();
		let destFolder = destFolder.to_owned();
		let folders = folders.clone();
		let password = password.map(|p| p.to_owned());
		let task = Work::spawn(async move {
			let file = std::fs::File::open(zipFile)?;
			let mut archive = ZipArchive::new(file)?;
			unzipWorker(&mut archive, password, destFolder.as_path(), folders, workerCount, i)
		});
		tasks.push(task);
	}
	for task in tasks {
		task.await??;
	}
	println!("Unzipped  {}", &zipFile.to_string_lossy());
	Ok(())
}

fn unzipWorker<'a, R: Read + io::Seek>(
	archive: &mut ZipArchive<R>, password: Option<Vec<u8>>, destFolder: &Path,
	foldersCreated: Arc<Mutex<HashSet<PathBuf>>>, workerCount: u32, wi: u32,
) -> io::Result<()> {
	let mut index: u32 = 0;
	
	for i in 0..archive.len() {
		let mut entry: ZipFile =
			if let Some(password) = &password {
				archive.by_index_decrypt(i, password.as_slice())?.map_err(|e| std::io::Error::from(ErrorKind::Other))?
			} else {
				archive.by_index(i)?
			};
		let entryPath = entry.mangled_name();
		
		if entry.is_dir() {
			let newPath = foldersCreated.access(|fld| {
				fld.insert(entryPath.clone())
			}).unwrap();
			if !newPath { continue; }
			std::fs::create_dir_all(&destFolder.join(&entryPath))?;
			// println!("{}", path.to_string_lossy());
			
			continue;
		}
		
		if !entry.is_file() { continue; }
		
		if workerCount > 1 {
			index += 1;
			if index % workerCount != wi { continue; }
		}
		
		let path = destFolder.join(entryPath);
		let mut file = std::fs::File::create(&path)?;
		std::io::copy(&mut entry, &mut file)?;
		// println!("Wrote {}", path.to_string_lossy());
	}
	
	Ok(())
}