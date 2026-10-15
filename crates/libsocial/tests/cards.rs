//! Карточки: чтение по фикстурам и запросы на запись. Сервер – wiremock.
//! Живые проверки на чтение помечены `#[ignore]`.

use std::time::Duration;

use libsocial::cards::*;
use libsocial::{Client, ClientConfig, Error};
use serde_json::{Map, Value, json};
use wiremock::matchers::{body_json, body_partial_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> Client {
    let config = ClientConfig {
        api_mirrors: vec![format!("{}/api", server.uri())],
        api_interval: Duration::ZERO,
        ..Default::default()
    };
    Client::new(config).unwrap()
}

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn ok(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

/// Мок на GET одного пути с готовым ответом.
async fn serve(server: &MockServer, p: &str, body: Value) {
    Mock::given(method("GET"))
        .and(path(p))
        .respond_with(ok(body))
        .mount(server)
        .await;
}

/// Мок на запись: проверяет метод, путь и тело, ответ пустой успешный.
async fn expect_write(server: &MockServer, m: &str, p: &str, body: Value, response: ResponseTemplate) {
    let builder = Mock::given(method(m)).and(path(p));
    // Пустой объект значит «тела нет»: метод ничего не шлёт.
    let builder = if body.as_object().is_some_and(Map::is_empty) {
        builder
    } else {
        builder.and(body_partial_json(body))
    };
    builder.respond_with(response).expect(1).mount(server).await;
}

// ------------------------------------------------------------------- чтение

#[tokio::test]
async fn catalog_page_skips_broken_items_and_counts_pages() {
    let server = MockServer::start().await;
    Mock::given(path("/api/cards"))
        .and(query_param("media_id", "7965"))
        .and(query_param("media_type", "manga"))
        .and(query_param("rank", "C"))
        .and(query_param("limit", "2"))
        .respond_with(ok(fixture("cards_list.json")))
        .expect(1)
        .mount(&server)
        .await;
    let query = CardQuery {
        page: 1,
        limit: Some(2),
        rank: Some(Rank::C),
        media: Some((TitleKind::Manga, 7965)),
        ..Default::default()
    };
    let page = client(&server).cards(&query).await.unwrap();
    assert_eq!(page.items.len(), 2);
    assert!(page.has_next);
    assert_eq!(page.total, Some(7));
    let first = &page.items[0];
    assert_eq!(
        (first.id, first.rank(), first.is_animated()),
        (980, Some(Rank::C), false)
    );
    assert_eq!(first.site_id, [1]);
    assert!(first.cover.default.ends_with("f1.webp"));
    assert_eq!(first.stats[0].tag, "titles");
    // У второй карты почти нет полей: остальное по умолчанию.
    assert!(page.items[1].is_animated());
    assert_eq!(page.items[1].rank(), Some(Rank::S));
}

#[tokio::test]
async fn last_page_has_no_next() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/api/cards",
        json!({"data": [], "links": {"next": null}, "meta": {"current_page": 4, "last_page": 4}}),
    )
    .await;
    let page = client(&server).cards(&CardQuery::default()).await.unwrap();
    assert!(!page.has_next && page.items.is_empty());
}

#[tokio::test]
async fn detailed_card() {
    let server = MockServer::start().await;
    serve(&server, "/api/cards/980--card-one", fixture("cards_card.json")).await;
    let card = client(&server).card("980--card-one").await.unwrap();
    assert_eq!(card.media[0].id, 7965);
    assert_eq!(card.albums[0].album.album_id, 3506);
    assert_eq!(card.owners.as_ref().unwrap().total, 58);
    assert_eq!(card.owners.as_ref().unwrap().preview[0].copies_count, 2);
    assert!(card.has_instances && !card.has_active_market_lots);
    assert_eq!(card.versions.len(), 1);
}

#[tokio::test]
async fn owners_with_exchange_filter() {
    let server = MockServer::start().await;
    Mock::given(path("/api/cards/980/owners"))
        .and(query_param("available_for_exchange", "1"))
        .and(query_param("page", "2"))
        .respond_with(ok(json!({
            "data": [{"id": 5, "username": "user2", "avatar": {"filename": null, "url": "u"}, "copies_count": 1}],
            "links": {"next": null}, "meta": {"current_page": 2, "last_page": 2, "total": 69}})))
        .expect(1)
        .mount(&server)
        .await;
    let page = client(&server).card_owners(980, true, 2, None).await.unwrap();
    assert_eq!(
        (page.items[0].username.as_str(), page.total, page.has_next),
        ("user2", Some(69), false)
    );
}

#[tokio::test]
async fn inventory_flags_stats_and_hidden_count() {
    let server = MockServer::start().await;
    Mock::given(path("/api/user/111/inventory"))
        .and(query_param("element", "ice"))
        .and(query_param("locked", "0"))
        .and(query_param("sort_by", "power_score"))
        .respond_with(ok(fixture("cards_inventory.json")))
        .expect(1)
        .mount(&server)
        .await;
    let query = InventoryQuery {
        element: Some("ice".into()),
        only_free: true,
        sort: Some(InventorySort::PowerScore),
        descending: true,
        ..Default::default()
    };
    let inv = client(&server).inventory(111, &query).await.unwrap();
    assert_eq!(inv.hidden_site_cards_count, 5);
    assert_eq!(inv.page.total, Some(2));
    let first = &inv.page.items[0];
    assert_eq!(
        (first.id, first.folder_id, first.locked, first.user_locked),
        (775420, Some(4), false, true)
    );
    let stats = first.card_stats.as_ref().unwrap();
    assert_eq!(
        (stats.power_score, stats.element.as_ref().unwrap().value.as_str()),
        (37, "ice")
    );
    assert_eq!(first.item.as_ref().unwrap().name, "Карта три");
    // Старая карта без характеристик и с null вместо флагов.
    let old = &inv.page.items[1];
    assert!(old.card_stats.is_none() && old.locked && !old.user_locked);
}

#[tokio::test]
async fn folders_and_statistics() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/api/user/111/card-folders",
        json!({"data": {"folders": [{"id": 4, "name": "Любимые", "color": null, "public": true, "count": 3}],
                        "total": 9, "is_owner": true, "can_view_private": true, "count": 1, "limit": 5, "can_create": true}}),
    )
    .await;
    // Статистика приходит без обёртки data.
    serve(
        &server,
        "/api/cards/statistics/111",
        json!({"user_id": 111, "total": 3, "total_by_ranks": {"S": 0, "A": 1, "B": 0, "C": 0, "D": 0, "E": 2}}),
    )
    .await;
    let c = client(&server);
    let folders = c.card_folders(111).await.unwrap();
    assert_eq!(
        (folders.folders[0].name.as_str(), folders.limit, folders.can_create),
        ("Любимые", 5, true)
    );
    let stat = c.card_statistics(111).await.unwrap();
    assert_eq!((stat.total, stat.count(Rank::E), stat.count(Rank::A)), (3, 2, 1));
}

#[tokio::test]
async fn album_list_and_details() {
    let server = MockServer::start().await;
    Mock::given(path("/api/cards/albums"))
        .and(query_param("site_id", "3"))
        .and(query_param("sort", "updated_at"))
        .and(query_param("direction", "desc"))
        .respond_with(ok(json!({"data": [fixture("cards_album.json")["data"].clone()],
                                "links": {"next": "x"}, "meta": {"total": 120, "last_page": 5}})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/api/cards/albums/3506"))
        .and(query_param("status", "owned_now"))
        .and(query_param("version", "animated"))
        .and(query_param("per_page", "2"))
        .respond_with(ok(fixture("cards_album.json")))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&server);
    let list = c
        .card_albums(&AlbumQuery {
            site_id: Some(3),
            sort: Some(AlbumSort::UpdatedAt),
            descending: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(list.has_next);
    assert_eq!(list.total, Some(120));
    assert!(list.items[0].can_claim);
    assert_eq!(list.items[0].shard_balance.as_ref().unwrap().amount_units, 240);

    let details = c
        .card_album(
            3506,
            &SlotsQuery {
                per_page: Some(2),
                status: Some(SlotStatus::OwnedNow),
                version: Some(CardKind::Animated),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(details.entry.album.name, "Альбом");
    assert_eq!(details.entry.missing_count, 6);
    assert!(details.slots.pagination.has_next_page);
    assert_eq!(details.slots.pagination.total, 7);
    let slot = &details.slots.items[0];
    assert_eq!(
        (slot.local_number, slot.owned_count, slot.status.as_str()),
        (1, 2, "owned_now")
    );
    assert_eq!(slot.card.as_ref().unwrap().id, 903);
}

#[tokio::test]
async fn user_albums_and_overview() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/api/user/111/card-albums",
        json!({"data": [{"album_id": 3506, "name": "Альбом", "type": "manga", "status": "in_progress", "progress_percent": 14.29,
                         "total_cards_count": 7, "collected_cards_count": 1, "shard_balance": {"amount_units": 240, "amount": 24}}],
               "links": {"next": null}, "meta": {"total": 1, "filters": {"statuses": []}}}),
    )
    .await;
    serve(
        &server,
        "/api/user/111/card-albums/info",
        json!({"data": {"summary": {"all": {"cards_collected": 1, "cards_total": 7588}}, "continue": {"all": []}}}),
    )
    .await;
    let c = client(&server);
    let page = c.user_albums(111, &UserAlbumsQuery::default()).await.unwrap();
    assert!((page.items[0].progress_percent - 14.29).abs() < 0.01);
    assert_eq!(page.items[0].collected_cards_count, 1);
    assert!(page.meta.get("filters").is_some());
    let info = c.user_albums_overview(111, None).await.unwrap();
    assert_eq!(info.summary["all"]["cards_total"], 7588);
    assert!(info.continue_albums["all"].is_array());
}

#[tokio::test]
async fn collection_shards_and_limits() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/api/cards/collection/title/manga/7965",
        json!({"data": {"album_id": 3506, "summary": {"total": 6, "collected": 1, "progress_percent": 16.7},
                        "ranks": [{"rank": "B", "rank_label": "Эпическая", "total": 4, "collected": 1, "progress_percent": 25,
                                   "cards": [{"card": {"id": 903, "name": "Карта"}, "versions": [], "owned_count": 1,
                                              "collected": true, "normal_collected": true, "animated_collected": false,
                                              "has_animated_version": false}]}]}}),
    )
    .await;
    serve(
        &server,
        "/api/cards/shards/title/manga/14841",
        fixture("cards_shards.json"),
    )
    .await;
    Mock::given(path("/api/profile/cards/shards"))
        .and(query_param("user_id", "111"))
        .and(query_param("sort_by", "amount"))
        .and(query_param("exchangeable", "1"))
        .respond_with(ok(json!({
            "data": [fixture("cards_shards.json")["data"].clone()],
            "links": {"next": null},
            "meta": {"total": 1, "weekly_limits": {"type": "split", "reset_at": "2026-10-19T06:00:00+00:00",
                "items": [{"key": "content", "label": "Чтение", "used": 6, "limit": 750, "remaining": 744, "percent": 0.8},
                          {"key": "extra", "label": "Действия", "used": 0, "limit": 250, "remaining": 250, "percent": 0}]}}})))
        .expect(1)
        .mount(&server)
        .await;
    serve(
        &server,
        "/api/cards/collectors/title/manga/7965",
        json!({"data": [{"id": 5, "username": "user2", "completed_at": "2026-10-01T00:00:00Z"}], "meta": {"total": 1, "cards_count": 6}}),
    )
    .await;

    let c = client(&server);
    let collection = c.title_collection(TitleKind::Manga, 7965).await.unwrap();
    assert_eq!(collection.summary.collected, 1);
    assert!(collection.ranks[0].cards[0].collected);

    let shards = c.title_shards(TitleKind::Manga, 14841).await.unwrap();
    // У ранобэ model равен book, а type – manga: в запросах нужен type.
    assert_eq!(
        (shards.title.model.as_str(), shards.title.title_kind()),
        ("book", TitleKind::Manga)
    );
    assert_eq!(shards.balance.amount_units, 1250);
    assert!(shards.option(Rank::E).unwrap().can_exchange);
    assert!(shards.option(Rank::D).unwrap().duplicate);
    assert!(shards.option(Rank::S).is_none());
    assert_eq!(shards.album.as_ref().unwrap().kind, "ranobe");

    let titles = c
        .shard_titles(
            111,
            &ShardTitlesQuery {
                sort: Some(ShardSort::Amount),
                exchangeable: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let limits = titles.weekly_limits.unwrap();
    assert_eq!(
        (limits.items[0].key.as_str(), limits.items[0].remaining),
        ("content", 744)
    );
    assert_eq!(titles.page.items.len(), 1);

    let collectors = c.title_collectors(TitleKind::Manga, 7965, None).await.unwrap();
    assert_eq!(
        (collectors.items[0].id, collectors.meta["cards_count"].clone()),
        (5, json!(6))
    );
}

#[tokio::test]
async fn unauthorized_shards() {
    let server = MockServer::start().await;
    Mock::given(path("/api/cards/shards/title/manga/1"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_json(json!({"data": {"toast": {"type": "error", "message": "Вы не авторизованы"}}})),
        )
        .mount(&server)
        .await;
    let err = client(&server).title_shards(TitleKind::Manga, 1).await.unwrap_err();
    assert!(matches!(err, Error::Unauthorized), "{err}");
}

#[tokio::test]
async fn market_lot_order_and_card_page() {
    let server = MockServer::start().await;
    Mock::given(path("/api/cards/trade-market"))
        .and(query_param("tab", "sell"))
        .and(query_param("sort", "offers_desc"))
        .and(query_param("available_for_me", "1"))
        .respond_with(ok(fixture("cards_market.json")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/api/cards/trade-market/cards/1594"))
        .and(query_param("tab", "wants"))
        .respond_with(ok(json!({"data": {
            "card": {"id": 1594, "name": "Карта"}, "items": [{"id": "card-offer:1594:1", "kind": "condition_group"}],
            "summary": {"active_lots_count": 1}, "filters": {}, "tab": "wants",
            "pagination": {"page": 1, "per_page": 20, "total": 1, "last_page": 1, "has_next_page": false}}})))
        .expect(1)
        .mount(&server)
        .await;
    serve(&server, "/api/cards/trade-market/lots/1632", fixture("cards_lot.json")).await;
    serve(
        &server,
        "/api/cards/trade-orders/1632",
        json!({"data": {"id": 1632, "user_id": 5, "type": "market_listing", "status": "active", "direction": "buy",
                        "want_mode": "rank_in_title", "wanted_rank": "B", "wanted_title_type": "manga", "wanted_title_id": 89959,
                        "wanted_quantity": 1, "pool_pick_count": 1, "pool_total_count": 1, "items": [{}], "groups": [{}, {}],
                        "events": [{"event": "created"}], "offers": []}}),
    )
    .await;
    let c = client(&server);
    let market = c
        .trade_market(&MarketQuery {
            tab: MarketTab::Sell,
            sort: Some(MarketSort::OffersDesc),
            available_for_me: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(market.pagination.has_next_page);
    assert_eq!((market.pagination.total, market.pagination.last_page), (402, 17));
    assert_eq!(market.items.len(), 2);
    assert_eq!(market.items[0].id, "card:1594");
    assert_eq!(market.items[0].power_range.as_ref().unwrap().max, Some(190));
    assert!(market.items[1].power_range.is_none());
    assert_eq!(market.summary["active_orders"], 551);

    let card_market = c.trade_market_card(1594, CardMarketTab::Wants, false, 1).await.unwrap();
    assert_eq!(card_market.card.unwrap().id, 1594);
    assert_eq!(card_market.items.len(), 1);

    let lot = c.market_lot(1632).await.unwrap();
    assert_eq!(lot.cannot_accept_reason.as_deref(), Some("no_matching_items"));
    assert_eq!(lot.matching_inventory[0].id, 775420);
    assert_eq!(lot.author.as_ref().unwrap().username, "user2");

    let order = c.trade_order(1632).await.unwrap();
    assert_eq!(
        (order.wanted_title_id, order.pool_total_count, order.groups.len()),
        (Some(89959), Some(1), 2)
    );
}

#[tokio::test]
async fn direct_trades_read() {
    let server = MockServer::start().await;
    Mock::given(path("/api/trade-orders"))
        .and(query_param("type", "direct_exchange"))
        .and(query_param("participant_user_id", "111"))
        .and(query_param("section", "history"))
        .and(query_param("direction", "incoming"))
        .respond_with(ok(json!({"data": [{"id": 3, "type": "direct_exchange", "status": "accepted"}], "links": {"next": null}, "meta": {}})))
        .expect(1)
        .mount(&server)
        .await;
    serve(
        &server,
        "/api/trade-orders/summary",
        json!({"data": {"active": 1, "incoming_active": 1, "history": 4}}),
    )
    .await;
    let c = client(&server);
    let page = c
        .direct_trades(111, OrderSection::History, Some(OrderDirection::Incoming), 1, None)
        .await
        .unwrap();
    assert_eq!(page.items[0].kind, "direct_exchange");
    assert_eq!(c.direct_trades_summary(111).await.unwrap().history, 4);
}

#[tokio::test]
async fn chaos_top_and_packs() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/api/cards/chaos-exchange",
        json!({"data": {"has_premium": false, "premium_bonus": {}, "animated_bonus": {}, "card_types": [],
                        "recipes": [{"key": "small", "input_count": 3, "input_ranks": ["E"], "available_cards_count": 5, "can_exchange": true}]}}),
    )
    .await;
    Mock::given(path("/api/cards/chaos-exchange/cards"))
        .and(query_param("recipe", "small"))
        .and(query_param("search", "ка"))
        .respond_with(ok(
            json!({"data": [{"id": 1, "item": {"id": 2, "name": "Карта"}}], "links": {"next": null}, "meta": {}}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/api/cards/creator-top"))
        .and(query_param("type", "animated"))
        .and(query_param("period", "previous"))
        .respond_with(ok(json!({"data": {
            "period": {"key": "previous", "from": "2026-09-01", "to": "2026-09-30"}, "type": "animated",
            "top": [{"place": 1, "delta": null, "user": {"id": 5, "username": "user2", "avatar": {"filename": null, "url": "u"}},
                     "accepted_count": 225, "titles_count": 27, "cards_preview": [],
                     "reward": {"place": 1, "premium_days": 30, "crystals": 1000}}],
            "top3": [], "rewarded": [], "me": null, "rewards": {"per_card_reward_crystals": 50}, "can_manage": false}})))
        .expect(1)
        .mount(&server)
        .await;
    serve(
        &server,
        "/api/card-packs",
        json!({"data": [{"id": 1, "name": "Пак", "description": null, "cards_per_open": 3, "cards": [], "hidden": false, "buyable": true}],
               "links": {"next": null}, "meta": {}}),
    )
    .await;
    let c = client(&server);
    let chaos = c.chaos_exchange().await.unwrap();
    assert_eq!(
        (chaos.recipes[0].key.as_str(), chaos.recipes[0].input_count),
        ("small", 3)
    );
    let cards = c
        .chaos_exchange_cards(
            "small",
            &ChaosCardsQuery {
                search: "ка".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(cards.items[0].id, 1);
    let top = c.creator_top(Some(CardKind::Animated), Some("previous")).await.unwrap();
    assert_eq!(
        (
            top.period.key.as_str(),
            top.top[0].accepted_count,
            top.top[0].reward.as_ref().unwrap()["crystals"].clone()
        ),
        ("previous", 225, json!(1000))
    );
    assert_eq!(top.rewards["per_card_reward_crystals"], 50);
    let packs = c.card_packs(1).await.unwrap();
    assert!(packs.items[0].buyable);
}

#[tokio::test]
async fn constants() {
    let server = MockServer::start().await;
    Mock::given(path("/api/constants"))
        .and(query_param("fields[]", "cardRanks"))
        .respond_with(ok(json!({"data": {
            "cardTypes": [{"id": "normal", "label": "Обычная"}, {"id": "animated", "label": "Анимированная"}],
            "cardRanks": [{"id": "S", "name": "S", "value": "S", "label": "Мифическая", "frame": {"front": {"orig": "/x.png"}}}],
            "cardElements": [{"value": "ice", "label": "Лёд"}],
            "cardChaosExchangeEnabled": false}})))
        .mount(&server)
        .await;
    let c = client(&server).card_constants().await.unwrap();
    assert_eq!(c.types[1].id, "animated");
    assert_eq!(c.ranks[0].rank(), Some(Rank::S));
    assert_eq!(
        (c.elements[0].id.as_str(), c.elements[0].label.as_str()),
        ("ice", "Лёд")
    );
    assert!(!c.chaos_exchange_enabled);
}

// ------------------------------------------------------------------- запись

#[tokio::test]
async fn lock_and_unlock() {
    let server = MockServer::start().await;
    expect_write(
        &server,
        "POST",
        "/api/user/inventory/775420/lock",
        json!({}),
        ok(json!({"user_locked": true})),
    )
    .await;
    expect_write(
        &server,
        "DELETE",
        "/api/user/inventory/775420/lock",
        json!({}),
        ResponseTemplate::new(204),
    )
    .await;
    let c = client(&server);
    c.lock_instance(775420).await.unwrap();
    c.unlock_instance(775420).await.unwrap();
}

#[tokio::test]
async fn folder_actions() {
    let server = MockServer::start().await;
    expect_write(
        &server,
        "POST",
        "/api/user/card-folders",
        json!({"name": "Новая", "public": true}),
        ok(json!({"data": {"id": 8}})),
    )
    .await;
    expect_write(
        &server,
        "PUT",
        "/api/user/card-folders/8",
        json!({"name": "Другая", "color": "red", "public": false}),
        ok(json!({"data": {"id": 8}})),
    )
    .await;
    expect_write(
        &server,
        "DELETE",
        "/api/user/card-folders/8",
        json!({}),
        ResponseTemplate::new(204),
    )
    .await;
    expect_write(
        &server,
        "PUT",
        "/api/user/card-folders/order",
        json!({"folder_ids": [3, 8, 4]}),
        ResponseTemplate::new(204),
    )
    .await;
    expect_write(
        &server,
        "PUT",
        "/api/user/card-folders/move",
        json!({"item_user_ids": [1, 2], "folder_id": null}),
        ResponseTemplate::new(204),
    )
    .await;
    let c = client(&server);
    assert_eq!(c.create_card_folder("Новая", true).await.unwrap()["id"], 8);
    c.update_card_folder(8, "Другая", Some("red"), false).await.unwrap();
    c.delete_card_folder(8).await.unwrap();
    c.reorder_card_folders(&[3, 8, 4]).await.unwrap();
    c.move_to_card_folder(&[1, 2], None).await.unwrap();
}

#[tokio::test]
async fn shard_exchange_returns_card_and_survives_odd_reply() {
    let server = MockServer::start().await;
    expect_write(
        &server,
        "POST",
        "/api/cards/shards/title/manga/14841/exchange",
        json!({"rank": "E"}),
        ok(json!({"data": {"card": {"id": 903, "name": "Карта"}}})),
    )
    .await;
    expect_write(
        &server,
        "POST",
        "/api/cards/shards/title/manga/1/exchange",
        json!({"rank": "A"}),
        ResponseTemplate::new(200).set_body_string(""),
    )
    .await;
    let c = client(&server);
    let got = c.exchange_shards(TitleKind::Manga, 14841, Rank::E).await.unwrap();
    assert_eq!(got.card.unwrap().id, 903);
    // Пустой ответ при успехе не ошибка.
    assert!(
        c.exchange_shards(TitleKind::Manga, 1, Rank::A)
            .await
            .unwrap()
            .card
            .is_none()
    );
}

#[tokio::test]
async fn claim_chaos_and_preview() {
    let server = MockServer::start().await;
    expect_write(
        &server,
        "POST",
        "/api/user/111/card-albums/3506/claim",
        json!({}),
        ok(json!({"data": {}})),
    )
    .await;
    expect_write(
        &server,
        "POST",
        "/api/cards/chaos-exchange",
        json!({"recipe": "small", "item_user_ids": [1, 2, 3]}),
        ok(json!({"data": {"output_card": {"id": 9}}})),
    )
    .await;
    expect_write(
        &server,
        "POST",
        "/api/cards/980/receive-preview",
        json!({"card_instance_stat_id": 77}),
        ok(json!({"data": {"owned": true, "first_preview": true, "rank": "A", "card_instance_stat_id": 77}})),
    )
    .await;
    let c = client(&server);
    c.claim_album_reward(111, 3506).await.unwrap();
    let chaos = c.chaos_exchange_run("small", &[1, 2, 3]).await.unwrap();
    assert_eq!(chaos.output_card.unwrap()["id"], 9);
    let preview = c.confirm_receive_preview(980, 77).await.unwrap();
    assert!(preview.owned && preview.first_preview);
    assert_eq!(
        (preview.rank.as_deref(), preview.card_instance_stat_id),
        (Some("A"), Some(77))
    );
}

#[tokio::test]
async fn direct_trade_actions() {
    let server = MockServer::start().await;
    let group = |side: &str, ids: Value| {
        json!({"side": side, "purpose": "give", "trade_type": "card", "requirement_type": "exact_items",
               "selection_policy": "all", "item_user_ids": ids})
    };
    expect_write(
        &server,
        "POST",
        "/api/trade-orders",
        json!({"type": "direct_exchange", "target_user_id": 5, "message": "обмен",
               "groups": [group("creator", json!([10])), group("counterparty", json!([20, 21]))]}),
        ok(json!({"data": {"id": 31}})),
    )
    .await;
    for action in ["accept", "decline", "cancel"] {
        expect_write(
            &server,
            "POST",
            &format!("/api/trade-orders/31/{action}"),
            json!({}),
            ok(json!({"data": {}})),
        )
        .await;
    }
    let c = client(&server);
    let trade = DirectTrade {
        target_user_id: 5,
        message: "обмен".into(),
        give: vec![10],
        receive: vec![20, 21],
    };
    assert_eq!(c.create_direct_trade(&trade).await.unwrap()["id"], 31);
    c.accept_direct_trade(31).await.unwrap();
    c.decline_direct_trade(31).await.unwrap();
    c.cancel_direct_trade(31).await.unwrap();
}

#[tokio::test]
async fn lots_offers_and_accept() {
    let server = MockServer::start().await;
    expect_write(
        &server,
        "POST",
        "/api/cards/trade-orders",
        json!({"direction": "buy", "want_mode": "exact_card", "give_mode": "item_pool", "wanted_canonical_card_id": 903,
               "wanted_quantity": 1, "pool_pick_count": 1, "pool_item_user_ids": [4, 5], "comment": "хочу",
               "wanted_card_version": "v2"}),
        ok(json!({"data": {"id": 40}})),
    )
    .await;
    expect_write(
        &server,
        "POST",
        "/api/cards/trade-orders",
        json!({"direction": "sell", "want_mode": "open_offers", "give_mode": "open_to_offers", "required_rank": "B",
               "required_quantity": 1, "min_offer_quantity": 1, "selling_item_user_id": 9, "comment": ""}),
        ok(json!({"data": {"id": 41}})),
    )
    .await;
    expect_write(
        &server,
        "POST",
        "/api/cards/trade-market/lots/40/accept",
        json!({"item_user_ids": [7], "selected_receive_item_user_ids": [4]}),
        ok(json!({"data": {"lot": {"id": 40}}})),
    )
    .await;
    expect_write(
        &server,
        "POST",
        "/api/cards/trade-orders/40/offers",
        json!({"wanted_item_user_id": 7, "selected_pool_item_user_ids": [4]}),
        ok(json!({"data": {"id": 50}})),
    )
    .await;
    expect_write(
        &server,
        "POST",
        "/api/cards/trade-orders/41/offers",
        json!({"offer_item_user_ids": [7, 8], "selected_pool_item_user_ids": []}),
        ok(json!({"data": {"id": 51}})),
    )
    .await;
    for (p, _) in [("accept", 0), ("decline", 0)] {
        expect_write(
            &server,
            "POST",
            &format!("/api/cards/trade-offers/50/{p}"),
            json!({}),
            ok(json!({"data": {}})),
        )
        .await;
    }
    expect_write(
        &server,
        "POST",
        "/api/cards/trade-orders/40/cancel",
        json!({}),
        ok(json!({"data": {}})),
    )
    .await;

    let c = client(&server);
    let mut extra = Map::new();
    extra.insert("wanted_card_version".into(), json!("v2"));
    let buy = BuyLot {
        want_mode: WantMode::ExactCard,
        wanted_canonical_card_id: Some(903),
        wanted_rank: None,
        wanted_title: None,
        wanted_card_type: None,
        wanted_element: None,
        wanted_min_power: None,
        wanted_quantity: 1,
        pool_pick_count: 1,
        pool_item_user_ids: vec![4, 5],
        comment: "хочу".into(),
        extra,
    };
    assert_eq!(c.create_buy_lot(&buy).await.unwrap()["id"], 40);
    let sell = SellLot {
        give: SellGive::Single(9),
        required_rank: Some(Rank::B),
        required_quantity: 1,
        min_offer_quantity: 1,
        comment: String::new(),
        extra: Map::new(),
    };
    assert_eq!(c.create_sell_lot(&sell).await.unwrap()["id"], 41);
    assert_eq!(c.accept_market_lot(40, &[7], &[4]).await.unwrap()["lot"]["id"], 40);
    let offer = Offer::ForBuyLot {
        wanted_item_user_id: 7,
        selected_pool_item_user_ids: vec![4],
    };
    assert_eq!(c.offer_to_lot(40, &offer).await.unwrap()["id"], 50);
    let offer = Offer::ForSellLot {
        offer_item_user_ids: vec![7, 8],
        selected_pool_item_user_ids: vec![],
    };
    assert_eq!(c.offer_to_lot(41, &offer).await.unwrap()["id"], 51);
    c.accept_offer(50).await.unwrap();
    c.decline_offer(50).await.unwrap();
    c.cancel_lot(40).await.unwrap();
}

#[tokio::test]
async fn pool_lot_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/cards/trade-orders"))
        .and(body_json(json!({
            "direction": "sell", "want_mode": "open_offers", "give_mode": "open_to_offers",
            "required_quantity": 1, "min_offer_quantity": 2, "comment": "",
            "pool_pick_count": 1, "pool_item_user_ids": [1, 2]})))
        .respond_with(ok(json!({"data": {"id": 1}})))
        .expect(1)
        .mount(&server)
        .await;
    let lot = SellLot {
        give: SellGive::Pool {
            pick_count: 1,
            item_user_ids: vec![1, 2],
        },
        required_rank: None,
        required_quantity: 1,
        min_offer_quantity: 2,
        comment: String::new(),
        extra: Map::new(),
    };
    client(&server).create_sell_lot(&lot).await.unwrap();
}

#[tokio::test]
async fn forbidden_write_is_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/user/inventory/1/lock"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"message": ""})))
        .mount(&server)
        .await;
    assert!(matches!(client(&server).lock_instance(1).await, Err(Error::Forbidden)));
}

// ---------------------------------------------------------------------- живые

fn live() -> Client {
    Client::new(ClientConfig {
        interface: std::env::var("POLKA_INTERFACE").ok(),
        api_interval: Duration::from_millis(1100),
        ..Default::default()
    })
    .unwrap()
}

/// Чтение без входа: `cargo test -p libsocial --test cards -- --ignored --test-threads=1`.
#[tokio::test]
#[ignore]
async fn live_read_without_login() {
    let c = live();
    let constants = c.card_constants().await.unwrap();
    assert_eq!(constants.ranks.len(), 6);

    let cards = c
        .cards(&CardQuery {
            limit: Some(2),
            media: Some((TitleKind::Manga, 7965)),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(cards.total.unwrap_or(0) >= 7 && !cards.items.is_empty());

    let card = c.card(&cards.items[0].id.to_string()).await.unwrap();
    assert!(!card.media.is_empty());
    let owners = c.card_owners(card.id, false, 1, Some(3)).await.unwrap();
    assert!(!owners.items.is_empty());

    let user = owners.items[0].id;
    let inv = c
        .inventory(
            user,
            &InventoryQuery {
                limit: Some(2),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(inv.page.total.is_some());
    c.card_statistics(user).await.unwrap();
    c.card_folders(user).await.unwrap();

    let albums = c
        .card_albums(&AlbumQuery {
            site_id: Some(3),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(!albums.items.is_empty());
    let album = c
        .card_album(
            3506,
            &SlotsQuery {
                per_page: Some(2),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(album.entry.album.album_id, 3506);
    assert!(!album.slots.items.is_empty());

    let collection = c.title_collection(TitleKind::Manga, 7965).await.unwrap();
    assert!(collection.summary.total > 0);
    let market = c.trade_market(&MarketQuery::default()).await.unwrap();
    assert!(!market.items.is_empty());
    let lot_id = market
        .items
        .iter()
        .find_map(|i| i.representative_order.as_ref()?.get("id")?.as_u64())
        .expect("в строках рынка есть заказ");
    assert_eq!(c.market_lot(lot_id).await.unwrap().id, lot_id);
    let top = c.creator_top(None, None).await.unwrap();
    assert!(!top.top.is_empty());
    c.card_packs(1).await.unwrap();
    // Осколки требуют входа.
    assert!(matches!(
        c.title_shards(TitleKind::Manga, 7965).await,
        Err(Error::Unauthorized)
    ));
}

#[tokio::test]
async fn card_history_requests() {
    let server = MockServer::start().await;
    // Фильтры приходят без обёртки `data`, как у настоящего ответа.
    Mock::given(method("GET"))
        .and(path("/api/card-history/options"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ranks": [{"value": "S", "label": "S"}],
            "card_types": [{"value": "normal", "label": "Обычная"}],
            "operation_types": [{"value": "pack_open", "label": "Пак"}, {"value": "trade", "label": "Обмен"}],
            "quality_values": [{"value": "25", "label": "25%"}],
            "sorts": [{"value": "latest", "label": "Сначала новые"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    // Записи синтетические: настоящую историю снять не удалось.
    Mock::given(method("GET"))
        .and(path("/api/user/1001/cards/history"))
        .and(query_param("sort", "latest"))
        .and(query_param("per_page", "18"))
        .and(query_param("rank", "A"))
        .and(query_param("op_types[]", "pack_open"))
        .and(query_param("op_types[]", "trade"))
        .and(query_param("quality_min", "50"))
        .and(query_param("q", "кот"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"instance_id": "900", "card": {"id": 5, "name": "Карта"},
                      "last_event": {"created_at": "2026-10-15T10:00:00.000000Z", "title": "Получена из пака", "tone": "in", "pack_id": 3}}],
            "meta": {"current_page": 1, "last_page": 2, "total_cards": 1}
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/card-instances/900/history"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
            "card": {"id": 5, "name": "Карта"}, "current_status": "locked", "events": [{"title": "Получена из пака"}]
        }})))
        .expect(1)
        .mount(&server)
        .await;

    let c = client(&server);
    let options = c.card_history_options().await.unwrap();
    assert_eq!(options.operation_types.len(), 2);
    let query = CardHistoryQuery {
        rank: Some(Rank::A),
        op_types: vec!["pack_open".into(), "trade".into()],
        quality_min: Some(50),
        card_type: Some(String::new()),
        text: " кот ".into(),
        ..Default::default()
    };
    let page = c.user_card_history(1001, &query).await.unwrap();
    assert!(page.has_next);
    let entry = &page.items[0];
    assert_eq!(entry.instance_id, 900);
    let event = entry.last_event.as_ref().unwrap();
    assert_eq!(event.tone, "in");
    assert_eq!(event.extra["pack_id"], 3);
    let history = c.card_instance_history(entry.instance_id).await.unwrap();
    assert_eq!(history.current_status, "locked");
    assert_eq!(history.events.len(), 1);
    // Пустой тип карты не уходит в запрос.
    let requests = server.received_requests().await.unwrap();
    assert!(!requests[1].url.query().unwrap_or_default().contains("card_type"));
}
