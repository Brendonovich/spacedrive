use crate::job::jobs::JobReportUpdate;
use crate::job::worker::WorkerEvent;
use crate::job::{jobs::Job, worker::WorkerContext};
use crate::sys::locations::{create_location, LocationResource};
use crate::util::time;
use crate::CoreContext;
use anyhow::{anyhow, Result};
use log::info;
use serde::{Deserialize, Serialize};
use std::{
	collections::HashMap, ffi::OsStr, fs, path::Path, path::PathBuf, time::Instant,
};
use walkdir::{DirEntry, WalkDir};

#[derive(Debug)]
pub struct IndexerJob {
	pub path: String,
}

#[async_trait::async_trait]
impl Job for IndexerJob {
	async fn run(&self, ctx: WorkerContext) -> Result<()> {
		// invoke scan_path function
		let core_ctx = ctx.core_ctx.clone();
		scan_path(&core_ctx, self.path.as_str(), move |progress| {
			// update job progress
			ctx.sender
				.send(WorkerEvent::Progressed(
					progress.iter().map(|p| p.clone().into()).collect(),
				))
				.unwrap_or(());
		})
		.await?;
		Ok(())
	}
}

#[derive(Clone)]
pub enum ScanProgress {
	ChunkCount(i64),
	SavedChunks(i64),
	Message(String),
}

// creates a vector of valid path buffers from a directory
pub async fn scan_path(
	ctx: &CoreContext,
	path: &str,
	on_progress: impl Fn(Vec<ScanProgress>) + Send + Sync + 'static,
) -> Result<()> {
	println!("Scanning directory: {}", &path);
	let db = &ctx.database;
	let path = path.to_string();

	let location = create_location(&ctx, &path).await?;

	println!("Location: {:?}", location);

	// query db to highers id, so we can increment it for the new files indexed
	#[derive(Deserialize, Serialize, Debug)]
	struct QueryRes {
		id: Option<i64>,
	}
	// grab the next id so we can increment in memory for batch inserting
	let first_file_id = match db
		._query_raw::<QueryRes>(r#"SELECT MAX(id) id FROM file_paths"#)
		.await
	{
		Ok(rows) => rows[0].id.unwrap_or(0),
		Err(e) => Err(anyhow!("Error querying for next file id: {}", e))?,
	};

	//check is path is a directory
	if !PathBuf::from(&path).is_dir() {
		return Err(anyhow::anyhow!("{} is not a directory", &path));
	}
	let dir_path = path.clone();

	let (paths, scan_start, on_progress) = tokio::task::spawn_blocking(move || {
		// store every valid path discovered
		let mut paths: Vec<(PathBuf, i64, Option<i64>)> = Vec::new();
		// store a hashmap of directories to their file ids for fast lookup
		let mut dirs: HashMap<String, i64> = HashMap::new();
		// begin timer for logging purposes
		let scan_start = Instant::now();

		let mut next_file_id = first_file_id;
		let mut get_id = || {
			next_file_id += 1;
			next_file_id
		};
		// walk through directory recursively
		for entry in WalkDir::new(&dir_path).into_iter().filter_entry(|dir| {
			let approved = !is_hidden(dir)
				&& !is_app_bundle(dir)
				&& !is_node_modules(dir)
				&& !is_library(dir);
			approved
		}) {
			// extract directory entry or log and continue if failed
			let entry = match entry {
				Ok(entry) => entry,
				Err(e) => {
					println!("Error reading file {}", e);
					continue;
				},
			};
			let path = entry.path();

			let parent_path = path
				.parent()
				.unwrap_or(Path::new(""))
				.to_str()
				.unwrap_or("");
			let parent_dir_id = dirs.get(&*parent_path);
			// println!("Discovered: {:?}, {:?}", &path, &parent_dir_id);
			on_progress(vec![ScanProgress::Message(format!("Found: {:?}", &path))]);

			let file_id = get_id();
			paths.push((path.to_owned(), file_id, parent_dir_id.cloned()));

			if entry.file_type().is_dir() {
				let _path = match path.to_str() {
					Some(path) => path.to_owned(),
					None => continue,
				};
				dirs.insert(_path, file_id);
			}
		}
		(paths, scan_start, on_progress)
	})
	.await
	.unwrap();

	let db_write_start = Instant::now();
	let scan_read_time = scan_start.elapsed();

	for (i, chunk) in paths.chunks(100).enumerate() {
		on_progress(vec![
			ScanProgress::ChunkCount(i as i64),
			ScanProgress::SavedChunks(i as i64),
			ScanProgress::Message(format!(
				"Writing {} files to db at chunk {}",
				chunk.len(),
				i
			)),
		]);

		info!(
			"{}",
			format!("Writing {} files to db at chunk {}", chunk.len(), i)
		);

		// vector to store active models
		let mut files: Vec<String> = Vec::new();
		for (file_path, file_id, parent_dir_id) in chunk {
			files.push(
				match prepare_values(&file_path, *file_id, &location, parent_dir_id) {
					Ok(file) => file,
					Err(e) => {
						println!(
							"Error creating file model from path {:?}: {}",
							file_path, e
						);
						continue;
					},
				},
			);
		}
		let raw_sql = format!(
			r#"
                INSERT INTO file_paths (id, is_dir, location_id, parent_id, materialized_path, date_indexed) 
                VALUES {}
            "#,
			files.join(", ")
		);
		let count = db._execute_raw(&raw_sql).await;
		// println!("Inserted {:?} records", count);
	}
	println!(
		"scan of {:?} completed in {:?}. {:?} files found. db write completed in {:?}",
		&path,
		scan_read_time,
		paths.len(),
		db_write_start.elapsed()
	);
	Ok(())
}

// reads a file at a path and creates an ActiveModel with metadata
fn prepare_values(
	file_path: &PathBuf,
	id: i64,
	location: &LocationResource,
	parent_id: &Option<i64>,
) -> Result<String> {
	let metadata = fs::metadata(&file_path)?;
	let location_path = location.path.as_ref().unwrap().as_str();
	// let size = metadata.len();
	// let name = extract_name(file_path.file_stem());
	// let extension = extract_name(file_path.extension());

	let materialized_path = match file_path.to_str() {
		Some(p) => p
			.clone()
			.strip_prefix(&location_path)
			// .and_then(|p| p.strip_suffix(format!("{}{}", name, extension).as_str()))
			.unwrap_or_default(),
		None => return Err(anyhow!("{}", file_path.to_str().unwrap_or_default())),
	};

	Ok(format!(
		"({}, {}, {}, {}, \"{}\", \"{}\")",
		id,
		metadata.is_dir(),
		location.id,
		parent_id
			.clone()
			.map(|id| id.to_string())
			.unwrap_or("NULL".to_string()),
		materialized_path,
		// &size.to_string(),
		&time::system_time_to_date_time(metadata.created())
			.unwrap()
			.to_string(),
		// &time::system_time_to_date_time(metadata.modified()).unwrap().to_string(),
	))
}

pub async fn test_scan(path: &str) -> Result<()> {
	let mut count: u32 = 0;
	for entry in WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
		let child_path = entry.path().to_str().unwrap();
		count = count + 1;
		println!("Reading file from dir {:?}", child_path);
	}
	println!("files found {}", count);
	Ok(())
}

// extract name from OsStr returned by PathBuff
fn extract_name(os_string: Option<&OsStr>) -> String {
	os_string
		.unwrap_or_default()
		.to_str()
		.unwrap_or_default()
		.to_owned()
}

fn is_hidden(entry: &DirEntry) -> bool {
	entry
		.file_name()
		.to_str()
		.map(|s| s.starts_with("."))
		.unwrap_or(false)
}

fn is_library(entry: &DirEntry) -> bool {
	entry
        .path()
        .to_str()
        // make better this is shit
        .map(|s| s.contains("/Library/"))
        .unwrap_or(false)
}

fn is_node_modules(entry: &DirEntry) -> bool {
	entry
		.file_name()
		.to_str()
		.map(|s| s.contains("node_modules"))
		.unwrap_or(false)
}

fn is_app_bundle(entry: &DirEntry) -> bool {
	let is_dir = entry.metadata().unwrap().is_dir();
	let contains_dot = entry
		.file_name()
		.to_str()
		.map(|s| s.contains(".app") | s.contains(".bundle"))
		.unwrap_or(false);

	let is_app_bundle = is_dir && contains_dot;
	// if is_app_bundle {
	//   let path_buff = entry.path();
	//   let path = path_buff.to_str().unwrap();

	//   self::path(&path, );
	// }

	is_app_bundle
}
// pub async fn scan_loc(core: &mut Core, location: &LocationResource) -> Result<()> {
// 	// get location by location_id from db and include location_paths
// 	// let job = core.queue(
// 	// 	job::JobResource::new(core.state.client_uuid.clone(), job::JobAction::ScanLoc, 1, |loc| {
// 	// 		if let Some(path) = &loc.path {
// 	// 			scan_path(core, path).await?;
// 	// 			watch_dir(path);
// 	// 		}
// 	// 	})
// 	// 	.await?,
// 	// );

// 	Ok(())
// }
// pub async fn scan_loc(core: &mut Core, location: &LocationResource) -> Result<()> {
// 	// get location by location_id from db and include location_paths
// 	let job = core.queue(
// 		job::JobResource::new(
// 			core.state.client_uuid.clone(),
// 			job::JobAction::ScanLoc,
// 			1,
// 			Some(vec![location.path.as_ref().unwrap().to_string()]),
// 		)
// 		.await?,
// 	);

// 	if let Some(path) = &location.path {
// 		scan_path(core, path).await?;
// 		watch_dir(path);
// 	}
// 	Ok(())
// }

impl From<ScanProgress> for JobReportUpdate {
	fn from(progress: ScanProgress) -> Self {
		match progress {
			ScanProgress::ChunkCount(count) => JobReportUpdate::TaskCount(count),
			ScanProgress::SavedChunks(count) => {
				JobReportUpdate::CompletedTaskCount(count)
			},
			ScanProgress::Message(message) => JobReportUpdate::Message(message),
		}
	}
}
