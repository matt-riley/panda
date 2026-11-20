use super::*;
use anyhow::ensure;

/// Provider-visible declarations are projected from the active branch at each turn boundary.
pub struct Exposure<'a> {
    inner: &'a dyn Toolset,
    deferred: BTreeSet<String>,
    hidden: BTreeSet<String>,
}
fn search_schema() -> Value {
    json!({"type":"function","function":{"name":"tool_search","description":"Discover deferred tools by name or description on this branch.","parameters":{"type":"object","properties":{"query":{"type":"string","minLength":1}},"required":["query"],"additionalProperties":false}}})
}
impl<'a> Exposure<'a> {
    pub fn new(
        inner: &'a dyn Toolset,
        deferred: BTreeSet<String>,
        hidden: BTreeSet<String>,
    ) -> Result<Self> {
        ensure!(
            deferred.is_disjoint(&hidden),
            "tool cannot be both hidden and deferred"
        );
        Ok(Self {
            inner,
            deferred,
            hidden,
        })
    }
    fn available(&self, mode: Mode, history: &[Message]) -> Vec<Value> {
        let discovered: BTreeSet<&str> = history
            .iter()
            .filter_map(|m| match m {
                Message::Result {
                    details: Some(ToolDetails::Discovery { tools }),
                    is_error: false,
                    ..
                } => Some(tools),
                _ => None,
            })
            .flatten()
            .map(String::as_str)
            .collect();
        let mut search = false;
        let mut schemas: Vec<_> = self
            .definitions(mode)
            .into_iter()
            .filter(|d| {
                let deferred = self.deferred.contains(d.name());
                search |= deferred;
                !deferred || discovered.contains(d.name())
            })
            .map(|d| d.schema)
            .collect();
        if search && !self.hidden.contains("tool_search") {
            schemas.push(search_schema());
        }
        schemas
    }
    fn invoke(
        &self,
        mode: Mode,
        history: &[Message],
        call: &Call,
        cancel: &Cancel,
        emit: &mut Emit,
        session: Option<&Session>,
    ) -> Result<(ToolOutput, StoreWrites)> {
        cancel.check()?;
        let allowed = self
            .available(mode, history)
            .into_iter()
            .find(|s| s["function"]["name"] == call.name)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "tool {} unavailable for this run ({mode:?} mode)",
                    call.name
                )
            })?;
        if call.name == "tool_search" {
            validate_tool_args(
                &allowed["function"]["parameters"],
                &call.args,
                "tool_search",
            )?;
            let query = field(&call.args, "query")?.to_lowercase();
            let matches: Vec<_> = self
                .definitions(mode)
                .into_iter()
                .filter(|d| {
                    self.deferred.contains(d.name())
                        && (d.name().to_lowercase().contains(&query)
                            || d.schema["function"]["description"]
                                .as_str()
                                .unwrap_or_default()
                                .to_lowercase()
                                .contains(&query))
                })
                .take(8)
                .collect();
            let names = matches.iter().map(|d| d.name().to_owned()).collect();
            let text =
                serde_json::to_string(&matches.into_iter().map(|d| d.schema).collect::<Vec<_>>())?;
            return Ok((
                ToolOutput {
                    text,
                    images: vec![],
                    details: Some(ToolDetails::Discovery { tools: names }),
                },
                StoreWrites::default(),
            ));
        }
        match session {
            Some(session) => self.inner.run_in_session(session, call, cancel, emit),
            None => Ok((
                self.inner.run_stream(mode, call, cancel, emit)?,
                StoreWrites::default(),
            )),
        }
    }
}
impl Toolset for Exposure<'_> {
    fn system(&self) -> Result<String> {
        self.inner.system()
    }
    fn definitions(&self, mode: Mode) -> Vec<ToolDefinition> {
        self.inner
            .definitions(mode)
            .into_iter()
            .filter(|d| !self.hidden.contains(d.name()))
            .collect()
    }
    fn schemas(&self, mode: Mode) -> Vec<Value> {
        self.available(mode, &[])
    }
    fn schemas_for(&self, s: &Session) -> Vec<Value> {
        self.available(s.mode, &s.history)
    }
    fn run_in_session(
        &self,
        s: &Session,
        c: &Call,
        k: &Cancel,
        e: &mut Emit,
    ) -> Result<(ToolOutput, StoreWrites)> {
        self.invoke(s.mode, &s.history, c, k, e, Some(s))
    }
    fn run_stream(&self, m: Mode, c: &Call, k: &Cancel, e: &mut Emit) -> Result<ToolOutput> {
        Ok(self.invoke(m, &[], c, k, e, None)?.0)
    }
    fn run(&self, m: Mode, c: &Call, k: &Cancel) -> Result<String> {
        Ok(self.run_stream(m, c, k, &mut |_| {})?.text)
    }
}
