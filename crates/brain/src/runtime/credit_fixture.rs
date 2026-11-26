use kumochi_protocol::message::DataToShell;

tokio::task_local! {
    pub(crate) static SATURATE: ();
}

pub(super) fn expand(message: &DataToShell) -> Option<DataToShell> {
    if !active() {
        return None;
    }
    let DataToShell::AssistantText { run, delta } = message else {
        return None;
    };
    if delta.len() != 1 {
        return None;
    }
    Some(DataToShell::AssistantText {
        run: *run,
        delta: delta.repeat(262_135).into(),
    })
}

pub(super) fn active() -> bool {
    SATURATE.try_with(|()| ()).is_ok()
}
