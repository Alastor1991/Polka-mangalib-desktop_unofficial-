# API lib.social: что известно

Официальной документации у сервиса нет. Здесь собрано то, что удалось выяснить по поведению сайта, его скриптам, официальному Android-приложению и открытым клиентам. Сведения актуальны на октябрь 2026 года, сервис может поменять что угодно в любой момент.

Код, который всё это реализует: `crates/libsocial`.

## Сайты сети

Все сайты обслуживает один API. Сайт выбирается заголовком `Site-Id`.

| Сайт | `Site-Id` | Домены сайта |
|---|---|---|
| MangaLib | 1 | mangalib.me, mangalib.org |
| SlashLib | 2 | slashlib.me, v2.slashlib.me |
| RanobeLib | 3 | ranobelib.me (клиент API и ядро знают, окно iced пока нет) |
| HentaiLib | 4 | hentailib.me |
| AnimeLib | 5 | anilib.me, animelib.org, v3.animelib.org |

## Адреса

| Назначение | Адреса |
|---|---|
| API | `https://api.cdnlibs.org/api`, `https://api.animelib.org/api`, `https://hapi.hentaicdn.org/api`, `https://api2.mangalib.me/api`, `https://api.imglib.info/api` |
| Вход | `https://auth.lib.social`, `https://auth.cdnlibs.org` |
| Обложки | `https://cover.cdnlibs.org` |
| Картинки и видео | присылает сам API в справочнике `/constants` |

Официальное Android-приложение ходит в `https://app-api.imglib.info/api`. Зеркала по-разному доступны из разных стран: например, `api2.mangalib.me` закрыт для немецких адресов, а `api.cdnlibs.org` открыт.

## Обязательные заголовки

```http
Site-Id: 1
Referer: https://mangalib.me/
Origin: https://mangalib.me
Accept: application/json
```

Без `Referer` и `Origin` одного из доменов сети API отвечает 403 со страницей блокировки, причём из любой страны. Если сайт кажется «заблокированным в регионе», сначала проверьте заголовки.

Серверы картинок и видео тоже требуют `Referer`. Без него обложка отдаёт 403.

`User-Agent` сервис не проверяет. Полка представляется честно: `Polka/0.1 (unofficial lib.social client; +ссылка на репозиторий)`.

Ответы почти всегда завёрнуты в `{"data": ...}`, страницы каталога ещё и в `"meta"`.

## Каталог

```http
GET /api/manga?site_id[]=1&page=1&q=берсерк&sort_by=rate_avg&sort_type=desc&fields[]=rate&fields[]=rate_avg
GET /api/anime?site_id[]=5&page=1
```

| Параметр | Значение |
|---|---|
| `site_id[]` | сайт, тот же номер, что в `Site-Id` |
| `page` | страница с единицы, по 60 тайтлов |
| `q` | поиск по названию |
| `sort_by` | `rate_avg`, `views`, `last_chapter_at`, `created_at`, `rus_name`, `name`, `chap_count`, без параметра по популярности |
| `sort_type` | `asc` или `desc` |
| `genres[]`, `genres_exclude[]`, `tags[]`, `types[]`, `status[]` | фильтры, номера из `/constants` |
| `chap_count_min=1` | только тайтлы с главами |
| `fields[]` | дополнительные поля: `rate`, `rate_avg`, `userBookmark`, `releaseDate` |

```json
{
  "data": [{
    "id": 7965, "name": "Chainsaw Man", "rus_name": "Человек-бензопила",
    "slug_url": "7965--chainsaw-man", "site": 1,
    "cover": {"thumbnail": "...", "default": "...", "md": "..."},
    "type": {"id": 1, "label": "Манга"},
    "rating": {"average": "9.42", "votes": 93785}
  }],
  "meta": {"has_next_page": true, "current_page": 1}
}
```

`slug_url` (`7965--chainsaw-man`) – основной идентификатор тайтла во всех остальных запросах. Число в его начале – id тайтла.

Последние обновления: `GET /api/latest-updates?page=1`.

## Тайтл

```http
GET /api/manga/7965--chainsaw-man?fields[]=summary&fields[]=genres&fields[]=authors&...
GET /api/anime/16133--jujutsu-kaisen-anime?fields[]=summary&...
```

Набор полей у манги и аниме разный, и лишнее поле сервер не терпит: отвечает 422, а на `format` у аниме и вовсе 500.

| Манга | Аниме |
|---|---|
| `summary`, `otherNames`, `genres`, `tags`, `authors`, `artists`, `teams`, `format`, `rate`, `releaseDate`, `status_id`, `manga_status_id` | `summary`, `otherNames`, `genres`, `tags`, `authors`, `teams`, `rate`, `releaseDate`, `anime_status_id`, `episodes_count`, `publisher` |

`summary` приходит в трёх видах: простой строкой, документом редактора ProseMirror (`{"type": "doc", "content": [...]}`) или этим же документом, упакованным в строку.

## Главы

```http
GET /api/manga/7965--chainsaw-man/chapters
```

```json
{"data": [{
  "id": 1766671, "volume": "1", "number": "1", "name": "Пёс и бензопила",
  "branches_count": 2,
  "branches": [{
    "id": 1766671, "branch_id": 4666, "created_at": "2021-12-31T09:30:47.000000Z",
    "teams": [{"id": 587, "name": "Japit Comics"}],
    "user": {"id": 1, "username": "uploader1"},
    "restricted_view": {"is_open": false, "expired_at": "2026-10-13T17:05:00.000000Z", "price": null}
  }]
}]}
```

Особенности:

- Номера тома и главы – строки, глава бывает дробной (`97.5`).
- Если одну главу перевели несколько команд, у неё несколько `branches`. Каждая ветка перевода – отдельный набор страниц. Список веток тайтла: `GET /api/branches/{id тайтла}`, первая в ответе – ветка по умолчанию на сайте.
- `restricted_view.is_open = false` значит, что глава закрыта: платная или в раннем доступе. Для раннего доступа `expired_at` говорит, когда она откроется бесплатно. Полка такие главы показывает с замком и не открывает.
- Лицензированные тайтлы приходят без глав (`data: []`).

## Страницы главы

```http
GET /api/manga/7965--chainsaw-man/chapter?volume=1&number=1&branch_id=4666
```

```json
{"data": {
  "id": 1766671, "volume": "1", "number": "1",
  "pages": [{"id": 13732349, "slug": 1, "url": "//manga/chainsaw-man/chapters/1766671/0_CK0f.jpg", "width": 850, "height": 1200}]
}}
```

`slug` – номер страницы, `url` – путь от корня сервера картинок. Полный адрес: адрес сервера плюс путь. Двойная косая черта в начале пути не мешает.

## Справочник

```http
GET /api/constants?fields[]=imageServers&fields[]=videoServers&fields[]=genres&fields[]=tags&fields[]=types
```

Серверы картинок у каждого сайта свои (`site_ids`):

| id | Что это |
|---|---|
| `compress` | сжатые картинки, быстрее всего. Сайт и приложение используют его по умолчанию |
| `main`, `secondary` | оригиналы |
| `download` | для скачивания |
| `crop` | нарезка длинных страниц, у части сайтов пуст |

Серверы видео: `main`, `secondary_1`, `secondary_2`. В адресах всех трёх есть путь `/.аs/`, и буква «а» в нём кириллическая. Это не опечатка: с латинской «a» сервер отвечает 404.

## Аниме

```http
GET /api/episodes?anime_id=16133--jujutsu-kaisen-anime
GET /api/episodes/{id серии}
```

Серия содержит список `players`, по одному на каждую озвучку или субтитры каждой команды:

```json
{"data": {"id": 41924, "number": "1", "players": [
  {"id": 5371, "player": "Kodik", "team": {"name": "..."}, "src": "//kodikplayer.com/seria/..."},
  {"id": 413938, "player": "Animelib", "team": {"name": "AniLibria.TV"}, "video_domain": "secondary_1",
   "video": {"id": 129151, "quality": [{"href": "/uploads/converted_videos/.../1080.mp4", "quality": 1080, "bitrate": 3000}]},
   "subtitles": [{"id": 1, "src": "...", "format": "vtt"}]}
]}}
```

- `Kodik` – сторонний встраиваемый плеер. И сайт, и официальное приложение открывают его как страницу. Полка поступает так же: открывает его в браузере.
- `Animelib` – собственный хостинг сервиса с прямыми ссылками на MP4 до 2160p. Сервер показывает такие плееры только после входа. Полный адрес видео: адрес сервера из `videoServers` (подсказка в `video_domain`) плюс `href`.
- В ответе для вошедшего пользователя у каждого плеера есть `is_viewed`: серия отмечена просмотренной в этом плеере. Поле `timecode` к прогрессу зрителя отношения не имеет: это разметка опенинга и эндинга, которую задаёт команда. Позицию внутри серии сервис хранит только в закладке, см. ниже.

## Вход

Вход устроен как OAuth2 Authorization Code с PKCE (S256).

1. Открыть в браузере
   `https://auth.lib.social/auth/oauth/authorize?scope=&client_id=1&response_type=code&redirect_uri=https://mangalib.org/ru/front/auth/oauth/callback&state=…&code_challenge=…&code_challenge_method=S256&prompt=consent`
2. После входа и согласия сервер перенаправит на `redirect_uri?code=…&state=…`.
3. Обменять код:
   ```http
   POST /api/auth/oauth/token
   {"grant_type": "authorization_code", "client_id": "1", "redirect_uri": "…", "code_verifier": "…", "code": "…"}
   ```
   Ответ: `access_token`, `refresh_token`, `expires_in` (секунды), `token_type`.
4. Обновлять токен:
   ```http
   POST /api/auth/oauth/token
   {"grant_type": "refresh_token", "client_id": "1", "refresh_token": "…", "scope": ""}
   ```
5. Запросы с токеном: `Authorization: Bearer …`. Кто вошёл: `GET /api/auth/me`.

Ловушки:

- `client_id=1` – это веб-клиент сайта. Он принимает `redirect_uri` только вида `https://<любой домен сети>/ru/front/auth/oauth/callback`. Другой путь или `http://127.0.0.1` сервер отвергает (401 `invalid_client`).
- Страница callback на сайте сама обменивает код и уводит на главную, поэтому скопировать код из адресной строки пользователь не успевает. Полка перехватывает переход на callback во встроенном окне до загрузки страницы.
- У официального Android-приложения свои клиенты: `client_id=3` с `redirect_uri=ru.libapp.oauth://type/callback` и `client_id=6` у MangaLib Lite. Полка их не использует: выдавать себя за чужое приложение нельзя.
- Сайт хранит токен в `localStorage` под ключом `auth`. Так его достают некоторые сторонние клиенты на Android, но Полке это не нужно.

## Закладки и отметки

Всё это работает только с токеном. Код: `crates/libsocial/src/bookmarks.rs`.

### Отметки «прочитано» и «просмотрено»

```http
POST /api/manga/{id тайтла}/chapters/{id главы}/view
POST /api/anime/{id тайтла}/players/{id плеера}/view
```

Тело пустое. Id главы – это id перевода в конкретной ветке (`branches[].id` в списке глав, `id` в ответе со страницами), а не id первой ветки. Id тайтла числовой. Серия отмечается для конкретного плеера, то есть озвучки.

Отметку видно в поле `is_viewed` ответа со страницами главы и у плееров серии. В списках глав и серий отметок нет, снять отметку через API нельзя.

### Закладки

У тайтла одна закладка. В ней папка и место, где пользователь остановился.

```http
GET /api/bookmarks/folder/{id пользователя}
GET /api/bookmarks?user_id=…&status={папка}&page=1
GET /api/{manga|anime}/{slug}/bookmark
POST /api/bookmarks
DELETE /api/bookmarks/{id закладки}?media_type={manga|anime}
```

Папки общие для аккаунта: у манги «Читаю» (1), «В планах» (2), «Брошено» (3), «Прочитано» (4), «Любимые» (5), у аниме свои (21–27), плюс созданные пользователем. В `site_ids` папки видно, к каким сайтам она относится.

Список закладок отдаёт тайтлы только того сайта, что указан в `Site-Id`: папку «Читаю» манги надо спрашивать отдельно у MangaLib, SlashLib и HentaiLib. По 24 на страницу, следующая страница есть, если `links.next` не `null`.

Поставить закладку или поменять её:

```json
{"media_type": "manga", "media_slug": "7965--chainsaw-man",
 "bookmark": {"status": 1, "item_id": 1292692, "progress": "20"},
 "meta": {"item_number": 2}}
```

- Если закладка на тайтл уже есть, сервер её обновляет и отвечает 200, новую создаёт с ответом 201. Перед тестами на своём аккаунте запомните старую закладку: тестовый запрос молча её перезапишет.
- Без `item_id` и `progress` место в закладке остаётся прежним, меняется только папка.
- `progress` у манги – номер страницы с единицы, у аниме – время `ММ:СС`.
- У аниме в `meta` ещё `player` (`Animelib`, `Kodik`), `team` и `translation_type`. Если эти поля есть, они не могут быть `null`: ответ 422.
- Ответ – закладка с полем `item`, где лежит сама глава или серия. В `updated_at` время последнего изменения, по нему Полка не откатывает закладку, которую позже двигали на другом устройстве.

## Вокруг тайтла

Код: `crates/libsocial/src/media.rs`. Путь `{media}` – `manga` (и для ранобэ, SlashLib, HentaiLib) или `anime`.

```http
GET /api/{media}/{slug}/covers                       все обложки, у каждой есть orig
GET /api/{media}/{slug}/stats?bookmarks=true&rating=true   без параметров ответ пустой
GET /api/{media}/{slug}/relations                    связанные, бывают на другом сайте сети
GET /api/{media}/{slug}/similar                      похожие с голосами, бывают сотни
POST /api/similar/{id}/vote       {"vote": 1|0}
POST /api/similar                 {"source_type", "source_id", "similar_id", "similar_type", "similar_by": [1..3]}
GET /api/character?media_id=7965&media_type=manga&position=Main&limit=30
GET /api/{people|character|franchise|publisher}/{slug_url}   только вошедшему, гостю 404
GET /api/favorites/{type}/{id}    POST /api/favorites {"source_type", "source_id"}   звёздочка-подписка
POST /api/{media}/rate            {"score": 1..10, "rateable_id", "rateable_type"}   score 0 снимает
POST /api/chapters/{id}/like      без тела, переключает
POST /api/chapters/{id}/translation-rating   {"translation_accuracy", "readability_adaptation", "editing_formatting"}
POST /api/branches/{branch_id}/subscribe     без тела, переключает подписку на новые главы ветки
GET|POST /api/{media}/{id}/subscriptions     {"items": [{"team_id": 0, "translation_type": 2}]}   озвучка и субтитры
GET /api/teams/{slug_url}         только slug_url и только вошедшему
GET /api/teams?sort_by=titles_count&page=1   GET /api/teams/{id}/users   GET /api/teams/{id}/chapters
POST|DELETE /api/teams/{id}/rating   POST|DELETE /api/teams/{id}/ignore
GET /api/reviews?reviewable_type=manga&reviewable_id=7965   GET /api/reviews/{id}
POST /api/reviews   PUT|DELETE /api/reviews/{id}   POST /api/review/{id}/vote
GET /api/manga/{slug_url}/chapters/players   озвучка манги видео, только по slug_url
```

- Карточка тайтла принимает много `fields[]` сверх описанных выше: `background`, `views`, `publisher`, `franchise`, `metadata` (`close_comments`, `count`), `chap_count` (даёт `items_count`), `translation_quality_rating`, у аниме `time`, `shiki_rate`, `episodesSchedule`. Своя оценка приходит в `rating.user` (`0` – не оценивал).
- `metadata.close_comments`: 0 комментарии открыты, 1 и 3 закрыты на странице тайтла, 2 и 3 в главах.
- Глава отдаёт `likes_count`, `is_liked`, `translation_quality_rating` и полную `restricted_view` с ценой. Признак закрытой главы надёжнее брать по `expired_type` (1 ранний доступ, 2 платная) вместе с `is_open == false`.
- У плееров аниме в `timecode` лежит разметка `[{"type": "opening", "from": 55, "to": 145}]` в секундах. Полка по ней пропускает отрезки в mpv, если это включено в настройках (`auto_skip`).
- Видео озвучки манги лежит на своих серверах (`/constants?fields[]=videoServersManga`), без кириллического `/.аs/` в пути.

## Комментарии

Код: `crates/libsocial/src/comments.rs`, дерево и голоса в `crates/polka-core/src/comments.rs`. Комментарии не зависят от `Site-Id`: id объектов во всей сети общие.

```http
GET /api/comments?post_type=chapter&post_id=223269&post_page=1&sort_by=id&sort_type=desc&page=1
GET /api/comments/{id}?post_type=…&post_id=…        ветка по id любого её комментария
GET /api/comments/sticky?post_type=…&post_id=…      закреплённые
POST /api/comments                                   написать
PUT /api/comments/{id}                               править (сайт разрешает два дня)
DELETE /api/comments/{id}
POST /api/comments/{id}/vote      {"vote": 1}        плюс; 0 – минус, тот же голос повторно снимает
POST /api/comments/{id}/report    {"reason": "3", "metadata": {"comment": ""}, "reportable_id": …, "reportable_type": "comment", "namespaces": "social_comments"}
GET /api/reports/types?namespaces[]=social_comments  причины жалоб, голый массив без data
GET /api/sticker-packs                               стикеры, только с входом
POST /api/upload/image/comment                       картинки, multipart, поле content[]
GET /api/user/{id}/comments                          что написал пользователь, по 20
```

- `post_type`: `manga` (и для ранобэ), `anime`, `chapter`, `episodes`, `chapter_player`, `review`, `collection`, `post` (новость), `discussion`, `card`. Для главы `post_id` – id перевода в ветке.
- Комментарии к странице главы: `post_type=chapter` и `post_page` – номер страницы с единицы. Без `post_page` приходят корни со всех страниц и без ответов. Счётчика комментариев на странице в API нет, сайт его не показывает.
- Ответ: `{"data": {"root": [...], "replies": [...]}, "meta": {"has_next_page": …}}`. Страницы по 25 корней. `replies` – плоский список ответов всех уровней, дерево собирается по `parent_comment`. В нём бывают повторы и ответы к корням с других страниц.
- Текст приходит HTML-строкой (`<p>`, `<blockquote>`, спойлеры нескольких видов, картинки с `data-original-src`), а уходит документом ProseMirror. Оба формата разбирает `libsocial::rich::Doc`.
- `sort_by` принимает только `id` и `votes_up`. Свой голос приходит в `votes.user`: `1`, `0` или `null`.
- В новом комментарии ответ задают `parent_comment`, `root_id` (корень родителя или сам родитель) и `comment_level` (уровень родителя плюс один).
- Картинки и стикеры в комментариях доступны только с премиумом.

## Каталог, справочник, ленты и поиск

Код: `crates/libsocial/src/catalog.rs` (фильтр и сортировки), `catalog_constants.rs` (справочник формы), `feeds.rs` (главная, обновления, топ, новости), `search.rs` (люди, команды, персонажи, издатели, франшизы, пользователи). Чтение работает без входа, кроме `user-latest-updates`.

```http
GET /api/manga?site_id[]=1&page=1&q=…&sort_by=…&sort_type=…&fields[]=rate&fields[]=rate_avg&genres[]=34&…   MangaLib, SlashLib, RanobeLib, HentaiLib
GET /api/anime?…                                       AnimeLib, с любым Site-Id
GET /api/constants?fields[]=genres&fields[]=tags&fields[]=types&fields[]=status&fields[]=scanlateStatus&fields[]=ageRestriction&fields[]=ageRestrictionAnime&fields[]=format
GET /api/                                              вся главная одним ответом
GET /api/latest-updates?page=1                         по 15 тайтлов
GET /api/user-latest-updates?page=1                    только из закладок, нужен вход
GET /api/media/top-views?time=day|week|month           группы по три тайтла
GET /api/media/top-views?time=…&popularity=3&page=1    плоский список по 56, с оценками
GET /api/media/hot-newest                              пусто на всех проверенных сайтах
GET /api/news?type[]=news&type[]=changelog&visible_for[]=all&platform[]=web&page=1
GET /api/news/308--versiia-204                         по голому id 404
GET /api/people|teams|character|publisher|franchise?q=…&sort_by=…&sort_type=…&site_ids[]=1&page=1
GET /api/people/{slug_url}                             то же для teams, character, publisher, franchise
GET /api/user?q=…&filter=weekly-top|points|moderators|daily-streak|weekly-views&sort_by=id|username&sort_type=…
```

Каталог:

- Полный перечень параметров: `site_id[]`, `q`, `sort_by`, `sort_type`, `genres[]`, `genres_exclude[]`, `genres_soft_search`, то же для `tags`, `types[]`, `status[]`, `scanlate_status[]`, `caution[]`, `format[]`, `format_exclude[]`, `extra_release_status[]` (`announced`, `ongoing`), `year_min/max`, `chap_count_min/max`, `episodes_min/max`, `rating_min/max` (оценка 0–10), `rate_min/max` (число оценок), `licensed`, `buy`, `long_no_translation`, `has_voice`, `has_droppable_cards`, `ignored_teams`, `bookmarks[]`, `bookmarks_exclude[]`, `target_model` с `target_id`, `moderated[]`, `seed`, `random_order`, `fields[]`. Булевы значения идут как `1` и `0`, массивы с суффиксом `[]`.
- `/anime` в `meta` не присылает `has_next_page`, как и часть лент. Следующую страницу выдаёт `links.next`; адреса внутри `links` и `meta.path` подставляют случайное зеркало, по ним ходить нельзя.
- Без `fields[]=rate` и `fields[]=rate_avg` оценка нулевая (`"0"`, 0 голосов). В карточках главной она нулевая всегда, оценки есть только у плоского `top-views` и в каталоге с полями.
- `site_id[]` сервер пересекает с тем, что разрешено для `Site-Id`: из MangaLib не виден SlashLib и HentaiLib, RanobeLib виден как `site_id[]=3`. Отдельного эндпоинта у ранобэ нет, `model` в карточке остаётся `manga`.
- Сортировки у манги и аниме разные. Чужая (`chap_count` у аниме, `episodes_count` у манги), чужое поле в `fields[]` (`metadata`, `extra_release_status` у аниме) и `format[]` вне 1–7 дают 422; клиент отказывает до запроса. Форматы 8–10 (ранобэ) есть в справочнике, но каталог их не принимает.
- `sort_by=rate_avg` без `rate_min=50` поднимает наверх тайтлы с единичными оценками; клиент добавляет `rate_min=50` сам, если нижняя граница не задана. `buy=1` сайт сопровождает `licensed=1`, клиент тоже.
- Случайный порядок (`sort_by=random_order`): первая страница идёт без вспомогательных параметров, `meta` отдаёт `seed` и `random_order` (число). Вторая страница без пары отвечает 422, выдуманное значение тоже, подходит только пара из ответа предыдущей. Клиент хранит её в `RandomCursor`.
- `episodes_min` на `/manga` и подобные фильтры другого вида контента сервер молча игнорирует. Неизвестный id жанра или папки закладок не ошибка, список просто пуст.
- Тайтлы человека, персонажа, команды, франшизы, издателя и пользователя отдаёт тот же каталог с `target_model` и `target_id`. Одиночный `target_id` игнорируется.

Справочник:

- Не зависит от `Site-Id`. Запрос без `fields[]` даёт пустой объект, неизвестные имена пропускаются. Сайт кеширует ответ на 20 минут, в крейте кеша нет.
- Отбор по сайту делает клиент по `site_ids`. Нуль в `site_ids` статусов сайтом не считается: «Выпуск прекращён» с нулём всё равно не относится к AnimeLib. У типа «Неизвестный» список пуст, его не показывают.
- Возрасты AnimeLib лежат отдельно (`ageRestrictionAnime`: PG, PG-13, R-17…). Жанры и теги бывают скрыты для страны (`blocked_for_country: ["RU"]`), скрывает клиент. `anilist_id` и `shiki_id` приходят строкой или `null`.

Ленты и новости:

- `latest-updates` кладёт свежие главы или серии в `metadata.latest_items`, «обновления популярных» на главной в `metadata.last_item`. В лентах `restricted_view.is_open` бывает `false` и у обычных глав: закрытой главу считают, только если есть цена или срок раннего доступа.
- `top-views` без `popularity` даёт группы `1`, `2`, `3` (манга: новинки, набирающие популярность, популярные) или `21`, `22`, `23` (аниме: завершённое, онгоинг, полнометражное). Чужая группа даёт пустой список. Пустые группы приходят как `[]`, а не `{}`.
- На главной `hot_newest` и `news` обычно пусты. Блоки, их порядок и видимость пользователь задаёт в настройках (`start_page_blocks`), это вне поиска. `slider` – рекламные баннеры.
- `metadata.contentVersion` новости приходит то строкой, то числом. Прочитанные новости лежат в `visited_news` ключами `<id>_<contentVersion>`.
- Текст новости – документ ProseMirror, картинки в нём ссылаются на `attachments[].name`, а адреса вложений (`/uploads/news/…`) лежат на хосте API, а не на `cover.cdnlibs.org`. `news_article` подставляет адреса в `Doc`, `Client::upload_url` собирает их для баннеров.
- Отметка новости прочитанной (`/news/{id}/visited`) не реализована: её метод (PUT или PATCH) по скриптам сайта не определить.

Поиск и списки:

- `q` короче двух символов в списках сущностей даёт 422, в каталоге `q=a` работает. Люди, команды и персонажи с `q` теряют `total` и `last_page`, остаётся `links.next`.
- Фильтр сайтов в списках сущностей называется `site_ids[]`, в каталоге `site_id[]`. `limit` принимает только `/character` (0–30). `sort_by=likes_count` есть у команд, у людей даёт 422.
- Число тайтлов по сайтам лежит в `titles_count_details`, ключи – номера сайтов строкой. `stats[].label` склоняется, искать надо по `stats[].tag`.
- У издателя слаг бывает пустым, тогда `slug_url` выглядит как `6848--` и всё равно годится как путь. Голый id принимают люди и персонажи, команды нет.
- `/user` с `filter=points` и `moderators` отдаёт всех сразу, без `meta` и `links`; у модераторов есть `roles`, у `weekly-top` и `points` есть `points_info`.

## Аккаунт и общение

Код: `notifications.rs`, `users.rs`, `social.rs`, `collections.rs`, `bookmarks_ext.rs` в `crates/libsocial/src`, общие страницы списков в `listing.rs`. Почти всё требует входа. Запросы записи сняты со скриптов сайта и вживую не проверялись. Websocket и мини-чат клиент не умеет: для них нужен пакет с поддержкой Pusher.

Списки пагинируются по-разному. Следующая страница есть, если `meta.has_next_page` истинно или `links.next` не `null`; `last_page` и `total` бывают не у всех, а у коротких ответов (сообщения малого диалога, общие друзья, топ-100) нет ни `meta`, ни `links`. Значение фильтра «все» в запрос не передают: `all` в `filter` и `post_type` сервер отвергает с 422.

### Уведомления

```http
GET    /api/notifications/count
GET    /api/notifications?notification_type=all&read_type=all&sort_type=desc&page=1&q=…&type=…
PUT    /api/notifications/{id}                          без тела
PUT    /api/notifications/bulk   {"notification_type": "all"}
DELETE /api/notifications/{id}
DELETE /api/notifications/bulk   {"notification_type": "all", "is_read": null}
```

- Без `notification_type` и `read_type` список отвечает 422, без `sort_type` падает с 500. Клиент шлёт все три всегда. По 20 записей, `per_page` сервер игнорирует.
- Приходят уведомления всех сайтов сети, сайт записи лежит в `data.site_id` или `data.media.site`.
- Поле `is_read` в записях не встречалось. Прочитанность определяется выбранным `read_type`.
- Типов записей больше, чем разобрано в `NotificationEvent`; для остальных всегда есть готовый текст в `content` (документ ProseMirror).
- Для `is_read` в массовом удалении `null` значит «все», `true` прочитанные, `false` непрочитанные.

### Профиль, статистика, история

```http
GET /api/user/{id}?fields[]=about&fields[]=points&…
GET /api/user/{id}/stats
GET /api/user/{id}/view-statistics?site_ids[]=1
GET /api/user/chapters/history?page=1           манга и ранобэ, по Site-Id
GET /api/user/chapters/players/history?page=1   озвучка манги
GET /api/user/players/history?page=1            аниме, Site-Id: 5
GET /api/user?page=1&filter=points&q=…&sort_by=id&sort_type=desc
```

- Без `fields[]` приходит только карточка пользователя. Закрытый профиль отдаёт урезанный набор полей, неизвестный `fields[]` вызывает 422, несуществующий пользователь – 404.
- `login_streak.last_login_at` пишется без `T` (`2026-10-13 03:54:30`), `avatar.filename` у аватара-заглушки равен `null`.
- Закрытая статистика просмотров отвечает 403. Минуты в ней прикидочные: число глав и серий, умноженное на `minutes_per_item`.
- История идёт по 40 записей без `total` и `last_page`, листать надо по `links.next`.
- В списке пользователей `filter` принимает `weekly-top`, `points` (все сто записей за раз), `moderators`, `daily-streak`, `weekly-views`. Для общего списка параметр опускают.

### Настройки

```http
GET|PUT  /api/user/settings/privacy
GET|PUT  /api/user/settings/notifications
GET|POST /api/user/{id}/content-filter          {"filters": [{"value": 123, "type": "tag", "site_id": 1}]}
GET|POST /api/user/{id}/personalization-settings
```

- Сохранять надо объект целиком, а не изменённые поля. Поля, которых клиент не знает, модели хранят в `extra` и возвращают обратно.
- `disable_old_comments_notif` приходит как `false` или число дней (7, 14, 30, 180, 360), а сайт отправляет `0`, когда выключено.
- `disable_friends_notif` – инверсия: `true` значит оповещения о заявках отключены.
- Видимость: `0` публичный, `1` для друзей, `2` для всех, кроме игнор-листа, `3` приватный. У старых ников значения `2` нет. В `statistics_site_ids` числа бывают строками.
- Фильтр контента заменяется целиком и действует на каталог с задержкой 10–20 минут.

### Друзья, игнор, сообщения

```http
GET /api/friendship?user_id={id}&status=1&page=1                друзья
GET /api/friendship?user_id={свой}&status=0&sender=0|1          входящие | отправленные заявки
GET /api/friendship/{id}/mutual?page=1
GET /api/friendship/{id}                                        флаги дружбы с пользователем
PUT /api/friendship/bulk   {"status": 1}                        принять все входящие; 2 – отклонить все
GET    /api/ignore?sourceable_type=user&sourceable_id={свой}&page=1
GET    /api/ignore/{id пользователя}
POST   /api/ignore   {"sourceable_id": …, "sourceable_type": "user", "user_id": …, "comment": "…"}
PUT    /api/ignore/{id записи}   {"comment": "…"}
DELETE /api/ignore/{id записи}
GET  /api/messenger/threads?page=1&archived=0&q=…
GET  /api/messenger/threads/{id}
GET  /api/messenger/threads/find/{id пользователя}
GET  /api/messenger/threads/{id}/messages?page=1
POST /api/messenger/threads   {"participants": [id]}
POST /api/messenger/threads/{id}/messages   {"body": "…"}
PUT  /api/messenger/threads/{id}/messages/{message}   {"body": "…"}
DELETE /api/messenger/threads/{id}/messages   {"messages": [id, …]}
POST /api/chat/check-online   {"users": [id, …]}
```

- Запросов «отправить, принять, отозвать, удалить дружбу» в доступных скриптах нет, они лежат в общем куске `shared/UserProfileWrap/user-profile-mnoF2eqC.js`. Клиент их не содержит.
- Друзья идут по 42 записи, `links.last` равен `null`. `GET /ignore` без `sourceable_type` отвечает 422; для игнора от имени команды (`team`) или автора (`people`) комментарий обязателен.
- Диалогов по 15 на страницу, `find` отвечает 404, если диалога нет, и находит архивные диалоги. Сообщения идут новыми сверху. Отдельного запроса «прочитано» нет, счётчик обнуляет сам запрос сообщений.
- Текст сообщения простой (до 2000 символов), но сайт вставляет его как HTML, при показе его надо экранировать.
- Ответ `check-online` – словарь `{"1001": "online"}`; клиент принимает его и с обёрткой `data`, и без.

### Подборки

```http
GET    /api/collections?limit=12&page=1&sort_by=popular&sort_type=desc&q=…&user_id=…&subscriptions=0|1&site_ids[]=1&type=titles
GET    /api/collections/{id}
POST   /api/collections
PUT    /api/collections/{id}
DELETE /api/collections/{id}
POST   /api/collection/{id}/vote   {"vote": 1}
```

- `limit` допускает только 12–24, иначе 422; клиент подгоняет значение. `sort_by` принимает `newest` и `popular`, остальное вызывает 422.
- `subscriptions=1` вместе с `user_id` отдаёт избранные подборки, `0` свои. Несуществующая и закрытая подборка отвечают 404.
- Описание – документ ProseMirror или `null`; сайт отправляет `null`, если текст короче двух символов. У блока клиент сам задаёт `uuid`, а `order` элементов считается с единицы.
- Голос «против» у подборок вживую не снимали: клиент шлёт то же значение, что для комментариев (`0`), а читает и `0`, и `-1`.

### Закладки: расширения

```http
GET    /api/bookmarks?user_id=…&status=0&sort_by=updated_at&sort_type=desc&q=…&page=1
POST   /api/bookmarks/folder                    {"name": "…"}
PUT    /api/bookmarks/folder/{id}               {"name": "…", "color": "#aabbcc", "notify": true, "public": true}
DELETE /api/bookmarks/folder/{id}               {"move_to": 1}
PUT    /api/bookmarks/folder/order              {"order": [1, 2, 51]}
PUT    /api/bookmarks/folder/notifications      {"folders": [{"id": 1, "notify": true}]}
POST   /api/bookmarks/bulk/store                {"media_type": "manga", "media_ids": [7965], "status": 2}
PUT    /api/bookmarks/bulk                      то же, перенос в другую папку
DELETE /api/bookmarks/bulk                      {"media_type": "manga", "media_ids": [7965]}
GET    /api/bookmarks/note?bookmark_id=…&bookmark_type=manga_bookmark&page=1
GET    /api/bookmarks/note/chapter-{id закладки}?number_first=…&number_secondary=…
POST   /api/bookmarks/note
DELETE /api/bookmarks/note/{id заметки}         {"bookmark_id": …, "bookmark_type": "manga_bookmark"}
```

- `user_id` в списке обязателен (422 без него), `status=0` значит «все папки». `sort_by` у аниме для даты выхода называется `last_episode_at`, у манги `last_chapter_at`. `meta.per_page` приходит массивом, размер страницы лучше не фиксировать.
- Поле `meta` закладки заполнено по-разному: у закладки без места `comment` бывает `false`, а числа `null`.
- Менять и удалять можно только папки с `id` больше 50. Что сервер делает с закладками при удалении папки без `move_to`, неизвестно.
- Заметка – до 15 записей на главу или серию, сохраняется целиком. Если заметки нет, `GET` по составному идентификатору отвечает 404.
- Комментарий и история пересмотров правятся тем же `POST /bookmarks`, что и папка, через поле `meta` (`comment`, `rewatches_history`). Место чтения остаётся прежним, если не передавать `item_id` и `progress`.

## Ранобэ

Код: `crates/libsocial/src/ranobe.rs` (текст главы), `exports.rs` (экспорт книг). Ранобэ живёт в том же API, что и манга, с `Site-Id: 3`: пути `/manga/...`, в ответах `model` равен `"manga"`. Слово `book` встречается только в адресах страниц сайта и в `model` внутри раздела карточек. `Site::RanobeLib` в крейте пока нет, поэтому запросы идут через `Client::ranobe_*` с явным заголовком.

```http
GET /api/manga/{slug_url}/chapters                                    Site-Id: 3
GET /api/manga/{slug_url}/chapter?volume=1&number=0&branch_id=9824    Site-Id: 3
POST /api/bookmarks   {"media_type": "manga", "media_slug": …, "bookmark": {"item_id": …, "progress": "17"}}
GET /api/constants?fields[]=bookExportEnabled&fields[]=bookExportPremiumOnly&fields[]=bookExportFormats
GET /api/book-exports?manga_id={id}              свои задачи, manga_id обязателен
POST /api/book-exports   {"manga_id": …, "format": "epub"|"fb2", "branch_id": …|null, "volume": "1"|null}
GET /api/book-exports/{id}                       опрос раз в poll_after секунд, 404 – задачу удалили
DELETE /api/book-exports/{id}
POST /api/book-exports/{id}/link?part={N}        {url, file_name}, дальше обычный GET
GET /api/book-exports/{id}/segments/{N}?v={key}  при delivery = "segments", 410 – файл устарел
```

- У главы нет `pages`, есть `content`, и он двух видов в одном тайтле: старые главы (id примерно до 2 млн) приходят строкой HTML из `<p>` и `<img>`, новые документом ProseMirror. Клиент разбирает оба в `rich::Doc` (`RanobeChapter::doc`).
- Картинки старых глав: `attachments[].url` строится по нынешнему slug тайтла, а файл лежит под старым, так что адрес даёт 404 (на `ranobelib.me` 500). Верный путь стоит в `src` внутри HTML. У новых глав путь строится по числовому id тайтла и рабочий, картинка в документе названа только именем вложения (`attrs.images[].image`), адрес ищется в `attachments`. Полный адрес – `https://ranobelib.me{url}`, `Referer` любого домена сети подходит.
- У каждого абзаца есть номер, нумерация общая по документу, с единицы, включая абзацы в цитатах и списках. Для HTML он лежит в `data-paragraph-index`, для ProseMirror считается по порядку узлов `paragraph`. `Doc` номеров не хранит, поэтому `RanobeChapter::spans` идёт параллельно `doc.blocks`, а `block_of_paragraph(n)` ищет блок по номеру.
- Закладка: автозакладка шлёт только `item_id` и `meta.item_number`, а `progress` для ранобэ не передаёт совсем. Закладка на абзац – тот же `POST /bookmarks` с `progress` равным номеру абзаца строкой (`RanobeChapter::bookmark_place`, `paragraph_from_progress`). Процент прокрутки на сервер не уходит, его Полка хранит у себя.
- Внутри HTML-текста попадаются настоящие переводы строк, а не `<br>`: для показа они пробелы.
- `branch_id` бывает `null` у всех глав тайтла, тогда параметр в запрос не кладут. Главы сортируют по `item_number`, потому что номер вроде `0.5` порядка не задаёт.
- Экспорт книг не проверен вживую: он доступен только Premium (`bookExportPremiumOnly`). Схема ответов взята из скриптов сайта, разбор терпимый, `id` задачи хранится строкой. Сегменты скачиваются по порядку и склеиваются, итог сверяется с суммой `segments[].size`; части скачиваются по временным ссылкам с паузой в полсекунды.

## Карточки

Код: `crates/libsocial/src/cards.rs`. Коллекционная игра внутри сайтов сети; ответы почти не зависят от `Site-Id` (запросы идут с MangaLib), кроме скрытия карт, которых нет на сайте запроса (`hidden_site_cards_count`).

```http
GET /api/cards?media_id=7965&media_type=manga&rank=C&site_id=3&animated=1   каталог
GET /api/cards/{id|slug_url}                         подробная карта: media, albums, owners, versions
GET /api/cards/{id}/owners?available_for_exchange=1
GET /api/user/{id}/inventory?element=ice&locked=0&sort_by=power_score        чужой читается, пока профиль открыт
GET /api/user/{id}/card-folders
GET /api/cards/statistics/{id}                       без обёртки data
GET /api/cards/albums?site_id=3&sort=updated_at&direction=desc               24 на страницу
GET /api/cards/albums/{id}?status=owned_now&version=animated&per_page=…      альбом и слоты data.slots
GET /api/user/{id}/card-albums, …/card-albums/info?site_id=…
GET /api/cards/collection/title/{manga|anime}/{titleId}
GET /api/cards/shards/title/{manga|anime}/{titleId}                          вход нужен
GET /api/profile/cards/shards?user_id=…&sort_by=amount                       meta.weekly_limits
GET /api/cards/collectors/title/{manga|anime}/{titleId}
GET /api/cards/trade-market?tab=buy|sell|my|offers   пагинация в data.pagination
GET /api/cards/trade-market/cards/{id}?tab=offers|wants|mine, /lots/{id}, /api/cards/trade-orders/{id}
GET /api/trade-orders?type=direct_exchange&participant_user_id=…&section=active|history, /trade-orders/summary
GET /api/cards/chaos-exchange, /chaos-exchange/cards?recipe=…               вход нужен
GET /api/cards/creator-top?type=normal|animated&period=current|previous
GET /api/card-packs, /api/card-packs/{id}
POST|DELETE /api/user/inventory/{item_user_id}/lock
POST /api/user/card-folders, PUT …/{id}, DELETE …/{id}, PUT …/order {folder_ids}, PUT …/move {item_user_ids, folder_id}
POST /api/cards/shards/title/{type}/{id}/exchange {rank}
POST /api/user/{id}/card-albums/{albumId}/claim
POST /api/cards/chaos-exchange {recipe, item_user_ids}
POST /api/trade-orders {type: "direct_exchange", target_user_id, message, groups}, …/{id}/accept|decline|cancel
POST /api/cards/trade-orders          лот «хочу» (direction: buy) или «отдаю» (direction: sell)
POST /api/cards/trade-market/lots/{id}/accept, /api/cards/trade-orders/{id}/offers|cancel, /api/cards/trade-offers/{id}/accept|decline
POST /api/cards/{id}/receive-preview {card_instance_stat_id}
```

- Ранги: `E` Обычная, `D` Необычная, `C` Редкая, `B` Эпическая, `A` Легендарная, `S` Мифическая. Ранг S за осколки не выдаётся.
- `type` тайтла в путях и параметрах равен `manga` для манги, ранобэ и слэша, `anime` для аниме. В ответе осколков у ранобэ `model` равен `"book"`, брать надо `type`.
- Id элемента инвентаря – `item_user_id`, не id карты: его принимают блокировка, папки, обмены. `locked` значит «занят в лоте или обмене», `user_locked` – ручная блокировка владельцем. `card_stats` у старых карт может быть `null`.
- Осколки считаются по тайтлу и хранятся в десятых долях (`amount_units`). Недельный лимит ограничивает чтение (750) и прочие действия (250) для обычного аккаунта.
- Параметры `all` в query слать нельзя (422), поиск `q` принимается от двух символов и кириллицу надо кодировать.
- Без входа читаются каталог, карта, владельцы, чужой открытый инвентарь, альбомы, рынок, лоты, топ. Осколки, хаос-обмен и прямые обмены требуют входа, а прямые обмены ещё и только свои.
- Запросы на запись и разбор их ответов не проверены вживую: они взяты из скриптов сайта. Платные действия (покупка и открытие паков, платежи, покупка глав) в клиент не входят.

## Ограничения

Сервис не публикует лимиты. Полка держится в таких рамках:

- к API не чаще двух запросов в секунду на процесс;
- к серверам картинок не больше 4 соединений по умолчанию (настраивается от 1 до 8);
- при ответе 429 тормозит вся очередь, `Retry-After` соблюдается; если сервис просит подождать дольше минуты, Полка до конца паузы не шлёт к API ни одного запроса;
- после нескольких сбоев подряд загрузчик делает паузу до 5 минут.

Слишком частые запросы, судя по опыту других клиентов, заканчиваются блокировкой адреса: API начинает отвечать 403.

## Ловушки, на которые ушло больше всего времени

| Симптом | Причина |
|---|---|
| 403 из любой страны | нет `Referer` и `Origin` сайта |
| Скачанная страница не открывается: «Invalid PNG signature» | сервер `compress` отдаёт JPEG по адресу с `.png`. Формат определяйте по первым байтам |
| 422 на карточку аниме | поля манги (`artists`, `status_id`) для аниме запрещены |
| Плеера Animelib нет в ответе | не выполнен вход |
| Код из адреса возврата пропадает | его забирает скрипт страницы callback |
| `rating.average` не разбирается как число | приходит строкой (`"9.42"`) |
| Адрес видео даёт 404 | буква «а» в `/.аs/` должна быть кириллической |

## Источники

- скрипты сайта mangalib.me (вход, адрес возврата);
- официальное Android-приложение lib.social версии 2.0.4, разобранное jadx (адреса, клиенты OAuth, разбор плееров);
- расширение для Mihon: [keiyoushi/extensions-source](https://github.com/keiyoushi/extensions-source), `lib-multisrc/libgroup`;
- [vypivshiy/anicli-api](https://github.com/vypivshiy/anicli-api) и [Fazer000/AnimeLib-Mobile](https://github.com/Fazer000/AnimeLib-Mobile) (плеер Animelib);
- Telegram-канал [@mangalib_social](https://t.me/mangalib_social).
