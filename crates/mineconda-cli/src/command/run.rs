use crate::command::search::to_run_loader_hint;
use crate::*;
pub(crate) fn cmd_run(
    root: &Path,
    args: RunCommandArgs,
    profiles: &[String],
    workspace: Option<&WorkspaceConfig>,
) -> Result<()> {
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
    let defaults = manifest
        .as_ref()
        .map(|m| m.server.clone())
        .unwrap_or_else(ServerProfile::default);
    let runtime = manifest.as_ref().map(|m| m.runtime.clone());
    let loader_hint = manifest
        .as_ref()
        .map(|manifest| to_run_loader_hint(manifest.project.loader.kind));
    let package_paths = if let Some(manifest) = manifest.as_ref() {
        let active_groups =
            activation_groups_with_profiles(manifest, workspace, &groups, all_groups, profiles)?;
        if manifest.groups.is_empty() && active_groups.len() == 1 {
            None
        } else {
            let lock = load_lockfile_required(root)?;
            ensure_lock_covers_groups(manifest, &lock, &active_groups)?;
            Some(
                filtered_lockfile(&lock, &active_groups)
                    .packages
                    .into_iter()
                    .map(|package| package.install_path_or_default())
                    .collect(),
            )
        }
    } else if !groups.is_empty() || all_groups || !profiles.is_empty() {
        bail!("group/profile filters require a manifest");
    } else {
        None
    };
    let args = if jvm_args.is_empty() {
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
        extra_jvm_args: args,
        username,
        instance_name: instance,
        mode,
        loader_hint,
        client_launcher_jar,
        server_launcher_jar,
        package_paths,
    };
    run_game_instance(&request)?;
    Ok(())
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
    use mineconda_core::{RuntimeProfile, ServerProfile};

    use super::*;

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
}
