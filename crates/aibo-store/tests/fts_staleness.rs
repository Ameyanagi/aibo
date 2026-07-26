//! The FTS5 external-content triggers are load-bearing (§12).
//!
//! §12: "REQUIRED. An external-content FTS5 table does NOT maintain itself;
//! SQLite makes the application responsible for consistency. Without these the
//! index silently goes stale and history search quietly stops finding recent
//! messages."
//!
//! Every test here is written so that it **fails if a trigger is removed**.
//! [`without_the_insert_trigger_search_silently_goes_blind`] proves the point
//! directly by dropping the trigger and watching search stop working — if that
//! test ever starts passing with the drop *and* the positive tests still pass,
//! something else is maintaining the index and this file needs rewriting.

use aibo_core::types::{MessageRole, Surface};
use aibo_store::Db;
use aibo_store::history::{NewMessage, create_conversation, insert_message};
use aibo_store::search::{index_is_consistent, rebuild_index, search};
use rusqlite::Connection;
use uuid::Uuid;

fn seeded() -> (Db, Uuid) {
    let db = Db::open_in_memory().expect("open");
    let conv = db
        .with_conn(|c| create_conversation(c, Surface::Ask, None))
        .expect("conversation");
    (db, conv)
}

fn add(db: &Db, conv: Uuid, body: &str) -> Uuid {
    db.with_conn(|c| {
        insert_message(
            c,
            conv,
            &NewMessage {
                role: MessageRole::User,
                content: body.to_owned(),
                ..Default::default()
            },
        )
    })
    .expect("insert")
}

fn hits(db: &Db, query: &str) -> usize {
    db.with_conn(|c| search(c, query, 50))
        .expect("search")
        .len()
}

#[test]
fn insert_is_indexed_immediately() {
    let (db, conv) = seeded();
    add(&db, conv, "the paranormal parrot squawked");
    assert_eq!(
        hits(&db, "paranormal"),
        1,
        "messages_ai must index the row on INSERT"
    );
}

#[test]
fn update_reindexes_and_the_old_text_stops_matching() {
    let (db, conv) = seeded();
    let id = add(&db, conv, "the paranormal parrot squawked");

    db.with_conn(|c| {
        c.execute(
            "UPDATE messages SET content = 'the ordinary pigeon cooed' WHERE id = ?1",
            rusqlite::params![id],
        )?;
        Ok(())
    })
    .expect("update");

    assert_eq!(
        hits(&db, "paranormal"),
        0,
        "messages_au must delete the stale index entry"
    );
    assert_eq!(
        hits(&db, "pigeon"),
        1,
        "messages_au must insert the new index entry"
    );
}

#[test]
fn delete_removes_the_index_entry() {
    let (db, conv) = seeded();
    let id = add(&db, conv, "the paranormal parrot squawked");

    db.with_conn(|c| {
        c.execute("DELETE FROM messages WHERE id = ?1", rusqlite::params![id])?;
        Ok(())
    })
    .expect("delete");

    assert_eq!(
        hits(&db, "paranormal"),
        0,
        "messages_ad must remove the index entry"
    );
}

#[test]
fn cascading_delete_of_a_conversation_also_clears_the_index() {
    // The subtle one: rows removed by ON DELETE CASCADE still fire AFTER DELETE
    // triggers, but only because `PRAGMA foreign_keys=ON` made the cascade
    // happen at all. This test fails both if the trigger is dropped and if the
    // pragma is.
    let (db, conv) = seeded();
    add(&db, conv, "the paranormal parrot squawked");

    db.with_conn(|c| {
        c.execute(
            "DELETE FROM conversations WHERE id = ?1",
            rusqlite::params![conv],
        )?;
        Ok(())
    })
    .expect("delete conversation");

    assert_eq!(hits(&db, "paranormal"), 0);
    assert!(
        db.with_conn(|c| index_is_consistent(c)).expect("check"),
        "the index must still agree with an emptied messages table"
    );
}

#[test]
fn the_index_stays_consistent_across_a_mixed_workload() {
    let (db, conv) = seeded();
    let mut ids = Vec::new();
    for i in 0..25 {
        ids.push(add(&db, conv, &format!("message number {i} about badgers")));
    }
    db.with_conn(|c| {
        for (n, id) in ids.iter().enumerate() {
            if n % 3 == 0 {
                c.execute("DELETE FROM messages WHERE id = ?1", rusqlite::params![id])?;
            } else if n % 3 == 1 {
                c.execute(
                    "UPDATE messages SET content = 'rewritten to mention otters' WHERE id = ?1",
                    rusqlite::params![id],
                )?;
            }
        }
        Ok(())
    })
    .expect("mixed workload");

    assert!(
        db.with_conn(|c| index_is_consistent(c)).expect("check"),
        "FTS5 integrity-check must pass after inserts, updates and deletes"
    );
    assert_eq!(hits(&db, "otters"), 8);
    assert_eq!(hits(&db, "badgers"), 8);
}

/// The negative control. Drop `messages_ai` and the index goes blind — which is
/// exactly the silent failure §12 warns about, demonstrated rather than
/// asserted from memory.
#[test]
fn without_the_insert_trigger_search_silently_goes_blind() {
    let (db, conv) = seeded();

    // With the trigger: found.
    add(&db, conv, "first sighting of the paranormal parrot");
    assert_eq!(hits(&db, "paranormal"), 1);

    db.with_conn(|c| {
        c.execute_batch("DROP TRIGGER messages_ai;")?;
        Ok(())
    })
    .expect("drop trigger");

    // Without it: the row is in `messages`, invisible to search, and no error
    // is raised anywhere. This is the whole hazard.
    add(&db, conv, "second sighting of the paranormal parrot");
    let stored: i64 = db
        .with_conn(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM messages WHERE content LIKE '%paranormal%'",
                [],
                |r| r.get(0),
            )?)
        })
        .expect("count");
    assert_eq!(stored, 2, "both messages are in the table");
    assert_eq!(
        hits(&db, "paranormal"),
        1,
        "but only one is in the index — this is the staleness §12 forbids"
    );

    // And the designed repair puts it right.
    db.with_conn(|c| rebuild_index(c)).expect("rebuild");
    assert_eq!(hits(&db, "paranormal"), 2);
}

#[test]
fn the_schema_really_uses_the_trigram_tokenizer() {
    // Guards the CJK requirement at the schema level, not just behaviourally:
    // trigram is "the only built-in tokenizer that works for CJK without a
    // custom segmenter" (§12).
    let db = Db::open_in_memory().expect("open");
    let sql: String = db
        .with_conn(|c: &mut Connection| {
            Ok(c.query_row(
                "SELECT sql FROM sqlite_schema WHERE name = 'messages_fts'",
                [],
                |r| r.get(0),
            )?)
        })
        .expect("schema");
    assert!(sql.contains("tokenize='trigram'"), "got: {sql}");
    assert!(sql.contains("content='messages'"), "got: {sql}");
    assert!(sql.contains("content_rowid='rowid'"), "got: {sql}");
}
