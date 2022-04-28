use camino::Utf8PathBuf;
use cargo_metadata::Metadata;
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::{self, exit, Command, Stdio};
use std::str::FromStr;
use structopt::StructOpt;
use tempfile::NamedTempFile;
use toml::macros::IntoDeserializer;
use toml::Value;
use toml_edit::{Document, Formatted};

use log::{debug, error, warn};

const PROGRESS_FLAG: &str = "--info=progress2";

#[derive(StructOpt, Debug)]
#[structopt(name = "cargo-remote", bin_name = "cargo")]
enum Opts {
    #[structopt(name = "remote")]
    Remote {
        #[structopt(short = "r", long = "remote", help = "Remote ssh build server")]
        remote: Option<String>,

        #[structopt(
            short = "b",
            long = "build-env",
            help = "Set remote environment variables. RUST_BACKTRACE, CC, LIB, etc. ",
            default_value = "RUST_BACKTRACE=1"
        )]
        build_env: String,

        #[structopt(
            short = "d",
            long = "rustup-default",
            help = "Rustup default (stable|beta|nightly)",
            default_value = "stable"
        )]
        rustup_default: String,

        #[structopt(
            short = "e",
            long = "env",
            help = "Environment profile. default_value = /etc/profile",
            default_value = "/etc/profile"
        )]
        env: String,

        #[structopt(
            short = "c",
            long = "copy-back",
            help = "Transfer the target folder or specific file from that folder back to the local machine"
        )]
        copy_back: Option<Option<String>>,

        #[structopt(
            long = "no-copy-lock",
            help = "don't transfer the Cargo.lock file back to the local machine"
        )]
        no_copy_lock: bool,

        #[structopt(
            long = "manifest-path",
            help = "Path to the manifest to execute",
            default_value = "Cargo.toml",
            parse(from_os_str)
        )]
        manifest_path: PathBuf,

        #[structopt(
            short = "h",
            long = "transfer-hidden",
            help = "Transfer hidden files and directories to the build server"
        )]
        hidden: bool,

        #[structopt(help = "cargo command that will be executed remotely")]
        command: String,

        #[structopt(
            help = "cargo options and flags that will be applied remotely",
            name = "remote options"
        )]
        options: Vec<String>,

        #[structopt(help = "ignore patches", long = "ignore-patches")]
        ignore_patches: bool,
    },
}

/// Tries to parse the file [`config_path`]. Logs warnings and returns [`None`] if errors occur
/// during reading or parsing, [`Some(Value)`] otherwise.
fn config_from_file(config_path: &Utf8PathBuf) -> Option<Value> {
    let config_file = std::fs::read_to_string(config_path)
        .map_err(|e| {
            warn!(
                "Can't parse config file '{}' (error: {})",
                config_path.to_string(),
                e
            );
        })
        .ok()?;

    let value = config_file
        .parse::<Value>()
        .map_err(|e| {
            warn!(
                "Can't parse config file '{}' (error: {})",
                config_path.to_string(),
                e
            );
        })
        .ok()?;

    Some(value)
}

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

fn build_patch_projects(
    mut manifest: &mut Document,
    patches_folder: PathBuf,
) -> Option<Vec<PatchProject>> {
    let mut known_projects: Vec<PatchProject> = Vec::new();

    let patches = manifest["patch"].as_table_mut();
    if patches.is_none() {
        log::info!("No patches in project.");
        return None;
    }
    let patches = patches.unwrap();
    // Project like polkadot or substrate
    for top_level_item in patches.iter_mut() {
        let mut crate_level_item = top_level_item.1.as_table_mut();
        if crate_level_item.is_none() {
            continue;
        }
        let mut crate_level_item = crate_level_item.unwrap();
        for kv in crate_level_item.iter_mut() {
            let maybe_table =
                kv.1.as_value_mut()
                    .map(|f| f.as_inline_table_mut())
                    .flatten();
            if let Some(table) = maybe_table {
                if let Some(path) = table.get("path") {
                    let path = PathBuf::from(path.as_str().unwrap().clone());
                    let mut known_project = known_projects
                        .iter()
                        .find(|known_target| path.starts_with(&known_target.local_path))
                        .cloned();
                    match known_project {
                        None => {
                            // Project needs to be copied
                            let path_to_copy = locate_workspace_folder(path.clone())
                                .expect("Can not determine workspace path");
                            let name = path_to_copy.file_name().unwrap().to_owned();
                            let mut remote_folder = PathBuf::from("../patches"); //patches_folder.clone();
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
                            log::info!("Point {:?} to {:?}", kv.0.to_string(), new_path);
                            table
                                .insert("path", toml_edit::Value::from(new_path.to_str().unwrap()));
                        }

                        Some(patch_target) => {
                            let mut new_path = patch_target.remote_path.clone();
                            new_path
                                .push(path.strip_prefix(patch_target.local_path).expect("Jawoll"));
                            log::info!("Point {:?} to {:?}", kv.0.to_string(), new_path);
                            table
                                .insert("path", toml_edit::Value::from(new_path.to_str().unwrap()));
                        }
                    }
                } else {
                    log::debug!(
                        "Ignoring patched crate '{}', not path given.",
                        kv.0.to_string()
                    );
                }
            }
        }
    }
    log::info!("patches: {:?}", known_projects);
    Some(known_projects)
}

fn handle_patches(
    manifest_path: Utf8PathBuf,
    mut build_folder: PathBuf,
) -> Option<(NamedTempFile, Vec<PatchProject>)> {
    let config_file_string = std::fs::read_to_string(manifest_path)
        .ok()
        .expect("Shold work");
    let mut doc = config_file_string.parse::<Document>().expect("invalid doc");
    build_folder.push("patches");
    let maybe_patches = build_patch_projects(&mut doc, build_folder);

    maybe_patches.map(|patches| {
        let mut temp = NamedTempFile::new().expect("No tempfile for us");
        temp.write_all(doc.to_string().as_bytes())
            .expect("Unable to write file");
        (temp, patches)
    })
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

fn main() {
    simple_logger::SimpleLogger::from_env().init().unwrap();

    let Opts::Remote {
        remote,
        build_env,
        rustup_default,
        env,
        copy_back,
        no_copy_lock,
        manifest_path,
        hidden,
        command,
        options,
        ignore_patches,
    } = Opts::from_args();

    let mut metadata_cmd = cargo_metadata::MetadataCommand::new();
    metadata_cmd.manifest_path(manifest_path).no_deps();

    let project_metadata = match metadata_cmd.exec() {
        Ok(m) => m,
        Err(cargo_metadata::Error::CargoMetadata { stderr }) => {
            error!("Cargo Metadata execution failed:\n{}", stderr);
            exit(1)
        }
        Err(e) => {
            error!("Cargo Metadata failed:\n{:?}", e);
            exit(1)
        }
    };
    let project_dir = project_metadata.workspace_root.clone();
    debug!("Project dir: {:?}", project_dir);

    let mut manifest_path = project_dir.clone();
    manifest_path.push("Cargo.toml");
    log::info!("Manifest_path: {:?}", manifest_path);

    let project_name = project_metadata
        .packages
        .iter()
        .find(|p| p.manifest_path == manifest_path)
        .map_or_else(
            || {
                debug!("No metadata found. Setting the remote dir name like the local. Or use --manifest_path for execute");
                project_dir.file_name().unwrap()
            },
            |p| &p.name,
        );

    let build_path_folder = "~/remote-builds/";
    let build_path = format!("{}/{}/", build_path_folder, project_name);

    debug!("Project name: {:?}", project_name);
    let configs = vec![
        config_from_file(&project_dir.join(".cargo-remote.toml")),
        xdg::BaseDirectories::with_prefix("cargo-remote")
            .ok()
            .and_then(|base| base.find_config_file("cargo-remote.toml"))
            .and_then(|p| {
                config_from_file(
                    &Utf8PathBuf::from_path_buf(p).expect("valid Unicode path succeeded"),
                )
            }),
    ];

    let maybe_patches = handle_patches(manifest_path, PathBuf::from(build_path_folder));

    // TODO: move Opts::Remote fields into own type and implement complete_from_config(&mut self, config: &Value)
    let build_server = remote
        .or_else(|| {
            configs
                .into_iter()
                .flat_map(|config| config.and_then(|c| c["remote"].as_str().map(String::from)))
                .next()
        })
        .unwrap_or_else(|| {
            error!("No remote build server was defined (use config file or --remote flag)");
            exit(-3);
        });

    debug!("Transferring sources to build server.");
    // transfer project to build server
    let mut rsync_to = Command::new("rsync");
    rsync_to
        .arg("-a")
        .arg("-q")
        .arg("--delete")
        .arg("--compress")
        .arg(PROGRESS_FLAG)
        .arg("--exclude")
        .arg("target");

    if !hidden {
        rsync_to.arg("--exclude").arg(".*");
    }

    rsync_to
        .arg("--rsync-path")
        .arg("mkdir -p remote-builds && rsync")
        .arg(format!("{}/", project_dir.to_string()))
        .arg(format!("{}:{}", build_server, build_path))
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .stdin(Stdio::inherit())
        .output()
        .unwrap_or_else(|e| {
            error!("Failed to transfer project to build server (error: {})", e);
            exit(-4);
        });

    if let Some((patched_cargo_file, project_list)) = maybe_patches {
        copy_patches_to_remote(&build_path, &build_server, patched_cargo_file, project_list);
    }

    debug!("Build ENV: {:?}", build_env);
    debug!("Environment profile: {:?}", env);
    debug!("Build path: {:?}", build_path);
    let build_command = format!(
        "source {}; rustup default {}; cd {}; {} cargo {} {}",
        env,
        rustup_default,
        build_path,
        build_env,
        command,
        options.join(" ")
    );

    debug!("Starting build process.");
    let output = Command::new("ssh")
        .arg("-t")
        .arg(&build_server)
        .arg(build_command)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .stdin(Stdio::inherit())
        .output()
        .unwrap_or_else(|e| {
            error!("Failed to run cargo command remotely (error: {})", e);
            exit(-5);
        });

    if let Some(file_name) = copy_back {
        debug!("Transferring artifacts back to client.");
        let file_name = file_name.unwrap_or_else(String::new);
        Command::new("rsync")
            .arg("-a")
            .arg("-q")
            .arg("--delete")
            .arg("--compress")
            .arg(PROGRESS_FLAG)
            .arg(format!(
                "{}:{}target/{}",
                build_server, build_path, file_name
            ))
            .arg(format!("{}/target/{}", project_dir.to_string(), file_name))
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .stdin(Stdio::inherit())
            .output()
            .unwrap_or_else(|e| {
                error!(
                    "Failed to transfer target back to local machine (error: {})",
                    e
                );
                exit(-6);
            });
    }

    if !no_copy_lock {
        debug!("Transferring Cargo.lock file back to client.");
        Command::new("rsync")
            .arg("-a")
            .arg("-q")
            .arg("--delete")
            .arg("--compress")
            .arg(PROGRESS_FLAG)
            .arg(format!("{}:{}/Cargo.lock", build_server, build_path))
            .arg(format!("{}/Cargo.lock", project_dir.to_string()))
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .stdin(Stdio::inherit())
            .output()
            .unwrap_or_else(|e| {
                error!(
                    "Failed to transfer Cargo.lock back to local machine (error: {})",
                    e
                );
                exit(-7);
            });
    }

    if !output.status.success() {
        exit(output.status.code().unwrap_or(1))
    }
}
