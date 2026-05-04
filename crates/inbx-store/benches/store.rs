use criterion::{Criterion, criterion_group, criterion_main};
use inbx_store::{FolderRow, MessageRow, Store};
use sqlx::sqlite::SqlitePoolOptions;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::runtime::Runtime;

// ---------------------------------------------------------------------------
// Budget reporting
// ---------------------------------------------------------------------------

fn report(name: &str, mean_ns: u128, budget_ms: u128) {
    let mean_ms = mean_ns / 1_000_000;
    let verdict = if mean_ms < budget_ms {
        "✓ PASS"
    } else {
        "✗ OVER BUDGET"
    };
    eprintln!("{name}: {mean_ms}ms (budget {budget_ms}ms) {verdict}");
}

// ---------------------------------------------------------------------------
// One-time seeded store (100k messages)
// ---------------------------------------------------------------------------

static SEEDED: OnceLock<Store> = OnceLock::new();

fn get_seeded_store(rt: &Runtime) -> Store {
    SEEDED
        .get_or_init(|| {
            rt.block_on(async {
                let pool = SqlitePoolOptions::new()
                    .max_connections(1)
                    .connect(":memory:")
                    .await
                    .unwrap();
                sqlx::migrate!("./migrations").run(&pool).await.unwrap();
                let store = Store::from_pool(pool);
                store
                    .upsert_folder(&FolderRow {
                        name: "INBOX".into(),
                        delim: None,
                        special_use: None,
                        attrs: None,
                        uidvalidity: Some(1),
                        uidnext: None,
                        delta_link: None,
                    })
                    .await
                    .unwrap();
                for i in 0..100_000u32 {
                    let m = MessageRow {
                        folder: "INBOX".into(),
                        uid: i as i64,
                        uidvalidity: 1,
                        message_id: Some(format!("<msg-{i}@bench.local>")),
                        subject: Some(format!("Bench message {i} hello world")),
                        from_addr: Some(format!("user{}@bench.local", i % 1000)),
                        to_addrs: Some("me@bench.local".into()),
                        date_unix: Some(1_700_000_000 + i as i64),
                        flags: String::new(),
                        maildir_path: None,
                        headers_only: 1,
                        fetched_at_unix: 0,
                        in_reply_to: None,
                        refs: None,
                        thread_id: None,
                        provider_id: None,
                    };
                    store.upsert_message(&m).await.unwrap();
                    store
                        .index_for_search(
                            "INBOX",
                            i as i64,
                            1,
                            m.subject.as_deref().unwrap_or(""),
                            m.from_addr.as_deref().unwrap_or(""),
                            m.to_addrs.as_deref().unwrap_or(""),
                            "body text content for search bench",
                        )
                        .await
                        .unwrap();
                }
                store
            })
        })
        .clone()
}

// ---------------------------------------------------------------------------
// bench_store_open — cold-start proxy: from_pool + first list_folders
// ---------------------------------------------------------------------------

fn bench_store_open(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("store_open");
    group.measurement_time(Duration::from_secs(5));
    group.bench_function("from_pool_list_folders", |b| {
        b.to_async(&rt).iter(|| async {
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect(":memory:")
                .await
                .unwrap();
            sqlx::migrate!("./migrations").run(&pool).await.unwrap();
            let store = Store::from_pool(pool);
            store
                .upsert_folder(&FolderRow {
                    name: "INBOX".into(),
                    delim: None,
                    special_use: None,
                    attrs: None,
                    uidvalidity: Some(1),
                    uidnext: None,
                    delta_link: None,
                })
                .await
                .unwrap();
            let _ = store.list_folders().await.unwrap();
        });
    });
    group.finish();

    // Single-shot timing for budget summary line.
    let t0 = std::time::Instant::now();
    rt.block_on(async {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(":memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let store = Store::from_pool(pool);
        store
            .upsert_folder(&FolderRow {
                name: "INBOX".into(),
                delim: None,
                special_use: None,
                attrs: None,
                uidvalidity: Some(1),
                uidnext: None,
                delta_link: None,
            })
            .await
            .unwrap();
        let _ = store.list_folders().await.unwrap();
    });
    report(
        "store_open/from_pool_list_folders",
        t0.elapsed().as_nanos(),
        200,
    );
}

// ---------------------------------------------------------------------------
// bench_list_messages_100k — folder-switch proxy (200-row limit)
// ---------------------------------------------------------------------------

fn bench_list_messages_100k(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let store = get_seeded_store(&rt);

    let mut group = c.benchmark_group("list_messages_100k");
    group.measurement_time(Duration::from_secs(5));
    group.bench_function("list_messages_200", |b| {
        let store = store.clone();
        b.to_async(&rt).iter(|| {
            let store = store.clone();
            async move {
                let _ = store.list_messages("INBOX", 200).await.unwrap();
            }
        });
    });
    group.finish();

    let t0 = std::time::Instant::now();
    rt.block_on(async { store.list_messages("INBOX", 200).await.unwrap() });
    report(
        "list_messages_100k/list_messages_200",
        t0.elapsed().as_nanos(),
        50,
    );
}

// ---------------------------------------------------------------------------
// bench_search_100k — FTS5 search budget
// ---------------------------------------------------------------------------

fn bench_search_100k(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let store = get_seeded_store(&rt);

    let mut group = c.benchmark_group("search_100k");
    group.measurement_time(Duration::from_secs(5));
    group.bench_function("search_hello_50", |b| {
        let store = store.clone();
        b.to_async(&rt).iter(|| {
            let store = store.clone();
            async move {
                let _ = store.search("hello", 50).await.unwrap();
            }
        });
    });
    group.finish();

    let t0 = std::time::Instant::now();
    rt.block_on(async { store.search("hello", 50).await.unwrap() });
    report("search_100k/search_hello_50", t0.elapsed().as_nanos(), 100);
}

// ---------------------------------------------------------------------------
// bench_threader_ingest — JWZ scaling: 100 vs 1000 messages
// ---------------------------------------------------------------------------

async fn seed_threader_pool(n: u32) -> Store {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(":memory:")
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    let store = Store::from_pool(pool);
    store
        .upsert_folder(&FolderRow {
            name: "INBOX".into(),
            delim: None,
            special_use: None,
            attrs: None,
            uidvalidity: Some(1),
            uidnext: None,
            delta_link: None,
        })
        .await
        .unwrap();

    for i in 0..n {
        let message_id = format!("<thread-msg-{i}@bench.local>");
        // Mix: every 10th message is orphan, otherwise chain back 1-3 messages.
        let (in_reply_to, refs) = if i % 10 == 0 {
            (None, vec![])
        } else if i % 3 == 0 && i >= 3 {
            // longer chain
            let parent = format!("<thread-msg-{}@bench.local>", i - 1);
            let grandparent = format!("<thread-msg-{}@bench.local>", i - 2);
            let great = format!("<thread-msg-{}@bench.local>", i - 3);
            (Some(parent.clone()), vec![great, grandparent, parent])
        } else {
            let parent = format!("<thread-msg-{}@bench.local>", i - 1);
            (Some(parent.clone()), vec![parent])
        };

        let prefix = if i % 5 == 0 { "Re: " } else { "" };
        let m = MessageRow {
            folder: "INBOX".into(),
            uid: i as i64,
            uidvalidity: 1,
            message_id: Some(message_id.clone()),
            subject: Some(format!("{prefix}Thread subject {}", i / 10)),
            from_addr: Some(format!("user{}@bench.local", i % 20)),
            to_addrs: Some("me@bench.local".into()),
            date_unix: Some(1_700_000_000 + i as i64),
            flags: String::new(),
            maildir_path: None,
            headers_only: 1,
            fetched_at_unix: 0,
            in_reply_to: in_reply_to.clone(),
            refs: if refs.is_empty() {
                None
            } else {
                Some(refs.join("\n"))
            },
            thread_id: None,
            provider_id: None,
        };
        store.upsert_message(&m).await.unwrap();
        store
            .set_threading(
                "INBOX",
                i as i64,
                1,
                Some(&message_id),
                in_reply_to.as_deref(),
                &refs,
            )
            .await
            .unwrap();
    }
    store
}

fn bench_threader_ingest(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let mut group = c.benchmark_group("threader_ingest");
    group.measurement_time(Duration::from_secs(10));

    // 100-message baseline
    group.bench_function("ingest_100", |b| {
        b.to_async(&rt).iter(|| async {
            let _ = seed_threader_pool(100).await;
        });
    });

    // 1000-message scaling target
    group.bench_function("ingest_1000", |b| {
        b.to_async(&rt).iter(|| async {
            let _ = seed_threader_pool(1000).await;
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Criterion main
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_store_open,
    bench_list_messages_100k,
    bench_search_100k,
    bench_threader_ingest,
);
criterion_main!(benches);
