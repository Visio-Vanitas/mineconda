use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    Client,
    Server,
    Both,
}

impl RunMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Server => "server",
            Self::Both => "both",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoaderHint {
    Fabric,
    Forge,
    NeoForge,
    Quilt,
}

impl LoaderHint {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fabric => "fabric",
            Self::Forge => "forge",
            Self::NeoForge => "neoforge",
            Self::Quilt => "quilt",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RunRequest {
    pub root: PathBuf,
    pub java_bin: String,
    pub memory: String,
    pub dry_run: bool,
    pub extra_jvm_args: Vec<String>,
    pub username: String,
    pub instance_name: String,
    pub mode: RunMode,
    pub loader_hint: Option<LoaderHint>,
    pub client_launcher_jar: Option<PathBuf>,
    pub server_launcher_jar: Option<PathBuf>,
    pub package_paths: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct RunPlan {
    pub mode: RunMode,
    pub launches: Vec<LaunchPlan>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchRole {
    Client,
    Server,
}

impl LaunchRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Server => "server",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LaunchPlan {
    pub role: LaunchRole,
    pub program: String,
    pub args: Vec<String>,
    pub instance_dir: PathBuf,
    pub working_dir: PathBuf,
}

#[derive(Debug, Clone)]
enum ServerLaunchTarget {
    Jar(PathBuf),
    ArgsFile(PathBuf),
}

pub fn build_run_plan(request: &RunRequest) -> Result<RunPlan> {
    let launches = match request.mode {
        RunMode::Client => vec![build_client_launch(request, false)?],
        RunMode::Server => vec![build_server_launch(request, false)?],
        RunMode::Both => vec![
            build_server_launch(request, true)?,
            build_client_launch(request, true)?,
        ],
    };

    Ok(RunPlan {
        mode: request.mode,
        launches,
    })
}

pub fn run_game_instance(request: &RunRequest) -> Result<()> {
    let plan = build_run_plan(request)?;
    for launch in &plan.launches {
        fs::create_dir_all(&launch.instance_dir)
            .with_context(|| format!("failed to create {}", launch.instance_dir.display()))?;
    }
    prepare_launch_inputs(request, &plan)?;

    if request.dry_run {
        println!("mode={}", plan.mode.as_str());
        println!("instance={}", request.instance_name);
        for launch in &plan.launches {
            println!(
                "dry-run [{}]: {} {}",
                launch.role.as_str(),
                launch.program,
                launch.args.join(" ")
            );
        }
        return Ok(());
    }

    match plan.mode {
        RunMode::Client | RunMode::Server => {
            let launch = plan
                .launches
                .first()
                .context("run plan did not contain launch command")?;
            let status = run_blocking(launch)?;
            if !status.success() {
                bail!(
                    "{} process exited with status {}",
                    launch.role.as_str(),
                    status
                );
            }
        }
        RunMode::Both => {
            let server = plan
                .launches
                .iter()
                .find(|launch| launch.role == LaunchRole::Server)
                .context("missing server launch plan for both mode")?;
            let client = plan
                .launches
                .iter()
                .find(|launch| launch.role == LaunchRole::Client)
                .context("missing client launch plan for both mode")?;

            let mut server_child = spawn_non_blocking(server)?;
            let client_status = run_blocking(client)?;
            let _ = server_child.kill();
            let _ = server_child.wait();

            if !client_status.success() {
                bail!("client process exited with status {client_status}");
            }
        }
    }

    Ok(())
}

const STAGED_DIRS: &[&str] = &[
    "mods",
    "config",
    "defaultconfigs",
    "kubejs",
    "resourcepacks",
    "shaderpacks",
    "datapacks",
];

const STAGED_FILES: &[&str] = &["eula.txt", "server.properties"];

fn prepare_launch_inputs(request: &RunRequest, plan: &RunPlan) -> Result<()> {
    let mut prepared = Vec::new();
    for launch in &plan.launches {
        if prepared.contains(&launch.instance_dir) {
            continue;
        }
        stage_project_inputs(
            &request.root,
            &launch.instance_dir,
            request.package_paths.as_deref(),
        )?;
        prepared.push(launch.instance_dir.clone());
    }
    Ok(())
}

fn stage_project_inputs(
    root: &Path,
    instance_dir: &Path,
    package_paths: Option<&[String]>,
) -> Result<()> {
    for relative in STAGED_DIRS {
        if package_paths.is_some() && *relative == "mods" {
            continue;
        }
        stage_selected_path(root, instance_dir, relative)?;
    }
    for relative in STAGED_FILES {
        stage_selected_path(root, instance_dir, relative)?;
    }
    if let Some(package_paths) = package_paths {
        stage_package_paths(root, instance_dir, package_paths)?;
    }
    Ok(())
}

fn stage_package_paths(root: &Path, instance_dir: &Path, package_paths: &[String]) -> Result<()> {
    let mut paths = package_paths.to_vec();
    paths.sort();
    paths.dedup();
    for relative in paths {
        stage_selected_path(root, instance_dir, &relative)?;
    }
    Ok(())
}

fn stage_selected_path(root: &Path, instance_dir: &Path, relative: &str) -> Result<()> {
    let source = root.join(relative);
    if !source.exists() {
        return Ok(());
    }

    let target = instance_dir.join(relative);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    if try_stage_symlink(&source, &target).is_ok() {
        return Ok(());
    }

    if source.is_dir() {
        copy_dir_all(&source, &target)?;
    } else {
        copy_file_replace(&source, &target)?;
    }
    Ok(())
}

fn try_stage_symlink(source: &Path, target: &Path) -> Result<()> {
    if target.exists() || target.is_symlink() {
        remove_path(target)?;
    }
    create_symlink(source, target)
}

#[cfg(unix)]
fn create_symlink(source: &Path, target: &Path) -> Result<()> {
    std::os::unix::fs::symlink(source, target).with_context(|| {
        format!(
            "failed to create symlink {} -> {}",
            target.display(),
            source.display()
        )
    })
}

#[cfg(windows)]
fn create_symlink(source: &Path, target: &Path) -> Result<()> {
    if source.is_dir() {
        std::os::windows::fs::symlink_dir(source, target).with_context(|| {
            format!(
                "failed to create symlink {} -> {}",
                target.display(),
                source.display()
            )
        })
    } else {
        std::os::windows::fs::symlink_file(source, target).with_context(|| {
            format!(
                "failed to create symlink {} -> {}",
                target.display(),
                source.display()
            )
        })
    }
}

fn remove_path(path: &Path) -> Result<()> {
    if path.is_symlink() || path.is_file() {
        fs::remove_file(path).with_context(|| format!("failed to remove {}", path.display()))?;
    } else if path.is_dir() {
        fs::remove_dir_all(path).with_context(|| format!("failed to remove {}", path.display()))?;
    }
    Ok(())
}

fn copy_dir_all(source: &Path, target: &Path) -> Result<()> {
    if target.exists() {
        remove_path(target)?;
    }
    fs::create_dir_all(target).with_context(|| format!("failed to create {}", target.display()))?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        if source_path.is_dir() {
            copy_dir_all(&source_path, &target_path)?;
        } else {
            copy_file_replace(&source_path, &target_path)?;
        }
    }
    Ok(())
}

fn copy_file_replace(source: &Path, target: &Path) -> Result<()> {
    if target.exists() || target.is_symlink() {
        remove_path(target)?;
    }
    fs::copy(source, target).with_context(|| {
        format!(
            "failed to copy {} to {}",
            source.display(),
            target.display()
        )
    })?;
    Ok(())
}

fn build_client_launch(request: &RunRequest, with_suffix: bool) -> Result<LaunchPlan> {
    let launcher_jar = select_client_launcher_jar(request)?;
    let instance_name = if with_suffix {
        format!("{}-client", request.instance_name)
    } else {
        request.instance_name.clone()
    };
    let instance_dir = request
        .root
        .join(".mineconda")
        .join("instances")
        .join(instance_name);
    let assets_dir = instance_dir.join("assets");

    let mut args = vec![
        format!("-Xms{}", request.memory),
        format!("-Xmx{}", request.memory),
        "-Dmineconda.mode=dev-client".to_string(),
        "-Dfabric.development=true".to_string(),
        format!("-Dminecraft.gamedir={}", instance_dir.display()),
    ];
    args.extend(request.extra_jvm_args.clone());
    args.push("-jar".to_string());
    args.push(launcher_jar.display().to_string());
    args.push("--username".to_string());
    args.push(request.username.clone());
    args.push("--version".to_string());
    args.push("mineconda-dev".to_string());
    args.push("--gameDir".to_string());
    args.push(instance_dir.display().to_string());
    args.push("--assetsDir".to_string());
    args.push(assets_dir.display().to_string());
    args.push("--accessToken".to_string());
    args.push("0".to_string());
    args.push("--userType".to_string());
    args.push("legacy".to_string());
    args.push("--versionType".to_string());
    args.push("mineconda-dev".to_string());

    Ok(LaunchPlan {
        role: LaunchRole::Client,
        program: request.java_bin.clone(),
        args,
        working_dir: instance_dir.clone(),
        instance_dir,
    })
}

fn build_server_launch(request: &RunRequest, with_suffix: bool) -> Result<LaunchPlan> {
    let launcher = select_server_launcher(request)?;
    let instance_name = if with_suffix {
        format!("{}-server", request.instance_name)
    } else {
        request.instance_name.clone()
    };
    let instance_dir = request
        .root
        .join(".mineconda")
        .join("instances")
        .join(instance_name);

    let mut args = vec![
        format!("-Xms{}", request.memory),
        format!("-Xmx{}", request.memory),
        "-Dmineconda.mode=dev-server".to_string(),
        "-Dfabric.development=true".to_string(),
    ];
    args.extend(request.extra_jvm_args.clone());
    match launcher {
        ServerLaunchTarget::Jar(path) => {
            args.push("-jar".to_string());
            args.push(path.display().to_string());
            args.push("nogui".to_string());
        }
        ServerLaunchTarget::ArgsFile(path) => {
            let user_jvm_args = instance_dir.join("user_jvm_args.txt");
            if user_jvm_args.exists() {
                args.push(format!("@{}", user_jvm_args.display()));
            }
            args.push(format!("@{}", path.display()));
            args.push("nogui".to_string());
        }
    }

    Ok(LaunchPlan {
        role: LaunchRole::Server,
        program: request.java_bin.clone(),
        args,
        working_dir: instance_dir.clone(),
        instance_dir,
    })
}

fn select_client_launcher_jar(request: &RunRequest) -> Result<PathBuf> {
    if let Some(path) = &request.client_launcher_jar {
        if !path.exists() {
            bail!("client launcher jar not found at {}", path.display());
        }
        return Ok(path.clone());
    }

    let candidates =
        build_launcher_candidates(&request.root, LaunchRole::Client, request.loader_hint);
    for candidate in &candidates {
        if candidate.exists() {
            return Ok(candidate.clone());
        }
    }

    let checked = candidates
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let loader = request
        .loader_hint
        .map(|value| format!(" (loader={})", value.as_str()))
        .unwrap_or_default();
    bail!("no client launcher jar found{loader}. checked: {checked}")
}

fn select_server_launcher(request: &RunRequest) -> Result<ServerLaunchTarget> {
    if let Some(path) = &request.server_launcher_jar {
        if !path.exists() {
            bail!("server launcher jar not found at {}", path.display());
        }
        return server_launch_target(path);
    }

    let candidates =
        build_launcher_candidates(&request.root, LaunchRole::Server, request.loader_hint);
    for candidate in &candidates {
        if candidate.exists() {
            return server_launch_target(candidate);
        }
    }

    let checked = candidates
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let loader = request
        .loader_hint
        .map(|value| format!(" (loader={})", value.as_str()))
        .unwrap_or_default();
    bail!("no server launcher jar found{loader}. checked: {checked}")
}

fn server_launch_target(path: &Path) -> Result<ServerLaunchTarget> {
    if path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("txt"))
    {
        return Ok(ServerLaunchTarget::ArgsFile(path.to_path_buf()));
    }
    Ok(ServerLaunchTarget::Jar(path.to_path_buf()))
}

fn build_launcher_candidates(
    root: &Path,
    role: LaunchRole,
    loader: Option<LoaderHint>,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let known_names = known_launcher_file_names(role, loader);
    let dev_root = root.join(".mineconda/dev");

    for name in &known_names {
        push_unique_path(&mut candidates, dev_root.join(name));
    }
    match role {
        LaunchRole::Client => push_unique_path(&mut candidates, dev_root.join("launcher.jar")),
        LaunchRole::Server => {
            push_unique_path(&mut candidates, dev_root.join("server-launcher.jar"))
        }
    }

    for name in &known_names {
        push_unique_path(&mut candidates, root.join(name));
    }
    match role {
        LaunchRole::Client => push_unique_path(&mut candidates, root.join("launcher.jar")),
        LaunchRole::Server => push_unique_path(&mut candidates, root.join("server.jar")),
    }

    candidates
}

fn known_launcher_file_names(role: LaunchRole, loader: Option<LoaderHint>) -> Vec<&'static str> {
    match role {
        LaunchRole::Client => {
            prioritized_names(loader, CLIENT_LAUNCHER_NAMES, loader_client_preferred)
        }
        LaunchRole::Server => {
            prioritized_names(loader, SERVER_LAUNCHER_NAMES, loader_server_preferred)
        }
    }
}

fn prioritized_names(
    loader: Option<LoaderHint>,
    all_names: &[&'static str],
    preferred_fn: fn(LoaderHint) -> &'static [&'static str],
) -> Vec<&'static str> {
    let mut out = Vec::new();
    if let Some(loader) = loader {
        for name in preferred_fn(loader) {
            if !out.contains(name) {
                out.push(*name);
            }
        }
    }
    for name in all_names {
        if !out.contains(name) {
            out.push(*name);
        }
    }
    out
}

fn loader_client_preferred(loader: LoaderHint) -> &'static [&'static str] {
    match loader {
        LoaderHint::Fabric => &["fabric-client-launch.jar", "fabric-client.jar"],
        LoaderHint::Forge => &["forge-client-launch.jar", "forge-client.jar"],
        LoaderHint::NeoForge => &[
            "neoforge-client-launch.jar",
            "neoforge-client.jar",
            "forge-client-launch.jar",
            "forge-client.jar",
        ],
        LoaderHint::Quilt => &[
            "quilt-client-launch.jar",
            "quilt-client.jar",
            "fabric-client-launch.jar",
            "fabric-client.jar",
        ],
    }
}

fn loader_server_preferred(loader: LoaderHint) -> &'static [&'static str] {
    match loader {
        LoaderHint::Fabric => &["fabric-server-launch.jar", "fabric-server.jar"],
        LoaderHint::Forge => &["forge-server-launch.jar", "forge-server.jar"],
        LoaderHint::NeoForge => &[
            "neoforge-server-launch.jar",
            "neoforge-server.jar",
            "forge-server-launch.jar",
            "forge-server.jar",
        ],
        LoaderHint::Quilt => &[
            "quilt-server-launch.jar",
            "quilt-server.jar",
            "fabric-server-launch.jar",
            "fabric-server.jar",
        ],
    }
}

fn push_unique_path(out: &mut Vec<PathBuf>, candidate: PathBuf) {
    if !out.contains(&candidate) {
        out.push(candidate);
    }
}

const CLIENT_LAUNCHER_NAMES: &[&str] = &[
    "fabric-client-launch.jar",
    "forge-client-launch.jar",
    "neoforge-client-launch.jar",
    "quilt-client-launch.jar",
    "fabric-client.jar",
    "forge-client.jar",
    "neoforge-client.jar",
    "quilt-client.jar",
    "minecraft-client.jar",
];

const SERVER_LAUNCHER_NAMES: &[&str] = &[
    "fabric-server-launch.jar",
    "forge-server-launch.jar",
    "neoforge-server-launch.jar",
    "quilt-server-launch.jar",
    "fabric-server.jar",
    "forge-server.jar",
    "neoforge-server.jar",
    "quilt-server.jar",
    "minecraft-server.jar",
];

fn run_blocking(launch: &LaunchPlan) -> Result<ExitStatus> {
    Command::new(&launch.program)
        .args(&launch.args)
        .current_dir(&launch.working_dir)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to start {}", launch.role.as_str()))
}

fn spawn_non_blocking(launch: &LaunchPlan) -> Result<std::process::Child> {
    Command::new(&launch.program)
        .args(&launch.args)
        .current_dir(&launch.working_dir)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to start {}", launch.role.as_str()))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    struct TempWorkspace {
        root: PathBuf,
    }

    impl TempWorkspace {
        fn new(tag: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock should be after unix epoch")
                .as_nanos();
            let root = std::env::temp_dir().join(format!("mineconda-runner-{tag}-{unique}"));
            fs::create_dir_all(&root).expect("failed to create temp workspace");
            Self { root }
        }
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn write_file(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("failed to create parent directory");
        }
        fs::write(path, b"fake jar\n").expect("failed to write fake jar");
    }

    fn request(root: &Path, mode: RunMode, loader_hint: Option<LoaderHint>) -> RunRequest {
        RunRequest {
            root: root.to_path_buf(),
            java_bin: "java".to_string(),
            memory: "4G".to_string(),
            dry_run: true,
            extra_jvm_args: Vec::new(),
            username: "tester".to_string(),
            instance_name: "dev".to_string(),
            mode,
            loader_hint,
            client_launcher_jar: None,
            server_launcher_jar: None,
            package_paths: None,
        }
    }

    fn launcher_from_plan(plan: &RunPlan, role: LaunchRole) -> String {
        let launch = plan
            .launches
            .iter()
            .find(|entry| entry.role == role)
            .expect("expected launch role in run plan");
        let jar_index = launch
            .args
            .iter()
            .position(|arg| arg == "-jar")
            .expect("run args should include -jar");
        launch
            .args
            .get(jar_index + 1)
            .expect("run args should include jar path")
            .to_string()
    }

    #[test]
    fn neoforge_loader_prefers_loader_specific_launcher_names() {
        let ws = TempWorkspace::new("neoforge-priority");
        write_file(&ws.root.join(".mineconda/dev/launcher.jar"));
        write_file(&ws.root.join(".mineconda/dev/server-launcher.jar"));
        write_file(&ws.root.join(".mineconda/dev/neoforge-client-launch.jar"));
        write_file(&ws.root.join(".mineconda/dev/neoforge-server-launch.jar"));

        let plan = build_run_plan(&request(
            &ws.root,
            RunMode::Both,
            Some(LoaderHint::NeoForge),
        ))
        .expect("failed to build run plan");

        assert_eq!(
            launcher_from_plan(&plan, LaunchRole::Client),
            ws.root
                .join(".mineconda/dev/neoforge-client-launch.jar")
                .display()
                .to_string()
        );
        assert_eq!(
            launcher_from_plan(&plan, LaunchRole::Server),
            ws.root
                .join(".mineconda/dev/neoforge-server-launch.jar")
                .display()
                .to_string()
        );
    }

    #[test]
    fn explicit_launcher_path_overrides_loader_hint() {
        let ws = TempWorkspace::new("explicit-override");
        write_file(&ws.root.join(".mineconda/dev/neoforge-client-launch.jar"));
        write_file(&ws.root.join("custom-client.jar"));

        let mut req = request(&ws.root, RunMode::Client, Some(LoaderHint::NeoForge));
        req.client_launcher_jar = Some(ws.root.join("custom-client.jar"));
        let plan = build_run_plan(&req).expect("failed to build run plan");

        assert_eq!(
            launcher_from_plan(&plan, LaunchRole::Client),
            ws.root.join("custom-client.jar").display().to_string()
        );
    }

    #[test]
    fn both_mode_uses_distinct_instance_directories() {
        let ws = TempWorkspace::new("both-instance-dirs");
        write_file(&ws.root.join(".mineconda/dev/neoforge-client-launch.jar"));
        write_file(&ws.root.join(".mineconda/dev/neoforge-server-launch.jar"));

        let plan = build_run_plan(&request(
            &ws.root,
            RunMode::Both,
            Some(LoaderHint::NeoForge),
        ))
        .expect("failed to build run plan");

        let client = plan
            .launches
            .iter()
            .find(|entry| entry.role == LaunchRole::Client)
            .expect("client plan");
        let server = plan
            .launches
            .iter()
            .find(|entry| entry.role == LaunchRole::Server)
            .expect("server plan");

        assert!(client.instance_dir.ends_with("dev-client"));
        assert!(server.instance_dir.ends_with("dev-server"));
        assert_ne!(client.instance_dir, server.instance_dir);
    }

    #[test]
    fn prepare_launch_inputs_stages_project_content_into_instance() {
        let ws = TempWorkspace::new("stage-project-content");
        write_file(&ws.root.join(".mineconda/dev/neoforge-server-launch.jar"));
        write_file(&ws.root.join("mods/example.jar"));
        write_file(&ws.root.join("config/example.toml"));
        fs::write(ws.root.join("eula.txt"), "eula=true\n").expect("failed to write eula");
        fs::write(ws.root.join("server.properties"), "motd=mineconda\n")
            .expect("failed to write server.properties");

        let request = request(&ws.root, RunMode::Server, Some(LoaderHint::NeoForge));
        let plan = build_run_plan(&request).expect("failed to build run plan");
        prepare_launch_inputs(&request, &plan).expect("failed to prepare launch inputs");

        let server = plan
            .launches
            .iter()
            .find(|entry| entry.role == LaunchRole::Server)
            .expect("server plan");

        assert!(server.instance_dir.join("mods/example.jar").exists());
        assert!(server.instance_dir.join("config/example.toml").exists());
        assert_eq!(
            fs::read_to_string(server.instance_dir.join("eula.txt")).expect("read eula"),
            "eula=true\n"
        );
        assert_eq!(
            fs::read_to_string(server.instance_dir.join("server.properties"))
                .expect("read server.properties"),
            "motd=mineconda\n"
        );
    }

    #[test]
    fn prepare_launch_inputs_filters_package_paths_when_requested() {
        let ws = TempWorkspace::new("stage-filtered-packages");
        write_file(&ws.root.join(".mineconda/dev/neoforge-server-launch.jar"));
        write_file(&ws.root.join("mods/client-only.jar"));
        write_file(&ws.root.join("mods/server-only.jar"));
        write_file(&ws.root.join("config/example.toml"));

        let mut request = request(&ws.root, RunMode::Server, Some(LoaderHint::NeoForge));
        request.package_paths = Some(vec!["mods/client-only.jar".to_string()]);
        let plan = build_run_plan(&request).expect("failed to build run plan");
        prepare_launch_inputs(&request, &plan).expect("failed to prepare launch inputs");

        let server = plan
            .launches
            .iter()
            .find(|entry| entry.role == LaunchRole::Server)
            .expect("server plan");

        assert!(server.instance_dir.join("mods/client-only.jar").exists());
        assert!(!server.instance_dir.join("mods/server-only.jar").exists());
        assert!(server.instance_dir.join("config/example.toml").exists());
    }

    #[test]
    fn explicit_server_unix_args_file_uses_managed_java_argfile_mode() {
        let ws = TempWorkspace::new("server-unix-args");
        let instance_dir = ws.root.join(".mineconda/instances/dev");
        write_file(&instance_dir.join("libraries/net/neoforged/neoforge/21.1.211/unix_args.txt"));
        fs::write(instance_dir.join("user_jvm_args.txt"), "# optional\n")
            .expect("failed to write user_jvm_args");

        let mut req = request(&ws.root, RunMode::Server, Some(LoaderHint::NeoForge));
        req.server_launcher_jar =
            Some(instance_dir.join("libraries/net/neoforged/neoforge/21.1.211/unix_args.txt"));

        let plan = build_run_plan(&req).expect("failed to build run plan");
        let server = plan
            .launches
            .iter()
            .find(|entry| entry.role == LaunchRole::Server)
            .expect("server plan");

        assert_eq!(server.program, "java");
        assert!(server.args.iter().any(|arg| {
            arg.as_str() == format!("@{}", instance_dir.join("user_jvm_args.txt").display())
        }));
        assert!(server.args.iter().any(|arg| {
            arg.as_str()
                == format!(
                    "@{}",
                    instance_dir
                        .join("libraries/net/neoforged/neoforge/21.1.211/unix_args.txt")
                        .display()
                )
        }));
        assert!(server.args.iter().any(|arg| arg == "nogui"));
    }
}
