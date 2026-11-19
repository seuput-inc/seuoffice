use std::collections::HashSet;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use jscompat::{JsMap, JsObject};
use main_core::docs::starred_files::read_starred_items;
use main_core::packages::ai_provider::agent_session_policy::process_platform;
use main_core::packages::electron_utils::default_save_dir::{
    PathProvider, Thrown, configured_default_save_dir,
};
use main_core::shell::app_settings::read_app_settings;
use main_core::shell::folder_roots::read_extra_roots;
use main_core::shell::projects::cloud_files::{
    ListingFailure, ListingRun, ListingRunOptions, PowerShellAttributeReader, ResolveOptions,
    resolve_cloud_only,
};
use main_core::shell::projects::project_files::{CloudOnlyResolver, ResolveFailure};
use main_core::shell::projects::project_index::{
    ProjectNameOnly, merge_index_roots, project_folder_roots,
};
use shell_contract::runtime::{AppPaths, PathName};
use shell_ipc_w::SharedSaveDirState;
use shell_ipc_w::projects::ProjectsHostRef;

use super::index_scheduler::IndexSources;
use crate::paths::ShellPaths;

struct Paths(ShellPaths);
impl PathProvider for Paths {
    fn get_path(&self, name: &str) -> Result<String, Thrown> {
        let name = match name {
            "userData" => PathName::UserData,
            "documents" => PathName::Documents,
            "home" => PathName::Home,
            _ => return Err(Thrown(format!("unknown application path: {name}"))),
        };
        Ok(self.0.path(name).to_string_lossy().into_owned())
    }
}

struct Listing(ProjectsHostRef);
impl ListingRun for Listing {
    fn run(
        &mut self,
        file: &str,
        args: &[String],
        options: &ListingRunOptions<'_>,
    ) -> Result<String, ListingFailure> {
        let env = options
            .env
            .entries()
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value.clone()))
            .collect::<Vec<_>>();
        self.0
            .run_listing(
                file,
                args,
                options.cwd,
                &env,
                options.timeout_ms,
                options.input,
            )
            .map_err(ListingFailure)
    }
}

struct Cloud(ProjectsHostRef);
impl CloudOnlyResolver for Cloud {
    fn resolve(&mut self, paths: &[String]) -> Result<JsMap<bool>, ResolveFailure> {
        let env: JsObject<String> = std::env::vars().collect();
        let mut reader = PowerShellAttributeReader::new(env.clone(), Listing(self.0.clone()));
        let home = self.0.os_home_dir();
        Ok(resolve_cloud_only(
            paths,
            &env,
            &ResolveOptions {
                platform: process_platform(),
                timeout_ms: None,
                home_dir: None,
                os_home_dir: &home,
                extra_roots: &[],
            },
            &mut reader,
        ))
    }
}

pub(super) struct Sources {
    pub save_dir: SharedSaveDirState,
    pub projects: ProjectsHostRef,
}

impl IndexSources for Sources {
    fn roots(&self) -> Vec<String> {
        let paths = ShellPaths::new();
        let user_data = paths.path(PathName::UserData);
        let settings = user_data
            .join("app-settings.json")
            .to_string_lossy()
            .into_owned();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_millis() as f64;
        let default = configured_default_save_dir(
            &Paths(paths),
            &mut self.save_dir.lock().expect("save directory lock poisoned"),
            now,
        )
        .expect("cannot resolve default save directory");
        let mut home = vec![default.clone()];
        home.extend(read_extra_roots(&read_app_settings(&settings), &default));
        home.retain(|path| std::path::Path::new(path).exists());
        merge_index_roots(&home, &project_folder_roots(&settings), None)
    }

    fn extra_paths(&self) -> Vec<String> {
        let user_data = ShellPaths::new().path(PathName::UserData);
        let recent = std::fs::read(user_data.join("recent.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect::<Vec<_>>();
        let starred_path = user_data
            .join("starred.json")
            .to_string_lossy()
            .into_owned();
        recent
            .into_iter()
            .chain(
                read_starred_items(&starred_path)
                    .into_iter()
                    .map(|item| item.path),
            )
            .collect()
    }

    fn name_only(&self, paths: &[String]) -> HashSet<String> {
        let settings = ShellPaths::new()
            .path(PathName::UserData)
            .join("app-settings.json")
            .to_string_lossy()
            .into_owned();
        let mut source = ProjectNameOnly::new(
            Box::new(move || project_folder_roots(&settings)),
            Cloud(self.projects.clone()),
            None,
        );
        source.name_only(paths).into_iter().collect()
    }
}

pub(super) fn shared(
    save_dir: SharedSaveDirState,
    projects: ProjectsHostRef,
) -> Arc<dyn IndexSources> {
    Arc::new(Sources { save_dir, projects })
}
