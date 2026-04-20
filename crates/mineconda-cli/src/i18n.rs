use std::env;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppLang {
    En,
    ZhCn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LangPreference {
    Auto,
    En,
    ZhCn,
}

static GLOBAL_LANG: OnceLock<AppLang> = OnceLock::new();

pub fn init(preference: LangPreference) {
    let _ = GLOBAL_LANG.set(resolve(preference));
}

pub fn current() -> AppLang {
    *GLOBAL_LANG.get_or_init(|| resolve(LangPreference::Auto))
}

pub fn text<'a>(en: &'a str, zh_cn: &'a str) -> &'a str {
    match current() {
        AppLang::En => en,
        AppLang::ZhCn => zh_cn,
    }
}

fn resolve(preference: LangPreference) -> AppLang {
    match preference {
        LangPreference::En => return AppLang::En,
        LangPreference::ZhCn => return AppLang::ZhCn,
        LangPreference::Auto => {}
    }

    if let Some(value) = env::var_os("MINECONDA_LANG")
        && let Some(lang) = parse_locale(value.to_string_lossy().as_ref())
    {
        return lang;
    }

    for key in ["LC_ALL", "LANG"] {
        if let Some(value) = env::var_os(key)
            && let Some(lang) = parse_locale(value.to_string_lossy().as_ref())
        {
            return lang;
        }
    }

    AppLang::En
}

fn parse_locale(raw: &str) -> Option<AppLang> {
    let normalized = raw
        .trim()
        .to_ascii_lowercase()
        .replace('_', "-")
        .split('.')
        .next()
        .unwrap_or("")
        .to_string();
    if normalized.is_empty() || normalized == "auto" {
        return None;
    }
    if normalized.starts_with("zh") {
        return Some(AppLang::ZhCn);
    }
    if normalized.starts_with("en") {
        return Some(AppLang::En);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{AppLang, parse_locale};

    #[test]
    fn parse_locale_supports_zh_aliases() {
        assert_eq!(parse_locale("zh"), Some(AppLang::ZhCn));
        assert_eq!(parse_locale("zh-CN"), Some(AppLang::ZhCn));
        assert_eq!(parse_locale("zh_CN.UTF-8"), Some(AppLang::ZhCn));
    }

    #[test]
    fn parse_locale_supports_en_aliases() {
        assert_eq!(parse_locale("en"), Some(AppLang::En));
        assert_eq!(parse_locale("en_US"), Some(AppLang::En));
        assert_eq!(parse_locale("en-US.UTF-8"), Some(AppLang::En));
    }

    #[test]
    fn parse_locale_rejects_unknown_and_auto() {
        assert_eq!(parse_locale("auto"), None);
        assert_eq!(parse_locale("fr-FR"), None);
        assert_eq!(parse_locale(""), None);
    }
}
