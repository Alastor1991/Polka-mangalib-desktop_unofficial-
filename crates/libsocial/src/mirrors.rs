//! Зеркала сервисов. Домены lib.social регулярно блокируют то в одной стране, то в
//! другой, поэтому у каждой части сервиса есть несколько адресов. Списки проверены в
//! октябре 2026 года из России и через VPN с выходом в Германии.

use crate::site::Site;

/// Адреса API, порядок важен: первый рабочий клиент запоминает.
pub const API: &[&str] = &[
    "https://api.cdnlibs.org/api",
    "https://api.animelib.org/api",
    "https://hapi.hentaicdn.org/api",
    "https://api2.mangalib.me/api",
    "https://api.imglib.info/api",
];

/// Серверы авторизации. Страница входа открывается в браузере пользователя.
pub const AUTH: &[&str] = &["https://auth.lib.social", "https://auth.cdnlibs.org"];

/// Адреса сайтов. Первый используется для возврата после входа и ссылок «открыть на
/// сайте», поэтому первым стоит домен, который открывается и в России, и через VPN.
pub fn web(site: Site) -> &'static [&'static str] {
    match site {
        Site::MangaLib => &["https://mangalib.org", "https://mangalib.me"],
        Site::SlashLib => &["https://slashlib.me", "https://v2.slashlib.me"],
        Site::HentaiLib => &["https://hentailib.me"],
        Site::AnimeLib => &["https://animelib.org", "https://v3.animelib.org", "https://anilib.me"],
    }
}

/// Список из настроек, а если он пуст, значения по умолчанию.
pub fn or_default(custom: &[String], default: &[&str]) -> Vec<String> {
    let custom: Vec<String> = custom
        .iter()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if custom.is_empty() {
        default.iter().map(|s| s.to_string()).collect()
    } else {
        custom
    }
}

/// Хост из адреса, для журнала неудачных серверов.
pub(crate) fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    rest.split(['/', '?']).next().unwrap_or(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts() {
        assert_eq!(host_of("https://img3.cdnlibs.org//manga/x.jpg"), "img3.cdnlibs.org");
        assert_eq!(host_of("https://video1.cdnlibs.org/.аs/a.mp4"), "video1.cdnlibs.org");
    }

    #[test]
    fn custom_lists() {
        assert_eq!(or_default(&[], API).len(), API.len());
        assert_eq!(or_default(&[" https://x/api/ ".into()], API), vec!["https://x/api"]);
    }
}
