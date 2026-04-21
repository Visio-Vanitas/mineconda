use crate::command::search::to_run_loader_hint;
use crate::*;
use mineconda_runner::build_run_plan;

struct PreparedRunRequest {
    request: RunRequest,
    groups: Vec<String>,
    profiles: Vec<String>,
}

pub(crate) fn cmd_run(
    root: &Path,
    args: RunCommandArgs,
    profiles: &[String],
    workspace: Option<&WorkspaceConfig>,
) -> Result<()> {
    let prepared = build_run_request(root, args, profiles, workspace)?;
    run_game_instance(&prepared.request)?;
    Ok(())
}

pub(crate) fn build_run_report(
    root: &Path,
    args: RunCommandArgs,
    profiles: &[String],
    workspace: Option<&WorkspaceConfig>,
) -> Result<CommandReport> {
    let prepared = build_run_request(root, args, profiles, workspace)?;
    let plan = build_run_plan(&prepared.request)?;
    let mut lines = vec![
        format!("mode={}", plan.mode.as_str()),
        format!("instance={}", prepared.request.instance_name),
    ];
    for launch in &plan.launches {
        lines.push(format!(
            "dry-run [{}]: {} {}",
            launch.role.as_str(),
            launch.program,
            launch.args.join(" ")
        ));
    }
    Ok(CommandReport {
        output: format!("{}\n", lines.join("\n")),
        exit_code: 0,
    })
}

pub(crate) fn build_run_json_report(
    root: &Path,
    args: RunCommandArgs,
    profiles: &[String],
    workspace: Option<&WorkspaceConfig>,
) -> Result<RunJsonReport> {
    let prepared = build_run_request(root, args, profiles, workspace)?;
    let plan = build_run_plan(&prepared.request)?;
    let mut messages = Vec::new();
    if !prepared.profiles.is_empty() {
        messages.push(format!("profiles={}", prepared.profiles.join(",")));
    }
    if !prepared.groups.is_empty() {
        messages.push(format!("groups={}", format_group_list(&prepared.groups)));
    }
    Ok(RunJsonReport {
        command: "run",
        groups: prepared.groups,
        profiles: prepared.profiles,
        mode: plan.mode.as_str().to_string(),
        instance: prepared.request.instance_name.clone(),
        dry_run: prepared.request.dry_run,
        java: prepared.request.java_bin.clone(),
        memory: prepared.request.memory.clone(),
        summary: RunJsonSummary {
            exit_code: 0,
            launches: plan.launches.len(),
        },
        launches: plan
            .launches
            .iter()
            .map(|launch| RunJsonLaunch {
                role: launch.role.as_str().to_string(),
                program: launch.program.clone(),
                args: launch.args.clone(),
            })
            .collect(),
        messages,
    })
}

fn build_run_request(
    root: &Path,
    args: RunCommandArgs,
    profiles: &[String],
    workspace: Option<&WorkspaceConfig>,
) -> Result<PreparedRunRequest> {
    let RunCommandArgs {
        dry_run,
        java,
        memory,
        jvm_args,
        mode,
        username,
        instance,
        launcher_jar,
        server_jar,
        groups,
        all_groups,
    } = args;

    let manifest = load_manifest_optional(root)?;
    let profile_names = normalized_profile_names(profiles)?;
    let defaults = manifest
        .as_ref()
        .map(|m| m.server.clone())
        .unwrap_or_else(ServerProfile::default);
    let runtime = manifest.as_ref().map(|m| m.runtime.clone());
    let loader_hint = manifest
        .as_ref()
        .map(|manifest| to_run_loader_hint(manifest.project.loader.kind));
    let (package_paths, active_groups) = if let Some(manifest) = manifest.as_ref() {
        let active_groups =
            activation_groups_with_profiles(manifest, workspace, &groups, all_groups, profiles)?;
        if manifest.groups.is_empty() && active_groups.len() == 1 {
            (None, active_groups)
        } else {
            let lock = load_lockfile_required(root)?;
            ensure_lock_covers_groups(manifest, &lock, &active_groups)?;
            (
                Some(
                    filtered_lockfile(&lock, &active_groups)
                        .packages
                        .into_iter()
                        .map(|package| package.install_path_or_default())
                        .collect(),
                ),
                active_groups,
            )
        }
    } else if !groups.is_empty() || all_groups || !profiles.is_empty() {
        bail!("group/profile filters require a manifest");
    } else {
        (None, BTreeSet::new())
    };
    let extra_jvm_args = if jvm_args.is_empty() {
        defaults.jvm_args.clone()
    } else {
        jvm_args
    };
    let java_bin = resolve_java_for_run(java, &defaults, runtime.as_ref())?;
    let mode = mode.to_core();
    let launcher_jar = launcher_jar.map(|path| {
        if path.is_absolute() {
            path
        } else {
            root.join(path)
        }
    });
    let server_jar = server_jar.map(|path| {
        if path.is_absolute() {
            path
        } else {
            root.join(path)
        }
    });

    let (client_launcher_jar, server_launcher_jar) = match mode {
        RunMode::Client => (launcher_jar, None),
        RunMode::Server => (None, server_jar.or(launcher_jar)),
        RunMode::Both => (launcher_jar, server_jar),
    };

    let request = RunRequest {
        root: root.to_path_buf(),
        java_bin,
        memory: memory.unwrap_or(defaults.memory),
        dry_run,
        extra_jvm_args,
        username,
        instance_name: instance,
        mode,
        loader_hint,
        client_launcher_jar,
        server_launcher_jar,
        package_paths,
    };
    Ok(PreparedRunRequest {
        request,
        groups: active_groups.into_iter().collect(),
        profiles: profile_names,
    })
}

pub(crate) fn resolve_java_for_run(
    java_override: Option<String>,
    server: &ServerProfile,
    runtime: Option<&RuntimeProfile>,
) -> Result<String> {
    if let Some(java) = java_override {
        return Ok(java);
    }

    if server.java != "java" {
        return Ok(server.java.clone());
    }

    if let Some(runtime) = runtime {
        let java_bin = resolve_java_binary(&runtime.java, runtime.provider, runtime.auto_install)?;
        return Ok(java_bin.display().to_string());
    }

    Ok("java".to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use mineconda_core::{RuntimeProfile, ServerProfile};
    use serde_json::Value;

    use super::*;
    use crate::cli::{RunCommandArgs, RunModeArg};
    use crate::test_support::TempProject;

    #[test]
    fn resolve_java_for_run_prefers_explicit_override() {
        let server = ServerProfile::default();
        let runtime = RuntimeProfile::default();
        let java =
            resolve_java_for_run(Some("custom-java".to_string()), &server, Some(&runtime)).unwrap();
        assert_eq!(java, "custom-java");
    }

    #[test]
    fn resolve_java_for_run_prefers_server_java_before_runtime() {
        let server = ServerProfile {
            java: "/opt/java/bin/java".to_string(),
            ..ServerProfile::default()
        };
        let runtime = RuntimeProfile::default();
        let java = resolve_java_for_run(None, &server, Some(&runtime)).unwrap();
        assert_eq!(java, "/opt/java/bin/java");
    }

    #[test]
    fn build_run_json_report_serializes_launch_plan() {
        let project = TempProject::new("run-json-report");
        fs::create_dir_all(project.path.join(".mineconda/dev")).expect("dev dir");
        fs::write(project.path.join(".mineconda/dev/launcher.jar"), b"jar").expect("launcher");

        let report = build_run_json_report(
            &project.path,
            RunCommandArgs {
                dry_run: true,
                java: Some("java".to_string()),
                memory: Some("2G".to_string()),
                jvm_args: vec!["-Ddemo=true".to_string()],
                mode: RunModeArg::Client,
                username: "DevPlayer".to_string(),
                instance: "dev".to_string(),
                launcher_jar: None,
                server_jar: None,
                groups: Vec::new(),
                all_groups: false,
            },
            &[],
            None,
        )
        .expect("run json report");

        let value: Value = serde_json::to_value(&report).expect("serialize report");
        assert_eq!(value["command"], "run");
        assert_eq!(value["mode"], "client");
        assert_eq!(value["instance"], "dev");
        assert_eq!(value["summary"]["launches"], 1);
        assert_eq!(value["launches"][0]["role"], "client");
        assert_eq!(value["launches"][0]["program"], "java");
    }
}
