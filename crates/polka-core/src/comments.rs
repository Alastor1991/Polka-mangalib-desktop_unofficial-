//! Комментарии без интерфейса: страницы с сервера складываются в дерево, голоса и
//! новые комментарии сразу видны в нём, ветки сворачиваются на заданной глубине.
//!
//! Сервер отдаёт страницу корней и плоский список ответов к ним всех уровней. В
//! ответах бывают повторы и ответы к корням с других страниц: показываем только
//! ветки от своих корней, каждый комментарий один раз.

use std::collections::{HashMap, HashSet};

use libsocial::comments::{Comment, CommentPage, CommentSort, Target, Vote};
use libsocial::rich::Doc;

/// Комментарий в списке для показа.
#[derive(Debug, Clone)]
pub struct Row<'a> {
    pub comment: &'a Comment,
    pub text: Doc,
    /// Глубина от корня, у корня 0.
    pub depth: usize,
    /// Ветка свёрнута, под ней столько скрытых ответов.
    pub hidden: usize,
}

/// Комментарии к одному объекту: страница тайтла, страница главы, серия.
#[derive(Debug, Clone)]
pub struct Thread {
    pub target: Target,
    pub sort: CommentSort,
    /// Закреплённые: над списком, в основном списке не повторяются.
    pub sticky: Vec<Comment>,
    roots: Vec<u64>,
    by_id: HashMap<u64, Comment>,
    /// Ответы по родителю, в порядке прихода.
    children: HashMap<u64, Vec<u64>>,
    /// Следующая страница корней, `None` – дальше нет.
    pub next_page: Option<u32>,
    /// Ветки, которые пользователь развернул или свернул сам.
    toggled: HashSet<u64>,
}

impl Thread {
    pub fn new(target: Target, sort: CommentSort) -> Thread {
        Thread {
            target,
            sort,
            sticky: Vec::new(),
            roots: Vec::new(),
            by_id: HashMap::new(),
            children: HashMap::new(),
            next_page: Some(1),
            toggled: HashSet::new(),
        }
    }

    /// Сколько корней загружено.
    pub fn len(&self) -> usize {
        self.roots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty() && self.sticky.is_empty()
    }

    pub fn set_sticky(&mut self, sticky: Vec<Comment>) {
        let ids: HashSet<u64> = sticky.iter().map(|c| c.id).collect();
        self.roots.retain(|id| !ids.contains(id));
        self.sticky = sticky;
    }

    /// Добавляет страницу с сервера.
    pub fn add_page(&mut self, page: CommentPage) {
        self.next_page = if page.has_next {
            self.next_page.map(|p| p + 1)
        } else {
            None
        };
        let sticky: HashSet<u64> = self.sticky.iter().map(|c| c.id).collect();
        for root in page.roots {
            if sticky.contains(&root.id) || self.by_id.contains_key(&root.id) {
                continue;
            }
            self.roots.push(root.id);
            self.by_id.insert(root.id, root);
        }
        for reply in page.replies {
            self.add_reply(reply, false);
        }
    }

    fn add_reply(&mut self, reply: Comment, first: bool) {
        if self.by_id.contains_key(&reply.id) {
            return;
        }
        let Some(parent) = reply.parent_comment else {
            return;
        };
        let list = self.children.entry(parent).or_default();
        if first {
            list.insert(0, reply.id);
        } else {
            list.push(reply.id);
        }
        self.by_id.insert(reply.id, reply);
    }

    /// Комментарий, который только что написал пользователь: корень встаёт в
    /// начало, ответ – первым под родителем.
    pub fn insert_posted(&mut self, comment: Comment) {
        if comment.is_root() {
            if !self.by_id.contains_key(&comment.id) {
                self.roots.insert(0, comment.id);
                self.by_id.insert(comment.id, comment);
            }
        } else {
            let parent = comment.parent_comment;
            self.add_reply(comment, true);
            // Ветку, в которую ответили, показываем развёрнутой.
            if let Some(p) = parent {
                self.toggled.insert(p);
            }
        }
    }

    /// Подменяет комментарий после правки.
    pub fn replace(&mut self, comment: Comment) {
        if let Some(c) = self.by_id.get_mut(&comment.id) {
            c.comment = comment.comment;
            c.votes = comment.votes;
        }
    }

    /// Убирает удалённый комментарий. Как и сайт, убираем только его: ответы
    /// без родителя больше не показываются.
    pub fn remove(&mut self, id: u64) {
        self.roots.retain(|r| *r != id);
        self.by_id.remove(&id);
        self.sticky.retain(|c| c.id != id);
    }

    pub fn get(&self, id: u64) -> Option<&Comment> {
        self.by_id.get(&id).or_else(|| self.sticky.iter().find(|c| c.id == id))
    }

    /// Голос пользователя сразу, не дожидаясь сервера. Повторный тот же голос
    /// снимает его, как на сайте. Возвращает прежние голоса, чтобы откатить при
    /// ошибке.
    pub fn apply_vote(&mut self, id: u64, vote: Vote) -> Option<libsocial::comments::Votes> {
        let comment = match self.by_id.get_mut(&id) {
            Some(c) => c,
            None => self.sticky.iter_mut().find(|c| c.id == id)?,
        };
        let before = comment.votes.clone();
        let v = &mut comment.votes;
        match v.user {
            Some(Vote::Up) => v.up = v.up.saturating_sub(1),
            Some(Vote::Down) => v.down = v.down.saturating_sub(1),
            None => {}
        }
        if v.user == Some(vote) {
            v.user = None;
        } else {
            v.user = Some(vote);
            match vote {
                Vote::Up => v.up += 1,
                Vote::Down => v.down += 1,
            }
        }
        Some(before)
    }

    pub fn restore_votes(&mut self, id: u64, votes: libsocial::comments::Votes) {
        if let Some(c) = self.by_id.get_mut(&id) {
            c.votes = votes;
        } else if let Some(c) = self.sticky.iter_mut().find(|c| c.id == id) {
            c.votes = votes;
        }
    }

    /// Разворачивает свёрнутую ветку или сворачивает развёрнутую.
    pub fn toggle(&mut self, id: u64) {
        if !self.toggled.remove(&id) {
            self.toggled.insert(id);
        }
    }

    /// Сколько ответов под комментарием, на всех уровнях.
    pub fn reply_count(&self, id: u64) -> usize {
        self.children
            .get(&id)
            .map(|kids| {
                kids.iter()
                    .filter(|k| self.by_id.contains_key(k))
                    .map(|k| 1 + self.reply_count(*k))
                    .sum()
            })
            .unwrap_or(0)
    }

    /// Список для показа: закреплённые, затем ветки от корней в глубину. Ветки
    /// глубже `collapse_at` свёрнуты, пока пользователь их не развернёт.
    pub fn rows(&self, collapse_at: usize) -> Vec<Row<'_>> {
        let mut out = Vec::new();
        for c in &self.sticky {
            out.push(Row {
                comment: c,
                text: c.text(),
                depth: 0,
                hidden: 0,
            });
        }
        for id in &self.roots {
            self.walk(*id, 0, collapse_at, &mut out);
        }
        out
    }

    fn walk<'a>(&'a self, id: u64, depth: usize, collapse_at: usize, out: &mut Vec<Row<'a>>) {
        let Some(comment) = self.by_id.get(&id) else {
            return;
        };
        let kids: Vec<u64> = self
            .children
            .get(&id)
            .map(|k| k.iter().copied().filter(|k| self.by_id.contains_key(k)).collect())
            .unwrap_or_default();
        // На глубине сворачивания ветка по умолчанию свёрнута, выше – развёрнута.
        let collapsed = !kids.is_empty() && ((depth + 1 >= collapse_at) != self.toggled.contains(&id));
        out.push(Row {
            comment,
            text: comment.text(),
            depth,
            hidden: if collapsed { self.reply_count(id) } else { 0 },
        });
        if !collapsed {
            for k in kids {
                self.walk(k, depth + 1, collapse_at, out);
            }
        }
    }
}

/// Сколько комментариев к странице главы, по первой странице ответа: число корней
/// и признак, что их больше. Сайт такого счётчика не даёт, поэтому считаем сами
/// и только по запросу: на каждую страницу это отдельный запрос.
pub async fn page_comment_count(
    client: &libsocial::Client,
    chapter_id: u64,
    page: u64,
) -> libsocial::Result<(usize, bool)> {
    let target = Target::Chapter {
        chapter_id,
        page: Some(page),
    };
    let first = client.comments(target, CommentSort::Newest, 1).await?;
    let count = first.roots.iter().filter(|c| c.post_page == Some(page)).count();
    Ok((count, first.has_next))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn c(id: u64, parent: Option<u64>, root: Option<u64>) -> Comment {
        serde_json::from_value(json!({
            "id": id, "parent_comment": parent, "root_id": root,
            "comment": format!("<p>{id}</p>"), "relation_type": "manga", "relation_id": 1,
            "votes": {"up": 2, "down": 1, "user": null}
        }))
        .unwrap()
    }

    fn page(roots: Vec<Comment>, replies: Vec<Comment>, has_next: bool) -> CommentPage {
        CommentPage {
            roots,
            replies,
            has_next,
        }
    }

    fn ids(t: &Thread, collapse: usize) -> Vec<(u64, usize, usize)> {
        t.rows(collapse)
            .iter()
            .map(|r| (r.comment.id, r.depth, r.hidden))
            .collect()
    }

    #[test]
    fn builds_tree_without_foreign_and_duplicate_replies() {
        let mut t = Thread::new(Target::Manga(1), CommentSort::Newest);
        // 99 – ответ к корню, которого на этой странице нет.
        t.add_page(page(
            vec![c(1, None, None), c(2, None, None)],
            vec![
                c(10, Some(1), Some(1)),
                c(11, Some(10), Some(1)),
                c(99, Some(50), Some(50)),
            ],
            true,
        ));
        assert_eq!(t.next_page, Some(2));
        assert_eq!(ids(&t, 10), vec![(1, 0, 0), (10, 1, 0), (11, 2, 0), (2, 0, 0)]);
        // Те же ответы на второй странице не задваиваются, корень 50 приводит свои.
        t.add_page(page(
            vec![c(50, None, None)],
            vec![c(10, Some(1), Some(1)), c(99, Some(50), Some(50))],
            false,
        ));
        assert_eq!(t.next_page, None);
        assert_eq!(ids(&t, 10).len(), 6);
        assert_eq!(t.reply_count(1), 2);
    }

    #[test]
    fn collapse_and_toggle() {
        let mut t = Thread::new(Target::Manga(1), CommentSort::Newest);
        t.add_page(page(
            vec![c(1, None, None)],
            vec![c(10, Some(1), Some(1)), c(11, Some(10), Some(1))],
            false,
        ));
        // Сворачиваем с первого уровня: под корнем скрыты оба ответа.
        assert_eq!(ids(&t, 1), vec![(1, 0, 2)]);
        t.toggle(1);
        assert_eq!(ids(&t, 1), vec![(1, 0, 0), (10, 1, 1)]);
    }

    #[test]
    fn votes_toggle_like_site() {
        let mut t = Thread::new(Target::Manga(1), CommentSort::Newest);
        t.add_page(page(vec![c(1, None, None)], vec![], false));
        t.apply_vote(1, Vote::Up);
        assert_eq!(
            (t.get(1).unwrap().votes.up, t.get(1).unwrap().votes.user),
            (3, Some(Vote::Up))
        );
        // Плюс на минус одним нажатием.
        t.apply_vote(1, Vote::Down);
        let v = &t.get(1).unwrap().votes;
        assert_eq!((v.up, v.down, v.user), (2, 2, Some(Vote::Down)));
        // Повтор снимает голос.
        let before = t.apply_vote(1, Vote::Down).unwrap();
        let v = &t.get(1).unwrap().votes;
        assert_eq!((v.up, v.down, v.user), (2, 1, None));
        t.restore_votes(1, before);
        assert_eq!(t.get(1).unwrap().votes.user, Some(Vote::Down));
    }

    #[test]
    fn posted_and_removed() {
        let mut t = Thread::new(Target::Manga(1), CommentSort::Newest);
        t.set_sticky(vec![c(5, None, None)]);
        t.add_page(page(vec![c(1, None, None), c(5, None, None)], vec![], false));
        // Закреплённый не повторяется в общем списке.
        assert_eq!(ids(&t, 10), vec![(5, 0, 0), (1, 0, 0)]);
        t.insert_posted(c(2, None, None));
        t.insert_posted(c(20, Some(1), Some(1)));
        assert_eq!(ids(&t, 1), vec![(5, 0, 0), (2, 0, 0), (1, 0, 0), (20, 1, 0)]);
        t.remove(1);
        assert_eq!(ids(&t, 10), vec![(5, 0, 0), (2, 0, 0)]);
    }
}
