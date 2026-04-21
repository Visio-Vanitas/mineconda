use crate::*;

use super::graph::{
    LockGraph, ensure_lock_dependency_graph, lock_graph_key, locked_package_display,
    locked_package_graph_key, resolve_locked_package, resolve_manifest_root_packages,
};

fn why_json_step(package: &LockedPackage) -> WhyJsonStep {
    WhyJsonStep {
        key: locked_package_graph_key(package),
        id: package.id.clone(),
        source: package.source.as_str().to_string(),
        version: package.version.clone(),
    }
}

fn compute_why_paths(
    graph: &LockGraph<'_>,
    roots: &[&LockedPackage],
    target: &LockedPackage,
) -> Result<(bool, Vec<Vec<String>>)> {
    let target_key = locked_package_graph_key(target);
    let root_keys: Vec<String> = roots
        .iter()
        .map(|package| locked_package_graph_key(package))
        .collect();

    if root_keys.iter().any(|key| key == &target_key) {
        return Ok((true, vec![vec![target_key]]));
    }

    let mut distance: HashMap<String, usize> = HashMap::new();
    let mut predecessors: HashMap<String, Vec<String>> = HashMap::new();
    let mut queue = VecDeque::new();

    for root_key in &root_keys {
        distance.insert(root_key.clone(), 0);
        queue.push_back(root_key.clone());
    }

    while let Some(current) = queue.pop_front() {
        let current_distance = distance.get(&current).copied().unwrap_or(0);
        for edge in graph.forward_edges.get(&current).into_iter().flatten() {
            if edge.kind != LockedDependencyKind::Required {
                continue;
            }
            let next_key = lock_graph_key(edge.source, &edge.id);
            if graph.package(&next_key).is_none() {
                continue;
            }

            match distance.get(&next_key).copied() {
                None => {
                    distance.insert(next_key.clone(), current_distance + 1);
                    predecessors.insert(next_key.clone(), vec![current.clone()]);
                    queue.push_back(next_key);
                }
                Some(existing) if existing == current_distance + 1 => {
                    predecessors
                        .entry(next_key)
                        .or_default()
                        .push(current.clone());
                }
                _ => {}
            }
        }
    }

    if !distance.contains_key(&target_key) {
        bail!(
            "{} is locked but not reachable from manifest roots; rerun `mineconda lock`",
            locked_package_display(target)
        );
    }

    let mut paths = Vec::new();
    let mut current = vec![target_key.clone()];
    collect_why_paths(&predecessors, &root_keys, &mut current, &mut paths);
    paths.sort();
    paths.dedup();
    Ok((false, paths))
}

pub(super) fn build_why_json_report(
    root: &Path,
    args: &WhyCommandArgs,
    selection: ProjectSelection<'_>,
) -> Result<WhyJsonReport> {
    let manifest = load_manifest(root)?;
    let lock = load_lockfile_required(root)?;
    ensure_lock_dependency_graph(&lock)?;
    let active_groups = selection.active_groups(&manifest)?;
    ensure_lock_covers_groups(&manifest, &lock, &active_groups)?;
    let graph = LockGraph::from_lock(&lock);
    let roots = resolve_manifest_root_packages(&manifest, &lock, &active_groups)?;
    let target = resolve_locked_package(
        &lock,
        &args.id,
        args.source.map(SourceArg::to_core),
        &active_groups,
    )?;
    let (direct, paths) = compute_why_paths(&graph, &roots, target)?;
    let target_step = why_json_step(target);
    Ok(WhyJsonReport {
        command: "why",
        groups: active_groups.iter().cloned().collect(),
        profiles: selection.normalized_profiles()?,
        workspace: selection.workspace_name(),
        member: selection.member_name.map(ToString::to_string),
        target: WhyJsonTarget {
            key: target_step.key.clone(),
            id: target_step.id.clone(),
            source: target_step.source.clone(),
            version: target_step.version.clone(),
        },
        reason: if direct {
            "direct".to_string()
        } else {
            "transitive".to_string()
        },
        direct,
        paths: paths
            .into_iter()
            .map(|path| {
                path.into_iter()
                    .filter_map(|key| graph.package(&key).map(why_json_step))
                    .collect::<Vec<_>>()
            })
            .collect(),
        messages: vec![format!(
            "selected groups={}",
            format_active_groups(&active_groups)
        )],
    })
}

pub(crate) fn cmd_why(
    root: &Path,
    args: WhyCommandArgs,
    selection: ProjectSelection<'_>,
) -> Result<()> {
    let manifest = load_manifest(root)?;
    let lock = load_lockfile_required(root)?;
    ensure_lock_dependency_graph(&lock)?;
    let active_groups = selection.active_groups(&manifest)?;
    ensure_lock_covers_groups(&manifest, &lock, &active_groups)?;
    let graph = LockGraph::from_lock(&lock);
    let roots = resolve_manifest_root_packages(&manifest, &lock, &active_groups)?;
    let target = resolve_locked_package(
        &lock,
        &args.id,
        args.source.map(SourceArg::to_core),
        &active_groups,
    )?;
    if args.json {
        return emit_json_report(&build_why_json_report(root, &args, selection)?, 0);
    }
    let output = render_why_report(&graph, &roots, target)?;
    print!("{output}");
    Ok(())
}

pub(super) fn render_why_report(
    graph: &LockGraph<'_>,
    roots: &[&LockedPackage],
    target: &LockedPackage,
) -> Result<String> {
    let (direct, paths) = compute_why_paths(graph, roots, target)?;
    if direct {
        return Ok(format!(
            "{} is a direct dependency\n{}\n",
            locked_package_display(target),
            locked_package_display(target)
        ));
    }
    let mut rendered_paths: Vec<String> = paths
        .into_iter()
        .map(|path| {
            path.into_iter()
                .map(|key| {
                    graph
                        .package(&key)
                        .map(locked_package_display)
                        .unwrap_or(key)
                })
                .collect::<Vec<_>>()
                .join(" -> ")
        })
        .collect();
    rendered_paths.sort();
    rendered_paths.dedup();

    let mut lines = vec![format!(
        "{} is a transitive dependency",
        locked_package_display(target)
    )];
    lines.extend(rendered_paths.into_iter().map(|path| format!("- {path}")));
    Ok(format!("{}\n", lines.join("\n")))
}

fn collect_why_paths(
    predecessors: &HashMap<String, Vec<String>>,
    root_keys: &[String],
    current_path: &mut Vec<String>,
    out: &mut Vec<Vec<String>>,
) {
    let Some(current_key) = current_path.last().cloned() else {
        return;
    };

    if root_keys.iter().any(|key| key == &current_key) {
        let mut path = current_path.clone();
        path.reverse();
        out.push(path);
        return;
    }

    let Some(previous) = predecessors.get(&current_key) else {
        return;
    };

    for predecessor in previous {
        current_path.push(predecessor.clone());
        collect_why_paths(predecessors, root_keys, current_path, out);
        current_path.pop();
    }
}

#[cfg(test)]
mod tests {
    use mineconda_core::{
        DEFAULT_GROUP_NAME, Manifest, ModSide, ModSource, ModSpec, lockfile_path, manifest_path,
        write_lockfile, write_manifest,
    };

    use super::*;
    use crate::cli::WhyCommandArgs;
    use crate::project::ProjectSelection;
    use crate::test_support::{
        TempProject, incompatible_dependency, required_dependency, test_lockfile, test_manifest,
        test_package,
    };

    #[test]
    fn why_json_report_contains_transitive_path() {
        let project = TempProject::new("why-json");
        let manifest: Manifest = toml::from_str(
            r#"
[project]
name = "pack"
minecraft = "1.21.1"

[project.loader]
kind = "neo-forge"
version = "latest"

[[mods]]
id = "alpha"
source = "modrinth"
version = "1.0.0"
side = "both"
"#,
        )
        .expect("manifest");
        let lock = test_lockfile(vec![
            test_package("alpha", "1.0.0", vec![required_dependency("beta")]),
            test_package("beta", "1.1.0", vec![required_dependency("gamma")]),
            test_package("gamma", "1.2.0", Vec::new()),
        ]);
        write_manifest(&manifest_path(&project.path), &manifest).expect("write manifest");
        write_lockfile(&lockfile_path(&project.path), &lock).expect("write lock");

        let report = build_why_json_report(
            &project.path,
            &WhyCommandArgs {
                id: "beta".to_string(),
                source: None,
                json: true,
            },
            ProjectSelection {
                groups: &[],
                all_groups: false,
                profiles: &[],
                workspace: None,
                member_name: None,
            },
        )
        .expect("why report");

        assert_eq!(report.command, "why");
        assert_eq!(report.reason, "transitive");
        assert!(!report.direct);
        assert_eq!(report.target.id, "beta");
        assert_eq!(report.paths.len(), 1);
        assert_eq!(
            report.paths[0]
                .iter()
                .map(|step| step.id.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );
    }

    #[test]
    fn why_report_marks_direct_dependency() {
        let alpha = test_package("alpha", "1.0.0", vec![required_dependency("beta")]);
        let lock = test_lockfile(vec![
            alpha.clone(),
            test_package("beta", "1.1.0", Vec::new()),
        ]);
        let graph = LockGraph::from_lock(&lock);

        let output = render_why_report(&graph, &[&alpha], &alpha).expect("direct why report");
        assert_eq!(
            output,
            "alpha [modrinth] 1.0.0 is a direct dependency\nalpha [modrinth] 1.0.0\n"
        );
    }

    #[test]
    fn why_report_lists_shortest_transitive_paths() {
        let alpha = test_package("alpha", "1.0.0", vec![required_dependency("beta")]);
        let beta = test_package("beta", "1.1.0", vec![required_dependency("gamma")]);
        let delta = test_package("delta", "2.0.0", vec![required_dependency("gamma")]);
        let gamma = test_package("gamma", "1.2.0", Vec::new());
        let manifest = test_manifest(vec![
            ModSpec::new(
                "alpha".to_string(),
                ModSource::Modrinth,
                "latest".to_string(),
                ModSide::Both,
            ),
            ModSpec::new(
                "delta".to_string(),
                ModSource::Modrinth,
                "latest".to_string(),
                ModSide::Both,
            ),
        ]);
        let lock = test_lockfile(vec![alpha.clone(), beta, delta.clone(), gamma.clone()]);
        let graph = LockGraph::from_lock(&lock);
        let groups = BTreeSet::from([DEFAULT_GROUP_NAME.to_string()]);
        let roots =
            resolve_manifest_root_packages(&manifest, &lock, &groups).expect("manifest roots");

        let output = render_why_report(&graph, &roots, &gamma).expect("transitive why report");
        assert_eq!(
            output,
            concat!(
                "gamma [modrinth] 1.2.0 is a transitive dependency\n",
                "- delta [modrinth] 2.0.0 -> gamma [modrinth] 1.2.0\n",
            )
        );
    }

    #[test]
    fn why_report_ignores_incompatible_edges() {
        let alpha = test_package("alpha", "1.0.0", vec![incompatible_dependency("gamma")]);
        let gamma = test_package("gamma", "1.2.0", Vec::new());
        let lock = test_lockfile(vec![alpha.clone(), gamma.clone()]);
        let graph = LockGraph::from_lock(&lock);

        let error =
            render_why_report(&graph, &[&alpha], &gamma).expect_err("should be unreachable");
        assert!(
            error
                .to_string()
                .contains("gamma [modrinth] 1.2.0 is locked but not reachable from manifest roots")
        );
    }
}
