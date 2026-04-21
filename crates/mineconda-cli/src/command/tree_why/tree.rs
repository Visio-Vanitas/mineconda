use crate::*;

use super::graph::{
    LockGraph, LockGraphEdge, ReverseLockGraphEdge, ensure_lock_dependency_graph, lock_graph_key,
    locked_package_display, locked_package_graph_key, resolve_locked_package,
    resolve_manifest_root_packages, sorted_lock_packages,
};

fn tree_json_node(package: &LockedPackage) -> TreeJsonNode {
    TreeJsonNode {
        key: locked_package_graph_key(package),
        id: package.id.clone(),
        source: package.source.as_str().to_string(),
        version: package.version.clone(),
        groups: normalized_package_groups(package),
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

pub(super) fn build_tree_json_report(
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

pub(super) fn render_forward_dependency_forest(
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

pub(super) fn render_reverse_dependency_tree(
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

#[cfg(test)]
mod tests {
    use mineconda_core::{Manifest, lockfile_path, manifest_path, write_lockfile, write_manifest};

    use super::*;
    use crate::cli::TreeCommandArgs;
    use crate::project::ProjectSelection;
    use crate::test_support::{TempProject, required_dependency, test_lockfile, test_package};

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
}
