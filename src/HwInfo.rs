use anyhow::anyhow;
use machineid_rs::{Encryption, HWIDComponent, IdBuilder};
use raw_cpuid::CpuId;
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, Pid, ProcessRefreshKind, RefreshKind, System};
use tokio::task::JoinHandle;
use crate::utils::ResultMsg;
use crate::Work;


#[derive(Debug, Clone)]
pub struct HwInfo {
	pub hwId: String,
	pub vendor: String,
	pub cpuName: String,
	pub familyId: u8,
	pub modelId: u8,
	pub steppingId: u8,
	pub frequency: Option<u64>,
	pub cores: u16,
	pub osName: String,
	pub hostName: String,
	pub totalSystemMemory: u64,
}

pub async fn collectInfo() -> ResultMsg<HwInfo> {
	macro_rules! get {($val:expr) => {$val.await.map_err(|_|anyhow!("Failed to execute CPU info"))?};}
	
	let hwId = Work::spawn(async {
		fn hwIdOf(comp: Vec<HWIDComponent>) -> Result<String, ()> {
			let mut b = IdBuilder::new(Encryption::MD5);
			for comp in comp {
				b.add_component(comp);
			}
			b.build("FooBar").map_err(|_| ())
		}
		
		let res = hwIdOf(vec![HWIDComponent::SystemID])
			.or_else(|_| hwIdOf(vec![HWIDComponent::MacAddress]))
			.or_else(|_| hwIdOf(vec![HWIDComponent::Username, HWIDComponent::MachineName, HWIDComponent::OSName]))
			.map_err(|e| anyhow!("Failed to create HW-ID"));
		// println!("id done");
		res
	});
	
	let cpuidRes: JoinHandle<ResultMsg<_>> = Work::spawn(async {
		let cpuid = CpuId::new();
		//println!("{:#?}", cpuid);
		
		let info = cpuid.get_feature_info().ok_or(anyhow!("Failed to get CPU info"))?;
		let vendor = cpuid.get_vendor_info().ok_or(anyhow!("Failed to get CPU Vendor info"))?
			.as_str().to_string();
		let cpuName = cpuid.get_processor_brand_string()
			.map(|b| b.as_str().to_string()).unwrap_or("".to_string());
		
		let familyId = info.family_id();
		let modelId = info.model_id();
		let steppingId = info.stepping_id();
		let frequency = cpuid.get_processor_frequency_info()
			.map(|v| (v.processor_max_frequency() as u64) * 1_000_000);
		
		let cores = cpuid.get_processor_capacity_feature_info()
			.map(|info| info.num_phys_threads() as u16);
		
		// println!("info done");
		Ok((vendor, cpuName, familyId, modelId, steppingId, frequency, cores))
	});
	let sysInfo = Work::spawn(async {
		let sys = System::new_with_specifics(RefreshKind::new()
			.with_cpu(CpuRefreshKind::everything())
			.with_memory(MemoryRefreshKind::new().with_ram())
		);
		let frequency = sys.cpus().first().map(|cpu| cpu.frequency() * 1_000_000);
		let mem = sys.total_memory() / 1024;
		if mem == 0 { return Err(anyhow!("Could not get total system memory")); }
		Ok((frequency, mem))
	});
	
	let (
		vendor, cpuName, familyId, modelId,
		steppingId, mut frequency, mut cores
	) = get!(cpuidRes)?;
	let hwId = get!(hwId)?;
	let (frequency2, totalSystemMemory) = get!(sysInfo)?;
	if frequency.is_none() {
		println!("Failed to get frequency from cpuid. Trying again");
		frequency = frequency2;
		if frequency.is_none() { println!("Failed to get frequency!"); }
	}
	if cores.is_none() {
		println!("Failed to get core count. Trying again");
		let sys = System::new_with_specifics(RefreshKind::new().with_cpu(CpuRefreshKind::everything()));
		let count = sys.cpus().len() as u16;
		if count > 0 { cores = Some(count); }
	}
	let cores = cores.ok_or(anyhow!("Failed to get core count!"))?;
	
	let osName = format!("{} {}",
	                     System::name().unwrap_or_else(|| "".into()),
	                     System::os_version().unwrap_or_else(|| "".into()));
	
	let hostName = System::host_name().unwrap_or_default();
	
	Ok(HwInfo {
		hwId,
		vendor,
		cpuName,
		familyId,
		modelId,
		steppingId,
		frequency,
		cores,
		osName,
		hostName,
		totalSystemMemory,
	})
}

pub fn getSystemFreeMemory() -> u64 {
	let sys = System::new_with_specifics(RefreshKind::new()
		.with_memory(MemoryRefreshKind::new().with_ram())
	);
	sys.free_memory() / 1024
}


pub fn getProcessWorkingSet(pid: u32) -> ResultMsg<u64> {
	let pid = Pid::from_u32(pid);
	let mut sys = System::new();
	sys.refresh_process_specifics(pid, ProcessRefreshKind::new().with_memory());
	let proc = sys.process(pid).ok_or(anyhow!("Could not find process {pid}"))?;
	if proc.memory() == 0 {
		Err(anyhow!("Could not retieve process memory: {pid}"))
	} else {
		Ok(proc.memory() / 1024)
	}
}