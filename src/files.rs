use std::collections::HashSet;
use std::ffi::OsStr;
use std::io;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex};
use positioned_io::RandomAccessFile;
use rc_zip_tokio::rc_zip::parse::EntryKind;
use rc_zip_tokio::{ArchiveHandle, ReadZip};
use tokio::fs;
use tokio::io::{AsyncWriteExt, Join};
use tokio::task::JoinHandle;
use crate::utils::Warn;
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

pub async fn unzipMem(zipFile: Vec<u8>, destFolder: &Path) -> io::Result<()> {
	fs::create_dir_all(&destFolder).await?;
	println!("Unzipping from {} bytes", zipFile.len());
	let archive = zipFile.read_zip().await?;
	unzipWorker(&archive, destFolder, Default::default(), 1, 0).await?;
	drop(archive);
	println!("Unzipped  from {} bytes", zipFile.len());
	Ok(())
}

pub async fn unzip(zipFile: &Path, destFolder: &Path) -> io::Result<()> {
	fs::create_dir_all(destFolder).await?;
	println!("Unzipping {}", &zipFile.to_string_lossy());
	let mut tasks: Vec<JoinHandle<io::Result<()>>> = vec![];
	let workerCount = 4;
	let folders: Arc<Mutex<HashSet<PathBuf>>> = Default::default();
	for i in 0..workerCount {
		let zipFile = zipFile.to_owned();
		let destFolder = destFolder.to_owned();
		let folders = folders.clone();
		let task = Work::spawn(async move {
			let file = Arc::new(RandomAccessFile::open(zipFile)?);
			let archive = file.read_zip().await?;
			unzipWorker(&archive, destFolder.as_path(), folders, workerCount, i).await
		});
		tasks.push(task);
	}
	for task in tasks {
		task.await??;
	}
	println!("Unzipped  {}", &zipFile.to_string_lossy());
	Ok(())
}

async fn unzipWorker<'a, T: rc_zip_tokio::HasCursor>(
	archive: &ArchiveHandle<'a, T>, destFolder: &Path,
	foldersCreated: Arc<Mutex<HashSet<PathBuf>>>, workerCount: u32, i: u32,
) -> io::Result<()> {
	let mut tasks: Vec<JoinHandle<io::Result<()>>> = vec![];
	let mut index: u32 = 0;
	
	for entry in archive.entries() {
		match entry.kind() {
			EntryKind::Directory => {
				let path = destFolder.join(&entry.name);
				{
					let mut fld = foldersCreated.lock().await;
					if fld.contains(&path) {
						continue;
					}
					fld.insert(path.clone());
				}
				fs::create_dir_all(&path).await?;
				// println!("{}", path.to_string_lossy());
			}
			EntryKind::File => {
				index += 1;
				if index % workerCount != i { continue; }
				cleanupTasks(&mut tasks).await?;
				
				let path = destFolder.join(&entry.name);
				if tasks.len() >= 5 || entry.uncompressed_size > 1024 * 1024 * 8 {
					let mut data = entry.reader();
					let mut file = fs::File::create(&path).await?;
					
					tokio::io::copy(&mut data, &mut file).await?;
					// println!("Wrote BLOCKING {}", path.to_string_lossy());
				} else {
					let data = entry.bytes().await?;
					let task = Work::spawn(async move {
						let mut file = fs::File::create(&path).await?;
						file.write_all(data.as_slice()).await?;
						// println!("Wrote {}", path.to_string_lossy());
						Ok(())
					});
					tasks.push(task);
				}
			}
			EntryKind::Symlink => {
				println!("UNKNOWN SYMLINK {}", entry.name);
			}
		}
	}
	
	for task in tasks {
		task.await??;
	}
	Ok(())
}