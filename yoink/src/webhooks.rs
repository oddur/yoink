//! Outbound HTTP webhooks fired on run-level deploy events.
//!
//! Operators declare `webhooks:` entries in `yoink.yaml`; this module
//! renders their `url` / `headers` / `body` against the run context and
//! POSTs (or GETs, etc.) to the receiver. Every fire is recorded as a
//! `WebhookFired` audit event so an operator can correlate "Slack went
//! silent" with "the deploy that should have pinged it".
//!
//! Failures are non-fatal — a flaky receiver must not wedge a deploy.
//! The audit log carries the diagnostic.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use minijinja::{Environment, UndefinedBehavior, context};

use crate::audit::{AuditEventKind, AuditSink, RunContext, build_operator_event};
use crate::config::{HttpMethod, WebhookSpec, WebhookTrigger};
use crate::secrets::SecretsBundle;

/// Template variables exposed to webhook bodies/headers/url.
///
/// Deliberately a fixed flat schema rather than the raw
/// `AuditEventKind` enum: the audit log evolves (new variants, renamed
/// fields), but operator-authored webhook templates should not break
/// when that happens. Add fields here only when there's a clear
/// notification use case.
#[derive(Debug, Clone)]
pub struct Context {
    pub trigger: WebhookTrigger,
    pub command: String,
    pub services: Vec<String>,
    pub deploy_id: String,
    pub actor: String,
    pub host: String,
    pub git_sha: Option<String>,
    pub yoink_version: String,
    pub error: Option<String>,
    pub ts: String,
}

impl Context {
    #[must_use]
    pub fn run_started(run: &RunContext, host: &str, services: Vec<String>) -> Self {
        Self::new(WebhookTrigger::RunStarted, run, host, services, None)
    }

    /// `error` is dropped on success and required (in practice) on failure.
    #[must_use]
    pub fn run_finished(
        run: &RunContext,
        host: &str,
        services: Vec<String>,
        ok: bool,
        error: Option<String>,
    ) -> Self {
        let trigger = if ok {
            WebhookTrigger::RunSucceeded
        } else {
            WebhookTrigger::RunFailed
        };
        Self::new(trigger, run, host, services, if ok { None } else { error })
    }

    fn new(
        trigger: WebhookTrigger,
        run: &RunContext,
        host: &str,
        services: Vec<String>,
        error: Option<String>,
    ) -> Self {
        Self {
            trigger,
            command: run.command.clone(),
            services,
            deploy_id: run.deploy_id.clone(),
            actor: run.actor.clone(),
            host: host.to_string(),
            git_sha: run.git_sha.clone(),
            yoink_version: run.yoink_version.clone(),
            error,
            ts: crate::audit::now_rfc3339_millis(),
        }
    }
}

const fn event_str(t: WebhookTrigger) -> &'static str {
    match t {
        WebhookTrigger::RunStarted => "run_started",
        WebhookTrigger::RunSucceeded => "run_succeeded",
        WebhookTrigger::RunFailed => "run_failed",
    }
}

const fn trigger_ok(t: WebhookTrigger) -> bool {
    !matches!(t, WebhookTrigger::RunFailed)
}

/// Render a templated string field. Two passes, in order: literal
/// `${secret:NAME}` substitution against `secrets`, then minijinja
/// rendering against `ctx`. The order ensures a secret value containing
/// `{{` won't be re-templated; `UndefinedBehavior::Strict` makes a
/// typo'd `{{ var }}` an error instead of an empty string.
pub fn render_field(
    template: &str,
    ctx: &Context,
    secrets: Option<&SecretsBundle>,
) -> Result<String, String> {
    render_field_with(&strict_env(), template, ctx, secrets)
}

fn render_field_with(
    env: &Environment<'_>,
    template: &str,
    ctx: &Context,
    secrets: Option<&SecretsBundle>,
) -> Result<String, String> {
    let with_secrets = substitute_secrets(template, secrets)?;
    let tmpl = env
        .template_from_str(&with_secrets)
        .map_err(|e| format!("template parse: {e}"))?;
    let event = event_str(ctx.trigger);
    let ok = trigger_ok(ctx.trigger);
    tmpl.render(context!(
        event => event,
        ok => ok,
        command => &ctx.command,
        services => &ctx.services,
        deploy_id => &ctx.deploy_id,
        actor => &ctx.actor,
        host => &ctx.host,
        git_sha => &ctx.git_sha,
        yoink_version => &ctx.yoink_version,
        error => &ctx.error,
        ts => &ctx.ts,
    ))
    .map_err(|e| format!("template render: {e}"))
}

fn strict_env<'a>() -> Environment<'a> {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    env
}

fn substitute_secrets(
    input: &str,
    secrets: Option<&SecretsBundle>,
) -> Result<String, String> {
    const NEEDLE: &str = "${secret:";
    if !input.contains(NEEDLE) {
        return Ok(input.to_string());
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.find(NEEDLE) {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + NEEDLE.len()..];
        let Some(end) = after.find('}') else {
            return Err(format!(
                "unterminated `${{secret:…}}` reference near {:?}",
                snippet(rest, pos)
            ));
        };
        let name = &after[..end];
        if name.is_empty() {
            return Err("empty secret name in `${secret:}` reference".into());
        }
        let Some(bundle) = secrets else {
            return Err(format!(
                "webhook references secret {name:?} but no `secrets:` provider is configured"
            ));
        };
        let Some(val) = bundle.get(name) else {
            return Err(format!(
                "webhook references secret {name:?} but it is not in the sealed bundle"
            ));
        };
        out.push_str(val);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn snippet(s: &str, pos: usize) -> &str {
    let start = pos.saturating_sub(8);
    let end = (pos + 16).min(s.len());
    &s[start..end]
}

/// Fire one webhook. Always returns `()` — every failure becomes a
/// `WebhookFired { ok: false }` audit event instead of propagating, so
/// dispatch can never abort a deploy.
pub async fn dispatch(
    spec: &WebhookSpec,
    ctx: &Context,
    secrets: Option<&SecretsBundle>,
    http: &reqwest::Client,
    audit: &Arc<dyn AuditSink>,
    run: &RunContext,
) {
    if !spec.on.contains(&ctx.trigger) {
        return;
    }
    let (ok, status, error) = classify(send(spec, ctx, secrets, http).await);
    audit
        .record(build_operator_event(
            run,
            "",
            AuditEventKind::WebhookFired {
                name: spec.name.clone(),
                ok,
                status,
                error,
            },
        ))
        .await;
}

fn classify(result: Result<u16, String>) -> (bool, Option<u16>, Option<String>) {
    match result {
        Ok(code) if (200..300).contains(&code) => (true, Some(code), None),
        Ok(code) => (false, Some(code), Some(format!("non-2xx: {code}"))),
        Err(e) => (false, None, Some(e)),
    }
}

async fn send(
    spec: &WebhookSpec,
    ctx: &Context,
    secrets: Option<&SecretsBundle>,
    http: &reqwest::Client,
) -> Result<u16, String> {
    let env = strict_env();
    let url = render_field_with(&env, &spec.url, ctx, secrets)?;
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in &spec.headers {
        let rendered = render_field_with(&env, v, ctx, secrets)
            .map_err(|e| format!("header {k:?}: {e}"))?;
        headers.insert(k.clone(), rendered);
    }
    let body = match &spec.body {
        Some(b) => Some(render_field_with(&env, b, ctx, secrets)?),
        None => None,
    };

    let method = match spec.method {
        HttpMethod::Get => reqwest::Method::GET,
        HttpMethod::Post => reqwest::Method::POST,
        HttpMethod::Put => reqwest::Method::PUT,
    };
    let mut req = http.request(method, &url);
    for (k, v) in &headers {
        req = req.header(k, v);
    }
    if let Some(b) = body {
        req = req.body(b);
    }
    req = req.timeout(spec.timeout);

    // Outer timeout backstops reqwest's per-request timer in the rare
    // case where a TLS / DNS hang escapes it (documented for some
    // platforms). +1s slack so reqwest's own error wins on the common path.
    let resp = tokio::time::timeout(spec.timeout + Duration::from_secs(1), req.send())
        .await
        .map_err(|_| format!("timed out after {:?}", spec.timeout))?
        .map_err(|e| format!("transport: {e}"))?;
    Ok(resp.status().as_u16())
}

/// Fan out every configured webhook for the given context. Calls run
/// concurrently — one slow receiver can't delay another. Awaited so
/// audit `WebhookFired` events are recorded before the deploy moves on.
pub async fn dispatch_all(
    specs: &[WebhookSpec],
    ctx: &Context,
    secrets: Option<&SecretsBundle>,
    http: &reqwest::Client,
    audit: &Arc<dyn AuditSink>,
    run: &RunContext,
) {
    if specs.is_empty() {
        return;
    }
    let futs = specs
        .iter()
        .map(|s| dispatch(s, ctx, secrets, http, audit, run));
    futures_util::future::join_all(futs).await;
}

/// Shared HTTP client. One per run, threaded through every dispatch site.
#[must_use]
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(format!("yoink/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("reqwest client builder with default config never fails")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::MemorySink;
    use std::collections::BTreeMap;
    use zeroize::Zeroizing;

    fn ctx() -> Context {
        Context {
            trigger: WebhookTrigger::RunSucceeded,
            command: "up".into(),
            services: vec!["api".into(), "web".into()],
            deploy_id: "01HFE9TEST".into(),
            actor: "alice@laptop".into(),
            host: "laptop".into(),
            git_sha: Some("abc1234".into()),
            yoink_version: "0.20.0".into(),
            error: None,
            ts: "2026-05-02T10:00:00.000Z".into(),
        }
    }

    fn bundle(pairs: &[(&str, &str)]) -> SecretsBundle {
        let mut map: BTreeMap<String, Zeroizing<String>> = BTreeMap::new();
        for (k, v) in pairs {
            map.insert((*k).into(), Zeroizing::new((*v).into()));
        }
        SecretsBundle::new(map)
    }

    #[test]
    fn substitute_resolves_secret() {
        let b = bundle(&[("TOK", "s3cr3t")]);
        let out = substitute_secrets("Bearer ${secret:TOK}", Some(&b)).unwrap();
        assert_eq!(out, "Bearer s3cr3t");
    }

    #[test]
    fn substitute_errors_on_missing_secret() {
        let b = bundle(&[]);
        let err = substitute_secrets("${secret:MISSING}", Some(&b)).unwrap_err();
        assert!(err.contains("MISSING"));
    }

    #[test]
    fn substitute_errors_when_no_bundle_but_referenced() {
        let err = substitute_secrets("${secret:X}", None).unwrap_err();
        assert!(err.contains("no `secrets:` provider"));
    }

    #[test]
    fn substitute_passes_through_when_no_reference() {
        assert_eq!(
            substitute_secrets("plain text", None).unwrap(),
            "plain text"
        );
    }

    #[test]
    fn render_field_runs_secrets_then_jinja() {
        let b = bundle(&[("T", "abc")]);
        let out = render_field(
            "{{ event }}:{{ services | join(\",\") }}:${secret:T}",
            &ctx(),
            Some(&b),
        )
        .unwrap();
        assert_eq!(out, "run_succeeded:api,web:abc");
    }

    #[test]
    fn render_field_errors_on_missing_var_strict() {
        let err = render_field("{{ no_such_var }}", &ctx(), None).unwrap_err();
        assert!(err.contains("template render"));
    }

    #[tokio::test]
    async fn dispatch_skips_non_matching_trigger() {
        let spec = WebhookSpec {
            name: "only-failure".into(),
            url: "http://127.0.0.1:1/".into(),
            on: vec![WebhookTrigger::RunFailed],
            method: HttpMethod::Post,
            timeout: Duration::from_millis(50),
            headers: BTreeMap::new(),
            body: None,
        };
        let mem: Arc<MemorySink> = Arc::new(MemorySink::new());
        let sink: Arc<dyn AuditSink> = mem.clone();
        let run = RunContext {
            deploy_id: "d".into(),
            actor: "a".into(),
            yoink_version: "v".into(),
            git_sha: None,
            command: "up".into(),
        };
        let http = http_client();
        dispatch(&spec, &ctx(), None, &http, &sink, &run).await;
        assert!(mem.events().await.is_empty());
    }
}
