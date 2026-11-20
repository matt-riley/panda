use super::*;

pub(crate) struct Completion {
    pub result: Result<(ToolOutput, StoreWrites)>,
    pub events: Vec<Event>,
}

/// Read-only calls observe the same immutable branch prefix. Mutations and tools with
/// private session state form barriers and commit before the next group is dispatched.
pub(crate) fn parallel_reads(
    tools: &dyn Toolset,
    session: &Session,
    calls: &[Call],
    cancel: &Cancel,
) -> Vec<Completion> {
    std::thread::scope(|scope| {
        let workers: Vec<_> = calls
            .iter()
            .map(|call| {
                scope.spawn(move || {
                    let mut events = Vec::new();
                    let result = cancel
                        .check()
                        .and_then(|()| tools.run(session, call, cancel, &mut |e| events.push(e)));
                    Completion { result, events }
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| {
                worker.join().unwrap_or_else(|_| Completion {
                    result: Err(anyhow::anyhow!("tool worker panicked")),
                    events: vec![],
                })
            })
            .collect()
    })
}
