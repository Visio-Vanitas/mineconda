use crate::*;
pub(crate) fn cmd_env(root: &Path, command: EnvCommands, scope: &ScopeArgs) -> Result<()> {
    if matches!(command, EnvCommands::List)
        && load_workspace_optional(root)?.is_some()
        && scope.all_members
    {
        return cmd_env_list_workspace(root);
    }

    let target = resolve_project_target(root, scope)?;
    match command {
        EnvCommands::Install {
            java,
            provider,
            force,
            use_for_project,
        } => cmd_env_install(&target.root, java, provider, force, use_for_project),
        EnvCommands::Use { java, provider } => cmd_env_use(&target.root, java, provider),
        EnvCommands::List => cmd_env_list(&target.root),
        EnvCommands::Which => cmd_env_which(&target.root),
    }
}

fn cmd_env_list_workspace(root: &Path) -> Result<()> {
    let workspace = load_workspace_required(root)?;
    for member in workspace_members(root, &workspace)? {
        let runtime = load_manifest_optional(&member.root)?
            .map(|manifest| {
                format!(
                    "{} ({})",
                    manifest.runtime.java,
                    manifest.runtime.provider.as_str()
                )
            })
            .unwrap_or_else(|| "unconfigured".to_string());
        println!("{}\t{}", member.name, runtime);
    }
    Ok(())
}

fn cmd_env_install(
    root: &Path,
    java: String,
    provider: JavaProviderArg,
    force: bool,
    use_for_project: bool,
) -> Result<()> {
    let provider = provider.to_core();
    let java_bin = ensure_java_runtime(&java, provider, force)?;
    println!(
        "installed java {} ({}) -> {}",
        java,
        provider.as_str(),
        java_bin.display()
    );

    if use_for_project {
        let mut manifest = load_manifest(root)?;
        manifest.runtime.java = java.clone();
        manifest.runtime.provider = provider;
        let path = manifest_path(root);
        write_manifest(&path, &manifest)?;
        println!(
            "project runtime pinned to {} ({}) in {}",
            java,
            provider.as_str(),
            path.display()
        );
    }

    Ok(())
}

fn cmd_env_use(root: &Path, java: String, provider: JavaProviderArg) -> Result<()> {
    let provider = provider.to_core();
    let java_bin = resolve_java_binary(&java, provider, true)?;
    let mut manifest = load_manifest(root)?;
    manifest.runtime.java = java.clone();
    manifest.runtime.provider = provider;

    let path = manifest_path(root);
    write_manifest(&path, &manifest)?;
    println!(
        "active runtime: java {} ({}) -> {}",
        java,
        provider.as_str(),
        java_bin.display()
    );
    println!("updated {}", path.display());
    Ok(())
}

fn cmd_env_list(root: &Path) -> Result<()> {
    let active = load_manifest_optional(root)?
        .map(|m| (m.runtime.java, m.runtime.provider))
        .unwrap_or_else(|| ("".to_string(), JavaProvider::Temurin));

    let runtimes = list_java_runtimes()?;
    if runtimes.is_empty() {
        println!("no managed java runtimes installed");
        return Ok(());
    }

    for runtime in runtimes {
        let marker = if active.0 == runtime.version && active.1 == runtime.provider {
            "*"
        } else {
            " "
        };
        println!(
            "{} java {} ({}) -> {}",
            marker,
            runtime.version,
            runtime.provider.as_str(),
            runtime.java_bin.display()
        );
    }

    Ok(())
}

fn cmd_env_which(root: &Path) -> Result<()> {
    let manifest = load_manifest(root)?;
    let runtime = manifest.runtime;
    match find_java_runtime(&runtime.java, runtime.provider)? {
        Some(path) => {
            println!(
                "java {} ({}) -> {}",
                runtime.java,
                runtime.provider.as_str(),
                path.display()
            );
            Ok(())
        }
        None => bail!(
            "java {} ({}) is not installed. run `mineconda env install {}`",
            runtime.java,
            runtime.provider.as_str(),
            runtime.java
        ),
    }
}
