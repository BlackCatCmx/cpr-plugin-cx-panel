//! Codex 账号额度、令牌到期时间与宿主管理操作

use std::collections::HashMap;

use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::{
        data::{QuotaFactsQuery, QuotaWindowFacts},
        host::{AuthGetRequest, AuthListRequest, AuthListResult, AuthRuntimeAccount},
        management::{
            ManagementPage, ManagementRegistration, ManagementRequest, ManagementResource,
            ManagementResponse, ManagementRoute,
        },
        middleware::{MiddlewareHeader, http::Version},
    },
    client::{
        ComposedPlugin, HostClient, HttpBody, HttpFrame, HttpRequest, PluginBuilder, SessionError,
        TypedCall, TypedReply,
    },
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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
    proxy_country: Option<String>,
    access_token_expires_at_ms: Option<i64>,
    observed_at_ms: Option<i64>,
    windows: Vec<QuotaWindowFacts>,
    error: Option<&'static str>,
}

#[derive(Deserialize)]
struct AdminEnvelope<T> {
    data: T,
}

#[derive(Deserialize)]
struct AdminPage<T> {
    items: Vec<T>,
    page: AdminPageMeta,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AdminPageMeta {
    total_pages: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccountProxy {
    id: String,
    outbound_proxy_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProxyRegion {
    id: String,
    enabled: bool,
    detected_location: Option<DetectedLocation>,
    last_test: Option<ProxyTest>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DetectedLocation {
    location: Country,
    exit_ipv4: Option<String>,
    exit_ipv6: Option<String>,
}

#[derive(Deserialize)]
struct Country {
    country: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProxyTest {
    success: bool,
    location: LocationStatus,
    exit_ipv4: Option<String>,
    exit_ipv6: Option<String>,
}

#[derive(Deserialize)]
struct LocationStatus {
    status: String,
}

impl ProxyRegion {
    fn country(&self) -> Option<&str> {
        let detected = self.detected_location.as_ref()?;
        let test = self.last_test.as_ref()?;
        // 与宿主代理页一致，检测失败、地区冲突或出口变化时不沿用旧地区
        if !self.enabled
            || !test.success
            || matches!(test.location.status.as_str(), "failed" | "conflict")
            || test.exit_ipv4 != detected.exit_ipv4
            || test.exit_ipv6 != detected.exit_ipv6
        {
            return None;
        }
        Some(&detected.location.country)
    }
}

pub fn plugin() -> Result<ComposedPlugin, gateway_plugin_sdk::client::AuthorError> {
    PluginBuilder::from_json(include_bytes!("../plugin.json"))?
        .management(registration(), handle)?
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
        resources: [
            "web/index.html",
            "web/app.css",
            "web/flags.js",
            "web/app.js",
        ]
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
                call.host
                    .refresh_account_quota(QuotaFactsQuery {
                        account_id: account.account_id,
                    })
                    .await
                    .map(|_| ())
            } else {
                let body = serde_json::to_vec(&json!({
                    "accountIds": [account.account_id],
                    "enabled": !account.enabled,
                }))
                .map_err(|_| PluginFault::new(ErrorCode::Fault, "账号操作编码失败"))?;
                let mut response = call
                    .host
                    .dispatch_http(HttpRequest {
                        settings: Value::Null,
                        method: "POST".into(),
                        uri: "/api/admin/accounts/batch-update".into(),
                        version: Version::Http11,
                        headers: vec![MiddlewareHeader {
                            name: "content-type".into(),
                            value: b"application/json".to_vec(),
                        }],
                        timeout_ms: Some(call.context.timeout_ms),
                        body: HttpBody::from_bytes(body),
                    })
                    .await?;
                let mut payload = Vec::new();
                while let Some(frame) = response.body.read().await? {
                    if let HttpFrame::Data(bytes) = frame {
                        payload.extend(bytes);
                    }
                }
                if (200..300).contains(&response.status) {
                    Ok(())
                } else {
                    let error: Value = serde_json::from_slice(&payload)
                        .map_err(|_| PluginFault::new(ErrorCode::Fault, "CPR 管理响应解析失败"))?;
                    Err(PluginFault::new(
                        ErrorCode::Upstream,
                        error["message"].as_str().ok_or_else(|| {
                            PluginFault::new(ErrorCode::Fault, "CPR 管理响应缺少错误信息")
                        })?,
                    ))
                }
            };
            match operation {
                Ok(()) => json!({"ok": true}),
                Err(error) => json!({"error": error.message}),
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
        headers: Vec::new(),
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
    if !output.is_empty() {
        let accounts: Vec<AccountProxy> =
            load_admin_pages(host, "/api/admin/accounts", "&provider=openai").await?;
        let proxies: Vec<ProxyRegion> = load_admin_pages(host, "/api/admin/proxies", "").await?;
        let countries: HashMap<_, _> = proxies
            .iter()
            .filter_map(|proxy| proxy.country().map(|country| (proxy.id.as_str(), country)))
            .collect();
        let account_countries: HashMap<_, _> = accounts
            .iter()
            .filter_map(|account| {
                let country = countries.get(account.outbound_proxy_id.as_deref()?)?;
                Some((account.id.as_str(), *country))
            })
            .collect();
        for account in &mut output {
            account.proxy_country = account_countries
                .get(account.id.as_str())
                .map(|s| (*s).into());
        }
    }
    Ok(output)
}

async fn load_admin_pages<T: DeserializeOwned>(
    host: &HostClient,
    path: &str,
    query: &str,
) -> Result<Vec<T>, PluginFault> {
    let mut page = 1;
    let mut output = Vec::new();
    loop {
        let mut response = host
            .dispatch_http(HttpRequest {
                settings: Value::Null,
                method: "GET".into(),
                uri: format!("{path}?page={page}&pageSize=200{query}"),
                version: Version::Http11,
                headers: Vec::new(),
                timeout_ms: None,
                body: HttpBody::empty(),
            })
            .await?;
        let mut payload = Vec::new();
        while let Some(frame) = response.body.read().await? {
            if let HttpFrame::Data(bytes) = frame {
                payload.extend(bytes);
            }
        }
        if response.status != 200 {
            return Err(PluginFault::new(
                ErrorCode::Upstream,
                format!("读取代理地区失败（{}）", response.status),
            ));
        }
        let result: AdminEnvelope<AdminPage<T>> = serde_json::from_slice(&payload)
            .map_err(|_| PluginFault::new(ErrorCode::Fault, "代理地区响应解析失败"))?;
        output.extend(result.data.items);
        if page >= result.data.page.total_pages {
            return Ok(output);
        }
        page += 1;
    }
}

async fn load_account(host: &HostClient, account: AuthRuntimeAccount) -> AccountView {
    let mut view = AccountView {
        id: account.account_id.clone(),
        name: account.name,
        email: account.email,
        enabled: account.enabled,
        credential_state: account.credential_state,
        plan: account.plan_type,
        proxy_country: None,
        access_token_expires_at_ms: account.access_token_expires_at_ms,
        observed_at_ms: None,
        windows: Vec::new(),
        error: None,
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_registers_management_page() {
        assert!(plugin().is_ok());
    }

    #[test]
    fn proxy_country_requires_current_successful_exit() {
        let proxy = json!({
            "id": "proxy-test",
            "enabled": true,
            "detectedLocation": {
                "location": {"country": "JP"},
                "exitIpv4": "192.0.2.1", "exitIpv6": "2001:db8::1",
            },
            "lastTest": {
                "success": true, "location": {"status": "detected"},
                "exitIpv4": "192.0.2.1", "exitIpv6": "2001:db8::1",
            },
        });
        let parsed: ProxyRegion = serde_json::from_value(proxy.clone()).unwrap();
        assert_eq!(parsed.country(), Some("JP"));

        let mut disabled = proxy.clone();
        disabled["enabled"] = json!(false);
        let parsed: ProxyRegion = serde_json::from_value(disabled).unwrap();
        assert_eq!(parsed.country(), None);

        for (field, value) in [
            ("success", json!(false)),
            ("location", json!({"status": "failed"})),
            ("location", json!({"status": "conflict"})),
            ("exitIpv4", json!("192.0.2.2")),
            ("exitIpv6", json!("2001:db8::2")),
        ] {
            let mut invalid = proxy.clone();
            invalid["lastTest"][field] = value;
            let parsed: ProxyRegion = serde_json::from_value(invalid).unwrap();
            assert_eq!(parsed.country(), None, "{field}");
        }

        for field in ["detectedLocation", "lastTest"] {
            let mut missing = proxy.clone();
            missing[field] = Value::Null;
            let parsed: ProxyRegion = serde_json::from_value(missing).unwrap();
            assert_eq!(parsed.country(), None, "{field}");
        }
    }
}
