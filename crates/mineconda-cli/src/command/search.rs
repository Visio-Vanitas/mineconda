use crate::command::lock::write_lock_from_manifest;
use crate::command::sync::cmd_sync;
use crate::*;
pub(crate) fn cmd_search(root: &Path, args: SearchCommandArgs) -> Result<()> {
    let SearchCommandArgs {
        query,
        source,
        limit,
        page,
        no_color,
        non_interactive,
        install_first,
        install_version,
        group,
    } = args;
    let interactive = !non_interactive
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::env::var_os("CI").is_none();
    let manifest_env = if let Some(manifest) = load_manifest_optional(root)? {
        Some((manifest.project.minecraft, manifest.project.loader.kind))
    } else {
        None
    };
    let (minecraft_filter, loader_filter, environment_label) = if interactive {
        if let Some((minecraft, loader)) = manifest_env.as_ref() {
            (
                Some(minecraft.clone()),
                Some(*loader),
                Some(format!("{}/{}", minecraft, loader_label(*loader))),
            )
        } else {
            (None, None, None)
        }
    } else {
        (None, None, None)
    };

    let request = SearchRequest {
        source: source.to_core(),
        query: query.clone(),
        limit,
        page,
        minecraft_version: minecraft_filter.clone(),
        loader: loader_filter,
    };
    let spinner_enabled = std::io::stderr().is_terminal()
        && std::env::var_os("CI").is_none()
        && std::env::var_os("MINECONDA_NO_SPINNER").is_none();
    let spinner_label = if let Some(label) = environment_label.as_deref() {
        format!(
            "{} `{query}` @{} [{label}]",
            i18n::text("searching", "正在搜索"),
            request.source.as_str()
        )
    } else {
        format!(
            "{} `{query}` @{}",
            i18n::text("searching", "正在搜索"),
            request.source.as_str()
        )
    };
    let spinner = SearchSpinner::start(spinner_label, spinner_enabled);
    let results = match search_mods(&request) {
        Ok(results) => results,
        Err(err) if source == SearchSourceArg::Mcmod => {
            eprintln!(
                "{}: {}",
                i18n::text(
                    "warning: mcmod.cn request failed, fallback to modrinth",
                    "警告：mcmod.cn 请求失败，已降级到 modrinth"
                ),
                err
            );
            let fallback = SearchRequest {
                source: SearchSource::Modrinth,
                query: query.clone(),
                limit,
                page,
                minecraft_version: minecraft_filter.clone(),
                loader: loader_filter,
            };
            search_mods(&fallback)?
        }
        Err(err) => return Err(err),
    };
    drop(spinner);

    if results.is_empty() {
        println!("{}", i18n::text("no results", "无结果"));
        return Ok(());
    }

    if install_version.is_some() && !install_first && !interactive {
        bail!("--install-version requires --install-first or interactive install");
    }

    if install_first {
        install_search_selection(
            root,
            &results[0],
            install_version.as_deref(),
            manifest_env.as_ref().map(|(value, _)| value.as_str()),
            manifest_env.as_ref().map(|(_, value)| *value),
            group.as_deref(),
        )?;
        return Ok(());
    }

    if interactive {
        if let Some(install_request) = search_tui::run_search_interactive(
            &query,
            &results,
            no_color,
            environment_label.as_deref(),
        )? {
            let selected = &results[install_request.index];
            let version = if let Some(version) = install_version.clone() {
                Some(version)
            } else if install_request.choose_version {
                let (id, source) = resolve_search_install_target(selected)?;
                search_tui::choose_install_version_interactive(
                    &id,
                    source,
                    selected.title.as_str(),
                    no_color,
                    minecraft_filter.as_deref(),
                    loader_filter,
                )?
            } else {
                None
            };
            if install_request.choose_version && version.is_none() {
                println!("{}", i18n::text("installation cancelled", "已取消安装"));
                return Ok(());
            }
            install_search_selection(
                root,
                selected,
                version.as_deref(),
                manifest_env.as_ref().map(|(value, _)| value.as_str()),
                manifest_env.as_ref().map(|(_, value)| *value),
                group.as_deref(),
            )?;
        }
        return Ok(());
    }

    let use_color =
        std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none() && !no_color;
    let layout = search_layout();
    println!(
        "{}",
        paint(
            &format!(
                "📋 {} ({})",
                i18n::text("Search Results", "搜索结果"),
                results.len()
            ),
            "1;35",
            use_color
        )
    );
    println!(
        "{}",
        paint(&"═".repeat(layout.total_width), "2;37", use_color)
    );
    for (index, item) in results.iter().enumerate() {
        print_search_result(item, index + 1, use_color, &layout);
    }

    Ok(())
}

pub(crate) fn loader_label(loader: LoaderKind) -> &'static str {
    match loader {
        LoaderKind::Fabric => "fabric",
        LoaderKind::Forge => "forge",
        LoaderKind::NeoForge => "neoforge",
        LoaderKind::Quilt => "quilt",
    }
}

pub(crate) fn to_run_loader_hint(loader: LoaderKind) -> LoaderHint {
    match loader {
        LoaderKind::Fabric => LoaderHint::Fabric,
        LoaderKind::Forge => LoaderHint::Forge,
        LoaderKind::NeoForge => LoaderHint::NeoForge,
        LoaderKind::Quilt => LoaderHint::Quilt,
    }
}

fn install_search_selection(
    root: &Path,
    item: &mineconda_resolver::SearchResult,
    version: Option<&str>,
    minecraft_version: Option<&str>,
    loader: Option<LoaderKind>,
    group: Option<&str>,
) -> Result<()> {
    let (id, source) = resolve_search_install_target(item)?;
    let mut manifest = load_manifest(root)?;
    let group = match group {
        Some(group) => normalize_group_selector(group)?,
        None => DEFAULT_GROUP_NAME.to_string(),
    };
    let side = item.supported_side.unwrap_or(ModSide::Both);
    let target_version =
        resolve_install_version_for_search(&id, source, version, minecraft_version, loader)?;

    if let Some(existing) = manifest
        .ensure_group_mods_mut(&group)
        .iter_mut()
        .find(|entry| entry.id == id && entry.source == source)
    {
        existing.version = target_version.to_string();
        existing.side = side;
    } else {
        manifest.ensure_group_mods_mut(&group).push(ModSpec::new(
            id.clone(),
            source,
            target_version.to_string(),
            side,
        ));
    }

    let path = manifest_path(root);
    write_manifest(&path, &manifest)
        .with_context(|| format!("failed to write {}", path.display()))?;
    let groups = if is_default_group_name(&group) {
        BTreeSet::new()
    } else {
        BTreeSet::from([group.clone()])
    };
    write_lock_from_manifest(root, &manifest, false, groups.clone())?;
    cmd_sync(
        root,
        SyncCommandArgs {
            prune: true,
            check: false,
            locked: false,
            offline: false,
            jobs: default_sync_jobs(),
            verbose_cache: false,
            groups: groups.into_iter().collect(),
            all_groups: false,
        },
        &[],
        None,
    )?;

    println!(
        "installed {} [{}]@{} and resolved dependencies",
        id,
        source.as_str(),
        target_version
    );
    Ok(())
}

fn resolve_install_version_for_search(
    id: &str,
    source: ModSource,
    explicit_version: Option<&str>,
    minecraft_version: Option<&str>,
    loader: Option<LoaderKind>,
) -> Result<String> {
    if let Some(version) = explicit_version {
        return Ok(version.to_string());
    }

    if !matches!(source, ModSource::Modrinth | ModSource::Curseforge) {
        return Ok("latest".to_string());
    }

    let versions = list_install_versions(&InstallVersionsRequest {
        source,
        id: id.to_string(),
        limit: 1,
        minecraft_version: minecraft_version.map(std::string::ToString::to_string),
        loader,
    });
    let versions = match versions {
        Ok(versions) => versions,
        Err(err) => {
            eprintln!(
                "warning: failed to preselect installable version for {} [{}], falling back to `latest`: {err}",
                id,
                source.as_str()
            );
            return Ok("latest".to_string());
        }
    };
    let selected = versions.into_iter().next().with_context(|| {
        format!(
            "no installable versions found for {} [{}]",
            id,
            source.as_str()
        )
    })?;
    Ok(selected.value)
}

pub(crate) fn resolve_search_install_target(
    item: &mineconda_resolver::SearchResult,
) -> Result<(String, ModSource)> {
    match item.source {
        SearchSource::Modrinth => {
            let id = optional_value(item.slug.as_str())
                .or_else(|| optional_value(item.id.as_str()))
                .context("selected Modrinth result has empty id/slug")?;
            Ok((id.to_string(), ModSource::Modrinth))
        }
        SearchSource::Curseforge => {
            if let Some(id) = optional_value(item.id.as_str())
                && id.chars().all(|ch| ch.is_ascii_digit())
            {
                return Ok((id.to_string(), ModSource::Curseforge));
            }
            if let Some(id) =
                optional_value(item.url.as_str()).and_then(extract_curseforge_project_id)
            {
                return Ok((id, ModSource::Curseforge));
            }
            bail!("selected CurseForge result is missing numeric project id")
        }
        SearchSource::Mcmod => {
            if let Some(url) = item.linked_modrinth_url.as_deref()
                && let Some(slug) = extract_modrinth_slug(url)
            {
                return Ok((slug, ModSource::Modrinth));
            }
            if let Some(url) = item.linked_curseforge_url.as_deref()
                && let Some(project_id) = extract_curseforge_project_id(url)
            {
                return Ok((project_id, ModSource::Curseforge));
            }
            bail!(
                "selected mcmod item has no installable Modrinth/CurseForge identifier; choose an item with source links"
            )
        }
    }
}

pub(crate) fn extract_modrinth_slug(url: &str) -> Option<String> {
    let normalized = url.split(['?', '#']).next().unwrap_or(url);
    let parts: Vec<&str> = normalized
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    for (index, part) in parts.iter().enumerate() {
        if matches!(
            *part,
            "mod" | "plugin" | "modpack" | "resourcepack" | "shader"
        ) && let Some(slug) = parts.get(index + 1)
            && !slug.is_empty()
        {
            return Some((*slug).to_string());
        }
    }
    None
}

pub(crate) fn extract_curseforge_project_id(url: &str) -> Option<String> {
    let normalized = url.split(['?', '#']).next().unwrap_or(url);
    for part in normalized.split('/') {
        if !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()) {
            return Some(part.to_string());
        }
    }

    if let Some(index) = normalized.find("modId=") {
        let value = &normalized[index + "modId=".len()..];
        let digits: String = value.chars().take_while(|ch| ch.is_ascii_digit()).collect();
        if !digits.is_empty() {
            return Some(digits);
        }
    }

    None
}

#[derive(Debug, Clone, Copy)]
struct SearchLayout {
    total_width: usize,
    two_col: bool,
    col_width: usize,
    content_width: usize,
}

pub(crate) struct SearchSpinner {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    enabled: bool,
}

impl SearchSpinner {
    pub(crate) fn start(label: String, enabled: bool) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        if !enabled {
            return Self {
                stop,
                handle: None,
                enabled: false,
            };
        }

        let stop_signal = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let mut index = 0usize;
            while !stop_signal.load(Ordering::Relaxed) {
                eprint!("\r{} {}", frames[index % frames.len()], label);
                let _ = std::io::stderr().flush();
                thread::sleep(Duration::from_millis(90));
                index += 1;
            }
        });

        Self {
            stop,
            handle: Some(handle),
            enabled: true,
        }
    }
}

impl Drop for SearchSpinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        if self.enabled {
            eprint!("\r\x1b[2K");
            let _ = std::io::stderr().flush();
        }
    }
}

fn search_layout() -> SearchLayout {
    let width = detect_terminal_width().clamp(60, 180);
    let two_col = width >= 96;
    let col_width = if two_col {
        width.saturating_sub(3) / 2
    } else {
        width.saturating_sub(2)
    };
    SearchLayout {
        total_width: width,
        two_col,
        col_width,
        content_width: width.saturating_sub(4).max(20),
    }
}

fn detect_terminal_width() -> usize {
    if let Some((Width(w), _)) = terminal_size()
        && w > 0
    {
        return w as usize;
    }

    if let Ok(raw) = std::env::var("COLUMNS")
        && let Ok(parsed) = raw.parse::<usize>()
        && parsed > 0
    {
        return parsed;
    }

    100
}

fn print_search_result(
    item: &mineconda_resolver::SearchResult,
    index: usize,
    use_color: bool,
    layout: &SearchLayout,
) {
    let downloads = item
        .downloads
        .map(|v| format!("📥 {}: {v}", i18n::text("Downloads", "下载")));
    let dependencies = if item.dependencies.is_empty() {
        None
    } else {
        Some(format!(
            "    🧩 {}: {}",
            i18n::text("Dependencies", "依赖"),
            item.dependencies.join(", ")
        ))
    };
    let homepage = optional_value(item.url.as_str())
        .map(|value| format!("🔗 {}: {value}", i18n::text("Homepage", "主页")));
    let supported_side = format_supported_side(item.supported_side);

    let title = paint(
        &format!(
            "🔎 {} #{index}  {}  ({})",
            i18n::text("Result", "结果"),
            item.title,
            item.source.as_str()
        ),
        "1;36",
        use_color,
    );
    let divider = paint(&"─".repeat(layout.total_width), "2;37", use_color);
    println!("{title}");
    println!("{divider}");

    print_two_col_optional(
        Some(format!(" └─ 🆔 ID: {}", item.id)),
        downloads,
        use_color,
        layout,
    );

    let slug = optional_value(item.slug.as_str()).map(|value| format!("    🏷️ Slug: {value}"));
    print_two_col_optional(
        slug,
        Some(format!(
            "🌐 {}: {}",
            i18n::text("Source", "源"),
            item.source.as_str()
        )),
        use_color,
        layout,
    );
    print_two_col_optional(dependencies, homepage, use_color, layout);

    if let Some(side) = supported_side {
        print_wrapped_kv(
            &format!("🖥️ {}:", i18n::text("Supported side", "适用端")),
            side,
            "0;37",
            use_color,
            layout,
        );
    }

    if let Some(summary) = optional_value(item.summary.as_str()) {
        println!(
            "{}",
            paint(
                &format!("📝 {}:", i18n::text("Summary", "简介")),
                "1;33",
                use_color
            )
        );
        for line in wrap_visual(summary, layout.content_width) {
            println!("  {}", paint(&line, "0;37", use_color));
        }
    }

    let link_items = [
        ("🌏 mcmod:", item.source_homepage.as_deref(), "36"),
        ("🌙 modrinth:", item.linked_modrinth_url.as_deref(), "32"),
        (
            "⚒️ curseforge:",
            item.linked_curseforge_url.as_deref(),
            "33",
        ),
        ("🐙 github:", item.linked_github_url.as_deref(), "1;34"),
    ];
    let has_any_link = link_items
        .iter()
        .any(|(_, value, _)| value.and_then(optional_value).is_some());
    if has_any_link {
        println!(
            "{}",
            paint(
                &format!("📚 {}:", i18n::text("Source links", "来源链接")),
                "1;35",
                use_color
            )
        );
        for (key, value, color) in link_items {
            if let Some(value) = value.and_then(optional_value) {
                print_wrapped_kv(key, value, color, use_color, layout);
            }
        }
    }
    println!();
}

fn print_two_col(left: &str, right: &str, use_color: bool, layout: &SearchLayout) {
    if layout.two_col {
        let left = truncate_visual(left, layout.col_width);
        let right = truncate_visual(right, layout.col_width);
        let left = pad_visual(&left, layout.col_width);
        let right = pad_visual(&right, layout.col_width);
        println!("{}", paint(&format!("{left} │ {right}"), "0;37", use_color));
        return;
    }

    for line in wrap_visual(left, layout.content_width) {
        println!("{}", paint(&line, "0;37", use_color));
    }
    for line in wrap_visual(right, layout.content_width) {
        println!("{}", paint(&line, "0;37", use_color));
    }
}

fn print_two_col_optional(
    left: Option<String>,
    right: Option<String>,
    use_color: bool,
    layout: &SearchLayout,
) {
    match (left, right) {
        (Some(left), Some(right)) => print_two_col(&left, &right, use_color, layout),
        (Some(line), None) | (None, Some(line)) => {
            for line in wrap_visual(line.trim(), layout.content_width) {
                println!("{}", paint(&line, "0;37", use_color));
            }
        }
        (None, None) => {}
    }
}

fn print_wrapped_kv(
    key: &str,
    value: &str,
    key_color: &str,
    use_color: bool,
    layout: &SearchLayout,
) {
    let Some(value) = optional_value(value) else {
        return;
    };
    let prefix = format!("  {key} ");
    let available = layout
        .total_width
        .saturating_sub(display_width(&prefix))
        .max(20);
    let wrapped = wrap_visual(value, available);
    if wrapped.is_empty() {
        println!("{} ", paint(&prefix, key_color, use_color));
        return;
    }

    println!(
        "{}{}",
        paint(&prefix, key_color, use_color),
        wrapped[0].as_str()
    );
    let indent = " ".repeat(display_width(&prefix));
    for line in wrapped.iter().skip(1) {
        println!("{indent}{line}");
    }
}

pub(crate) fn format_supported_side(side: Option<ModSide>) -> Option<&'static str> {
    side.map(|value| match value {
        ModSide::Both => i18n::text("both", "双端"),
        ModSide::Client => i18n::text("client", "客户端"),
        ModSide::Server => i18n::text("server", "服务端"),
    })
}

pub(crate) fn optional_value(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "-" {
        None
    } else {
        Some(trimmed)
    }
}

pub(crate) fn truncate_visual(input: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }

    if display_width(input) <= max_width {
        return input.to_string();
    }

    if max_width == 1 {
        return "…".to_string();
    }

    let mut value = String::new();
    let mut width = 0usize;
    let keep_width = max_width - 1;
    for ch in input.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if !value.is_empty() && width + ch_width > keep_width {
            break;
        }
        if value.is_empty() && ch_width > keep_width {
            break;
        }
        value.push(ch);
        width += ch_width;
    }
    value.push('…');
    value
}

pub(crate) fn wrap_visual(input: &str, max_width: usize) -> Vec<String> {
    if input.trim().is_empty() {
        return vec!["-".to_string()];
    }
    if max_width == 0 {
        return vec![input.to_string()];
    }

    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    for word in input.split_whitespace() {
        let word_width = display_width(word);
        if word_width > max_width {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
            }
            lines.extend(split_by_width(word, max_width));
            continue;
        }

        let spacer = if current.is_empty() { 0 } else { 1 };
        if current_width + spacer + word_width > max_width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        if !current.is_empty() {
            current.push(' ');
            current_width += 1;
        }
        current.push_str(word);
        current_width += word_width;
    }

    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.extend(split_by_width(input, max_width));
    }
    if lines.is_empty() {
        lines.push("-".to_string());
    }
    lines
}

fn split_by_width(input: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![input.to_string()];
    }

    let mut out = Vec::new();
    let mut chunk = String::new();
    let mut chunk_width = 0usize;

    for ch in input.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if chunk_width + ch_width > max_width && !chunk.is_empty() {
            out.push(std::mem::take(&mut chunk));
            chunk_width = 0;
        }
        if ch_width > max_width {
            if !chunk.is_empty() {
                out.push(std::mem::take(&mut chunk));
                chunk_width = 0;
            }
            out.push(ch.to_string());
            continue;
        }
        chunk.push(ch);
        chunk_width += ch_width;
        if chunk_width >= max_width {
            out.push(std::mem::take(&mut chunk));
            chunk_width = 0;
        }
    }

    if !chunk.is_empty() {
        out.push(chunk);
    }
    if out.is_empty() {
        out.push(input.to_string());
    }
    out
}

pub(crate) fn display_width(input: &str) -> usize {
    UnicodeWidthStr::width(input)
}

fn pad_visual(input: &str, target_width: usize) -> String {
    let mut value = input.to_string();
    let pad = target_width.saturating_sub(display_width(input));
    if pad > 0 {
        value.push_str(&" ".repeat(pad));
    }
    value
}

pub(crate) fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut idx = 0usize;
    while value >= 1024.0 && idx + 1 < UNITS.len() {
        value /= 1024.0;
        idx += 1;
    }
    if idx == 0 {
        format!("{} {}", bytes, UNITS[idx])
    } else {
        format!("{value:.1} {}", UNITS[idx])
    }
}

pub(crate) fn paint(text: &str, code: &str, enabled: bool) -> String {
    if !enabled {
        return text.to_string();
    }
    format!("\x1b[{code}m{text}\x1b[0m")
}

#[cfg(test)]
mod tests {
    use mineconda_core::ModSource;
    use mineconda_resolver::{SearchResult, SearchSource};

    use super::*;

    #[test]
    fn wrap_visual_keeps_cjk_line_width() {
        let lines = wrap_visual("这是一个中文简介用于测试终端宽度自动换行效果", 12);
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|line| display_width(line) <= 12));
    }

    #[test]
    fn wrap_visual_keeps_emoji_line_width() {
        let lines = wrap_visual("emoji 😀 shader pack loader for minecraft", 14);
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|line| display_width(line) <= 14));
    }

    #[test]
    fn truncate_visual_respects_display_width() {
        let value = truncate_visual("中文Minecraft模组管理器", 10);
        assert!(value.ends_with('…'));
        assert!(display_width(&value) <= 10);
    }

    #[test]
    fn extract_modrinth_slug_works() {
        let slug = extract_modrinth_slug("https://modrinth.com/mod/sodium?foo=bar");
        assert_eq!(slug.as_deref(), Some("sodium"));
    }

    #[test]
    fn extract_curseforge_project_id_works() {
        let id = extract_curseforge_project_id("https://www.curseforge.com/projects/394468");
        assert_eq!(id.as_deref(), Some("394468"));
    }

    #[test]
    fn resolve_install_target_prefers_modrinth_for_mcmod_item() {
        let item = SearchResult {
            id: "123".to_string(),
            slug: "123".to_string(),
            title: "Sodium".to_string(),
            summary: "test".to_string(),
            source: SearchSource::Mcmod,
            downloads: None,
            url: "https://www.mcmod.cn/class/123.html".to_string(),
            dependencies: Vec::new(),
            supported_side: None,
            source_homepage: Some("https://www.mcmod.cn/class/123.html".to_string()),
            linked_modrinth_url: Some("https://modrinth.com/mod/sodium".to_string()),
            linked_curseforge_url: Some("https://www.curseforge.com/projects/394468".to_string()),
            linked_github_url: None,
        };
        let (id, source) = resolve_search_install_target(&item).expect("resolve target");
        assert_eq!(id, "sodium");
        assert_eq!(source, ModSource::Modrinth);
    }
}
