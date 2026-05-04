use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

async fn make_pool() -> SqlitePool {
    let opts = SqliteConnectOptions::new().in_memory(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .expect("pool");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrate");
    pool
}

/// Insert a minimal message row so set_threading can fetch the subject.
async fn insert_msg(pool: &SqlitePool, folder: &str, uid: i64, message_id: &str, subject: &str) {
    sqlx::query(
        "INSERT INTO messages
         (folder, uid, uidvalidity, message_id, subject, flags, headers_only, fetched_at_unix)
         VALUES (?1, ?2, 1, ?3, ?4, '', 0, 0)
         ON CONFLICT(folder, uid, uidvalidity) DO UPDATE SET
            message_id = excluded.message_id,
            subject    = excluded.subject",
    )
    .bind(folder)
    .bind(uid)
    .bind(message_id)
    .bind(subject)
    .execute(pool)
    .await
    .expect("insert_msg");
}

async fn get_thread_id(pool: &SqlitePool, message_id: &str) -> String {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT thread_id FROM messages WHERE message_id = ?1")
            .bind(message_id)
            .fetch_optional(pool)
            .await
            .unwrap();
    row.and_then(|(t,)| t).unwrap_or_default()
}

fn make_store(pool: SqlitePool) -> inbx_store::Store {
    inbx_store::Store::from_pool(pool)
}

// -----------------------------------------------------------------------
// simple_chain
// -----------------------------------------------------------------------
#[tokio::test]
async fn simple_chain() {
    let pool = make_pool().await;
    let store = make_store(pool);

    insert_msg(store.pool(), "INBOX", 1, "<a>", "Hello").await;
    store
        .set_threading("INBOX", 1, 1, Some("<a>"), None, &[])
        .await
        .unwrap();

    insert_msg(store.pool(), "INBOX", 2, "<b>", "Re: Hello").await;
    store
        .set_threading("INBOX", 2, 1, Some("<b>"), Some("<a>"), &[])
        .await
        .unwrap();

    let ta = get_thread_id(store.pool(), "<a>").await;
    let tb = get_thread_id(store.pool(), "<b>").await;
    assert_eq!(ta, tb, "<a> and <b> must share thread_id");
}

// -----------------------------------------------------------------------
// references_chain
// -----------------------------------------------------------------------
#[tokio::test]
async fn references_chain() {
    let pool = make_pool().await;
    let store = make_store(pool);

    insert_msg(store.pool(), "INBOX", 1, "<a>", "Topic").await;
    store
        .set_threading("INBOX", 1, 1, Some("<a>"), None, &[])
        .await
        .unwrap();

    insert_msg(store.pool(), "INBOX", 2, "<b>", "Re: Topic").await;
    store
        .set_threading(
            "INBOX",
            2,
            1,
            Some("<b>"),
            Some("<a>"),
            &["<a>".to_string()],
        )
        .await
        .unwrap();

    insert_msg(store.pool(), "INBOX", 3, "<c>", "Re: Topic").await;
    store
        .set_threading(
            "INBOX",
            3,
            1,
            Some("<c>"),
            Some("<b>"),
            &["<a>".to_string(), "<b>".to_string()],
        )
        .await
        .unwrap();

    let ta = get_thread_id(store.pool(), "<a>").await;
    let tb = get_thread_id(store.pool(), "<b>").await;
    let tc = get_thread_id(store.pool(), "<c>").await;

    assert_eq!(ta, "<a>", "root must be <a>");
    assert_eq!(tb, "<a>");
    assert_eq!(tc, "<a>");
}

// -----------------------------------------------------------------------
// out_of_order_arrival
// -----------------------------------------------------------------------
#[tokio::test]
async fn out_of_order_arrival() {
    let pool = make_pool().await;
    let store = make_store(pool);

    // <c> arrives first, referencing <a> and <b> which don't exist yet.
    insert_msg(store.pool(), "INBOX", 3, "<c>", "Re: Topic").await;
    store
        .set_threading(
            "INBOX",
            3,
            1,
            Some("<c>"),
            Some("<b>"),
            &["<a>".to_string(), "<b>".to_string()],
        )
        .await
        .unwrap();

    // <b> arrives next.
    insert_msg(store.pool(), "INBOX", 2, "<b>", "Re: Topic").await;
    store
        .set_threading(
            "INBOX",
            2,
            1,
            Some("<b>"),
            Some("<a>"),
            &["<a>".to_string()],
        )
        .await
        .unwrap();

    // <a> arrives last (the true root).
    insert_msg(store.pool(), "INBOX", 1, "<a>", "Topic").await;
    store
        .set_threading("INBOX", 1, 1, Some("<a>"), None, &[])
        .await
        .unwrap();

    let ta = get_thread_id(store.pool(), "<a>").await;
    let tb = get_thread_id(store.pool(), "<b>").await;
    let tc = get_thread_id(store.pool(), "<c>").await;

    assert_eq!(ta, "<a>", "root must be <a>");
    assert_eq!(tb, "<a>", "<b> thread_id must be <a>");
    assert_eq!(tc, "<a>", "<c> thread_id must be <a>");
}

// -----------------------------------------------------------------------
// subject_loose_match
// -----------------------------------------------------------------------
#[tokio::test]
async fn subject_loose_match() {
    let pool = make_pool().await;
    let store = make_store(pool);

    // Two siblings with no References overlap.
    insert_msg(store.pool(), "INBOX", 1, "<x1>", "Foo").await;
    store
        .set_threading("INBOX", 1, 1, Some("<x1>"), None, &[])
        .await
        .unwrap();

    insert_msg(store.pool(), "INBOX", 2, "<x2>", "Re: Foo").await;
    store
        .set_threading("INBOX", 2, 1, Some("<x2>"), None, &[])
        .await
        .unwrap();

    let t1 = get_thread_id(store.pool(), "<x1>").await;
    let t2 = get_thread_id(store.pool(), "<x2>").await;
    assert_eq!(t1, t2, "loose subject match must group <x1> and <x2>");
}

// -----------------------------------------------------------------------
// cycle_resistance
// -----------------------------------------------------------------------
#[tokio::test]
async fn cycle_resistance() {
    let pool = make_pool().await;
    let store = make_store(pool);

    // <a> references <b>, <b> references <a> — broken client.
    insert_msg(store.pool(), "INBOX", 1, "<a>", "Ping").await;
    store
        .set_threading(
            "INBOX",
            1,
            1,
            Some("<a>"),
            Some("<b>"),
            &["<b>".to_string()],
        )
        .await
        .unwrap();

    insert_msg(store.pool(), "INBOX", 2, "<b>", "Re: Ping").await;
    store
        .set_threading(
            "INBOX",
            2,
            1,
            Some("<b>"),
            Some("<a>"),
            &["<a>".to_string()],
        )
        .await
        .unwrap();

    // Both must be in one thread (no infinite loop, no error).
    let t1 = get_thread_id(store.pool(), "<a>").await;
    let t2 = get_thread_id(store.pool(), "<b>").await;
    assert_eq!(t1, t2, "cycle must still land in same thread");
}

// -----------------------------------------------------------------------
// subject_normalize
// -----------------------------------------------------------------------
#[test]
fn subject_normalize() {
    use inbx_store::normalize_subject;

    assert_eq!(normalize_subject("Re: Hello"), "hello");
    assert_eq!(normalize_subject("RE: Hello"), "hello");
    assert_eq!(normalize_subject("Re[2]: Hello"), "hello");
    assert_eq!(normalize_subject("Fwd: Hello"), "hello");
    assert_eq!(normalize_subject("Fw: Hello"), "hello");
    assert_eq!(normalize_subject("Re: Re: Hello"), "hello");
    assert_eq!(normalize_subject("Re: Fwd: Hello"), "hello");
    assert_eq!(normalize_subject("Hello"), "hello");
    assert_eq!(normalize_subject("  Re: Hello  "), "hello");
}

// -----------------------------------------------------------------------
// subject_normalize_mailing_list_brackets
// -----------------------------------------------------------------------
#[test]
fn subject_normalize_mailing_list_brackets() {
    use inbx_store::normalize_subject;

    // Single bracket tag before Re:
    assert_eq!(normalize_subject("[list] Re: foo"), "foo");
    // Re: before bracket tag
    assert_eq!(normalize_subject("Re: [list] foo"), "foo");
    // Two bracket tags then Re:
    assert_eq!(normalize_subject("[list-a] [list-b] Re: foo"), "foo");
    // Bracket tag only
    assert_eq!(normalize_subject("[django-users] hello"), "hello");
    // Interleaved: Re: [list] Re: [list2] foo
    assert_eq!(normalize_subject("Re: [list] Re: [list2] foo"), "foo");
    // Mid-string bracket must NOT be stripped
    assert_eq!(
        normalize_subject("Build [#1234] failed"),
        "build [#1234] failed"
    );
    // Mailing list with rust-internals style name
    assert_eq!(normalize_subject("[rust-internals] Re: foo"), "foo");
}
