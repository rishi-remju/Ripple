// Copyright 2023 Comcast Cable Communications Management, LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// SPDX-License-Identifier: Apache-2.0
//
use super::privacy_rpc::{self};
use crate::{
    firebolt::rpc::RippleRPCProvider,
    processor::storage::storage_manager::StorageManager,
    service::apps::app_events::{AppEventDecorationError, AppEventDecorator},
    state::platform_state::PlatformState,
};
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
    RpcModule,
};
use ripple_sdk::api::{gateway::rpc_gateway_api::CallContext, storage_property::StorageProperty};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const ADVERTISING_APP_BUNDLE_ID_SUFFIX: &str = "Comcast";

#[derive(Debug)]
pub struct AdvertisingId {
    pub ifa: String,
    pub ifa_type: String,
    pub lmt: String,
}

impl Serialize for AdvertisingId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serde_json::Map::new();
        map.insert("ifa".to_string(), self.ifa.clone().into());
        // include both ifaType and ifa_type for backward compatibility
        map.insert("ifaType".to_string(), self.ifa_type.clone().into());
        map.insert("ifa_type".to_string(), self.ifa_type.clone().into());
        map.insert("lmt".to_string(), self.lmt.clone().into());
        map.serialize(serializer)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdvertisingPolicy {
    pub skip_restriction: String,
    pub limit_ad_tracking: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct AdvertisingIdRPCRequest {
    pub options: Option<ScopeOption>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ScopeOption {
    pub scope: Option<Scope>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Scope {
    #[serde(rename = "type")]
    pub _type: ScopeType,
    pub id: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub enum ScopeType {
    Browse,
    Content,
}

impl ScopeType {
    pub fn as_string(&self) -> &'static str {
        match self {
            ScopeType::Browse => "browse",
            ScopeType::Content => "content",
        }
    }
}

#[rpc(server)]
pub trait Advertising {
    #[method(name = "advertising.appBundleId")]
    fn app_bundle_id(&self, ctx: CallContext) -> RpcResult<String>;
    #[method(name = "advertising.policy")]
    async fn policy(&self, ctx: CallContext) -> RpcResult<AdvertisingPolicy>;
}
const NONE: &str = "none";
async fn get_advertisting_policy(platform_state: &PlatformState) -> AdvertisingPolicy {
    AdvertisingPolicy {
        skip_restriction: StorageManager::get_string(
            platform_state,
            StorageProperty::SkipRestriction,
        )
        .await
        .unwrap_or_else(|_| String::from(NONE)),
        limit_ad_tracking: !privacy_rpc::PrivacyImpl::get_allow_app_content_ad_targeting(
            platform_state,
        )
        .await,
    }
}

#[derive(Clone)]
//Clippy does not seem to know this is not actually dead code. It is used in the decorator, but
//maybe dynamic nature of rules is confusing it.
#[allow(dead_code)]
struct AdvertisingPolicyEventDecorator;
#[async_trait]
impl AppEventDecorator for AdvertisingPolicyEventDecorator {
    async fn decorate(
        &self,
        ps: &PlatformState,
        _ctx: &CallContext,
        _event_name: &str,
        _val_in: &Value,
    ) -> Result<Value, AppEventDecorationError> {
        Ok(serde_json::to_value(get_advertisting_policy(ps).await)?)
    }
    fn dec_clone(&self) -> Box<dyn AppEventDecorator + Send + Sync> {
        Box::new(self.clone())
    }
}

pub struct AdvertisingImpl {
    pub state: PlatformState,
}

#[async_trait]
impl AdvertisingServer for AdvertisingImpl {
    fn app_bundle_id(&self, ctx: CallContext) -> RpcResult<String> {
        Ok(format!(
            "{}.{}",
            ctx.app_id, ADVERTISING_APP_BUNDLE_ID_SUFFIX
        ))
    }

    async fn policy(&self, _ctx: CallContext) -> RpcResult<AdvertisingPolicy> {
        Ok(get_advertisting_policy(&self.state).await)
    }
}

pub struct AdvertisingRPCProvider;
impl RippleRPCProvider<AdvertisingImpl> for AdvertisingRPCProvider {
    fn provide(state: PlatformState) -> RpcModule<AdvertisingImpl> {
        (AdvertisingImpl { state }).into_rpc()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::firebolt::handlers::advertising_rpc::AdvertisingImpl;
    use ripple_sdk::{api::gateway::rpc_gateway_api::JsonRpcApiRequest, tokio};
    use ripple_tdk::utils::test_utils::Mockable;
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    #[derive(Serialize, Deserialize, Clone, Debug)]
    struct CallContextContainer {
        pub ctx: Option<CallContext>,
    }

    fn merge(a: &mut Value, b: &Value) {
        match (a, b) {
            (&mut Value::Object(ref mut a), Value::Object(ref b)) => {
                for (k, v) in b {
                    merge(a.entry(k.clone()).or_insert(Value::Null), v);
                }
            }
            (a, b) => {
                *a = b.clone();
            }
        }
    }

    fn test_request(
        method_name: String,
        call_context: Option<CallContext>,
        params: Option<Value>,
    ) -> String {
        let mut the_map = params.map_or(json!({}), |v| v);
        merge(
            &mut the_map,
            &serde_json::json!(CallContextContainer { ctx: call_context }),
        );
        let v = serde_json::to_value(JsonRpcApiRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(1),
            method: method_name,
            params: Some(the_map),
        })
        .unwrap();
        serde_json::to_string(&v).unwrap()
    }

    #[tokio::test]
    pub async fn test_app_bundle_id() {
        let ad_module = (AdvertisingImpl {
            state: PlatformState::mock(),
        })
        .into_rpc();

        let request = test_request(
            "advertising.appBundleId".to_string(),
            Some(CallContext::mock()),
            None,
        );

        assert!(ad_module.raw_json_request(&request).await.is_ok());
    }
}
