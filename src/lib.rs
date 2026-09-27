use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::{
        data::{QuotaFactsQuery, QuotaWindowFacts},
        host::{
            AuthCredential, AuthGetRequest, AuthListRequest, AuthListResult, AuthRuntimeAccount,
        },
        management::{
            ManagementPage, ManagementRegistration, ManagementRequest, ManagementResource,
            ManagementResponse, ManagementRoute,
        },
    },
    client::{ComposedPlugin, HostClient, PluginBuilder, SessionError, TypedCall, TypedReply},
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::sync::Mutex;

#[derive(Deserialize)]
pub struct PanelConfig {
    #[serde(default)]
    admin_username: String,
    admin_password: String,
    admin_port: Option<u16>,
}

pub struct AdminClient {
    client: reqwest::Client,
    base_url: String,
    username: String,
    password: String,
    session: Mutex<Option<String>>,
}

impl AdminClient {
    pub fn new(config: PanelConfig) -> Result<Self, reqwest::Error> {
        let port = config
            .admin_port
            .or_else(|| {
                std::env::var("CPR_SERVER_PORT")
                    .ok()
                    .and_then(|value| value.parse().ok())
            })
            .unwrap_or(8080);
        Ok(Self {
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(65))
                .build()?,
            base_url: format!("http://127.0.0.1:{port}"),
            username: config.admin_username,
            password: config.admin_password,
            session: Mutex::new(None),
        })
    }

    async fn login(&self) -> Result<String, String> {
        let username = if self.username.is_empty() {
            None
        } else {
            Some(self.username.as_str())
        };
        let login = self
            .client
            .post(format!("{}/api/auth/login", self.base_url))
            .json(&json!({"mode": "admin", "username": username, "password": self.password}))
            .send()
            .await
            .map_err(|_| "连接 CPR 管理接口失败，请检查内部端口".to_string())?;
        if !login.status().is_success() {
            return Err("管理员登录失败，请检查用户名和密码".into());
        }
        login
            .headers()
            .get(reqwest::header::SET_COOKIE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::to_owned)
            .ok_or_else(|| "CPR 未返回管理会话".to_string())
    }

    async fn post_with_cookie(
        &self,
        path: &str,
        body: &Value,
        cookie: &str,
    ) -> Result<reqwest::Response, String> {
        self.client
            .post(format!("{}{}", self.base_url, path))
            .header(reqwest::header::COOKIE, cookie)
            .json(body)
            .send()
            .await
            .map_err(|_| "CPR 管理请求失败".to_string())
    }

    async fn post(&self, path: &str, body: Value) -> Result<(), String> {
        let cookie = {
            let mut session = self.session.lock().await;
            if session.is_none() {
                *session = Some(self.login().await?);
            }
            session.clone().unwrap()
        };
        let mut response = self.post_with_cookie(path, &body, &cookie).await?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            let new_cookie = {
                let mut session = self.session.lock().await;
                if session.as_deref() == Some(cookie.as_str()) {
                    *session = Some(self.login().await?);
                }
                session.clone().unwrap()
            };
            response = self.post_with_cookie(path, &body, &new_cookie).await?;
        }
        if !response.status().is_success() {
            return Err(format!(
                "CPR 管理请求失败（{}）",
                response.status().as_u16()
            ));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AccountAction {
    account_id: String,
}

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

pub fn plugin(
    admin: Arc<AdminClient>,
) -> Result<ComposedPlugin, gateway_plugin_sdk::client::AuthorError> {
    PluginBuilder::from_json(include_bytes!("../plugin.json"))?
        .management(registration(), move |call| {
            let admin = Arc::clone(&admin);
            async move { handle(call, &admin).await }
        })?
        .build()
}

fn registration() -> ManagementRegistration {
    ManagementRegistration {
        routes: [
            ("GET", "api/accounts"),
            ("POST", "api/refresh"),
            ("POST", "api/status"),
        ]
        .into_iter()
        .map(|(method, path)| ManagementRoute {
            method: method.into(),
            path: path.into(),
            request_content_types: if method == "POST" {
                vec!["application/json".into()]
            } else {
                Vec::new()
            },
            response_content_types: vec!["application/json".into()],
        })
        .collect(),
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
            description: Some("账号额度与调度状态".into()),
            entry: "web/index.html".into(),
            icon: None,
        }],
        callbacks: Vec::new(),
    }
}

async fn handle(
    call: TypedCall<ManagementRequest>,
    admin: &AdminClient,
) -> Result<TypedReply<ManagementResponse>, PluginFault> {
    if !call.request.query.is_empty() {
        return Err(PluginFault::new(
            ErrorCode::InvalidInput,
            "invalid panel request",
        ));
    }
    let result = match (call.request.method.as_str(), call.request.path.as_str()) {
        ("GET", "api/accounts") if call.payload.is_empty() => {
            json!({ "accounts": load_accounts(&call.host).await? })
        }
        ("POST", "api/refresh" | "api/status")
            if call.request.content_type.as_deref() == Some("application/json") =>
        {
            let action: AccountAction = serde_json::from_slice(&call.payload)
                .map_err(|_| PluginFault::new(ErrorCode::InvalidInput, "invalid account action"))?;
            let account: AuthRuntimeAccount = auth_call(
                &call.host,
                "host.auth.get_runtime",
                &AuthGetRequest {
                    account_id: action.account_id,
                },
            )
            .await?;
            if account.provider_id != "openai" || account.authentication_kind != "oauth" {
                return Err(PluginFault::new(
                    ErrorCode::InvalidInput,
                    "account is not Codex OAuth",
                ));
            }
            let operation = if call.request.path == "api/refresh" {
                admin
                    .post(
                        "/api/admin/accounts/quota/refresh",
                        json!({"accountId": account.account_id}),
                    )
                    .await
            } else {
                admin
                    .post(
                        "/api/admin/accounts/batch-update",
                        json!({
                            "accountIds": [account.account_id],
                            "enabled": !account.enabled,
                        }),
                    )
                    .await
            };
            match operation {
                Ok(()) => json!({"ok": true}),
                Err(message) => json!({"error": message}),
            }
        }
        _ => {
            return Err(PluginFault::new(
                ErrorCode::InvalidInput,
                "invalid panel request",
            ));
        }
    };
    let status = if result.get("error").is_some() {
        502
    } else {
        200
    };
    let payload = serde_json::to_vec(&result)
        .map_err(|_| PluginFault::new(ErrorCode::Fault, "panel response failed"))?;
    Ok(TypedReply::new(ManagementResponse {
        status,
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
