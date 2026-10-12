use crate::client::Client;
use crate::error::{Error, Result};
use crate::models::{Episode, EpisodeDetails, MediaServer, Player, PlayerKind, VideoQuality};
use crate::site::Site;

/// Готовая к воспроизведению или скачиванию ссылка плеера Animelib.
#[derive(Debug, Clone)]
pub struct ResolvedVideo {
    pub url: String,
    /// Тот же файл на остальных серверах, на случай если первый закрыт.
    pub fallbacks: Vec<String>,
    pub quality: u64,
    pub subtitles: Vec<ResolvedSubtitle>,
}

#[derive(Debug, Clone)]
pub struct ResolvedSubtitle {
    pub url: String,
    pub format: String,
}

impl Client {
    /// Список эпизодов аниме по `slug_url`.
    pub async fn episodes(&self, slug_url: &str) -> Result<Vec<Episode>> {
        self.get_data(Site::AnimeLib, "episodes", &[("anime_id", slug_url)])
            .await
    }

    /// Эпизод со всеми вариантами просмотра. Плеер Animelib сервер показывает
    /// только после входа.
    pub async fn episode(&self, episode_id: u64) -> Result<EpisodeDetails> {
        self.get_data(Site::AnimeLib, &format!("episodes/{episode_id}"), &())
            .await
    }

    /// Серверы видео из справочника.
    pub async fn video_servers(&self) -> Result<Vec<MediaServer>> {
        Ok(self.constants().await?.video_servers)
    }

    /// Собирает адрес видео и субтитров для плеера Animelib. Порядок серверов: выбранный
    /// пользователем, указанный в ответе, остальные; недавно отказавшие идут последними.
    /// С `verify` первый адрес проверяется запросом, и при отказе берётся следующий.
    pub async fn resolve_video(
        &self,
        player: &Player,
        max_quality: u64,
        server_id: Option<&str>,
        verify: bool,
    ) -> Result<ResolvedVideo> {
        if player.kind() != PlayerKind::Animelib {
            return Err(Error::Other(
                "у этого варианта нет прямой ссылки, он открывается в браузере".into(),
            ));
        }
        let video = player
            .video
            .as_ref()
            .ok_or_else(|| Error::Other("у озвучки нет видео".into()))?;
        let quality: &VideoQuality = video
            .pick(max_quality)
            .ok_or_else(|| Error::Other("у озвучки нет ни одного качества".into()))?;
        let mut servers = self.video_servers().await?;
        servers.retain(|s| !s.url.trim().is_empty());
        if servers.is_empty() {
            return Err(Error::Other("сервер не прислал адресов видео".into()));
        }
        let rank = |s: &MediaServer| {
            if Some(s.id.as_str()) == server_id {
                0
            } else if Some(s.id.as_str()) == player.video_domain.as_deref() {
                1
            } else {
                2
            }
        };
        servers.sort_by_key(rank);

        let mut urls = self.order_by_health(servers.iter().map(|s| s.join(&quality.href)).collect());
        if verify {
            let mut alive = None;
            for (i, url) in urls.iter().enumerate() {
                if self.probe_url(Site::AnimeLib, url).await {
                    alive = Some(i);
                    break;
                }
            }
            match alive {
                Some(i) => urls.rotate_left(i),
                None => return Err(Error::Other("ни один сервер видео не ответил".into())),
            }
        }
        let main = urls.remove(0);
        let server = servers
            .iter()
            .find(|s| main.starts_with(s.url.trim_end_matches('/')))
            .unwrap_or(&servers[0]);
        let subtitles = player
            .subtitles
            .iter()
            .map(|s| ResolvedSubtitle {
                url: if s.src.starts_with("//") {
                    format!("https:{}", s.src)
                } else {
                    server.join(&s.src)
                },
                format: s.format.clone().unwrap_or_else(|| "vtt".into()),
            })
            .collect();
        Ok(ResolvedVideo {
            url: main,
            fallbacks: urls,
            quality: quality.quality,
            subtitles,
        })
    }
}
