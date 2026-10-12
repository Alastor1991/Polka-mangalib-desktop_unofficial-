use crate::client::Client;
use crate::error::{Error, Result};
use crate::models::{Episode, EpisodeDetails, MediaServer, Player, PlayerKind, VideoQuality};
use crate::site::Site;

/// Готовая к воспроизведению или скачиванию ссылка плеера Animelib.
#[derive(Debug, Clone)]
pub struct ResolvedVideo {
    pub url: String,
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

    /// Собирает адрес видео и субтитров для плеера Animelib.
    pub async fn resolve_video(
        &self,
        player: &Player,
        max_quality: u64,
        server_id: Option<&str>,
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
        let servers = self.video_servers().await?;
        let server = server_id
            .and_then(|id| servers.iter().find(|s| s.id == id))
            .or_else(|| {
                player
                    .video_domain
                    .as_deref()
                    .and_then(|id| servers.iter().find(|s| s.id == id))
            })
            .or(servers.first())
            .ok_or_else(|| Error::Other("сервер не прислал адресов видео".into()))?;
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
            url: server.join(&quality.href),
            quality: quality.quality,
            subtitles,
        })
    }
}
