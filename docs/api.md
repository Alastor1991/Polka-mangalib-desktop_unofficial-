# API lib.social: что известно

Официальной документации у сервиса нет. Здесь собрано то, что удалось выяснить по поведению сайта, его скриптам, официальному Android-приложению и открытым клиентам. Сведения актуальны на октябрь 2026 года, сервис может поменять что угодно в любой момент.

Код, который всё это реализует: `crates/libsocial`.

## Сайты сети

Все сайты обслуживает один API. Сайт выбирается заголовком `Site-Id`.

| Сайт | `Site-Id` | Домены сайта |
|---|---|---|
| MangaLib | 1 | mangalib.me, mangalib.org |
| SlashLib | 2 | slashlib.me, v2.slashlib.me |
| RanobeLib | 3 | ranobelib.me (в Полке пока не поддерживается) |
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
