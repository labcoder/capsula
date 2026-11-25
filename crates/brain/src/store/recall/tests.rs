use super::*;
use crate::store::{RunEnd, Store};
use crate::test_support::Scratch;

mod diagnostics;
mod read;
mod recovery;
mod search;
mod tasks;

fn world(name: &str) -> (Scratch, Store) {
    let scratch = Scratch::new(name);
    let mut store = Store::open(&scratch.db()).unwrap();
    store.recover().unwrap();
    (scratch, store)
}

fn task(store: &Store, id: u64, title: &str) {
    store
        .conn
        .execute(
            "INSERT INTO threads(id,title,created_ms,updated_ms) VALUES (?1,?2,0,?1)",
            params![i64::try_from(id).unwrap(), title],
        )
        .unwrap();
}

fn say(store: &Store, id: u64, task: u64, kind: &str, text: &str) {
    store
        .conn
        .execute(
            "INSERT INTO events(id,thread,kind,text,created_ms) VALUES (?1,?2,?3,?4,0)",
            params![
                i64::try_from(id).unwrap(),
                i64::try_from(task).unwrap(),
                kind,
                text
            ],
        )
        .unwrap();
}

fn select(store: &Store, origin: Origin, query: &Query) -> Result<Selection, Error> {
    Snapshot::begin(&store.conn)
        .unwrap()
        .select_recall(origin, query)
}

fn render(store: &Store, origin: Origin, selection: &Selection) -> (Value, BTreeSet<ThreadId>) {
    let mut sources = BTreeSet::new();
    let text = Snapshot::begin(&store.conn)
        .unwrap()
        .render_recall(origin, selection, &mut sources)
        .unwrap();
    (serde_json::from_str(&text).unwrap(), sources)
}
