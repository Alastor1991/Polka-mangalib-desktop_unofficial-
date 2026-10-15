//! Отметки для сервиса: прочитанные главы, просмотренные серии и место в закладке.
//!
//! Отметка сначала пишется в базу (таблица `outbox`), потом фоновая задача отправляет
//! её на сайт. Без сети или без входа отметки копятся и уходят позже, поэтому главы,
//! прочитанные без сети, тоже попадают в историю аккаунта.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use libsocial::{Client, Place, Site};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::db::Db;

/// Как часто заглядывать в очередь без сигнала: вдруг появилась сеть или вход.
const IDLE: Duration = Duration::from_secs(60);
/// Сколько подождать после сигнала. Читатель листает главы подряд, и отметки
/// о нескольких главах уходят одной пачкой, а место в закладке – одно, последнее.
const SETTLE: Duration = Duration::from_secs(3);
/// После стольких неудач отметка, которую сайт не принимает, выбрасывается.
const MAX_ATTEMPTS: u32 = 20;
/// Запас на расхождение часов устройства и сервера при сравнении времени закладки.
const CLOCK_SKEW: u64 = 120;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Mark {
    /// `chapter_id` – id перевода главы в выбранной ветке.
    ChapterViewed {
        site: Site,
        title_id: u64,
        chapter_id: u64,
    },
    EpisodeViewed {
        anime_id: u64,
        player_id: u64,
    },
    /// Место, где остановились. Пишется, только если тайтл уже в закладках.
    Place {
        site: Site,
        slug: String,
        place: Place,
    },
}

impl Mark {
    /// Ключ в очереди. У места он один на тайтл: важно только последнее.
    fn key(&self) -> String {
        match self {
            Mark::ChapterViewed { site, chapter_id, .. } => format!("chapter:{}:{chapter_id}", site.key()),
            Mark::EpisodeViewed { player_id, .. } => format!("episode:{player_id}"),
            Mark::Place { site, slug, .. } => format!("place:{}:{slug}", site.key()),
        }
    }
}

/// Очередь отметок. Дёшево клонируется.
#[derive(Clone)]
pub struct Sync {
    db: Db,
    wake: Arc<Notify>,
}

impl Sync {
    /// Создаёт очередь и запускает отправку в текущем рантайме tokio.
    pub fn start(client: Client, db: Db) -> Sync {
        let sync = Sync {
            db,
            wake: Arc::new(Notify::new()),
        };
        let (db, wake) = (sync.db.clone(), sync.wake.clone());
        tokio::spawn(async move {
            loop {
                let _ = tokio::time::timeout(IDLE, wake.notified()).await;
                tokio::time::sleep(SETTLE).await;
                if let Some(wait) = client.cooldown_remaining() {
                    tokio::time::sleep(wait).await;
                }
                if client.is_logged_in().await {
                    flush(&client, &db).await;
                }
            }
        });
        sync
    }

    /// Очередь без отправки: отметки только копятся в базе. Для тестов.
    #[cfg(test)]
    pub fn detached(db: Db) -> Sync {
        Sync {
            db,
            wake: Arc::new(Notify::new()),
        }
    }

    pub fn push(&self, mark: Mark) {
        let saved = serde_json::to_string(&mark)
            .map_err(anyhow::Error::from)
            .and_then(|json| self.db.push_mark(&mark.key(), &json));
        match saved {
            Ok(()) => self.wake.notify_one(),
            Err(e) => eprintln!("не удалось запомнить отметку для сайта: {e:#}"),
        }
    }
}

/// Отправляет накопившиеся отметки. Если сервис недоступен или просит подождать,
/// останавливается: остальное уйдёт в следующий раз.
pub async fn flush(client: &Client, db: &Db) {
    let marks = match db.pending_marks(100) {
        Ok(marks) => marks,
        Err(e) => {
            eprintln!("очередь отметок недоступна: {e:#}");
            return;
        }
    };
    for (key, json, created_at) in marks {
        let Ok(mark) = serde_json::from_str::<Mark>(&json) else {
            // Отметка от другой версии Полки, которую эта не понимает.
            let _ = db.drop_mark(&key, &json);
            continue;
        };
        match send(client, &key, &mark, created_at).await {
            Ok(()) => {}
            Err(e) => match fate(&e) {
                Fate::Stop => return,
                Fate::Later => {
                    // Неудачные уходят в конец очереди и после предела выбрасываются,
                    // чтобы одна отметка не держала остальные вечно.
                    if db.mark_failed(&key, &json).unwrap_or(0) < MAX_ATTEMPTS {
                        continue;
                    }
                    eprintln!("отметка {key} не ушла за {MAX_ATTEMPTS} попыток: {e}");
                }
                Fate::Drop => eprintln!("сайт не принял отметку {key}: {e}"),
            },
        }
        let _ = db.drop_mark(&key, &json);
    }
}

enum Fate {
    /// Сервис недоступен целиком: дальше не идём.
    Stop,
    /// Сбой у этой отметки: повторим её позже.
    Later,
    /// Сервис её отверг (нет главы, устарела): повторять бессмысленно.
    Drop,
}

fn fate(e: &libsocial::Error) -> Fate {
    use libsocial::Error;
    match e {
        Error::Network(_) | Error::ServiceDown { .. } | Error::RateLimited { .. } | Error::Unauthorized => Fate::Stop,
        Error::Forbidden => Fate::Later,
        Error::Http { status, .. } if status.is_server_error() => Fate::Later,
        _ => Fate::Drop,
    }
}

/// Время закладок, которые записала сама Полка в этом запуске, по ключу места.
/// Свою запись не принимаем за правку с другого устройства.
fn own_writes() -> &'static Mutex<HashMap<String, u64>> {
    static OWN: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    OWN.get_or_init(Default::default)
}

async fn send(client: &Client, key: &str, mark: &Mark, created_at: u64) -> libsocial::Result<()> {
    match mark {
        Mark::ChapterViewed {
            site,
            title_id,
            chapter_id,
        } => client.mark_chapter_viewed(*site, *title_id, *chapter_id).await,
        Mark::EpisodeViewed { anime_id, player_id } => client.mark_episode_viewed(*anime_id, *player_id).await,
        Mark::Place { site, slug, place } => {
            // Тайтла нет в закладках: место хранить негде, сами закладку не заводим.
            let Some(bookmark) = client.bookmark(*site, slug).await? else {
                return Ok(());
            };
            // На другом устройстве закладку меняли позже: назад её не откатываем.
            let updated = bookmark.updated_secs();
            let ours =
                updated.is_some() && own_writes().lock().expect("own_writes отравлен").get(key) == updated.as_ref();
            if !ours && updated.is_some_and(|t| t > created_at + CLOCK_SKEW) {
                return Ok(());
            }
            if bookmark.item_id == Some(place.item_id) && bookmark.progress == place.progress {
                return Ok(());
            }
            let saved = client.set_bookmark(*site, slug, bookmark.status, Some(place)).await?;
            if let Some(t) = saved.updated_secs() {
                own_writes()
                    .lock()
                    .expect("own_writes отравлен")
                    .insert(key.to_string(), t);
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use libsocial::ClientConfig;
    use serde_json::json;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn place(item_id: u64) -> Place {
        Place {
            item_id,
            item_number: Some(2),
            progress: Some("5".into()),
            player: None,
        }
    }

    fn bookmark(updated_at: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({"data": {
            "id": 1, "status": 4, "item_id": 1, "progress": "1", "meta": {}, "updated_at": updated_at
        }}))
    }

    #[tokio::test]
    async fn marks_reach_the_site() {
        let s = MockServer::start().await;
        let ok = || ResponseTemplate::new(200).set_body_json(json!({"data": {"message": null}}));
        Mock::given(method("POST"))
            .and(path("/api/manga/7/chapters/70/view"))
            .respond_with(ok())
            .expect(1)
            .mount(&s)
            .await;
        // Главы на сервере больше нет: отметку выбрасываем, а не повторяем вечно.
        Mock::given(method("POST"))
            .and(path("/api/manga/7/chapters/71/view"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message": "нет"})))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/anime/8/players/80/view"))
            .respond_with(ok())
            .expect(1)
            .mount(&s)
            .await;
        // В закладке старое место: обновляем, папку не трогаем.
        Mock::given(method("GET"))
            .and(path("/api/manga/1--old/bookmark"))
            .respond_with(bookmark("2020-01-01T00:00:00.000000Z"))
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/bookmarks"))
            .and(body_partial_json(
                json!({"media_slug": "1--old", "bookmark": {"status": 4, "item_id": 9}}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"id": 1, "status": 4}})))
            .expect(1)
            .mount(&s)
            .await;
        // Закладку двигали на другом устройстве позже: не откатываем.
        Mock::given(method("GET"))
            .and(path("/api/manga/2--newer/bookmark"))
            .respond_with(bookmark("2099-01-01T00:00:00.000000Z"))
            .mount(&s)
            .await;
        // Тайтла нет в закладках: закладку сами не заводим.
        Mock::given(method("GET"))
            .and(path("/api/manga/3--none/bookmark"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": null})))
            .mount(&s)
            .await;

        let client = Client::new(ClientConfig {
            api_mirrors: vec![format!("{}/api", s.uri())],
            api_interval: Duration::ZERO,
            ..Default::default()
        })
        .unwrap();
        let db = Db::in_memory().unwrap();
        let sync = Sync {
            db: db.clone(),
            wake: Arc::new(Notify::new()),
        };
        sync.push(Mark::ChapterViewed {
            site: Site::MangaLib,
            title_id: 7,
            chapter_id: 70,
        });
        sync.push(Mark::ChapterViewed {
            site: Site::MangaLib,
            title_id: 7,
            chapter_id: 71,
        });
        sync.push(Mark::EpisodeViewed {
            anime_id: 8,
            player_id: 80,
        });
        for slug in ["1--old", "2--newer", "3--none"] {
            sync.push(Mark::Place {
                site: Site::MangaLib,
                slug: slug.into(),
                place: place(3),
            });
        }
        // Второе место того же тайтла заменяет первое: уходит только последнее.
        sync.push(Mark::Place {
            site: Site::MangaLib,
            slug: "1--old".into(),
            place: place(9),
        });

        flush(&client, &db).await;
        assert!(db.pending_marks(10).unwrap().is_empty());
    }

    fn test_client(s: &MockServer) -> Client {
        Client::new(ClientConfig {
            api_mirrors: vec![format!("{}/api", s.uri())],
            api_interval: Duration::ZERO,
            ..Default::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn own_write_does_not_block_next_place() {
        let s = MockServer::start().await;
        // Сервер помечает закладку временем из будущего: так её свежесть заведомо
        // больше времени любой отметки, и только своя запись может её объяснить.
        let future = "2099-01-01T00:00:00.000000Z";
        Mock::given(method("GET"))
            .and(path("/api/manga/5--own/bookmark"))
            .respond_with(bookmark("2020-01-01T00:00:00.000000Z"))
            .up_to_n_times(1)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/manga/5--own/bookmark"))
            .respond_with(bookmark(future))
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/bookmarks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
                "id": 1, "status": 4, "updated_at": future
            }})))
            .expect(2)
            .mount(&s)
            .await;
        let client = test_client(&s);
        let db = Db::in_memory().unwrap();
        let sync = Sync::detached(db.clone());
        for item in [3, 4] {
            sync.push(Mark::Place {
                site: Site::MangaLib,
                slug: "5--own".into(),
                place: place(item),
            });
            flush(&client, &db).await;
        }
        assert!(db.pending_marks(10).unwrap().is_empty());
    }

    #[tokio::test]
    async fn stuck_mark_is_dropped_after_limit() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/manga/7/chapters/70/view"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({"message": "нельзя"})))
            .mount(&s)
            .await;
        let client = test_client(&s);
        let db = Db::in_memory().unwrap();
        Sync::detached(db.clone()).push(Mark::ChapterViewed {
            site: Site::MangaLib,
            title_id: 7,
            chapter_id: 70,
        });
        for _ in 1..MAX_ATTEMPTS {
            flush(&client, &db).await;
        }
        assert_eq!(db.pending_marks(10).unwrap().len(), 1);
        flush(&client, &db).await;
        assert!(db.pending_marks(10).unwrap().is_empty());
    }

    #[tokio::test]
    async fn offline_marks_wait() {
        let client = Client::new(ClientConfig {
            api_mirrors: vec!["http://127.0.0.1:9/api".into()],
            api_interval: Duration::ZERO,
            ..Default::default()
        })
        .unwrap();
        let db = Db::in_memory().unwrap();
        let sync = Sync {
            db: db.clone(),
            wake: Arc::new(Notify::new()),
        };
        sync.push(Mark::EpisodeViewed {
            anime_id: 8,
            player_id: 80,
        });
        flush(&client, &db).await;
        assert_eq!(db.pending_marks(10).unwrap().len(), 1);
    }
}
