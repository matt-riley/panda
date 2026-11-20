use super::*;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
pub type Connect = fn(&Path, &str, &str) -> Result<Box<dyn Provider + Send>>;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub provider: String,
    pub model: String,
    pub thinking: Option<String>,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub default: Target,
    #[serde(default)]
    pub efforts: BTreeMap<String, Target>,
    pub image: Option<Target>,
}
impl Route {
    pub fn targets(&self) -> impl Iterator<Item = &Target> {
        std::iter::once(&self.default)
            .chain(self.efforts.values())
            .chain(self.image.iter())
    }
    pub fn window(&self, catalog: &Catalog) -> Option<u64> {
        self.targets()
            .map(|t| {
                Some(
                    catalog
                        .providers
                        .get(&t.provider)?
                        .model(&t.model)
                        .ok()?
                        .context_window,
                )
            })
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .min()
    }
}
pub fn prices(
    state: &Path,
    catalog: &Catalog,
    provider: &str,
    model: &str,
) -> Result<Option<TokenPrices>> {
    if let Some(ep) = catalog.providers.get(provider) {
        return Ok(ep.model(model)?.cost.clone());
    }
    let cache = state.join("catalog-openrouter-prices.json");
    if provider == "openrouter" && cache.try_exists()? {
        let mut prices: BTreeMap<String, TokenPrices> = serde_json::from_slice(&fs::read(cache)?)?;
        return Ok(prices.remove(model));
    }
    Ok(None)
}
pub struct Router {
    pub id: ModelId,
    pub state: PathBuf,
    pub route: Route,
    pub connect: Connect,
    pub persist: bool,
    pub last_price: Option<TokenPrices>,
}
impl Provider for Router {
    fn model(&self) -> Option<ModelId> {
        Some(self.id.clone())
    }
    fn default_model(&self) -> Option<ModelId> {
        self.persist.then(|| self.id.clone())
    }
    fn prices(&self) -> Option<TokenPrices> {
        self.last_price.clone()
    }
    fn reply(
        &mut self,
        s: &str,
        h: &[Message],
        t: &[Value],
        c: &Cancel,
        e: &mut Emit,
    ) -> Result<(String, Vec<Call>)> {
        self.reply_with_effort(s, h, t, c, e, None)
    }
    fn reply_with_effort(
        &mut self,
        system: &str,
        h: &[Message],
        tools: &[Value],
        cancel: &Cancel,
        emit: &mut Emit,
        effort: Option<&str>,
    ) -> Result<(String, Vec<Call>)> {
        cancel.check()?;
        self.last_price = None;
        let image = matches!(
            h.iter().rev().find(|m| m.user()),
            Some(Message::Images { .. })
        );
        let target = effort
            .and_then(|e| self.route.efforts.get(e))
            .or(self.route.image.as_ref().filter(|_| image))
            .unwrap_or(&self.route.default);
        emit(Event::BackingModel(ModelId {
            provider: target.provider.clone(),
            model: target.model.clone(),
        }));
        self.last_price = prices(
            &self.state,
            &Catalog::read(&self.state)?,
            &target.provider,
            &target.model,
        )?;
        let mut provider = (self.connect)(&self.state, &target.provider, &target.model)?;
        let effort = target
            .thinking
            .as_deref()
            .or(effort)
            .filter(|e| *e != "default");
        provider.reply_with_effort(system, h, tools, cancel, emit, effort)
    }
}
