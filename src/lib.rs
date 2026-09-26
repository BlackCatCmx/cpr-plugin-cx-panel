use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::{
        data::{QuotaFactsQuery, QuotaWindowFacts},
        host::{AuthCredential, AuthListRequest, AuthListResult, AuthRuntimeAccount},
        management::{
            ManagementPage, ManagementRegistration, ManagementRequest, ManagementResource,
            ManagementResponse, ManagementRoute,
        },
    },
    client::{ComposedPlugin, HostClient, PluginBuilder, SessionError, TypedCall, TypedReply},
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountView {
    id: String,
    name: String,
    email: Option<String>,
    enabled: bool,
    credential_state: String,
    plan: Option<String>,
    subscription_until: Option<Value>,
    observed_at_ms: Option<i64>,
    windows: Vec<QuotaWindowFacts>,
    error: Option<&'static str>,
}

pub fn plugin() -> Result<ComposedPlugin, gateway_plugin_sdk::client::AuthorError> {
    PluginBuilder::from_json(include_bytes!("../plugin.json"))?
        .management(registration(), handle)?
        .build()
}

fn registration() -> ManagementRegistration {
    ManagementRegistration {
        routes: vec![ManagementRoute {
            method: "GET".into(),
            path: "api/accounts".into(),
            request_content_types: Vec::new(),
            response_content_types: vec!["application/json".into()],
        }],
        resources: ["web/index.html", "web/app.css", "web/app.js"]
            .into_iter()
            .map(|path| ManagementResource {
                path: path.into(),
                public: false,
            })
            .collect(),
        pages: vec![ManagementPage {
            id: "codex-quota".into(),
            title: "Codex 额度".into(),
            description: Some("账号被动额度与令牌中的套餐信息".into()),
            entry: "web/index.html".into(),
            icon: None,
        }],
        callbacks: Vec::new(),
    }
}

async fn handle(
    call: TypedCall<ManagementRequest>,
) -> Result<TypedReply<ManagementResponse>, PluginFault> {
    if call.request.method != "GET"
        || call.request.path != "api/accounts"
        || !call.request.query.is_empty()
        || !call.payload.is_empty()
    {
        return Err(PluginFault::new(
            ErrorCode::InvalidInput,
            "invalid panel request",
        ));
    }
    let accounts = load_accounts(&call.host).await?;
    let payload = serde_json::to_vec(&json!({ "accounts": accounts }))
        .map_err(|_| PluginFault::new(ErrorCode::Fault, "panel response failed"))?;
    Ok(TypedReply::new(ManagementResponse {
        status: 200,
        content_type: "application/json".into(),
    })
    .with_payload(payload))
}

async fn load_accounts(host: &HostClient) -> Result<Vec<AccountView>, PluginFault> {
    let mut cursor = None;
    let mut output = Vec::new();
    loop {
        let page: AuthListResult = auth_call(
            host,
            "host.auth.list",
            &AuthListRequest {
                provider_id: Some("openai".into()),
                cursor,
                limit: 200,
            },
        )
        .await?;
        for account in page.accounts {
            if account.authentication_kind == "oauth" {
                output.push(load_account(host, account).await);
            }
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    Ok(output)
}

async fn load_account(host: &HostClient, account: AuthRuntimeAccount) -> AccountView {
    let mut view = AccountView {
        id: account.account_id.clone(),
        name: account.name,
        email: account.email,
        enabled: account.enabled,
        credential_state: account.credential_state,
        plan: account.plan_type,
        subscription_until: None,
        observed_at_ms: None,
        windows: Vec::new(),
        error: None,
    };
    match auth_call::<_, AuthCredential>(
        host,
        "host.auth.get",
        &gateway_plugin_sdk::call::host::AuthGetRequest {
            account_id: account.account_id.clone(),
        },
    )
    .await
    {
        Ok(credential) => {
            let (plan, until) = jwt_subscription(credential.facts.material.get("id_token"));
            view.plan = plan.or(view.plan);
            view.subscription_until = until;
        }
        Err(_) => view.error = Some("读取账号资料失败"),
    }
    match host
        .quota_facts(QuotaFactsQuery {
            account_id: account.account_id,
        })
        .await
    {
        Ok(quota) => {
            view.observed_at_ms = quota.observed_at_ms;
            view.windows = quota.windows;
        }
        Err(_) => view.error = Some("读取被动额度失败"),
    }
    view
}

async fn auth_call<T: Serialize, R: DeserializeOwned>(
    host: &HostClient,
    method: &str,
    request: &T,
) -> Result<R, PluginFault> {
    let payload = serde_json::to_vec(request)
        .map_err(|_| PluginFault::new(ErrorCode::InvalidInput, "invalid account request"))?;
    let reply = host
        .call(method, json!({}), payload)
        .await
        .map_err(SessionError::into_plugin_fault)?;
    serde_json::from_slice(&reply.payload)
        .map_err(|_| PluginFault::new(ErrorCode::Fault, "invalid account response"))
}

fn jwt_subscription(token: Option<&Value>) -> (Option<String>, Option<Value>) {
    let Some(token) = token.and_then(Value::as_str) else {
        return (None, None);
    };
    let Some(payload) = token.split('.').nth(1) else {
        return (None, None);
    };
    let Ok(decoded) = URL_SAFE_NO_PAD.decode(payload) else {
        return (None, None);
    };
    let Ok(claims) = serde_json::from_slice::<Value>(&decoded) else {
        return (None, None);
    };
    let Some(auth) = claims.get("https://api.openai.com/auth") else {
        return (None, None);
    };
    let plan = auth
        .get("chatgpt_plan_type")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let until = auth
        .get("chatgpt_subscription_active_until")
        .filter(|value| value.is_string() || value.is_number())
        .cloned();
    (plan, until)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    #[test]
    fn reads_subscription_claim_without_exposing_token() {
        let claims = json!({
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": "plus",
                "chatgpt_subscription_active_until": 1791067560
            }
        });
        let token = format!(
            "header.{}.signature",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        );
        assert_eq!(
            jwt_subscription(Some(&Value::String(token))),
            (Some("plus".into()), Some(json!(1791067560)))
        );
    }
}
