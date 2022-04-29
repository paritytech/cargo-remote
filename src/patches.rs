use crate::PROGRESS_FLAG;
use camino::Utf8PathBuf;
use log::error;
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::{exit, Command, Stdio};
use tempfile::NamedTempFile;
use toml_edit::Document;

pub fn locate_workspace_folder(mut crate_path: PathBuf) -> Result<PathBuf, String> {
    let cargo = std::env::var("CARGO").unwrap_or("cargo".to_owned());
    log::debug!("Checking workspace root of path {:?}", crate_path);
    crate_path.push("Cargo.toml");
    let output = Command::new(cargo)
        .arg("locate-project")
        .arg("--workspace")
        .arg("--manifest-path")
        .arg(crate_path.as_os_str().clone())
        .output()
        .expect("jojo");

    if !output.status.success() {
        return Err(format!("{:?}", output.status));
    }

    let output = String::from_utf8(output.stdout).map_err(|e| e.to_string())?;
    let parsed = json::parse(&output).map_err(|e| e.to_string())?;
    let root = parsed["root"].as_str().ok_or(String::from("no root"))?;
    let mut result = PathBuf::from(root);

    // Remove the trailing "/Cargo.toml"
    result.pop();
    Ok(result)
}

#[derive(Debug, Clone)]
pub struct PatchProject {
    pub name: OsString,
    pub local_path: PathBuf,
    pub remote_path: PathBuf,
}

impl PatchProject {
    pub fn new(name: OsString, path: PathBuf, remote_path: PathBuf) -> Self {
        PatchProject {
            name,
            local_path: path,
            remote_path,
        }
    }
}

fn extract_patched_projects_and_patch_manifest<F: Fn(PathBuf) -> Result<PathBuf, String>>(
    manifest_content: String,
    locate_workspace: F,
) -> Option<(Document, Vec<PatchProject>)> {
    let mut manifest = manifest_content.parse::<Document>().expect("invalid doc");
    let mut known_projects: Vec<PatchProject> = Vec::new();

    let patches = manifest["patch"].as_table_mut();
    log::info!("{:?}", patches);
    if patches.is_none() {
        log::debug!("No patches in project.");
        return None;
    }

    let patches = patches.unwrap();

    for crate_level_item in patches.iter_mut().filter_map(|tli| tli.1.as_table_mut()) {
        for table in crate_level_item.iter_mut().filter_map(|kv| {
            kv.1.as_value_mut()
                .map(|f| f.as_inline_table_mut())
                .flatten()
        }) {
            if let Some(path) = table.get("path") {
                let path = PathBuf::from(path.as_str().unwrap().clone());
                let known_project = known_projects
                    .iter()
                    .find(|known_target| path.starts_with(&known_target.local_path));
                match known_project {
                    None => {
                        // Project is unknown and needs to be copied
                        let path_to_copy = locate_workspace(path.clone())
                            .expect("Can not determine workspace path");
                        let name = path_to_copy.file_name().unwrap().to_owned();
                        let mut remote_folder = PathBuf::from("../patches");
                        remote_folder.push(name.clone());
                        log::info!(
                            "Found project '{:?}', will copy to '{:?}'",
                            &path_to_copy,
                            remote_folder
                        );

                        known_projects.push(PatchProject::new(
                            name,
                            path_to_copy.clone(),
                            remote_folder.clone(),
                        ));
                        let mut new_path = remote_folder.clone();
                        new_path.push(path.strip_prefix(path_to_copy).expect("Jawoll"));
                        table.insert("path", toml_edit::Value::from(new_path.to_str().unwrap()));
                    }

                    Some(patch_target) => {
                        let mut new_path = patch_target.remote_path.clone();
                        new_path.push(path.strip_prefix(&patch_target.local_path).expect("Jawoll"));
                        table.insert("path", toml_edit::Value::from(new_path.to_str().unwrap()));
                    }
                }
            }
        }
    }
    log::info!("patches: {:?}", known_projects);
    Some((manifest, known_projects))
}

/// Handle patched dependencies in a Cargo.toml file.
/// Adjustments are only needed when patches point to local files.
/// Steps:
/// 1. Read Cargo.toml of project
/// 2. Extract list of patches
/// 3. For each patched crate, check if there is a path given. If not, ignore.
/// 4. Find the workspace of the patched crate via `cargo locate-project --workspace`
/// 5. Add workspace to the list of projects that need to be copied
/// 6. Copy folders via rsync
pub fn handle_patches(
    build_path: &String,
    build_server: &String,
    manifest_path: Utf8PathBuf,
) -> Result<(), String> {
    let cargo_file_content = std::fs::read_to_string(manifest_path)
        .ok()
        .expect("Shold work");
    let maybe_patches = extract_patched_projects_and_patch_manifest(cargo_file_content, |p| {
        locate_workspace_folder(p)
    });

    if let Some((patched_cargo_doc, project_list)) = maybe_patches {
        let mut tmp_cargo_file = NamedTempFile::new().expect("No tempfile for us");
        tmp_cargo_file
            .write_all(patched_cargo_doc.to_string().as_bytes())
            .expect("Unable to write file");

        copy_patches_to_remote(&build_path, &build_server, tmp_cargo_file, project_list);
    }
    Ok(())
}

fn copy_patches_to_remote(
    build_path: &String,
    build_server: &String,
    patched_cargo_file: NamedTempFile,
    projects_to_copy: Vec<PatchProject>,
) {
    log::info!(
        "Found patches in project. Copying {} ({:?}) projects.",
        projects_to_copy.len(),
        projects_to_copy
            .iter()
            .map(|p| &p.name)
            .collect::<Vec<&OsString>>()
    );
    for patch_operation in projects_to_copy.iter() {
        let local_proj_path = format!("{}/", patch_operation.local_path.to_string_lossy());
        let remote_proj_path = format!(
            "{}:{}",
            build_server,
            patch_operation.remote_path.to_string_lossy()
        );
        log::info!(
            "Copying {:?} from {} to {}.",
            patch_operation.name,
            &local_proj_path,
            &remote_proj_path
        );
        // transfer project to build server
        let mut rsync_to = Command::new("rsync");
        rsync_to
            .arg("-a")
            .arg("-q")
            .arg("--delete")
            .arg("--compress")
            .arg(PROGRESS_FLAG)
            .arg("--exclude")
            .arg("target")
            .arg("--exclude")
            .arg(".*")
            .arg("--rsync-path")
            .arg("mkdir -p remote-builds/patches && rsync")
            .arg(local_proj_path)
            .arg(remote_proj_path)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .stdin(Stdio::inherit())
            .output()
            .unwrap_or_else(|e| {
                error!("Failed to transfer project to build server (error: {})", e);
                exit(-4);
            });
    }

    let local_toml_path = patched_cargo_file.path().to_string_lossy();
    let remote_toml_path = format!("{}:{}/Cargo.toml", build_server, build_path);
    log::debug!(
        "Copying Cargo.toml from {} to {}.",
        &local_toml_path,
        &remote_toml_path
    );
    let mut rsync_toml = Command::new("rsync");
    rsync_toml
        .arg("-vz")
        .arg(PROGRESS_FLAG)
        .arg(local_toml_path.to_string())
        .arg(remote_toml_path)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .stdin(Stdio::inherit())
        .output()
        .unwrap_or_else(|e| {
            error!("Failed to transfer project to build server (error: {})", e);
            exit(-4);
        });
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::patches::extract_patched_projects_and_patch_manifest;

    #[test]
    fn simple_modification_replaces_path() {
        let input = r#"
"hello" = 'toml!'
[patch.a]
a-crate = { path = "/some/prefix/a/src/a-crate" }
a-other-crate = { path = "/some/prefix/a/src/subfolder/a-other-crate" }
git-patched-crate = { git = "https://some-url/test/test" }
[patch.b]
b-crate = { path = "/some/prefix/b/src/b-crate" }
b-other-crate = { path = "/some/prefix/b/src/subfolder/b-other-crate" }
git-patched-crate = { git = "https://some-url/test/test" }
"#
        .to_string();
        let expect = r#"
"hello" = 'toml!'
[patch.a]
a-crate = { path = "../patches/a/src/a-crate" }
a-other-crate = { path = "../patches/a/src/subfolder/a-other-crate" }
git-patched-crate = { git = "https://some-url/test/test" }
[patch.b]
b-crate = { path = "../patches/b/src/b-crate" }
b-other-crate = { path = "../patches/b/src/subfolder/b-other-crate" }
git-patched-crate = { git = "https://some-url/test/test" }
"#
        .to_string();

        let result = extract_patched_projects_and_patch_manifest(input, |p| {
            if p.starts_with("/some/prefix/a") {
                return Ok(PathBuf::from("/some/prefix/a"));
            } else if p.starts_with("/some/prefix/b") {
                return Ok(PathBuf::from("/some/prefix/b"));
            }
            Err("Invalid Path".to_string())
        })
        .unwrap();
        assert_eq!(result.0.to_string(), expect);
    }
}
