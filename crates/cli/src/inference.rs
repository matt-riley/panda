//! Host-owned model inventory and typed Codemode inference.
use super::{Catalog, configured};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use panda_core::{
    Access, Attempt, Call, Cancel, Emit, Event, FailedUsage, Mode, ModelId, OutputKind, Session,
    StoreWrites, ToolDefinition, ToolOutput, Toolset,
};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path};

pub struct Models<'a> {
    pub inner: &'a dyn Toolset,
    pub state: &'a Path,
    pub catalog: &'a Catalog,
}

fn capabilities(mode: Mode) -> Vec<ToolDefinition> {
    let model = json!({"type":"string","minLength":1});
    let make = |name, description, access, output, properties, required| ToolDefinition {
        schema: json!({"type":"function","function":{"name":name,"description":description,
            "parameters":{"type":"object","properties":properties,"required":required,"additionalProperties":false}}}),
        access,
        output,
        namespace: Some("models".into()),
    };
    let mut definitions = vec![make(
        "list_models",
        "List configured models and credential availability. Filter by an exact provider/model ID.",
        Access::Read,
        OutputKind::Json,
        json!({"model":model}),
        json!([]),
    )];
    if mode == Mode::Code {
        definitions.push(make("classify","Classify JSON input or a local image block. Supply model, labels and exactly one of input or image. Returns {label}.",Access::Mutate,OutputKind::Json,
            json!({"model":model,"labels":{"type":"array","items":{"type":"string","minLength":1},"minItems":1},"input":{},"image":{"type":"object"}}),json!(["model","labels"])));
        definitions.push(make(
            "generate_image",
            "Generate one image from model and prompt. Returns a local image block for image().",
            Access::Mutate,
            OutputKind::File,
            json!({"model":model,"prompt":{"type":"string","minLength":1}}),
            json!(["model", "prompt"]),
        ));
    }
    definitions
}

fn image_url(mime: &str, data: &str) -> Result<String> {
    ensure!(
        data.len() <= (8_usize * 1024 * 1024).div_ceil(3) * 4,
        "image exceeds size limit"
    );
    let bytes = STANDARD.decode(data).context("invalid image encoding")?;
    ensure!(
        bytes.len() <= 8 * 1024 * 1024 && panda_coding::image_mime_type(&bytes) == Some(mime),
        "unsupported image or MIME mismatch"
    );
    Ok(format!("data:{mime};base64,{data}"))
}

fn request_body(call: &Call, model: &super::model_catalog::Model) -> Result<Value> {
    let args = &call.args;
    if call.name == "generate_image" {
        return Ok(json!({"model":model.id,"prompt":args["prompt"],"n":1}));
    }
    let labels = args["labels"].as_array().context("expected labels")?;
    ensure!(
        labels.len() <= 128
            && labels
                .iter()
                .filter_map(Value::as_str)
                .collect::<BTreeSet<_>>()
                .len()
                == labels.len(),
        "expected up to 128 unique labels"
    );
    let content = match (args.get("input"), args.get("image")) {
        (Some(input), None) => json!(input.to_string()),
        (None, Some(image)) => {
            ensure!(
                model.input.iter().any(|i| i == "image"),
                "model does not accept image input"
            );
            let url = image_url(
                image["mimeType"]
                    .as_str()
                    .context("missing image MIME type")?,
                image["data"].as_str().context("missing image data")?,
            )?;
            json!([{"type":"image_url","image_url":{"url":url}}])
        }
        _ => anyhow::bail!("provide exactly one of input or image"),
    };
    let schema = json!({
        "type": "object",
        "properties": {"label": {"type": "string", "enum": labels}},
        "required": ["label"],
        "additionalProperties": false
    });
    Ok(json!({
        "model": model.id,
        "max_tokens": model.max_tokens,
        "messages": [
            {"role": "system", "content": "Choose one of the supplied labels for the user input."},
            {"role": "user", "content": content}
        ],
        "response_format": {
            "type": "json_schema",
            "json_schema": {"name": "classification", "strict": true, "schema": schema}
        }
    }))
}

fn decode(call: &Call, response: &Value) -> Result<ToolOutput> {
    if call.name == "generate_image" {
        let data = response
            .pointer("/data/0/b64_json")
            .and_then(Value::as_str)
            .context("expected a base64 PNG from image model")?;
        return Ok(ToolOutput {
            text: "Generated image".into(),
            images: vec![image_url("image/png", data)?],
            details: None,
        });
    }
    let label: Value = serde_json::from_str(
        response
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .context("missing classification")?,
    )?;
    ensure!(
        label.as_object().is_some_and(|o| o.len() == 1)
            && call.args["labels"]
                .as_array()
                .is_some_and(|choices| choices.contains(&label["label"])),
        "invalid classification label"
    );
    Ok(label.to_string().into())
}

impl Models<'_> {
    fn list(&self, session: &Session, filter: Option<&str>) -> Result<Value> {
        let mut rows = Vec::new();
        for (provider, endpoint) in &self.catalog.providers {
            let available = configured(self.state, provider)?;
            for model in &endpoint.models {
                let id = format!("{provider}/{}", model.id);
                if filter.is_none_or(|f| f == id) {
                    rows.push(json!({"id":id,"available":available,"inference":model.inference,"input":model.input}));
                }
            }
        }
        for provider in self
            .catalog
            .ids()
            .into_iter()
            .filter(|id| !self.catalog.providers.contains_key(*id))
        {
            let mut models = BTreeSet::new();
            if provider == "router" {
                models.extend(self.catalog.routes.keys().cloned());
            } else if let Some(text) = panda_local::resources::optional_text(
                &self.state.join(format!("catalog-{provider}.json")),
            )? {
                let cached: Vec<(String, Option<u64>)> = serde_json::from_str(&text)?;
                models.extend(cached.into_iter().map(|(model, _)| model));
            }
            if let Some(id) = &session.model
                && id.provider == provider
            {
                models.insert(id.model.clone());
            }
            if !models.is_empty() {
                let available = configured(self.state, provider)?;
                for model in models {
                    let id = format!("{provider}/{model}");
                    if filter.is_none_or(|f| f == id) {
                        rows.push(json!({"id":id,"available":available,"inference":[]}));
                    }
                }
            }
        }
        Ok(json!(rows))
    }
    fn invoke(
        &self,
        s: &Session,
        call: &Call,
        cancel: &Cancel,
        emit: &mut Emit,
    ) -> Result<ToolOutput> {
        if call.name == "list_models" {
            return Ok(self
                .list(s, call.args["model"].as_str())?
                .to_string()
                .into());
        }
        let (provider, model_id) = call.args["model"]
            .as_str()
            .and_then(|id| id.split_once('/'))
            .context("expected provider/model")?;
        let endpoint = self
            .catalog
            .providers
            .get(provider)
            .context("unavailable inference provider")?;
        let model = endpoint.model(model_id)?;
        ensure!(
            model.inference.contains(&call.name) && configured(self.state, provider)?,
            "inference model unavailable"
        );
        let key = endpoint.key(self.state, provider)?;
        let body = request_body(call, model)?;
        cancel.check()?;
        let response = panda_providers::inference::request(
            &endpoint.base_url,
            &key,
            &call.name,
            &body,
            &cancel.for_request(s.request_policy.timeout_ms),
        );
        let report = match &response {
            Ok(value) => value.get("usage").cloned(),
            Err(error) => error.downcast_ref::<FailedUsage>().map(|u| u.0.clone()),
        };
        let responder = response
            .as_ref()
            .ok()
            .and_then(|r| r["model"].as_str())
            .unwrap_or(model_id);
        let id = ModelId {
            provider: provider.into(),
            model: responder.into(),
        };
        let prices = (responder == model_id)
            .then_some(model.cost.as_ref())
            .flatten();
        let result = response.and_then(|v| decode(call, &v));
        emit(Event::ModelAttempt(Attempt::from_report(
            Some(id),
            &call.name,
            s.history.len(),
            report.as_ref(),
            prices,
            result.is_ok(),
        )));
        result
    }
}
impl Toolset for Models<'_> {
    fn definitions(&self, mode: Mode) -> Vec<ToolDefinition> {
        let mut defs = self.inner.definitions(mode);
        defs.extend(capabilities(mode).into_iter().filter(|def| {
            def.name() == "list_models"
                || self
                    .catalog
                    .providers
                    .values()
                    .flat_map(|ep| &ep.models)
                    .any(|model| model.inference.contains(def.name()))
        }));
        defs
    }
    fn system(&self) -> Result<String> {
        self.inner.system()
    }
    fn run(
        &self,
        s: &Session,
        call: &Call,
        cancel: &Cancel,
        emit: &mut Emit,
    ) -> Result<(ToolOutput, StoreWrites)> {
        if matches!(
            call.name.as_str(),
            "list_models" | "generate_image" | "classify"
        ) {
            let def = capabilities(s.mode)
                .into_iter()
                .find(|d| d.name() == call.name)
                .context("inference unavailable in this mode")?;
            def.validate(&call.args)?;
            return Ok((self.invoke(s, call, cancel, emit)?, StoreWrites::default()));
        }
        self.inner.run(s, call, cancel, emit)
    }
}
