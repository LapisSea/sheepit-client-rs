use std::env;
use std::sync::Arc;
use indoc::indoc;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use crate::job::Job;
use crate::ServerConnection;
use crate::utils::{ResultJMsg, ResultMsg};

pub async fn createBlenderCommand(job: Arc<Job>, server: Arc<ServerConnection>) -> ResultMsg<Command> {
	let tmpDir = &server.clientConf.tmpDir();
	
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
					let _ = fs::remove_file(&path).await;
					let file = &mut swait!(fs::File::create(&path),"Failed to create pre_load_script")?;
					write(file, &job.info.script).await?;
					write(file, indoc! {r#"
						import sys
						print('" + POST_LOAD_NOTIFICATION + "')
						sys.stdout.flush()
						"#}).await?;
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
	
	Ok(command)
}