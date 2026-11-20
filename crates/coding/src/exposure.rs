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
        self.definitions(mode)
            .into_iter()
            .filter(|d| {
                d.name() == "tool_search"
                    || !self.deferred.contains(d.name())
                    || discovered.contains(d.name())
            })
            .map(|d| d.schema)
            .collect()
    }
}
impl Toolset for Exposure<'_> {
    fn system(&self) -> Result<String> {
        self.inner.system()
    }
    fn definitions(&self, mode: Mode) -> Vec<ToolDefinition> {
        let mut definitions: Vec<_> = self
            .inner
            .definitions(mode)
            .into_iter()
            .filter(|d| !self.hidden.contains(d.name()))
            .collect();
        if !self.hidden.contains("tool_search")
            && definitions.iter().any(|d| self.deferred.contains(d.name()))
        {
            definitions.retain(|d| d.name() != "tool_search");
            definitions.push(ToolDefinition {
                schema: search_schema(),
                access: Access::Stateful,
                output: OutputKind::Json,
                namespace: None,
            });
        }
        definitions
    }
    fn schemas_for(&self, s: &Session) -> Vec<Value> {
        self.available(s.mode, &s.history)
    }
    fn run(
        &self,
        session: &Session,
        call: &Call,
        cancel: &Cancel,
        emit: &mut Emit,
    ) -> Result<(ToolOutput, StoreWrites)> {
        cancel.check()?;
        let allowed = self
            .available(session.mode, &session.history)
            .into_iter()
            .find(|s| s["function"]["name"] == call.name)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "tool {} unavailable for this run ({:?} mode)",
                    call.name,
                    session.mode
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
                .definitions(session.mode)
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
        self.inner.run(session, call, cancel, emit)
    }
}
