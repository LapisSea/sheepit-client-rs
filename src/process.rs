use std::{env, io};
use std::io::{BufRead, Read};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Instant;
use anyhow::anyhow;
use indoc::indoc;
use lazy_static::lazy_static;
use regex::Regex;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use crate::defs::POST_LOAD_NOTIFICATION;
use crate::job::Job;
use crate::{defs, log, ServerConnection};
use crate::utils::{ResultJMsg, ResultMsg};

pub async fn createBlenderCommand(job: Arc<Job>, server: Arc<ServerConnection>) -> ResultMsg<(Command, Vec<PathBuf>)> {
	let tmpDir = &server.clientConf.tmpDir();
	let mut files = vec![];
	
	let mut command = Command::new(server.clientConf.rendererPathExecutable(&job.info));
	command
		.env("BLENDER_USER_RESOURCES", "")
		.env("BLENDER_USER_CONFIG", "")
		.env("BLENDER_USER_SCRIPTS", "")
		.env("BLENDER_USER_DATAFILES", "")
		.env("BLENDER_SYSTEM_RESOURCES", "")
		.env("BLENDER_SYSTEM_SCRIPTS", "")
		.env("BLENDER_SYSTEM_DATAFILES", "")
		.env("BLENDER_SYSTEM_PYTHON", "")
		
		.env("OCIO", "")
		.env("TEMP", tmpDir)
		.env("TMP", tmpDir)
		
		.env("CORES", job.conf.cores.to_string())
		// .env("PRIORITY", )//TODO
		.env(
			"LD_LIBRARY_PATH",
			server.clientConf.rendererPath(&job.info).join("lib:")
				.join(env::var("LD_LIBRARY_PATH").unwrap_or_default()),
		);
	
	for arg in job.info.rendererInfo.commandline.split(' ') {
		match arg {
			"" => {}
			".c" => {
				async fn write(file: &mut fs::File, txt: &str) -> ResultJMsg {
					swait!(file.write_all(txt.as_bytes()),"Failed to create script")?;
					swait!(file.write_all("\n".as_bytes()),"Failed to create script")
				}
				{
					let path = server.clientConf.workPath.join("pre_load_script.py");
					files.push(path.clone());
					let _ = fs::remove_file(&path).await;
					let file = &mut swait!(fs::File::create(&path),"Failed to create pre_load_script")?;
					write(file, indoc! {r#"
						import bpy
						import sys
						from bpy.app.handlers import persistent
						@persistent
						def hide_stuff(hide_dummy):
							print('PRE_LOAD_SCRIPT_hide_viewport')
							#Hide collections in the viewport
							for col in bpy.data.collections:
								col.hide_viewport = True
							for obj in bpy.data.objects:
								#Hide objects in the viewport
								obj.hide_viewport = True
								#Hide modifier in the viewport
								for mod in obj.modifiers:
									mod.show_viewport = False
							sys.stdout.flush()
						bpy.app.handlers.version_update.append(hide_stuff)
						"#}).await?;
					
					command.arg("-P").arg(path);
				}
				
				command.arg(&server.clientConf.scenePathBlend(&job.info));
				
				{
					let path = server.clientConf.workPath.join("post_load_script.py");
					files.push(path.clone());
					let _ = fs::remove_file(&path).await;
					let file = &mut swait!(fs::File::create(&path),"Failed to create pre_load_script")?;
					write(file, &job.info.script).await?;
					write(file, &indoc! {r#"
						import sys
						print('{}')
						sys.stdout.flush()
						"#}.replace("{}", POST_LOAD_NOTIFICATION)).await?;
					//Core script
					write(file, &format!(
						"sheepit_set_compute_device(\"{}\", \"{}\", \"{}\")\n",
						"NONE",//TODO if job.info.useGPU { "GPU" } else { "CPU" },
						"CPU",//TODO
						"CPU"//TODO
					)).await?;
					
					write(file, indoc! {r#"
						import signal
						def hndl(signum, frame):
							pass
							
						signal.signal(signal.SIGINT, hndl)
						"#}).await?;
					
					command.arg("-P").arg(path);
				}
			}
			".e" => {
				// the number of cores has to be put after the binary and before the scene arg
				command.arg("-t");
				command.arg(job.conf.cores.to_string());
			}
			".o" => {
				command.arg(server.clientConf.workPath.join(format!("{}_", job.info.id)));
			}
			".f" => {
				command.arg(&job.info.frameNumber);
			}
			arg => {
				command.arg(arg);
			}
		}
	}
	
	command.stdout(Stdio::piped());
	command.stderr(Stdio::piped());
	command.kill_on_drop(true);
	
	Ok((command, files))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcStatus {
	Running,
	ExitedOk,
	Crashed,
	TookTooLong,
}

#[derive(Clone, Debug)]
pub struct BlenderVersionStr {
	shortVersion: String,
	longVersion: String,
}

#[derive(Clone, Debug)]
pub struct BlendProcessInfo {
	pub prepStart: Option<Instant>,
	pub renderStart: Option<Instant>,
	pub compositStart: Option<Instant>,
	pub status: ProcStatus,
	pub memUsage: u64,
	pub maxMemUsage: u64,
	pub blenderVersion: Option<BlenderVersionStr>,
	pub progress: f32,
	pub speedSamplesRendered: f32,
}

impl Default for BlendProcessInfo {
	fn default() -> Self {
		Self {
			prepStart: None,
			renderStart: None,
			compositStart: None,
			status: ProcStatus::Running,
			memUsage: 0,
			maxMemUsage: 0,
			blenderVersion: None,
			progress: 0.0,
			speedSamplesRendered: 0.0,
		}
	}
}

lazy_static! {
	static ref BLENDER_VERSION: Regex = Regex::new(r"Blender (([0-9]{1,3}\.[0-9]{0,3}).*)$").unwrap();
	static ref RENDER_PROGRESS: Regex = Regex::new(r" (Rendered|Path Tracing Tile|Rendering|Sample) (\d+)\s?/\s?(\d+)( Tiles| samples|,)*").unwrap();
	static ref BEGIN_POST_PROCESSING: Regex = Regex::new(r"^Fra:\d* \w*(.)* \| (Compositing|Denoising|Finished)").unwrap();
	static ref SAMPLE_SPEED: Regex = Regex::new(r"^Rendered (\d+) samples in ([\d.]+) seconds$").unwrap();
}

pub fn observeBlendStdout(info: &mut BlendProcessInfo, line: String) {
	macro_rules! timeIt {
    ($info:expr, $var: ident) => {
		$info.$var = $info.$var.or_else(|| Some(Instant::now()));
    };
}
	
	let line = line.trim();
	if info.blenderVersion.is_none() {
		if let Some(captures) = BLENDER_VERSION.captures(line) {
			info.blenderVersion = Some(BlenderVersionStr {
				longVersion: captures.get(1).unwrap().as_str().to_string(),
				shortVersion: captures.get(2).unwrap().as_str().to_string(),
			});
			log!("## {line}");
		}
	}
	
	if line == POST_LOAD_NOTIFICATION {
		timeIt!(info, prepStart);
		log!("## {line}");
	}
	
	if let Some(captures) = RENDER_PROGRESS.captures(line) {
		let tileJustProcessed = captures.get(2).and_then(|s| s.as_str().parse::<i32>().ok());
		let totalTilesInJob = captures.get(3).and_then(|s| s.as_str().parse::<i32>().ok());
		if let (Some(tileJustProcessed), Some(totalTilesInJob)) = (tileJustProcessed, totalTilesInJob) {
			timeIt!(info, renderStart);
			let progress = (tileJustProcessed as f32 * 100f32) / totalTilesInJob as f32;
			info.progress = progress;
			log!("## {line}");
		}
	}
	
	if BEGIN_POST_PROCESSING.captures(line).is_some() {
		timeIt!(info, compositStart);
		log!("## {line}");
	}
	
	if let Some(captures) = SAMPLE_SPEED.captures(line) {
		let amount = captures.get(1).and_then(|s| s.as_str().parse::<i32>().ok());
		let duration = captures.get(2).and_then(|s| s.as_str().parse::<f32>().ok());
		if let (Some(amount), Some(duration)) = (amount, duration) {
			if duration != 0f32 {
				info.speedSamplesRendered = amount as f32 / duration;
				log!("## {line}");
			}
		}
	}
}