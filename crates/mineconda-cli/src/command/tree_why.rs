use crate::*;
#[derive(Debug, Clone)]
pub(crate) struct LockGraphEdge {
    source: ModSource,
    id: String,
    kind: LockedDependencyKind,
    constraint: Option<String>,
}

#[derive(Debug, Clone)]
struct ReverseLockGraphEdge {
    dependent_key: String,
    kind: LockedDependencyKind,
    constraint: Option<String>,
}

pub(crate) struct LockGraph<'a> {
    packages_by_key: HashMap<String, &'a LockedPackage>,
    forward_edges: HashMap<String, Vec<LockGraphEdge>>,
    reverse_edges: HashMap<String, Vec<ReverseLockGraphEdge>>,
}

impl<'a> LockGraph<'a> {
    pub(crate) fn from_lock(lock: &'a Lockfile) -> Self {
        let mut packages_by_key = HashMap::new();
        let mut forward_edges = HashMap::new();
        let mut reverse_edges: HashMap<String, Vec<ReverseLockGraphEdge>> = HashMap::new();

        for package in &lock.packages {
            let key = lock_graph_key(package.source, &package.id);
            packages_by_key.insert(key.clone(), package);

            let mut edges: Vec<LockGraphEdge> = package
                .dependencies
                .iter()
                .map(|dependency| LockGraphEdge {
                    source: dependency.source,
                    id: dependency.id.clone(),
                    kind: dependency.kind,
                    constraint: dependency.constraint.clone(),
                })
                .collect();
            edges.sort_by(|left, right| {
                lock_graph_key(left.source, &left.id).cmp(&lock_graph_key(right.source, &right.id))
            });

            for edge in &edges {
                let target_key = lock_graph_key(edge.source, &edge.id);
                reverse_edges
                    .entry(target_key)
                    .or_default()
                    .push(ReverseLockGraphEdge {
                        dependent_key: key.clone(),
                        kind: edge.kind,
                        constraint: edge.constraint.clone(),
                    });
            }
            forward_edges.insert(key, edges);
        }

        for edges in reverse_edges.values_mut() {
            edges.sort_by(|left, right| left.dependent_key.cmp(&right.dependent_key));
        }

        Self {
            packages_by_key,
            forward_edges,
            reverse_edges,
        }
    }

    fn package(&self, key: &str) -> Option<&'a LockedPackage> {
        self.packages_by_key.get(key).copied()
    }
}

fn tree_json_node(package: &LockedPackage) -> TreeJsonNode {
    TreeJsonNode {
        key: locked_package_graph_key(package),
        id: package.id.clone(),
        source: package.source.as_str().to_string(),
        version: package.version.clone(),
        groups: normalized_package_groups(package),
    }
}

fn why_json_step(package: &LockedPackage) -> WhyJsonStep {
    WhyJsonStep {
        key: locked_package_graph_key(package),
        id: package.id.clone(),
        source: package.source.as_str().to_string(),
        version: package.version.clone(),
    }
}

fn collect_forward_tree_json(
    graph: &LockGraph<'_>,
    key: &str,
    path: &mut Vec<String>,
    nodes: &mut HashMap<String, TreeJsonNode>,
    edges: &mut HashMap<(String, String, String, Option<String>), TreeJsonEdge>,
) {
    if path.iter().any(|entry| entry == key) {
        return;
    }

    let Some(package) = graph.package(key) else {
        return;
    };
    nodes
        .entry(key.to_string())
        .or_insert_with(|| tree_json_node(package));

    path.push(key.to_string());
    for edge in graph.forward_edges.get(key).into_iter().flatten() {
        let child_key = lock_graph_key(edge.source, &edge.id);
        edges
            .entry((
                key.to_string(),
                child_key.clone(),
                edge.kind.as_str().to_string(),
                edge.constraint.clone(),
            ))
            .or_insert_with(|| TreeJsonEdge {
                from: key.to_string(),
                to: child_key.clone(),
                kind: edge.kind.as_str().to_string(),
                constraint: edge.constraint.clone(),
            });
        collect_forward_tree_json(graph, &child_key, path, nodes, edges);
    }
    path.pop();
}

fn collect_reverse_tree_json(
    graph: &LockGraph<'_>,
    key: &str,
    groups: &BTreeSet<String>,
    path: &mut Vec<String>,
    nodes: &mut HashMap<String, TreeJsonNode>,
    edges: &mut HashMap<(String, String, String, Option<String>), TreeJsonEdge>,
) {
    if path.iter().any(|entry| entry == key) {
        return;
    }

    let Some(package) = graph.package(key) else {
        return;
    };
    nodes
        .entry(key.to_string())
        .or_insert_with(|| tree_json_node(package));

    path.push(key.to_string());
    for edge in graph.reverse_edges.get(key).into_iter().flatten() {
        let Some(dependent) = graph.package(&edge.dependent_key) else {
            continue;
        };
        if !package_in_groups(dependent, groups) {
            continue;
        }
        edges
            .entry((
                key.to_string(),
                edge.dependent_key.clone(),
                edge.kind.as_str().to_string(),
                edge.constraint.clone(),
            ))
            .or_insert_with(|| TreeJsonEdge {
                from: key.to_string(),
                to: edge.dependent_key.clone(),
                kind: edge.kind.as_str().to_string(),
                constraint: edge.constraint.clone(),
            });
        collect_reverse_tree_json(graph, &edge.dependent_key, groups, path, nodes, edges);
    }
    path.pop();
}

pub(crate) fn build_tree_json_report(
    root: &Path,
    args: &TreeCommandArgs,
    selection: ProjectSelection<'_>,
) -> Result<TreeJsonReport> {
    let manifest = load_manifest(root)?;
    let lock = load_lockfile_required(root)?;
    ensure_lock_dependency_graph(&lock)?;
    let active_groups = selection.active_groups(&manifest)?;
    ensure_lock_covers_groups(&manifest, &lock, &active_groups)?;
    let graph = LockGraph::from_lock(&lock);
    let source_filter = args.source.map(SourceArg::to_core);
    let normalized_profiles = selection.normalized_profiles()?;

    let mut nodes = HashMap::new();
    let mut edges = HashMap::new();
    let mut path = Vec::new();
    let (mode, direction, roots): (String, String, Vec<String>) =
        if let Some(query) = args.invert.as_deref() {
            let target = resolve_locked_package(&lock, query, source_filter, &active_groups)?;
            let key = locked_package_graph_key(target);
            collect_reverse_tree_json(
                &graph,
                &key,
                &active_groups,
                &mut path,
                &mut nodes,
                &mut edges,
            );
            ("invert".to_string(), "reverse".to_string(), vec![key])
        } else if args.all {
            let packages = sorted_lock_packages(&lock, &active_groups);
            let keys = packages
                .iter()
                .map(|package| locked_package_graph_key(package))
                .collect::<Vec<_>>();
            for key in &keys {
                collect_forward_tree_json(&graph, key, &mut path, &mut nodes, &mut edges);
            }
            ("all".to_string(), "forward".to_string(), keys)
        } else if let Some(query) = args.id.as_deref() {
            let target = resolve_locked_package(&lock, query, source_filter, &active_groups)?;
            let key = locked_package_graph_key(target);
            collect_forward_tree_json(&graph, &key, &mut path, &mut nodes, &mut edges);
            ("target".to_string(), "forward".to_string(), vec![key])
        } else {
            let roots = resolve_manifest_root_packages(&manifest, &lock, &active_groups)?;
            let keys = roots
                .iter()
                .map(|package| locked_package_graph_key(package))
                .collect::<Vec<_>>();
            for key in &keys {
                collect_forward_tree_json(&graph, key, &mut path, &mut nodes, &mut edges);
            }
            ("roots".to_string(), "forward".to_string(), keys)
        };

    let mut node_values = nodes.into_values().collect::<Vec<_>>();
    node_values.sort_by(|left, right| left.key.cmp(&right.key));
    let mut edge_values = edges.into_values().collect::<Vec<_>>();
    edge_values.sort_by(|left, right| {
        (
            left.from.as_str(),
            left.to.as_str(),
            left.kind.as_str(),
            left.constraint.as_deref().unwrap_or(""),
        )
            .cmp(&(
                right.from.as_str(),
                right.to.as_str(),
                right.kind.as_str(),
                right.constraint.as_deref().unwrap_or(""),
            ))
    });

    Ok(TreeJsonReport {
        command: "tree",
        groups: active_groups.iter().cloned().collect(),
        profiles: normalized_profiles,
        workspace: selection.workspace_name(),
        member: selection.member_name.map(ToString::to_string),
        mode,
        direction,
        roots,
        nodes: node_values,
        edges: edge_values,
        messages: vec![format!(
            "selected groups={}",
            format_active_groups(&active_groups)
        )],
    })
}

pub(crate) fn cmd_tree(
    root: &Path,
    args: TreeCommandArgs,
    selection: ProjectSelection<'_>,
) -> Result<()> {
    let manifest = load_manifest(root)?;
    let lock = load_lockfile_required(root)?;
    ensure_lock_dependency_graph(&lock)?;
    let active_groups = selection.active_groups(&manifest)?;
    ensure_lock_covers_groups(&manifest, &lock, &active_groups)?;
    let graph = LockGraph::from_lock(&lock);
    let source_filter = args.source.map(SourceArg::to_core);

    if args.json {
        return emit_json_report(&build_tree_json_report(root, &args, selection)?, 0);
    }

    let output = if let Some(query) = args.invert.as_deref() {
        let target = resolve_locked_package(&lock, query, source_filter, &active_groups)?;
        render_reverse_dependency_tree(&graph, target, &active_groups)
    } else if args.all {
        let packages = sorted_lock_packages(&lock, &active_groups);
        render_forward_dependency_forest(&graph, &packages)
    } else if let Some(query) = args.id.as_deref() {
        let target = resolve_locked_package(&lock, query, source_filter, &active_groups)?;
        render_forward_dependency_forest(&graph, &[target])
    } else {
        let roots = resolve_manifest_root_packages(&manifest, &lock, &active_groups)?;
        render_forward_dependency_forest(&graph, &roots)
    };

    print!("{output}");
    Ok(())
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

pub(crate) fn build_why_json_report(
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

pub(crate) fn render_why_report(
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

pub(crate) fn render_forward_dependency_forest(
    graph: &LockGraph<'_>,
    roots: &[&LockedPackage],
) -> String {
    let mut lines = Vec::new();
    for (index, root) in roots.iter().enumerate() {
        if index > 0 {
            lines.push(String::new());
        }
        let key = locked_package_graph_key(root);
        render_forward_tree_node(graph, &key, "", true, None, &mut Vec::new(), &mut lines);
    }
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

pub(crate) fn render_reverse_dependency_tree(
    graph: &LockGraph<'_>,
    root: &LockedPackage,
    groups: &BTreeSet<String>,
) -> String {
    let mut state = ReverseTreeRenderState {
        groups,
        path: Vec::new(),
        lines: Vec::new(),
    };
    let key = locked_package_graph_key(root);
    render_reverse_tree_node(graph, &key, "", true, None, &mut state);
    if state.lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", state.lines.join("\n"))
    }
}

struct ReverseTreeRenderState<'a> {
    groups: &'a BTreeSet<String>,
    path: Vec<String>,
    lines: Vec<String>,
}

fn render_forward_tree_node(
    graph: &LockGraph<'_>,
    key: &str,
    prefix: &str,
    is_last: bool,
    incoming: Option<&LockGraphEdge>,
    path: &mut Vec<String>,
    lines: &mut Vec<String>,
) {
    let connector = if incoming.is_none() {
        ""
    } else if is_last {
        "`-- "
    } else {
        "|-- "
    };

    if path.iter().any(|entry| entry == key) {
        lines.push(format!("{prefix}{connector}{key} (cycle)"));
        return;
    }

    let label = if let Some(package) = graph.package(key) {
        locked_package_display(package)
    } else if let Some(edge) = incoming {
        format!("{} [{}] <not-locked>", edge.id, edge.source.as_str())
    } else {
        key.to_string()
    };
    let edge_label = incoming.map(format_tree_edge_label).unwrap_or_default();
    lines.push(format!("{prefix}{connector}{label}{edge_label}"));

    path.push(key.to_string());
    let child_prefix = if incoming.is_none() {
        String::new()
    } else if is_last {
        format!("{prefix}    ")
    } else {
        format!("{prefix}|   ")
    };

    let edges = graph.forward_edges.get(key).cloned().unwrap_or_default();
    for (index, edge) in edges.iter().enumerate() {
        let child_key = lock_graph_key(edge.source, &edge.id);
        render_forward_tree_node(
            graph,
            &child_key,
            &child_prefix,
            index + 1 == edges.len(),
            Some(edge),
            path,
            lines,
        );
    }
    path.pop();
}

fn render_reverse_tree_node(
    graph: &LockGraph<'_>,
    key: &str,
    prefix: &str,
    is_last: bool,
    incoming: Option<&ReverseLockGraphEdge>,
    state: &mut ReverseTreeRenderState<'_>,
) {
    let connector = if incoming.is_none() {
        ""
    } else if is_last {
        "`-- "
    } else {
        "|-- "
    };

    if state.path.iter().any(|entry| entry == key) {
        state
            .lines
            .push(format!("{prefix}{connector}{key} (cycle)"));
        return;
    }

    let label = graph
        .package(key)
        .map(locked_package_display)
        .unwrap_or_else(|| key.to_string());
    let edge_label = incoming
        .map(format_reverse_tree_edge_label)
        .unwrap_or_default();
    state
        .lines
        .push(format!("{prefix}{connector}{label}{edge_label}"));

    state.path.push(key.to_string());
    let child_prefix = if incoming.is_none() {
        String::new()
    } else if is_last {
        format!("{prefix}    ")
    } else {
        format!("{prefix}|   ")
    };

    let edges = graph.reverse_edges.get(key).cloned().unwrap_or_default();
    let visible_edges: Vec<ReverseLockGraphEdge> = edges
        .into_iter()
        .filter(|edge| {
            graph
                .package(&edge.dependent_key)
                .is_some_and(|package| package_in_groups(package, state.groups))
        })
        .collect();
    for (index, edge) in visible_edges.iter().enumerate() {
        render_reverse_tree_node(
            graph,
            &edge.dependent_key,
            &child_prefix,
            index + 1 == visible_edges.len(),
            Some(edge),
            state,
        );
    }
    state.path.pop();
}

fn format_tree_edge_label(edge: &LockGraphEdge) -> String {
    let mut suffix = edge.kind.as_str().to_string();
    if let Some(constraint) = edge.constraint.as_deref() {
        suffix.push_str(&format!(", {constraint}"));
    }
    format!(" ({suffix})")
}

fn format_reverse_tree_edge_label(edge: &ReverseLockGraphEdge) -> String {
    let mut suffix = format!("depended on via {}", edge.kind.as_str());
    if let Some(constraint) = edge.constraint.as_deref() {
        suffix.push_str(&format!(", {constraint}"));
    }
    format!(" ({suffix})")
}

fn ensure_lock_dependency_graph(lock: &Lockfile) -> Result<()> {
    if lock.metadata.dependency_graph {
        return Ok(());
    }
    bail!("lockfile does not contain dependency graph data; rerun `mineconda lock` first")
}

pub(crate) fn resolve_manifest_root_packages<'a>(
    manifest: &Manifest,
    lock: &'a Lockfile,
    groups: &BTreeSet<String>,
) -> Result<Vec<&'a LockedPackage>> {
    let mut roots = Vec::new();
    let mut seen = HashSet::new();

    for (group, spec) in selected_manifest_specs(manifest, groups) {
        let single_group = BTreeSet::from([group.clone()]);
        let package = lock
            .packages
            .iter()
            .find(|package| package_in_groups(package, &single_group) && lock_package_matches_spec(package, spec))
            .with_context(|| {
                format!(
                    "manifest entry `{}` [{}] in group `{}` is not present in lockfile; rerun `mineconda lock`",
                    spec.id, spec.source.as_str(), group
                )
            })?;
        let key = locked_package_graph_key(package);
        if seen.insert(key) {
            roots.push(package);
        }
    }

    Ok(roots)
}

pub(crate) fn resolve_locked_package<'a>(
    lock: &'a Lockfile,
    id: &str,
    source: Option<ModSource>,
    groups: &BTreeSet<String>,
) -> Result<&'a LockedPackage> {
    let matches: Vec<&LockedPackage> = lock
        .packages
        .iter()
        .filter(|package| {
            package_in_groups(package, groups) && lock_package_matches_request(package, id, source)
        })
        .collect();

    match matches.as_slice() {
        [] => bail!("package `{id}` not found in lockfile"),
        [package] => Ok(*package),
        _ => bail!("multiple lockfile entries match `{id}`, use `--source` to disambiguate"),
    }
}

pub(crate) fn sorted_lock_packages<'a>(
    lock: &'a Lockfile,
    groups: &BTreeSet<String>,
) -> Vec<&'a LockedPackage> {
    let mut packages: Vec<&LockedPackage> = lock
        .packages
        .iter()
        .filter(|package| package_in_groups(package, groups))
        .collect();
    packages.sort_by(|left, right| {
        locked_package_graph_key(left).cmp(&locked_package_graph_key(right))
    });
    packages
}

pub(crate) fn lock_graph_key(source: ModSource, id: &str) -> String {
    format!("{}@{}", id, source.as_str())
}

pub(crate) fn locked_package_graph_key(package: &LockedPackage) -> String {
    lock_graph_key(package.source, &package.id)
}

pub(crate) fn locked_package_display(package: &LockedPackage) -> String {
    format!(
        "{} [{}] {}",
        package.id,
        package.source.as_str(),
        package.version
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use mineconda_core::{
        DEFAULT_GROUP_NAME, Manifest, ModSide, ModSource, ModSpec, lockfile_path, manifest_path,
        write_lockfile, write_manifest,
    };

    use super::*;
    use crate::cli::{TreeCommandArgs, WhyCommandArgs};
    use crate::project::ProjectSelection;
    use crate::test_support::{
        TempProject, incompatible_dependency, required_dependency, test_lockfile, test_manifest,
        test_package,
    };

    #[test]
    fn forward_tree_renders_nested_dependencies() {
        let alpha = test_package("alpha", "1.0.0", vec![required_dependency("beta")]);
        let beta = test_package("beta", "1.1.0", vec![required_dependency("gamma")]);
        let gamma = test_package("gamma", "1.2.0", Vec::new());
        let lock = test_lockfile(vec![alpha.clone(), beta, gamma]);
        let graph = LockGraph::from_lock(&lock);

        let output = render_forward_dependency_forest(&graph, &[&alpha]);
        assert_eq!(
            output,
            concat!(
                "alpha [modrinth] 1.0.0\n",
                "`-- beta [modrinth] 1.1.0 (required)\n",
                "    `-- gamma [modrinth] 1.2.0 (required)\n",
            )
        );
    }

    #[test]
    fn reverse_tree_renders_nested_dependents() {
        let alpha = test_package("alpha", "1.0.0", vec![required_dependency("beta")]);
        let beta = test_package("beta", "1.1.0", vec![required_dependency("gamma")]);
        let gamma = test_package("gamma", "1.2.0", Vec::new());
        let lock = test_lockfile(vec![alpha, beta.clone(), gamma.clone()]);
        let graph = LockGraph::from_lock(&lock);
        let groups = BTreeSet::from([DEFAULT_GROUP_NAME.to_string()]);

        let output = render_reverse_dependency_tree(&graph, &gamma, &groups);
        assert_eq!(
            output,
            concat!(
                "gamma [modrinth] 1.2.0\n",
                "`-- beta [modrinth] 1.1.0 (depended on via required)\n",
                "    `-- alpha [modrinth] 1.0.0 (depended on via required)\n",
            )
        );
    }

    #[test]
    fn tree_json_report_contains_nodes_and_edges() {
        let project = TempProject::new("tree-json");
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

        let report = build_tree_json_report(
            &project.path,
            &TreeCommandArgs {
                id: None,
                invert: None,
                all: false,
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
        .expect("tree report");

        assert_eq!(report.command, "tree");
        assert_eq!(report.mode, "roots");
        assert_eq!(report.direction, "forward");
        assert_eq!(report.roots, vec!["alpha@modrinth".to_string()]);
        assert_eq!(report.nodes.len(), 3);
        assert!(report.edges.iter().any(|edge| {
            edge.from == "alpha@modrinth" && edge.to == "beta@modrinth" && edge.kind == "required"
        }));
    }

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
