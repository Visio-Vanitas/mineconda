use crate::*;

#[derive(Debug, Clone)]
pub(super) struct LockGraphEdge {
    pub(super) source: ModSource,
    pub(super) id: String,
    pub(super) kind: LockedDependencyKind,
    pub(super) constraint: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct ReverseLockGraphEdge {
    pub(super) dependent_key: String,
    pub(super) kind: LockedDependencyKind,
    pub(super) constraint: Option<String>,
}

pub(super) struct LockGraph<'a> {
    packages_by_key: HashMap<String, &'a LockedPackage>,
    pub(super) forward_edges: HashMap<String, Vec<LockGraphEdge>>,
    pub(super) reverse_edges: HashMap<String, Vec<ReverseLockGraphEdge>>,
}

impl<'a> LockGraph<'a> {
    pub(super) fn from_lock(lock: &'a Lockfile) -> Self {
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

    pub(super) fn package(&self, key: &str) -> Option<&'a LockedPackage> {
        self.packages_by_key.get(key).copied()
    }
}

pub(super) fn ensure_lock_dependency_graph(lock: &Lockfile) -> Result<()> {
    if lock.metadata.dependency_graph {
        return Ok(());
    }
    bail!("lockfile does not contain dependency graph data; rerun `mineconda lock` first")
}

pub(super) fn resolve_manifest_root_packages<'a>(
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
            .find(|package| {
                package_in_groups(package, &single_group) && lock_package_matches_spec(package, spec)
            })
            .with_context(|| {
                format!(
                    "manifest entry `{}` [{}] in group `{}` is not present in lockfile; rerun `mineconda lock`",
                    spec.id,
                    spec.source.as_str(),
                    group
                )
            })?;
        let key = locked_package_graph_key(package);
        if seen.insert(key) {
            roots.push(package);
        }
    }

    Ok(roots)
}

pub(super) fn resolve_locked_package<'a>(
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

pub(super) fn sorted_lock_packages<'a>(
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

pub(super) fn locked_package_display(package: &LockedPackage) -> String {
    format!(
        "{} [{}] {}",
        package.id,
        package.source.as_str(),
        package.version
    )
}
