use crate::copy_to_remote;
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use toml_edit::{Document, InlineTable};

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
    manifest_path: PathBuf,
    copy_hidden_files: bool,
    no_transfer_git: bool,
) -> Result<(), String> {
    let cargo_file_content = std::fs::read_to_string(&manifest_path).map_err(|err| {
        format!(
            "Unable to read cargo manifest at {}: {:?}",
            manifest_path.display(),
            err
        )
    })?;

    let maybe_patches =
        extract_patched_crates_and_adjust_toml(cargo_file_content, |p| locate_workspace_folder(p))?;

    if let Some((patched_cargo_doc, project_list)) = maybe_patches {
        copy_patches_to_remote(
            &build_path,
            &build_server,
            patched_cargo_doc,
            project_list,
            copy_hidden_files,
            no_transfer_git,
        )?;
    }
    Ok(())
}

fn locate_workspace_folder(mut crate_path: PathBuf) -> Result<PathBuf, String> {
    crate_path.push("Cargo.toml");
    let metadata_cmd = cargo_metadata::MetadataCommand::new()
        .manifest_path(&crate_path)
        .no_deps()
        .exec()
        .map_err(|err| {
            format!(
                "Unable to call cargo metadata on path {}: {:?}",
                crate_path.display(),
                err
            )
        })?;

    Ok(metadata_cmd.workspace_root.into_std_path_buf())
}

#[derive(Debug, Clone)]
struct PatchProject {
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

fn extract_patched_crates_and_adjust_toml<F: Fn(PathBuf) -> Result<PathBuf, String>>(
    manifest_content: String,
    locate_workspace: F,
) -> Result<Option<(Document, Vec<PatchProject>)>, String> {
    let mut manifest = manifest_content.parse::<Document>().map_err(|err| {
        format!(
            "Unable to parse Cargo.toml: {:?} content: {}",
            err, manifest_content
        )
    })?;
    let mut workspaces_to_copy: Vec<PatchProject> = Vec::new();

    // A list of inline tables like
    // { path = "/some/path" }
    let patched_paths: Option<Vec<&mut InlineTable>> =
        manifest["patch"].as_table_mut().map(|patch| {
            patch
                .iter_mut()
                .filter_map(|(_, crate_table)| crate_table.as_table_mut())
                .flat_map(|crate_table| {
                    crate_table
                        .iter_mut()
                        .filter_map(|(_, patch_table)| patch_table.as_inline_table_mut())
                })
                .collect()
        });

    let patched_paths = if let Some(p) = patched_paths {
        p
    } else {
        log::debug!("No patches in project.");
        return Ok(None);
    };

    for inline_crate_table in patched_paths {
        // We only act if there is a path given for a crate
        if let Some(path) = inline_crate_table.get("path") {
            let path = PathBuf::from(
                path.as_str()
                    .ok_or("Unable to get path from toml Value")?
            );

            // Check if the current crate is located in a subfolder of a workspace we
            // already know.
            let known_workspace = workspaces_to_copy
                .iter()
                .find(|known_target| path.starts_with(&known_target.local_path));
            match known_workspace {
                None => {
                    // Project is unknown and needs to be copied
                    let workspace_folder_path = locate_workspace(path.clone()).map_err(|err| {
                        format!(
                            "Can not determine workspace path for project at {}: {}",
                            &path.display(),
                            err
                        )
                    })?;
                    let workspace_folder_name = workspace_folder_path
                        .file_name()
                        .ok_or("Unable to get file name from workspace folder.")?
                        .to_owned();

                    let mut remote_folder = PathBuf::from("../");
                    remote_folder.push(workspace_folder_name.clone());

                    log::debug!(
                        "Found referenced project '{}', will copy to '{}'",
                        &workspace_folder_path.display(),
                        &remote_folder.display()
                    );

                    // Add workspace to the list so it will be rsynced to the remote server
                    workspaces_to_copy.push(PatchProject::new(
                        workspace_folder_name,
                        workspace_folder_path.clone(),
                        remote_folder.clone(),
                    ));

                    // Build a new path for the crate relative to the workspace folder
                    remote_folder.push(path.strip_prefix(workspace_folder_path).map_err(
                        |err| format!("Unable to construct remote folder path: {}", err),
                    )?);

                    inline_crate_table.insert(
                        "path",
                        toml_edit::Value::from(remote_folder.to_str().unwrap()),
                    );
                }

                Some(patch_target) => {
                    let mut new_path = patch_target.remote_path.clone();
                    new_path.push(path.strip_prefix(&patch_target.local_path).map_err(|err| {
                        format!("Unable to construct remote folder path: {}", err)
                    })?);

                    inline_crate_table.insert(
                        "path",
                        toml_edit::Value::from(
                            new_path.to_str().ok_or("Unable to modify path in toml.")?,
                        ),
                    );
                }
            }
        }
    }
    Ok(Some((manifest, workspaces_to_copy)))
}

fn copy_patches_to_remote(
    build_path: &String,
    build_server: &String,
    patched_cargo_doc: Document,
    projects_to_copy: Vec<PatchProject>,
    copy_hidden_files: bool,
    no_transfer_git: bool,
) -> Result<(), String> {
    for patch_operation in projects_to_copy.iter() {
        let local_proj_path = format!("{}/", patch_operation.local_path.display());
        let remote_proj_path = format!(
            "{}:remote-builds/{}",
            build_server,
            patch_operation.name.to_string_lossy()
        );
        log::debug!(
            "Copying workspace {:?} from {} to {}.",
            patch_operation.name,
            &local_proj_path,
            &remote_proj_path
        );
        // transfer project to build server
        copy_to_remote(&local_proj_path, &remote_proj_path, copy_hidden_files, no_transfer_git).map_err(|err| {
            format!(
                "Failed to transfer project {} to build server (error: {})",
                local_proj_path, err
            )
        })?;
    }

    let remote_toml_path = format!("{}/Cargo.toml", build_path);
    log::debug!("Writing adjusted Cargo.toml to {}.", &remote_toml_path);
    let mut child = Command::new("ssh")
        .args(&[build_server, "-T", "cat > ", &remote_toml_path])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();

    child
        .stdin
        .take()
        .unwrap()
        .write_all(patched_cargo_doc.to_string().as_bytes())
        .map_err(|err| format!("Unable to copy patched Cargo.toml to remote: {}", err))?;

    child
        .wait_with_output()
        .map_err(|err| format!("Unable to copy patched Cargo.toml to remote: {}", err))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::patches::extract_patched_crates_and_adjust_toml;

    #[test]
    fn simple_modification_replaces_path() {
        let input = r#"
"hello" = 'toml!'
[patch.a]
a-crate = { path = "/some/prefix/a/src/a-crate" }
a-other-crate = { path = "/some/prefix/a/src/subfolder/a-other-crate" }
git-patched-crate = { git = "https://some-url/test/test" }
a-crate-different-folder = { path = "/some/prefix/a-2/src/subfolder/a-crate-different-folder" }
[patch.b]
b-crate = { path = "/some/prefix/b/src/b-crate" }
b-other-crate = { path = "/some/prefix/b/src/subfolder/b-other-crate" }
git-patched-crate = { git = "https://some-url/test/test" }
"#
        .to_string();
        let expect = r#"
"hello" = 'toml!'
[patch.a]
a-crate = { path = "../a/src/a-crate" }
a-other-crate = { path = "../a/src/subfolder/a-other-crate" }
git-patched-crate = { git = "https://some-url/test/test" }
a-crate-different-folder = { path = "../a-2/src/subfolder/a-crate-different-folder" }
[patch.b]
b-crate = { path = "../b/src/b-crate" }
b-other-crate = { path = "../b/src/subfolder/b-other-crate" }
git-patched-crate = { git = "https://some-url/test/test" }
"#
        .to_string();

        let result = extract_patched_crates_and_adjust_toml(input, |p| {
            if p.starts_with("/some/prefix/a") {
                return Ok(PathBuf::from("/some/prefix/a"));
            } else if p.starts_with("/some/prefix/a-2") {
                return Ok(PathBuf::from("/some/prefix/a-2"));
            } else if p.starts_with("/some/prefix/b") {
                return Ok(PathBuf::from("/some/prefix/b"));
            }
            Err("Invalid Path".to_string())
        })
        .expect("Toml patching failed")
        .unwrap();
        assert_eq!(result.0.to_string(), expect);
    }
}
