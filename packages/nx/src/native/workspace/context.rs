use napi::bindgen_prelude::External;
use std::collections::HashMap;

use crate::native::hasher::hash;
use crate::native::utils::Normalize;
use napi::bindgen_prelude::*;
use rayon::prelude::*;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use crate::native::logger::enable_logger;
use crate::native::project_graph::utils::{find_project_for_path, ProjectRootMappings};
use crate::native::types::FileData;
use parking_lot::{Condvar, Mutex};
use tracing::{trace, warn};

use crate::native::walker::nx_walker;
use crate::native::workspace::types::{
    FileMap, NxWorkspaceFilesExternals, ProjectFiles, UpdatedWorkspaceFiles,
};
use crate::native::workspace::{config_files, workspace_files};

#[napi]
pub struct WorkspaceContext {
    pub workspace_root: String,
    workspace_root_path: PathBuf,
    files_worker: FilesWorker,
}

type Files = Vec<(PathBuf, String)>;
struct FilesWorker(Option<Arc<(Mutex<Files>, Condvar)>>);
impl FilesWorker {
    fn gather_files(workspace_root: &Path) -> Self {
        if !workspace_root.exists() {
            warn!(
                "workspace root does not exist: {}",
                workspace_root.display()
            );
            return FilesWorker(None);
        }

        let files_lock = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
        let files_lock_clone = Arc::clone(&files_lock);
        let workspace_root = workspace_root.to_owned();

        thread::spawn(move || {
            trace!("locking files");
            let (lock, cvar) = &*files_lock_clone;
            let mut workspace_files = lock.lock();
            let files = nx_walker(workspace_root, |rec| {
                let mut file_hashes: Vec<(PathBuf, String)> = vec![];
                for (path, content) in rec {
                    file_hashes.push((path, hash(&content)));
                }
                file_hashes
            });

            workspace_files.extend(files);
            workspace_files.par_sort();
            let files_len = workspace_files.len();
            trace!(?files_len, "files retrieved");

            cvar.notify_all();
        });

        FilesWorker(Some(files_lock))
    }

    pub fn get_files(&self) -> Vec<FileData> {
        if let Some(files_sync) = &self.0 {
            let (files_lock, cvar) = files_sync.deref();
            trace!("locking files");
            let mut files = files_lock.lock();
            let files_len = files.len();
            if files_len == 0 {
                trace!("waiting for files");
                cvar.wait(&mut files);
            }

            let file_data = files
                .iter()
                .map(|(path, hash)| FileData {
                    file: path.to_normalized_string(),
                    hash: hash.clone(),
                })
                .collect();

            drop(files);

            trace!("files are available");
            file_data
        } else {
            vec![]
        }
    }

    pub fn update_files(
        &self,
        workspace_root_path: &Path,
        updated_files: Vec<&str>,
        deleted_files: Vec<&str>,
    ) -> HashMap<String, String> {
        let Some(files_sync) = &self.0 else {
            trace!("there were no files because the workspace root did not exist");
            return HashMap::new();
        };

        let (files_lock, _) = &files_sync.deref();
        let mut files = files_lock.lock();
        let mut map: HashMap<PathBuf, String> = files.drain(..).collect();

        for deleted_file in deleted_files {
            map.remove(&PathBuf::from(deleted_file));
        }

        let updated_files_hashes: HashMap<String, String> = updated_files
            .par_iter()
            .filter_map(|path| {
                let full_path = workspace_root_path.join(path);
                let Ok(content) = std::fs::read(&full_path) else {
                    trace!("could not read file: {full_path:?}");
                    return None;
                };
                Some((path.to_string(), hash(&content)))
            })
            .collect();

        for (file, hash) in &updated_files_hashes {
            map.entry(file.into())
                .and_modify(|e| *e = hash.clone())
                .or_insert(hash.clone());
        }

        *files = map.into_iter().collect();
        files.par_sort();

        updated_files_hashes
    }
}

#[napi]
impl WorkspaceContext {
    #[napi(constructor)]
    pub fn new(workspace_root: String) -> Self {
        enable_logger();

        trace!(?workspace_root);

        let workspace_root_path = PathBuf::from(&workspace_root);

        WorkspaceContext {
            files_worker: FilesWorker::gather_files(&workspace_root_path),
            workspace_root,
            workspace_root_path,
        }
    }

    #[napi(ts_return_type = "Promise<NxWorkspaceFiles>")]
    pub fn get_workspace_files<ConfigurationParser>(
        &self,
        env: Env,
        globs: Vec<String>,
        parse_configurations: ConfigurationParser,
    ) -> anyhow::Result<Option<Object>>
    where
        ConfigurationParser: Fn(Vec<String>) -> napi::Result<Promise<HashMap<String, String>>>,
    {
        workspace_files::get_files(env, globs, parse_configurations, self.all_file_data())
            .map_err(anyhow::Error::from)
    }

    #[napi]
    pub fn glob(
        &self,
        globs: Vec<String>,
        exclude: Option<Vec<String>>,
    ) -> napi::Result<Vec<String>> {
        let file_data = self.all_file_data();
        let globbed_files = config_files::glob_files(&file_data, globs, exclude)?;
        Ok(globbed_files.map(|file| file.file.to_owned()).collect())
    }

    #[napi]
    pub fn hash_files_matching_glob(
        &self,
        globs: Vec<String>,
        exclude: Option<Vec<String>>,
    ) -> napi::Result<String> {
        let files = &self.all_file_data();
        let globbed_files = config_files::glob_files(&files, globs, exclude)?;
        Ok(hash(
            &globbed_files
                .map(|file| file.hash.as_bytes())
                .collect::<Vec<_>>()
                .concat(),
        ))
    }

    #[napi(ts_return_type = "Promise<Record<string, string>>")]
    pub fn get_project_configurations<ConfigurationParser>(
        &self,
        env: Env,
        globs: Vec<String>,
        parse_configurations: ConfigurationParser,
    ) -> napi::Result<Object>
    where
        ConfigurationParser: Fn(Vec<String>) -> napi::Result<Promise<HashMap<String, String>>>,
    {
        let promise = config_files::get_project_configurations(
            globs,
            &self.all_file_data(),
            parse_configurations,
        )?;
        env.spawn_future(async move {
            let result = promise.await?;
            Ok(result)
        })
    }

    #[napi]
    pub fn incremental_update(
        &self,
        updated_files: Vec<&str>,
        deleted_files: Vec<&str>,
    ) -> HashMap<String, String> {
        self.files_worker
            .update_files(&self.workspace_root_path, updated_files, deleted_files)
    }

    #[napi]
    pub fn update_project_files(
        &self,
        project_root_mappings: ProjectRootMappings,
        project_files: External<ProjectFiles>,
        global_files: External<Vec<FileData>>,
        updated_files: HashMap<String, String>,
        deleted_files: Vec<&str>,
    ) -> UpdatedWorkspaceFiles {
        trace!("updating project files");
        trace!("{project_root_mappings:?}");
        let mut project_files_map = project_files.clone();
        let mut global_files = global_files
            .iter()
            .map(|f| (f.file.clone(), f.hash.clone()))
            .collect::<HashMap<_, _>>();

        trace!(
            "adding {} updated files to project files",
            updated_files.len()
        );
        for updated_file in updated_files.into_iter() {
            let file = updated_file.0;
            let hash = updated_file.1;
            if let Some(project_files) = find_project_for_path(&file, &project_root_mappings)
                .and_then(|project| project_files_map.get_mut(project))
            {
                trace!("{file:?} was found in a project");
                if let Some(file) = project_files.iter_mut().find(|f| f.file == file) {
                    trace!("updating hash for file");
                    file.hash = hash;
                } else {
                    trace!("{file:?} was not part of a project, adding to project files");
                    project_files.push(FileData { file, hash });
                }
            } else {
                trace!("{file:?} was not found in any project, updating global files");
                global_files
                    .entry(file)
                    .and_modify(|e| *e = hash.clone())
                    .or_insert(hash);
            }
        }

        trace!(
            "removing {} deleted files from project files",
            deleted_files.len()
        );
        for deleted_file in deleted_files.into_iter() {
            if let Some(project_files) = find_project_for_path(deleted_file, &project_root_mappings)
                .and_then(|project| project_files_map.get_mut(project))
            {
                if let Some(pos) = project_files.iter().position(|f| f.file == deleted_file) {
                    trace!("removing file: {deleted_file:?} from project");
                    project_files.remove(pos);
                }
            }

            if global_files.contains_key(deleted_file) {
                trace!("removing {deleted_file:?} from global files");
                global_files.remove(deleted_file);
            }
        }

        let non_project_files = global_files
            .into_iter()
            .map(|(file, hash)| FileData { file, hash })
            .collect::<Vec<_>>();

        UpdatedWorkspaceFiles {
            file_map: FileMap {
                project_file_map: project_files_map.clone(),
                non_project_files: non_project_files.clone(),
            },
            external_references: NxWorkspaceFilesExternals {
                project_files: External::new(project_files_map),
                global_files: External::new(non_project_files),
                all_workspace_files: External::new(self.all_file_data()),
            },
        }
    }

    #[napi]
    pub fn all_file_data(&self) -> Vec<FileData> {
        self.files_worker.get_files()
    }
}
