//! Уведомления и личные сообщения без интерфейса.
//!
//! Сайт не получает их в реальном времени: счётчик уведомлений он запрашивает при
//! старте, диалоги при открытии. `Inbox` опрашивает счётчики сам, только с входом и
//! не чаще раза в минуту, а оболочка узнаёт о переменах через канал событий, как у
//! загрузчика. Диалоги запрашиваются, когда изменился счётчик вкладки «Личка», и для
//! верности раз в несколько опросов: приходит ли уведомление о каждом личном
//! сообщении, вживую не проверено.
//!
//! Списки для экранов, `NotificationList` и `Dialogs`, в сеть не ходят, как `Catalog`:
//! оболочка берёт у них запрос, выполняет его на клиенте и отдаёт ответ обратно.
//! Ответ на устаревший запрос (сменили вкладку, пока грузилось) выбрасывается.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use libsocial::notifications::{
    Notification, NotificationEvent, NotificationTab, NotificationsQuery, ReadFilter, TabCounts,
};
use libsocial::rich::Doc;
use libsocial::social::MessageThread;
use libsocial::{Client, Page};
use tokio::sync::{Notify, broadcast};
use tokio::time::Instant;

/// Пауза между опросами, пока окно на виду.
const FOREGROUND: Duration = Duration::from_secs(120);
/// Пауза между опросами, пока окно свёрнуто или закрыто в трей.
const BACKGROUND: Duration = Duration::from_secs(600);
/// Чаще этого счётчики не запрашиваются, даже если оболочка просит обновить.
const MIN_GAP: Duration = Duration::from_secs(60);
/// Если счётчик «Лички» не менялся, диалоги перепроверяются раз в столько опросов.
const THREADS_EVERY: u32 = 5;

#[derive(Debug, Clone, Copy)]
struct Timing {
    foreground: Duration,
    background: Duration,
    min_gap: Duration,
}

const TIMING: Timing = Timing {
    foreground: FOREGROUND,
    background: BACKGROUND,
    min_gap: MIN_GAP,
};

/// Что изменилось с прошлого опроса или после правки на месте.
#[derive(Debug, Clone)]
pub enum InboxEvent {
    /// Изменились непрочитанные уведомления. `added` – на сколько их стало больше, чем
    /// было, без вкладки «Личка»: повод для системного уведомления. О личных
    /// сообщениях такой повод даёт `Messages`, чтобы одно сообщение не показывалось
    /// дважды. Первый опрос после входа только задаёт отсчёт, у него `added` равно нулю.
    Notifications { unread: TabCounts, added: u64 },
    /// Изменилось число непрочитанных сообщений в первой странице диалогов. `fresh` –
    /// диалоги, где непрочитанного прибавилось, в порядке сервера; у первого опроса
    /// пусто. Свежую страницу целиком отдаёт `Inbox::threads`.
    Messages { unread: u64, fresh: Vec<MessageThread> },
    /// Входа больше нет или вошёл другой пользователь: счётчики и диалоги сброшены.
    SignedOut,
}

#[derive(Default)]
struct State {
    /// Кого назвала оболочка. Смена пользователя сбрасывает отсчёт.
    user: Option<u64>,
    /// Оболочка уже называла пользователя. Первый раз опрос не будится: он и так
    /// уходит сразу после запуска.
    named: bool,
    /// Непрочитанные по вкладкам. `None` – с этим входом ещё не опрашивали.
    unread: Option<TabCounts>,
    /// Первая страница диалогов из последнего опроса.
    threads: Option<Vec<MessageThread>>,
    /// Опросов подряд без запроса диалогов.
    polls_since_threads: u32,
    /// Растёт при каждой правке на месте и при сбросе. Ответ опроса, ушедшего до
    /// правки, устарел и не применяется, а следующий опрос уходит после
    /// минимальной паузы, не дожидаясь обычной.
    edits: u64,
    /// Последний опрос выброшен как устаревший.
    stale: bool,
    /// Последняя ошибка в журнале: одну и ту же не повторяем каждый опрос.
    last_error: Option<String>,
}

impl State {
    /// Забывает всё, что знал о прежнем входе. Говорит, было ли что забывать.
    fn forget(&mut self) -> bool {
        let had = self.unread.is_some() || self.threads.is_some();
        self.unread = None;
        self.threads = None;
        self.polls_since_threads = 0;
        self.edits += 1;
        had
    }

    fn unread_messages(&self) -> Option<u64> {
        self.threads
            .as_ref()
            .map(|list| list.iter().map(MessageThread::unread).sum())
    }
}

/// Счётчики уведомлений и сообщений с фоновым опросом. Дёшево клонируется.
#[derive(Clone)]
pub struct Inbox {
    state: Arc<Mutex<State>>,
    wake: Arc<Notify>,
    background: Arc<AtomicBool>,
    events: broadcast::Sender<InboxEvent>,
}

impl Inbox {
    /// Создаёт счётчики и запускает опрос в текущем рантайме tokio. Первый опрос
    /// уходит сразу, если есть вход.
    pub fn start(client: Client) -> Inbox {
        Inbox::start_with(client, TIMING)
    }

    fn start_with(client: Client, timing: Timing) -> Inbox {
        let inbox = Inbox::detached();
        let this = inbox.clone();
        tokio::spawn(async move { this.run(client, timing).await });
        inbox
    }

    /// Счётчики без опроса: меняются только правками на месте. `start_with` запускает
    /// опрос поверх них, тесты берут как есть.
    pub(crate) fn detached() -> Inbox {
        let (events, _) = broadcast::channel(64);
        Inbox {
            state: Arc::new(Mutex::new(State::default())),
            wake: Arc::new(Notify::new()),
            background: Arc::new(AtomicBool::new(false)),
            events,
        }
    }

    async fn run(self, client: Client, timing: Timing) {
        let mut last: Option<Instant> = None;
        loop {
            if let Some(at) = last {
                let pause = if self.background.load(Ordering::Relaxed) {
                    timing.background
                } else {
                    timing.foreground
                };
                let _ = tokio::time::timeout(pause, self.wake.notified()).await;
                tokio::time::sleep_until(at + timing.min_gap).await;
            }
            if let Some(wait) = client.cooldown_remaining() {
                tokio::time::sleep(wait).await;
            }
            for event in poll(&client, &self.state).await {
                let _ = self.events.send(event);
            }
            last = Some(Instant::now());
            if std::mem::take(&mut self.state().stale) {
                self.wake.notify_one();
            }
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<InboxEvent> {
        self.events.subscribe()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn emit(&self, event: InboxEvent) {
        let _ = self.events.send(event);
    }

    /// Непрочитанные уведомления по вкладкам. Нули, пока опроса не было.
    pub fn unread(&self) -> TabCounts {
        self.state().unread.clone().unwrap_or_default()
    }

    /// Непрочитанные сообщения в первой странице диалогов.
    pub fn unread_messages(&self) -> u64 {
        self.state().unread_messages().unwrap_or(0)
    }

    /// Первая страница диалогов из последнего опроса, для `Dialogs::merge_latest`.
    pub fn threads(&self) -> Vec<MessageThread> {
        self.state().threads.clone().unwrap_or_default()
    }

    /// Кто вошёл, по ответу `/auth/me`; `None` после выхода. Если пользователь
    /// сменился, счётчики прежнего забываются, а опрос уходит, как только позволит
    /// пауза между опросами.
    pub fn set_user(&self, user: Option<u64>) {
        let had = {
            let mut s = self.state();
            if s.named && s.user == user {
                return;
            }
            let switched = s.user.is_some();
            s.user = user;
            let first = !std::mem::replace(&mut s.named, true);
            (switched && s.forget(), first)
        };
        if had.0 {
            self.emit(InboxEvent::SignedOut);
        }
        if !had.1 {
            self.wake.notify_one();
        }
    }

    /// Окно ушло в фон или вернулось. В фоне опрос реже, а по возвращении уходит
    /// сразу, если с прошлого прошла минута.
    pub fn set_background(&self, background: bool) {
        let was = self.background.swap(background, Ordering::Relaxed);
        if was && !background {
            self.wake.notify_one();
        }
    }

    /// Просит опросить сейчас, например после того как запись отметили прочитанной
    /// на сервере. Чаще раза в минуту опрос всё равно не уходит.
    pub fn refresh(&self) {
        self.wake.notify_one();
    }

    /// Сервер принял отметку о прочтении: счётчик уменьшается сразу, не дожидаясь
    /// опроса. Звать после успешного ответа, а не до него: иначе опрос, который
    /// обгонит запись на сервере, вернёт прежнее число, и уменьшение пропадёт до
    /// следующего опроса. Запись – та, которую `NotificationList::mark_read` назвал
    /// непрочитанной.
    pub fn note_read(&self, notification: &Notification) {
        let tab = NotificationTab::parse(&notification.category).filter(|t| *t != NotificationTab::All);
        self.edit_unread(|unread| {
            unread.all = unread.all.saturating_sub(1);
            if let Some(tab) = tab {
                let slot = unread.get_mut(tab);
                *slot = slot.saturating_sub(1);
            }
        });
    }

    /// Сервер отметил прочитанными все уведомления вкладки (`NotificationTab::All` –
    /// все сразу). Звать после успешного ответа, как `note_read`.
    pub fn note_all_read(&self, tab: NotificationTab) {
        self.edit_unread(|unread| {
            if tab == NotificationTab::All {
                *unread = TabCounts::default();
            } else {
                let n = std::mem::take(unread.get_mut(tab));
                unread.all = unread.all.saturating_sub(n);
            }
        });
    }

    /// Правит счётчики на месте. Если ничего не изменилось, события нет и опрос в
    /// пути не выбрасывается.
    fn edit_unread(&self, edit: impl FnOnce(&mut TabCounts)) {
        let unread = {
            let mut s = self.state();
            let Some(unread) = s.unread.as_mut() else {
                return;
            };
            let before = unread.clone();
            edit(unread);
            if *unread == before {
                return;
            }
            let unread = unread.clone();
            s.edits += 1;
            unread
        };
        self.emit(InboxEvent::Notifications { unread, added: 0 });
    }

    /// Диалог открыли: сервер сам обнулил его непрочитанное, когда отдал сообщения.
    pub fn note_thread_read(&self, thread_id: u64) {
        let unread = {
            let mut s = self.state();
            let Some(thread) = s
                .threads
                .as_mut()
                .and_then(|list| list.iter_mut().find(|t| t.id == thread_id))
                .filter(|t| t.unread() > 0)
            else {
                return;
            };
            thread.read_info.auth.unread_count = 0;
            s.edits += 1;
            s.unread_messages().unwrap_or(0)
        };
        self.emit(InboxEvent::Messages {
            unread,
            fresh: Vec::new(),
        });
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().expect("inbox отравлен")
}

/// Один опрос: счётчики, при надобности диалоги. Отдаёт события по переменам.
async fn poll(client: &Client, state: &Mutex<State>) -> Vec<InboxEvent> {
    if !client.is_logged_in().await {
        return if lock(state).forget() {
            vec![InboxEvent::SignedOut]
        } else {
            Vec::new()
        };
    }
    let edits = lock(state).edits;
    let counts = match client.notification_counts().await {
        Ok(counts) => counts,
        Err(e) => {
            note_error(state, &e);
            return Vec::new();
        }
    };
    let want_threads = {
        let s = lock(state);
        s.threads.is_none()
            || s.polls_since_threads + 1 >= THREADS_EVERY
            || s.unread.as_ref().is_some_and(|u| u.message != counts.unread.message)
    };
    let threads = if want_threads {
        match client.message_threads(false, "", 1).await {
            Ok(page) => Some(Ok(page.items)),
            Err(e) => Some(Err(e)),
        }
    } else {
        None
    };

    let mut s = lock(state);
    if s.edits != edits {
        s.stale = true;
        return Vec::new();
    }
    let mut events = Vec::new();
    let unread = counts.unread;
    if s.unread.as_ref() != Some(&unread) {
        let added = s
            .unread
            .as_ref()
            .map_or(0, |prev| reasons(&unread).saturating_sub(reasons(prev)));
        events.push(InboxEvent::Notifications {
            unread: unread.clone(),
            added,
        });
    }
    s.unread = Some(unread);

    match threads {
        Some(Ok(list)) => {
            s.polls_since_threads = 0;
            let before = s.unread_messages();
            let fresh: Vec<MessageThread> = match &s.threads {
                None => Vec::new(),
                Some(prev) => {
                    let was: HashMap<u64, u64> = prev.iter().map(|t| (t.id, t.unread())).collect();
                    list.iter()
                        .filter(|t| t.unread() > was.get(&t.id).copied().unwrap_or(0))
                        .cloned()
                        .collect()
                }
            };
            s.threads = Some(list);
            let total = s.unread_messages().unwrap_or(0);
            if before != Some(total) || !fresh.is_empty() {
                events.push(InboxEvent::Messages { unread: total, fresh });
            }
            s.last_error = None;
        }
        Some(Err(e)) => {
            // Диалоги не пришли: попробуем на следующем опросе.
            s.polls_since_threads = THREADS_EVERY;
            drop(s);
            note_error(state, &e);
        }
        None => {
            s.polls_since_threads += 1;
            s.last_error = None;
        }
    }
    events
}

/// Непрочитанные уведомления, о которых стоит сказать системным уведомлением: все,
/// кроме личных сообщений.
fn reasons(c: &TabCounts) -> u64 {
    c.all.saturating_sub(c.message)
}

/// Пишет ошибку в журнал, если она не сетевая и не та же, что в прошлый раз. Без
/// сети и при недоступном сервисе опрос просто ждёт следующего раза.
fn note_error(state: &Mutex<State>, e: &libsocial::Error) {
    use libsocial::Error;
    if matches!(
        e,
        Error::Network(_) | Error::ServiceDown { .. } | Error::RateLimited { .. }
    ) {
        return;
    }
    let text = e.to_string();
    let mut s = lock(state);
    if s.last_error.as_deref() != Some(text.as_str()) {
        eprintln!("уведомления не обновились: {text}");
        s.last_error = Some(text);
    }
}

/// Метка запроса списка: по ней `apply` узнаёт устаревший ответ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ticket {
    generation: u64,
}

/// Общее у списков: следующая страница, запрос в пути, поколение.
#[derive(Debug, Clone)]
struct Paging {
    next_page: Option<u32>,
    loading: bool,
    generation: u64,
}

impl Paging {
    fn new() -> Paging {
        Paging {
            next_page: Some(1),
            loading: false,
            generation: 0,
        }
    }

    fn reset(&mut self) {
        self.generation += 1;
        self.next_page = Some(1);
        self.loading = false;
    }

    /// Номер следующей страницы, если она есть и прежний запрос вернулся.
    fn start(&mut self) -> Option<(Ticket, u32)> {
        if self.loading {
            return None;
        }
        let page = self.next_page?;
        self.loading = true;
        Some((
            Ticket {
                generation: self.generation,
            },
            page,
        ))
    }

    /// Ответ свежий: принимаем его и сдвигаем страницу. Устаревший – `false`.
    fn finish(&mut self, ticket: Ticket, ok_has_next: Option<bool>) -> bool {
        if ticket.generation != self.generation {
            return false;
        }
        self.loading = false;
        if let Some(has_next) = ok_has_next {
            self.next_page = match self.next_page {
                Some(current) if has_next => Some(current + 1),
                _ => None,
            };
        }
        true
    }

    /// Первая страница уже пришла.
    fn started(&self) -> bool {
        self.next_page != Some(1)
    }
}

/// Запрос страницы уведомлений. Выполняет его оболочка через `run`.
#[derive(Debug, Clone)]
pub struct NotificationsRequest {
    ticket: Ticket,
    pub query: NotificationsQuery,
}

impl NotificationsRequest {
    pub fn ticket(&self) -> Ticket {
        self.ticket
    }

    pub async fn run(&self, client: &Client) -> libsocial::Result<Page<Notification>> {
        client.notifications(&self.query).await
    }
}

/// Уведомление в списке для показа.
#[derive(Debug, Clone)]
pub struct NotificationRow<'a> {
    pub notification: &'a Notification,
    pub event: NotificationEvent,
    pub text: Doc,
    /// Не прочитано. `None` – неизвестно: при фильтре «все» сервер прочитанность не
    /// присылает.
    pub unread: Option<bool>,
}

/// Лента уведомлений одной вкладки с одним фильтром.
#[derive(Debug, Clone)]
pub struct NotificationList {
    query: NotificationsQuery,
    items: Vec<Notification>,
    seen: HashSet<u64>,
    /// Отмеченные прочитанными здесь. Сервер прочитанность в записи не присылает,
    /// её знает только фильтр, по которому запись пришла.
    read: HashSet<u64>,
    paging: Paging,
}

impl NotificationList {
    pub fn new(query: NotificationsQuery) -> NotificationList {
        NotificationList {
            query,
            items: Vec::new(),
            seen: HashSet::new(),
            read: HashSet::new(),
            paging: Paging::new(),
        }
    }

    pub fn query(&self) -> &NotificationsQuery {
        &self.query
    }

    /// Другая вкладка, фильтр, порядок или поиск: список с первой страницы.
    pub fn set_query(&mut self, query: NotificationsQuery) {
        self.query = query;
        self.reload();
    }

    pub fn reload(&mut self) {
        self.paging.reset();
        self.items.clear();
        self.seen.clear();
        self.read.clear();
    }

    pub fn items(&self) -> &[Notification] {
        &self.items
    }

    pub fn has_next(&self) -> bool {
        self.paging.next_page.is_some()
    }

    pub fn is_loading(&self) -> bool {
        self.paging.loading
    }

    /// Следующая страница, если она есть и предыдущий запрос уже вернулся. После
    /// ошибки просто зовите снова: страница та же.
    pub fn next_request(&mut self) -> Option<NotificationsRequest> {
        let (ticket, page) = self.paging.start()?;
        Some(NotificationsRequest {
            ticket,
            query: NotificationsQuery {
                page,
                ..self.query.clone()
            },
        })
    }

    /// Ответ на запрос. Отдаёт записи, которые добавились в конец. Ответ на
    /// устаревший запрос не меняет ничего.
    pub fn apply<E>(&mut self, ticket: Ticket, result: Result<Page<Notification>, E>) -> Result<&[Notification], E> {
        let has_next = result.as_ref().ok().map(|p| p.has_next);
        if !self.paging.finish(ticket, has_next) {
            return Ok(&[]);
        }
        let start = self.items.len();
        for n in result?.items {
            if self.seen.insert(n.id) {
                self.items.push(n);
            }
        }
        Ok(&self.items[start..])
    }

    /// Не прочитано ли уведомление, насколько это известно списку.
    pub fn is_unread(&self, n: &Notification) -> Option<bool> {
        if self.read.contains(&n.id) {
            return Some(false);
        }
        match self.query.read {
            ReadFilter::Unread => Some(true),
            ReadFilter::Read => Some(false),
            ReadFilter::All => n.is_read.map(|r| !r),
        }
    }

    pub fn rows(&self) -> Vec<NotificationRow<'_>> {
        self.items
            .iter()
            .map(|n| NotificationRow {
                notification: n,
                event: n.event(),
                text: n.text(),
                unread: self.is_unread(n),
            })
            .collect()
    }

    /// Отмечает прочитанным в списке сразу, до ответа сервера. Возвращает запись,
    /// если она точно была непрочитанной: после успешного ответа её отдают
    /// `Inbox::note_read`. Если прочитанность не была известна, после ответа зовите
    /// `Inbox::refresh`. При ошибке – `unmark_read`.
    pub fn mark_read(&mut self, id: u64) -> Option<Notification> {
        let n = self.items.iter().find(|n| n.id == id)?;
        let was = self.is_unread(n);
        let n = n.clone();
        self.read.insert(id);
        (was == Some(true)).then_some(n)
    }

    /// Откат `mark_read`, если сервер ответил ошибкой.
    pub fn unmark_read(&mut self, id: u64) {
        self.read.remove(&id);
    }

    /// Все загруженные записи прочитаны (после `mark_notifications_read`).
    pub fn mark_all_read(&mut self) {
        self.read.extend(self.items.iter().map(|n| n.id));
    }

    /// Убирает удалённую запись.
    pub fn remove(&mut self, id: u64) -> Option<Notification> {
        let at = self.items.iter().position(|n| n.id == id)?;
        self.read.remove(&id);
        Some(self.items.remove(at))
    }
}

/// Запрос страницы диалогов. Выполняет его оболочка через `run`.
#[derive(Debug, Clone)]
pub struct DialogsRequest {
    ticket: Ticket,
    pub archived: bool,
    pub text: String,
    pub page: u32,
}

impl DialogsRequest {
    pub fn ticket(&self) -> Ticket {
        self.ticket
    }

    pub async fn run(&self, client: &Client) -> libsocial::Result<Page<MessageThread>> {
        client.message_threads(self.archived, &self.text, self.page).await
    }
}

/// Список диалогов: обычные или архив, с поиском или без.
#[derive(Debug, Clone)]
pub struct Dialogs {
    archived: bool,
    text: String,
    items: Vec<MessageThread>,
    paging: Paging,
}

impl Dialogs {
    pub fn new(archived: bool) -> Dialogs {
        Dialogs {
            archived,
            text: String::new(),
            items: Vec::new(),
            paging: Paging::new(),
        }
    }

    pub fn archived(&self) -> bool {
        self.archived
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn set_archived(&mut self, archived: bool) {
        if self.archived != archived {
            self.archived = archived;
            self.reload();
        }
    }

    /// Поиск по диалогам. Пробелы по краям не в счёт.
    pub fn set_text(&mut self, text: &str) {
        let text = text.trim();
        if self.text != text {
            self.text = text.to_string();
            self.reload();
        }
    }

    pub fn reload(&mut self) {
        self.paging.reset();
        self.items.clear();
    }

    pub fn items(&self) -> &[MessageThread] {
        &self.items
    }

    pub fn has_next(&self) -> bool {
        self.paging.next_page.is_some()
    }

    pub fn is_loading(&self) -> bool {
        self.paging.loading
    }

    pub fn next_request(&mut self) -> Option<DialogsRequest> {
        let (ticket, page) = self.paging.start()?;
        Some(DialogsRequest {
            ticket,
            archived: self.archived,
            text: self.text.clone(),
            page,
        })
    }

    /// Ответ на запрос. Отдаёт диалоги, которые добавились в конец. Диалог, который
    /// уже есть в списке (его подняло наверх новое сообщение), не повторяется.
    pub fn apply<E>(&mut self, ticket: Ticket, result: Result<Page<MessageThread>, E>) -> Result<&[MessageThread], E> {
        let has_next = result.as_ref().ok().map(|p| p.has_next);
        if !self.paging.finish(ticket, has_next) {
            return Ok(&[]);
        }
        let start = self.items.len();
        for t in result?.items {
            if !self.items.iter().any(|x| x.id == t.id) {
                self.items.push(t);
            }
        }
        Ok(&self.items[start..])
    }

    /// Свежая первая страница из опроса (`Inbox::threads`). Её диалоги встают наверх
    /// в порядке сервера, остальные сохраняют свой. Архив и поиск она не трогает,
    /// как и список, у которого первая страница ещё не пришла.
    pub fn merge_latest(&mut self, latest: &[MessageThread]) {
        if self.archived || !self.text.is_empty() || !self.paging.started() {
            return;
        }
        let ids: HashSet<u64> = latest.iter().map(|t| t.id).collect();
        self.items.retain(|t| !ids.contains(&t.id));
        self.items.splice(0..0, latest.iter().cloned());
    }

    /// Диалог открыли: сервер обнулил его непрочитанное, когда отдал сообщения.
    pub fn mark_read(&mut self, thread_id: u64) {
        if let Some(t) = self.items.iter_mut().find(|t| t.id == thread_id) {
            t.read_info.auth.unread_count = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use libsocial::ClientConfig;
    use serde_json::{Value, json};
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    async fn client(server: &MockServer, signed_in: bool) -> Client {
        let client = Client::new(ClientConfig {
            api_mirrors: vec![format!("{}/api", server.uri())],
            api_interval: Duration::ZERO,
            ..Default::default()
        })
        .unwrap();
        if signed_in {
            client
                .set_token(Some(libsocial::Token {
                    access_token: "t".into(),
                    refresh_token: None,
                    token_type: "Bearer".into(),
                    expires_at: libsocial::auth::now_secs() + 3600,
                    client_id: "1".into(),
                }))
                .await;
        }
        client
    }

    fn counts(all: u64, chapter: u64, message: u64) -> Value {
        let tabs = json!({"all": all, "chapter": chapter, "comments": all - chapter - message, "message": message});
        json!({"data": {"unread": tabs, "read": {}, "all": {}}})
    }

    fn thread(id: u64, unread: u64) -> Value {
        json!({"id": id, "subject": format!("u{id}"), "read_info": {"auth": {"unread_count": unread}}})
    }

    async fn mount_counts(server: &MockServer, body: Value) {
        Mock::given(method("GET"))
            .and(path("/api/notifications/count"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    async fn mount_threads(server: &MockServer, threads: Vec<Value>) {
        Mock::given(method("GET"))
            .and(path("/api/messenger/threads"))
            .and(query_param("archived", "0"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": threads})))
            .mount(server)
            .await;
    }

    async fn requests_to(server: &MockServer, p: &str) -> usize {
        let all = server.received_requests().await.unwrap_or_default();
        all.iter().filter(|r| r.url.path() == p).count()
    }

    #[tokio::test]
    async fn first_poll_is_baseline_then_growth_is_reported() {
        let server = MockServer::start().await;
        let client = client(&server, true).await;
        let state = Mutex::new(State::default());
        mount_counts(&server, counts(2, 1, 0)).await;
        mount_threads(&server, vec![thread(1, 0)]).await;

        let events = poll(&client, &state).await;
        assert!(matches!(
            &events[..],
            [InboxEvent::Notifications { unread, added: 0 }, InboxEvent::Messages { unread: 0, fresh }]
                if unread.all == 2 && fresh.is_empty()
        ));
        // Ничего не изменилось: событий нет, диалоги не запрашиваются.
        assert!(poll(&client, &state).await.is_empty());
        assert_eq!(requests_to(&server, "/api/messenger/threads").await, 1);

        server.reset().await;
        mount_counts(&server, counts(5, 4, 0)).await;
        let events = poll(&client, &state).await;
        assert!(matches!(&events[..], [InboxEvent::Notifications { added: 3, .. }]));
        assert_eq!(requests_to(&server, "/api/messenger/threads").await, 0);
    }

    #[tokio::test]
    async fn threads_follow_message_counter_and_report_fresh_dialogs() {
        let server = MockServer::start().await;
        let client = client(&server, true).await;
        let state = Mutex::new(State::default());
        mount_counts(&server, counts(0, 0, 0)).await;
        mount_threads(&server, vec![thread(1, 0), thread(2, 1)]).await;
        let _ = poll(&client, &state).await;

        // Пришло личное сообщение: счётчик «Лички» вырос, диалог 1 поднялся наверх.
        server.reset().await;
        mount_counts(&server, counts(1, 0, 1)).await;
        mount_threads(&server, vec![thread(1, 2), thread(2, 1)]).await;
        let events = poll(&client, &state).await;
        let fresh = events.iter().find_map(|e| match e {
            InboxEvent::Messages { unread, fresh } => Some((*unread, fresh.iter().map(|t| t.id).collect::<Vec<_>>())),
            _ => None,
        });
        assert_eq!(fresh, Some((3, vec![1])));
        // Уведомление о том же сообщении повода для показа не даёт: его даёт `Messages`.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, InboxEvent::Notifications { added: 0, unread } if unread.message == 1))
        );

        // Без перемен в счётчике диалоги перепроверяются раз в THREADS_EVERY опросов.
        server.reset().await;
        mount_counts(&server, counts(1, 0, 1)).await;
        mount_threads(&server, vec![thread(1, 2), thread(2, 1)]).await;
        for _ in 0..THREADS_EVERY {
            assert!(poll(&client, &state).await.is_empty());
        }
        assert_eq!(requests_to(&server, "/api/messenger/threads").await, 1);
    }

    #[tokio::test]
    async fn guest_is_not_polled_and_logout_clears() {
        let server = MockServer::start().await;
        let client = client(&server, false).await;
        let state = Mutex::new(State::default());
        assert!(poll(&client, &state).await.is_empty());
        assert!(server.received_requests().await.unwrap().is_empty());

        lock(&state).unread = Some(TabCounts::default());
        assert!(matches!(&poll(&client, &state).await[..], [InboxEvent::SignedOut]));
        assert!(lock(&state).unread.is_none());
    }

    #[tokio::test]
    async fn local_edit_during_poll_discards_stale_answer() {
        let server = MockServer::start().await;
        let client = client(&server, true).await;
        let inbox = Inbox::detached();
        lock(&inbox.state).unread = Some(serde_json::from_value(json!({"all": 3, "chapter": 3})).unwrap());
        Mock::given(path("/api/notifications/count"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(counts(3, 3, 0))
                    .set_delay(Duration::from_secs(1)),
            )
            .mount(&server)
            .await;
        mount_threads(&server, vec![]).await;
        let state = inbox.state.clone();
        let flight = tokio::spawn(async move { poll(&client, &state).await });
        // Правим, когда запрос уже ушёл и ответ ещё не вернулся.
        for _ in 0..100 {
            if requests_to(&server, "/api/notifications/count").await > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        inbox.note_all_read(NotificationTab::Chapter);
        assert!(flight.await.unwrap().is_empty());
        assert_eq!(inbox.unread().all, 0);
        // Выброшенный опрос просит повторить его без обычной паузы.
        assert!(lock(&inbox.state).stale);
    }

    #[tokio::test]
    async fn local_edits_adjust_counters() {
        let inbox = Inbox::detached();
        let mut events = inbox.subscribe();
        // До первого опроса править нечего.
        inbox.note_all_read(NotificationTab::All);
        assert!(events.try_recv().is_err());

        {
            let mut s = lock(&inbox.state);
            s.unread = Some(serde_json::from_value(json!({"all": 4, "chapter": 2, "comments": 2})).unwrap());
            s.threads = Some(vec![
                serde_json::from_value(thread(1, 2)).unwrap(),
                serde_json::from_value(thread(2, 1)).unwrap(),
            ]);
        }
        let note: Notification =
            serde_json::from_value(json!({"id": 7, "type": "comment", "category": "comments"})).unwrap();
        inbox.note_read(&note);
        assert_eq!((inbox.unread().all, inbox.unread().comments), (3, 1));
        inbox.note_all_read(NotificationTab::Chapter);
        assert_eq!((inbox.unread().all, inbox.unread().chapter), (1, 0));
        inbox.note_thread_read(1);
        assert_eq!(inbox.unread_messages(), 1);
        // Уже прочитанный диалог событий не даёт.
        inbox.note_thread_read(1);

        let mut got = Vec::new();
        while let Ok(e) = events.try_recv() {
            got.push(e);
        }
        assert!(matches!(
            &got[..],
            [
                InboxEvent::Notifications { added: 0, .. },
                InboxEvent::Notifications { added: 0, .. },
                InboxEvent::Messages { unread: 1, .. }
            ]
        ));
    }

    #[tokio::test]
    async fn switching_user_forgets_previous_counters() {
        let inbox = Inbox::detached();
        let mut events = inbox.subscribe();
        inbox.set_user(Some(1));
        lock(&inbox.state).unread = Some(TabCounts::default());
        // Тот же пользователь – ничего не забываем.
        inbox.set_user(Some(1));
        assert!(events.try_recv().is_err());
        inbox.set_user(Some(2));
        assert!(matches!(events.try_recv(), Ok(InboxEvent::SignedOut)));
        assert!(lock(&inbox.state).unread.is_none());
    }

    #[tokio::test]
    async fn background_task_polls_and_keeps_minimal_gap() {
        let server = MockServer::start().await;
        mount_counts(&server, counts(1, 1, 0)).await;
        mount_threads(&server, vec![]).await;
        let client = client(&server, true).await;
        // Подписка до запуска опроса: первое событие не потеряется ни в каком рантайме.
        let inbox = Inbox::detached();
        let mut events = inbox.subscribe();
        let timing = Timing {
            foreground: Duration::from_secs(30),
            background: Duration::from_secs(30),
            min_gap: Duration::from_millis(800),
        };
        tokio::spawn(inbox.clone().run(client, timing));
        let first = tokio::time::timeout(Duration::from_secs(5), events.recv()).await;
        assert!(matches!(first, Ok(Ok(InboxEvent::Notifications { added: 0, .. }))));
        assert_eq!(inbox.unread().all, 1);

        // Просьба обновить сразу после опроса ждёт минимальной паузы.
        inbox.refresh();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(requests_to(&server, "/api/notifications/count").await, 1);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(requests_to(&server, "/api/notifications/count").await, 2);
    }

    fn notes(ids: &[u64], has_next: bool) -> Page<Notification> {
        let mut page = Page::empty();
        page.items = ids
            .iter()
            .map(|id| serde_json::from_value(json!({"id": id, "type": "chapter", "category": "chapter"})).unwrap())
            .collect();
        page.has_next = has_next;
        page
    }

    #[test]
    fn notification_list_pages_and_drops_stale_answers() {
        let mut list = NotificationList::new(NotificationsQuery {
            read: ReadFilter::Unread,
            ..Default::default()
        });
        let first = list.next_request().unwrap();
        assert_eq!(first.query.page, 1);
        // Второй запрос не уходит, пока не вернулся первый.
        assert!(list.next_request().is_none());
        let added = list.apply::<()>(first.ticket(), Ok(notes(&[1, 2], true))).unwrap();
        assert_eq!(added.len(), 2);

        let second = list.next_request().unwrap();
        assert_eq!(second.query.page, 2);
        // Сменили вкладку, пока страница грузилась: ответ выбрасывается.
        list.set_query(NotificationsQuery {
            tab: NotificationTab::Chapter,
            ..Default::default()
        });
        assert!(
            list.apply::<()>(second.ticket(), Ok(notes(&[3], false)))
                .unwrap()
                .is_empty()
        );
        assert!(list.items().is_empty());

        let req = list.next_request().unwrap();
        assert_eq!(req.query.tab, NotificationTab::Chapter);
        assert!(list.apply(req.ticket(), Err("сеть")).is_err());
        // После ошибки та же страница.
        assert_eq!(list.next_request().unwrap().query.page, 1);
    }

    #[test]
    fn notification_list_read_marks() {
        let mut list = NotificationList::new(NotificationsQuery {
            read: ReadFilter::Unread,
            ..Default::default()
        });
        let req = list.next_request().unwrap();
        list.apply::<()>(req.ticket(), Ok(notes(&[1, 2, 2, 3], false))).unwrap();
        assert_eq!(list.items().len(), 3);
        assert!(!list.has_next());

        assert!(list.mark_read(1).is_some());
        // Повторно – уже прочитано, счётчик второй раз не трогаем.
        assert!(list.mark_read(1).is_none());
        list.unmark_read(1);
        assert_eq!(list.rows()[0].unread, Some(true));
        list.mark_all_read();
        assert!(list.rows().iter().all(|r| r.unread == Some(false)));
        assert!(list.remove(2).is_some());
        assert_eq!(list.items().len(), 2);

        // При фильтре «все» прочитанность неизвестна.
        let mut all = NotificationList::new(NotificationsQuery::default());
        let req = all.next_request().unwrap();
        all.apply::<()>(req.ticket(), Ok(notes(&[1], false))).unwrap();
        assert_eq!(all.rows()[0].unread, None);
        assert!(all.mark_read(1).is_none());
        assert_eq!(all.rows()[0].unread, Some(false));
    }

    fn threads(ids: &[(u64, u64)], has_next: bool) -> Page<MessageThread> {
        let mut page = Page::empty();
        page.items = ids
            .iter()
            .map(|(id, unread)| serde_json::from_value(thread(*id, *unread)).unwrap())
            .collect();
        page.has_next = has_next;
        page
    }

    #[test]
    fn dialogs_merge_latest_page_on_top() {
        let mut d = Dialogs::new(false);
        let latest = threads(&[(9, 1)], false).items;
        // Первая страница ещё не пришла: сливать не с чем.
        d.merge_latest(&latest);
        assert!(d.items().is_empty());

        let req = d.next_request().unwrap();
        d.apply::<()>(req.ticket(), Ok(threads(&[(1, 0), (2, 0), (3, 0)], true)))
            .unwrap();
        // Новое сообщение в диалоге 3 и новый диалог 9.
        d.merge_latest(&threads(&[(3, 1), (9, 1), (1, 0)], false).items);
        let ids: Vec<u64> = d.items().iter().map(|t| t.id).collect();
        assert_eq!(ids, [3, 9, 1, 2]);
        assert_eq!(d.items()[0].unread(), 1);

        // Вторая страница сдвинулась: диалог 2 пришёл ещё раз и не повторяется.
        let req = d.next_request().unwrap();
        assert_eq!(req.page, 2);
        let added = d
            .apply::<()>(req.ticket(), Ok(threads(&[(2, 0), (4, 0)], false)))
            .unwrap();
        assert_eq!(added.len(), 1);

        d.mark_read(3);
        assert_eq!(d.items()[0].unread(), 0);

        // Поиск и архив свежей страницей не трогаются.
        d.set_text("  u4 ");
        assert_eq!(d.text(), "u4");
        let req = d.next_request().unwrap();
        assert_eq!(req.text, "u4");
        d.apply::<()>(req.ticket(), Ok(threads(&[(4, 0)], false))).unwrap();
        d.merge_latest(&latest);
        assert_eq!(d.items().len(), 1);
    }
}
