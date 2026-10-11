//! The two interface languages: Simplified Chinese for a Chinese session,
//! English for every other.
//!
//! Both the panel and the daemon (whose messages reach the panel) decide from
//! the same environment, the way gettext would: a `C`/`POSIX` locale means no
//! translation, otherwise the first entry of `LANGUAGE` wins, then `LC_ALL`,
//! `LC_MESSAGES` and `LANG`. Text is written next to its use as an
//! English/Chinese pair with [`tr!`](crate::tr) or [`trf!`](crate::trf), so
//! a missing translation is a compile error rather than a blank label.

use std::sync::OnceLock;

/// Whether this process shows Chinese. Decided once; a session's language
/// does not change under a running process.
pub fn is_chinese() -> bool {
    static CHINESE: OnceLock<bool> = OnceLock::new();
    *CHINESE.get_or_init(|| prefers_chinese(|name| std::env::var(name).ok()))
}

/// The decision itself, over any environment lookup so tests need not mutate
/// the process environment.
pub fn prefers_chinese(var: impl Fn(&str) -> Option<String>) -> bool {
    let set = |name: &str| var(name).filter(|value| !value.is_empty());
    let locale = set("LC_ALL")
        .or_else(|| set("LC_MESSAGES"))
        .or_else(|| set("LANG"));
    // gettext ignores LANGUAGE under the C locale, and so does this.
    if matches!(locale.as_deref(), None | Some("C" | "POSIX"))
        || locale.as_deref().is_some_and(|l| l.starts_with("C."))
    {
        return false;
    }
    let preferred = set("LANGUAGE")
        .and_then(|list| {
            list.split(':')
                .find(|entry| !entry.is_empty())
                .map(str::to_owned)
        })
        .or(locale)
        .unwrap_or_default();
    preferred.starts_with("zh")
}

/// A count of items: "1 item", "3 items", "3 项".
pub fn items(count: u64) -> String {
    match (is_chinese(), count) {
        (true, _) => format!("{count} 项"),
        (false, 1) => "1 item".into(),
        (false, _) => format!("{count} items"),
    }
}

/// A fixed string in both languages: `tr!("Sync now", "立即同步")`.
#[macro_export]
macro_rules! tr {
    ($en:literal, $zh:literal $(,)?) => {
        if $crate::i18n::is_chinese() {
            $zh
        } else {
            $en
        }
    };
}

/// A formatted string in both languages, with the same arguments:
/// `trf!("{n} files", "{n} 个文件")`.
#[macro_export]
macro_rules! trf {
    ($en:literal, $zh:literal $(, $($args:tt)*)?) => {
        if $crate::i18n::is_chinese() {
            format!($zh $(, $($args)*)?)
        } else {
            format!($en $(, $($args)*)?)
        }
    };
}

#[cfg(test)]
mod tests {
    use super::prefers_chinese;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    }

    #[test]
    fn chinese_sessions_get_chinese() {
        assert!(prefers_chinese(env(&[("LANG", "zh_CN.UTF-8")])));
        assert!(prefers_chinese(env(&[("LANG", "zh_TW.UTF-8")])));
        assert!(prefers_chinese(env(&[
            ("LANG", "en_US.UTF-8"),
            ("LC_MESSAGES", "zh_CN.UTF-8")
        ])));
        assert!(prefers_chinese(env(&[
            ("LANG", "en_US.UTF-8"),
            ("LANGUAGE", "zh_CN:en")
        ])));
    }

    #[test]
    fn everything_else_gets_english() {
        assert!(!prefers_chinese(env(&[])));
        assert!(!prefers_chinese(env(&[("LANG", "en_US.UTF-8")])));
        assert!(!prefers_chinese(env(&[("LANG", "de_DE.UTF-8")])));
        assert!(!prefers_chinese(env(&[
            ("LANG", "zh_CN.UTF-8"),
            ("LANGUAGE", "en_GB:zh_CN")
        ])));
        assert!(!prefers_chinese(env(&[
            ("LANG", "zh_CN.UTF-8"),
            ("LC_ALL", "C")
        ])));
        assert!(!prefers_chinese(env(&[
            ("LANGUAGE", "zh_CN"),
            ("LANG", "C.UTF-8")
        ])));
    }
}
