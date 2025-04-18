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

use ripple_sdk::{
    api::{
        firebolt::fb_capabilities::{
            FireboltPermission, CAPABILITY_NOT_AVAILABLE, JSON_RPC_STANDARD_ERROR_INVALID_PARAMS,
        },
        gateway::rpc_gateway_api::{
            ApiMessage, ApiProtocol, CallContext, JsonRpcApiRequest, JsonRpcApiResponse,
            RpcRequest, RPC_V2,
        },
        observability::log_signal::LogSignal,
        session::AccountSession,
    },
    extn::extn_client_message::{ExtnEvent, ExtnMessage},
    framework::RippleResponse,
    log::{debug, error, trace},
    tokio::{
        self,
        sync::mpsc::{self, Receiver, Sender},
    },
    utils::error::RippleError,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, RwLock,
    },
};

use crate::{
    broker::broker_utils::BrokerUtils,
    firebolt::firebolt_gateway::{FireboltGatewayCommand, JsonRpcError},
    service::extn::ripple_client::RippleClient,
    state::{metrics_state::MetricsState, platform_state::PlatformState, session_state::Session},
    utils::router_utils::{
        add_telemetry_status_code, capture_stage, get_rpc_header, return_extn_response,
    },
};

use super::{
    event_management_utility::EventManagementUtility,
    extn_broker::ExtnBroker,
    http_broker::HttpBroker,
    provider_broker_state::{ProvideBrokerState, ProviderResult},
    rules_engine::{jq_compile, Rule, RuleEndpoint, RuleEndpointProtocol, RuleEngine},
    thunder_broker::ThunderBroker,
    websocket_broker::WebsocketBroker,
    workflow_broker::WorkflowBroker,
};

#[derive(Clone, Debug)]
pub struct BrokerSender {
    pub sender: Sender<BrokerRequest>,
}

#[derive(Clone, Debug, Default)]
pub struct BrokerCleaner {
    pub cleaner: Option<Sender<String>>,
}

impl BrokerCleaner {
    async fn cleanup_session(&self, appid: &str) {
        if let Some(cleaner) = self.cleaner.clone() {
            if let Err(e) = cleaner.send(appid.to_owned()).await {
                error!("Couldnt cleanup {} {:?}", appid, e)
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct BrokerRequest {
    pub rpc: RpcRequest,
    pub rule: Rule,
    pub subscription_processed: Option<bool>,
    pub workflow_callback: Option<BrokerCallback>,
    pub telemetry_response_listeners: Vec<Sender<BrokerOutput>>,
}
impl ripple_sdk::api::observability::log_signal::ContextAsJson for BrokerRequest {
    fn as_json(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        map.insert(
            "session_id".to_string(),
            serde_json::Value::String(self.rpc.ctx.session_id.clone()),
        );
        map.insert(
            "request_id".to_string(),
            serde_json::Value::String(self.rpc.ctx.request_id.clone()),
        );
        map.insert(
            "app_id".to_string(),
            serde_json::Value::String(self.rpc.ctx.app_id.clone()),
        );
        map.insert(
            "call_id".to_string(),
            serde_json::Value::Number(serde_json::Number::from(self.rpc.ctx.call_id)),
        );
        // map.insert(
        //     "protocol".to_string(),
        //     serde_json::Value::String(self.rpc.ctx.protocol.clone()),
        // );
        map.insert(
            "method".to_string(),
            serde_json::Value::String(self.rpc.method.clone()),
        );
        // map.insert(
        //     "cid".to_string(),
        //     serde_json::Value::String(self.rpc.ctx.cid.clone()),
        // );
        map.insert(
            "gateway_secure".to_string(),
            serde_json::Value::Bool(self.rpc.ctx.gateway_secure),
        );
        serde_json::Value::Object(map)
    }
}
impl std::fmt::Display for BrokerRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "BrokerRequest {{ rpc: {:?}, rule: {:?}, subscription_processed: {:?}, workflow_callback: {:?} }}",
            self.rpc, self.rule, self.subscription_processed, self.workflow_callback
        )
    }
}

pub type BrokerSubMap = HashMap<String, Vec<BrokerRequest>>;

#[derive(Clone, Debug)]
pub struct BrokerConnectRequest {
    pub key: String,
    pub endpoint: RuleEndpoint,
    pub sub_map: BrokerSubMap,
    pub session: Option<AccountSession>,
    pub reconnector: Sender<BrokerConnectRequest>,
}
impl Default for BrokerConnectRequest {
    fn default() -> Self {
        Self {
            key: "".to_owned(),
            endpoint: RuleEndpoint::default(),
            sub_map: HashMap::new(),
            session: None,
            reconnector: mpsc::channel(2).0,
        }
    }
}
impl From<BrokerRequest> for JsonRpcApiRequest {
    fn from(value: BrokerRequest) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            id: Some(value.rpc.ctx.call_id),
            method: value.rpc.ctx.method,
            params: serde_json::from_str(&value.rpc.params_json).unwrap_or(None),
        }
    }
}
impl From<BrokerRequest> for JsonRpcApiResponse {
    fn from(value: BrokerRequest) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            id: Some(value.rpc.ctx.call_id),
            result: None,
            error: None,
            method: None,
            params: None,
        }
    }
}

impl BrokerConnectRequest {
    pub fn new(
        key: String,
        endpoint: RuleEndpoint,
        reconnector: Sender<BrokerConnectRequest>,
    ) -> Self {
        Self {
            key,
            endpoint,
            sub_map: HashMap::new(),
            session: None,
            reconnector,
        }
    }

    pub fn new_with_sesssion(
        key: String,
        endpoint: RuleEndpoint,
        reconnector: Sender<BrokerConnectRequest>,
        session: Option<AccountSession>,
    ) -> Self {
        Self {
            key,
            endpoint,
            sub_map: HashMap::new(),
            session,
            reconnector,
        }
    }
}

impl BrokerRequest {
    pub fn is_subscription_processed(&self) -> bool {
        self.subscription_processed.is_some()
    }
}

impl BrokerRequest {
    pub fn new(
        rpc_request: &RpcRequest,
        rule: Rule,
        workflow_callback: Option<BrokerCallback>,
        telemetry_response_listeners: Vec<Sender<BrokerOutput>>,
    ) -> BrokerRequest {
        BrokerRequest {
            rpc: rpc_request.clone(),
            rule,
            subscription_processed: None,
            workflow_callback,
            telemetry_response_listeners,
        }
    }

    pub fn get_id(&self) -> String {
        self.rpc.ctx.session_id.clone()
    }
}

/// BrokerCallback will be used by the communication broker to send the firebolt response
/// back to the gateway for client consumption
#[derive(Clone, Debug)]
pub struct BrokerCallback {
    pub sender: Sender<BrokerOutput>,
}
impl Default for BrokerCallback {
    fn default() -> Self {
        Self {
            sender: mpsc::channel(2).0,
        }
    }
}

static ATOMIC_ID: AtomicU64 = AtomicU64::new(0);

impl BrokerCallback {
    pub async fn send_json_rpc_api_response(&self, response: JsonRpcApiResponse) {
        let output = BrokerOutput::new(response);
        if let Err(e) = self.sender.send(output).await {
            error!("couldnt send response for {:?}", e);
        }
    }
    /// Default method used for sending errors via the BrokerCallback
    pub async fn send_error(&self, request: BrokerRequest, error: RippleError) {
        let value = serde_json::to_value(JsonRpcError {
            code: JSON_RPC_STANDARD_ERROR_INVALID_PARAMS,
            message: format!("Error with {:?}", error),
            data: None,
        })
        .unwrap();
        let data = JsonRpcApiResponse {
            jsonrpc: "2.0".to_owned(),
            id: Some(request.rpc.ctx.call_id),
            error: Some(value),
            result: None,
            method: None,
            params: None,
        };
        self.send_json_rpc_api_response(data).await;
    }
}

#[derive(Debug)]
pub struct BrokerContext {
    pub app_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct BrokerOutput {
    pub data: JsonRpcApiResponse,
}

impl BrokerOutput {
    pub fn new(data: JsonRpcApiResponse) -> Self {
        Self { data }
    }
    pub fn with_jsonrpc_response(&mut self, data: JsonRpcApiResponse) -> &mut Self {
        self.data = data;
        self
    }
    pub fn is_result(&self) -> bool {
        self.data.result.is_some()
    }

    pub fn get_event(&self) -> Option<u64> {
        if let Some(e) = &self.data.method {
            let event: Vec<&str> = e.split('.').collect();
            if let Some(v) = event.first() {
                if let Ok(r) = v.parse::<u64>() {
                    return Some(r);
                }
            }
        }
        None
    }
    pub fn is_error(&self) -> bool {
        self.data.error.is_some()
    }
    pub fn is_success(&self) -> bool {
        self.data.result.is_some()
    }
    pub fn get_result(&self) -> Option<Value> {
        self.data.result.clone()
    }
    pub fn get_error(&self) -> Option<Value> {
        self.data.error.clone()
    }
    pub fn get_error_string(&self) -> String {
        if let Some(e) = self.data.error.clone() {
            if let Ok(v) = serde_json::to_string(&e) {
                return v;
            }
        }
        "unknown".to_string()
    }
}

impl From<CallContext> for BrokerContext {
    fn from(value: CallContext) -> Self {
        Self {
            app_id: value.app_id,
        }
    }
}

impl BrokerSender {
    // Method to send the request to the underlying broker for handling.
    pub async fn send(&self, request: BrokerRequest) -> RippleResponse {
        if let Err(e) = self.sender.send(request).await {
            error!("Error sending to broker {:?}", e);
            Err(RippleError::SendFailure)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone)]
pub struct EndpointBrokerState {
    endpoint_map: Arc<RwLock<HashMap<String, BrokerSender>>>,
    callback: BrokerCallback,
    request_map: Arc<RwLock<HashMap<u64, BrokerRequest>>>,
    extension_request_map: Arc<RwLock<HashMap<u64, ExtnMessage>>>,
    rule_engine: RuleEngine,
    cleaner_list: Arc<RwLock<Vec<BrokerCleaner>>>,
    reconnect_tx: Sender<BrokerConnectRequest>,
    provider_broker_state: ProvideBrokerState,
    metrics_state: MetricsState,
}
impl Default for EndpointBrokerState {
    fn default() -> Self {
        Self {
            endpoint_map: Arc::new(RwLock::new(HashMap::new())),
            callback: BrokerCallback::default(),
            request_map: Arc::new(RwLock::new(HashMap::new())),
            extension_request_map: Arc::new(RwLock::new(HashMap::new())),
            rule_engine: RuleEngine::default(),
            cleaner_list: Arc::new(RwLock::new(Vec::new())),
            reconnect_tx: mpsc::channel(2).0,
            provider_broker_state: ProvideBrokerState::default(),
            metrics_state: MetricsState::default(),
        }
    }
}

impl EndpointBrokerState {
    pub fn new(
        metrics_state: MetricsState,
        tx: Sender<BrokerOutput>,
        rule_engine: RuleEngine,
        ripple_client: RippleClient,
    ) -> Self {
        let (reconnect_tx, rec_tr) = mpsc::channel(2);
        let state = Self {
            endpoint_map: Arc::new(RwLock::new(HashMap::new())),
            callback: BrokerCallback { sender: tx },
            request_map: Arc::new(RwLock::new(HashMap::new())),
            extension_request_map: Arc::new(RwLock::new(HashMap::new())),
            rule_engine,
            cleaner_list: Arc::new(RwLock::new(Vec::new())),
            reconnect_tx,
            provider_broker_state: ProvideBrokerState::default(),
            metrics_state,
        };
        state.reconnect_thread(rec_tr, ripple_client);
        state
    }
    pub fn with_rules_engine(mut self, rule_engine: RuleEngine) -> Self {
        self.rule_engine = rule_engine;
        self
    }

    fn reconnect_thread(&self, mut rx: Receiver<BrokerConnectRequest>, client: RippleClient) {
        let mut state = self.clone();
        tokio::spawn(async move {
            while let Some(v) = rx.recv().await {
                if matches!(v.endpoint.protocol, RuleEndpointProtocol::Thunder) {
                    if client
                        .send_gateway_command(FireboltGatewayCommand::StopServer)
                        .is_err()
                    {
                        error!("Stopping server")
                    }
                    break;
                } else {
                    state.build_endpoint(None, v)
                }
            }
        });
    }

    fn get_request(&self, id: u64) -> Result<BrokerRequest, RippleError> {
        let result = { self.request_map.read().unwrap().get(&id).cloned() };
        if result.is_none() {
            return Err(RippleError::InvalidInput);
        }

        let result = result.unwrap();
        if !result.rpc.is_subscription() {
            let _ = self.request_map.write().unwrap().remove(&id);
        }
        Ok(result)
    }

    fn update_unsubscribe_request(&self, id: u64) {
        let mut result = self.request_map.write().unwrap();
        if let Some(mut value) = result.remove(&id) {
            value.subscription_processed = Some(true);
            let _ = result.insert(id, value);
        }
    }

    fn get_extn_message(&self, id: u64, is_event: bool) -> Result<ExtnMessage, RippleError> {
        if is_event {
            let v = { self.extension_request_map.read().unwrap().get(&id).cloned() };
            if let Some(v1) = v {
                Ok(v1)
            } else {
                Err(RippleError::NotAvailable)
            }
        } else {
            let result = { self.extension_request_map.write().unwrap().remove(&id) };
            match result {
                Some(v) => Ok(v),
                None => Err(RippleError::NotAvailable),
            }
        }
    }

    pub fn get_next_id() -> u64 {
        ATOMIC_ID.fetch_add(1, Ordering::Relaxed);
        ATOMIC_ID.load(Ordering::Relaxed)
    }

    fn update_request(
        &self,
        rpc_request: &RpcRequest,
        rule: Rule,
        extn_message: Option<ExtnMessage>,
        workflow_callback: Option<BrokerCallback>,
        telemetry_response_listeners: Vec<Sender<BrokerOutput>>,
    ) -> (u64, BrokerRequest) {
        let id = Self::get_next_id();
        let mut rpc_request_c = rpc_request.clone();
        {
            let mut request_map = self.request_map.write().unwrap();
            let _ = request_map.insert(
                id,
                BrokerRequest {
                    rpc: rpc_request.clone(),
                    rule: rule.clone(),
                    subscription_processed: None,
                    workflow_callback: workflow_callback.clone(),
                    telemetry_response_listeners: telemetry_response_listeners.clone(),
                },
            );
        }

        if extn_message.is_some() {
            let mut extn_map = self.extension_request_map.write().unwrap();
            let _ = extn_map.insert(id, extn_message.unwrap());
        }

        rpc_request_c.ctx.call_id = id;
        (
            id,
            BrokerRequest::new(
                &rpc_request_c,
                rule,
                workflow_callback,
                telemetry_response_listeners,
            ),
        )
    }
    pub fn build_thunder_endpoint(&mut self) {
        if let Some(endpoint) = self.rule_engine.rules.endpoints.get("thunder").cloned() {
            let request = BrokerConnectRequest::new(
                "thunder".to_owned(),
                endpoint.clone(),
                self.reconnect_tx.clone(),
            );
            self.build_endpoint(None, request);
        }
    }

    pub fn build_other_endpoints(&mut self, ps: PlatformState, session: Option<AccountSession>) {
        for (key, endpoint) in self.rule_engine.rules.endpoints.clone() {
            // skip thunder endpoint as it is already built using build_thunder_endpoint
            if let RuleEndpointProtocol::Thunder = endpoint.protocol {
                continue;
            }
            let request = BrokerConnectRequest::new_with_sesssion(
                key,
                endpoint.clone(),
                self.reconnect_tx.clone(),
                session.clone(),
            );
            self.build_endpoint(Some(ps.clone()), request);
        }
    }

    fn add_endpoint(&mut self, key: String, endpoint: BrokerSender) {
        let mut endpoint_map = self.endpoint_map.write().unwrap();
        endpoint_map.insert(key, endpoint);
    }
    pub fn get_endpoints(&self) -> HashMap<String, BrokerSender> {
        self.endpoint_map.read().unwrap().clone()
    }
    pub fn get_other_endpoints(&self, me: &str) -> HashMap<String, BrokerSender> {
        let f = self.endpoint_map.read().unwrap().clone();
        let mut result = HashMap::new();
        for (k, v) in f.iter() {
            if k.as_str() != me {
                result.insert(k.clone(), v.clone());
            }
        }
        result
    }

    fn build_endpoint(&mut self, ps: Option<PlatformState>, request: BrokerConnectRequest) {
        let endpoint = request.endpoint.clone();
        let key = request.key.clone();
        let (broker, cleaner) = match endpoint.protocol {
            RuleEndpointProtocol::Http => (
                HttpBroker::get_broker(None, request, self.callback.clone(), self).get_sender(),
                None,
            ),
            RuleEndpointProtocol::Websocket => {
                let ws_broker =
                    WebsocketBroker::get_broker(None, request, self.callback.clone(), self);
                (ws_broker.get_sender(), Some(ws_broker.get_cleaner()))
            }
            RuleEndpointProtocol::Thunder => {
                let thunder_broker =
                    ThunderBroker::get_broker(None, request, self.callback.clone(), self);
                (
                    thunder_broker.get_sender(),
                    Some(thunder_broker.get_cleaner()),
                )
            }
            RuleEndpointProtocol::Workflow => (
                WorkflowBroker::get_broker(None, request, self.callback.clone(), self).get_sender(),
                None,
            ),
            RuleEndpointProtocol::Extn => (
                ExtnBroker::get_broker(ps, request, self.callback.clone(), self).get_sender(),
                None,
            ),
        };
        self.add_endpoint(key, broker);

        if let Some(cleaner) = cleaner {
            let mut cleaner_list = self.cleaner_list.write().unwrap();
            cleaner_list.push(cleaner);
        }
    }

    fn handle_static_request(
        &self,
        rpc_request: RpcRequest,
        extn_message: Option<ExtnMessage>,
        rule: Rule,
        callback: BrokerCallback,
        workflow_callback: Option<BrokerCallback>,
        telemetry_response_listeners: Vec<Sender<BrokerOutput>>,
    ) {
        let (id, _updated_request) = self.update_request(
            &rpc_request,
            rule.clone(),
            extn_message,
            workflow_callback,
            telemetry_response_listeners,
        );
        let mut data = JsonRpcApiResponse::default();
        // return empty result and handle the rest with jq rule
        let jv: Value = "".into();
        data.result = Some(jv);
        data.id = Some(id);
        let output = BrokerOutput::new(data);

        capture_stage(&self.metrics_state, &rpc_request, "static_rule_request");
        tokio::spawn(async move { callback.sender.send(output).await });
    }

    fn handle_provided_request(
        &self,
        rpc_request: &RpcRequest,
        rule: Rule,
        callback: BrokerCallback,
        permission: Vec<FireboltPermission>,
        session: Option<Session>,
        telemetry_response_listeners: Vec<Sender<BrokerOutput>>,
    ) {
        let (id, request) =
            self.update_request(rpc_request, rule, None, None, telemetry_response_listeners);
        match self.provider_broker_state.check_provider_request(
            rpc_request,
            &permission,
            session.clone(),
        ) {
            Some(ProviderResult::Registered) => {
                // return empty result and handle the rest with jq rule
                let data = JsonRpcApiResponse {
                    id: Some(id),
                    jsonrpc: "2.0".to_string(),
                    result: Some(Value::Null),
                    error: None,
                    method: None,
                    params: None,
                };

                let output = BrokerOutput { data };
                tokio::spawn(async move { callback.sender.send(output).await });
            }
            Some(ProviderResult::Session(s)) => {
                ProvideBrokerState::send_to_provider(request, id, s);
            }
            Some(ProviderResult::NotAvailable(p)) => {
                // Not Available
                let data = JsonRpcApiResponse::new(
                    Some(id),
                    Some(json!({
                        "error": CAPABILITY_NOT_AVAILABLE,
                        "messsage": format!("{} not available", p)
                    })),
                );

                let output = BrokerOutput { data };
                tokio::spawn(async move { callback.sender.send(output).await });
            }
            None => {
                // Not Available
                let data = JsonRpcApiResponse::new(
                    Some(id),
                    Some(json!({
                        "error": CAPABILITY_NOT_AVAILABLE,
                        "messsage": "capability not available".to_string()
                    })),
                );

                let output = BrokerOutput { data };
                tokio::spawn(async move { callback.sender.send(output).await });
            }
        }
    }

    fn get_sender(&self, hash: &str) -> Option<BrokerSender> {
        self.endpoint_map.read().unwrap().get(hash).cloned()
    }

    /// Main handler method whcih checks for brokerage and then sends the request for
    /// asynchronous processing
    pub fn handle_brokerage(
        &self,
        rpc_request: RpcRequest,
        extn_message: Option<ExtnMessage>,
        requestor_callback: Option<BrokerCallback>,
        permissions: Vec<FireboltPermission>,
        session: Option<Session>,
        telemetry_response_listeners: Vec<Sender<BrokerOutput>>,
    ) -> bool {
        let mut handled: bool = true;
        let callback = self.callback.clone();
        let mut broker_sender = None;
        let mut found_rule = None;
        LogSignal::new(
            "handle_brokerage".to_string(),
            "starting brokerage".to_string(),
            rpc_request.ctx.clone(),
        )
        .emit_debug();
        if let Some(rule) = self.rule_engine.get_rule(&rpc_request) {
            found_rule = Some(rule.clone());

            if let Some(endpoint) = rule.endpoint {
                LogSignal::new(
                    "handle_brokerage".to_string(),
                    "rule found".to_string(),
                    rpc_request.ctx.clone(),
                )
                .with_diagnostic_context_item("rule_alias", &rule.alias)
                .with_diagnostic_context_item("endpoint", &endpoint)
                .emit_debug();
                if let Some(endpoint) = self.get_sender(&endpoint) {
                    broker_sender = Some(endpoint);
                }
            } else if rule.alias != "static" {
                LogSignal::new(
                    "handle_brokerage".to_string(),
                    "rule found".to_string(),
                    rpc_request.ctx.clone(),
                )
                .with_diagnostic_context_item("rule_alias", &rule.alias)
                .with_diagnostic_context_item("static", rule.alias.as_str())
                .emit_debug();
                if let Some(endpoint) = self.get_sender("thunder") {
                    broker_sender = Some(endpoint);
                }
            }
        } else {
            LogSignal::new(
                "handle_brokerage".to_string(),
                "rule not found".to_string(),
                rpc_request.ctx.clone(),
            )
            .emit_debug();
        }
        trace!("found rule {:?}", found_rule);
        if found_rule.is_some() {
            let rule = found_rule.unwrap();

            if rule.alias == *"static" {
                trace!("handling static request for {:?}", rpc_request);
                self.handle_static_request(
                    rpc_request.clone(),
                    extn_message,
                    rule,
                    callback,
                    requestor_callback,
                    telemetry_response_listeners,
                );
            } else if rule.alias.eq_ignore_ascii_case("provided") {
                self.handle_provided_request(
                    &rpc_request,
                    rule,
                    callback,
                    permissions,
                    session,
                    telemetry_response_listeners,
                );
            } else if broker_sender.is_some() {
                trace!("handling not static request for {:?}", rpc_request);
                let broker_sender = broker_sender.unwrap();
                let (_, updated_request) = self.update_request(
                    &rpc_request,
                    rule,
                    extn_message,
                    requestor_callback,
                    telemetry_response_listeners,
                );
                capture_stage(&self.metrics_state, &rpc_request, "broker_request");
                let thunder = self.get_sender("thunder");
                let request_context = updated_request.rpc.ctx.clone();
                tokio::spawn(async move {
                    /*
                    process "unlisten" requests here - the broker layers require state, which does not exist , as the
                    state has already been deleted by the time the unlisten request is processed.
                    */
                    if updated_request.rpc.is_unlisten() {
                        let result: JsonRpcApiResponse = updated_request.clone().rpc.into();
                        LogSignal::new(
                            "handle_brokerage".to_string(),
                            "unlisten request".to_string(),
                            request_context.clone(),
                        )
                        .emit_debug();
                        /*
                        This is suboptimal, but the only way to handle this is to send the unlisten request to the thunder, and then
                        */
                        if let Some(thunder) = thunder {
                            match thunder.send(updated_request.clone()).await {
                                Ok(_) => callback.send_json_rpc_api_response(result).await,
                                Err(e) => callback.send_error(updated_request, e).await,
                            }
                        }
                    } else if let Err(e) = broker_sender.send(updated_request.clone()).await {
                        LogSignal::new(
                            "handle_brokerage".to_string(),
                            "broker send error".to_string(),
                            request_context.clone(),
                        )
                        .emit_error();
                        callback.send_error(updated_request, e).await
                    }
                });
            } else {
                handled = false;
            }
        } else {
            handled = false;
        }
        LogSignal::new(
            "handle_brokerage".to_string(),
            "brokerage complete".to_string(),
            rpc_request.ctx.clone(),
        )
        .with_diagnostic_context_item("handled", handled.to_string().as_str())
        .emit_debug();

        handled
    }

    pub fn handle_broker_response(&self, data: JsonRpcApiResponse) {
        if let Err(e) = self.callback.sender.try_send(BrokerOutput { data }) {
            error!("Cannot forward broker response {:?}", e)
        }
    }

    pub fn get_rule(&self, rpc_request: &RpcRequest) -> Option<Rule> {
        self.rule_engine.get_rule(rpc_request)
    }

    // Method to cleanup all subscription on App termination
    pub async fn cleanup_for_app(&self, app_id: &str) {
        let cleaners = { self.cleaner_list.read().unwrap().clone() };
        for cleaner in cleaners {
            cleaner.cleanup_session(app_id).await
        }
    }
}

/// Trait which contains all the abstract methods for a Endpoint Broker
/// There could be Websocket or HTTP protocol implementations of the given trait
pub trait EndpointBroker {
    fn get_broker(
        ps: Option<PlatformState>,
        request: BrokerConnectRequest,
        callback: BrokerCallback,
        endpoint_broker: &mut EndpointBrokerState,
    ) -> Self;

    fn get_sender(&self) -> BrokerSender;

    fn prepare_request(&self, rpc_request: &BrokerRequest) -> Result<Vec<String>, RippleError> {
        let response = Self::update_request(rpc_request)?;
        Ok(vec![response])
    }

    /// Adds BrokerContext to a given request used by the Broker Implementations
    /// just before sending the data through the protocol
    fn update_request(rpc_request: &BrokerRequest) -> Result<String, RippleError> {
        let v = Self::apply_request_rule(rpc_request)?;
        trace!("transformed request {:?}", v);
        let id = rpc_request.rpc.ctx.call_id;
        let method = rpc_request.rule.alias.clone();
        if let Value::Null = v {
            Ok(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method
            })
            .to_string())
        } else {
            Ok(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": v
            })
            .to_string())
        }
    }

    /// Generic method which takes the given parameters from RPC request and adds rules using rule engine
    fn apply_request_rule(rpc_request: &BrokerRequest) -> Result<Value, RippleError> {
        if let Ok(mut params) = serde_json::from_str::<Vec<Value>>(&rpc_request.rpc.params_json) {
            let last = if params.len() > 1 {
                params.pop().unwrap()
            } else {
                Value::Null
            };

            if let Some(filter) = rpc_request
                .rule
                .transform
                .get_transform_data(super::rules_engine::RuleTransformType::Request)
            {
                let transformed_request_res = jq_compile(
                    last,
                    &filter,
                    format!("{}_request", rpc_request.rpc.ctx.method),
                );

                LogSignal::new(
                    "endpoint_broker".to_string(),
                    "apply_request_rule".to_string(),
                    rpc_request.rpc.ctx.clone(),
                )
                .with_diagnostic_context_item("success", "true")
                .with_diagnostic_context_item("result", &format!("{:?}", transformed_request_res))
                .emit_debug();

                return transformed_request_res;
            }
            LogSignal::new(
                "endpoint_broker".to_string(),
                "apply_request_rule".to_string(),
                rpc_request.rpc.ctx.clone(),
            )
            .with_diagnostic_context_item("success", "true")
            .with_diagnostic_context_item("result", &last.to_string())
            .emit_debug();
            return Ok(serde_json::to_value(&last).unwrap());
        }
        LogSignal::new(
            "endpoint_broker".to_string(),
            "apply_request_rule: parse error".to_string(),
            rpc_request.rpc.ctx.clone(),
        )
        .emit_error();
        Err(RippleError::ParseError)
    }

    /// Default handler method for the broker to remove the context and send it back to the
    /// client for consumption
    fn handle_jsonrpc_response(
        result: &[u8],
        callback: BrokerCallback,
        _params: Option<Value>,
    ) -> Result<BrokerOutput, RippleError> {
        let mut final_result = Err(RippleError::ParseError);
        if let Ok(data) = serde_json::from_slice::<JsonRpcApiResponse>(result) {
            final_result = Ok(BrokerOutput::new(data));
        }
        if let Ok(output) = final_result.clone() {
            tokio::spawn(async move { callback.sender.send(output).await });
        } else {
            error!("Bad broker response {}", String::from_utf8_lossy(result));
        }
        final_result
    }

    fn get_cleaner(&self) -> BrokerCleaner;

    fn send_broker_success_response(
        callback: &BrokerCallback,
        success_message: JsonRpcApiResponse,
    ) {
        BrokerOutputForwarder::send_json_rpc_response_to_broker(success_message, callback.clone());
    }
    fn send_broker_failure_response(callback: &BrokerCallback, error_message: JsonRpcApiResponse) {
        BrokerOutputForwarder::send_json_rpc_response_to_broker(error_message, callback.clone());
    }
}

/// Forwarder gets the BrokerOutput and forwards the response to the gateway.
pub struct BrokerOutputForwarder;

impl BrokerOutputForwarder {
    pub fn start_forwarder(mut platform_state: PlatformState, mut rx: Receiver<BrokerOutput>) {
        // set up the event utility
        let event_utility = Arc::new(EventManagementUtility::new());
        event_utility.register_custom_functions();
        let event_utility_clone = event_utility.clone();

        tokio::spawn(async move {
            while let Some(output) = rx.recv().await {
                let output_c = output.clone();
                let mut response = output.data.clone();
                let mut is_event = false;
                // First validate the id check if it could be an event
                let id = if let Some(e) = output_c.get_event() {
                    is_event = true;
                    Some(e)
                } else {
                    response.id
                };

                if let Some(id) = id {
                    if let Ok(broker_request) = platform_state.endpoint_state.get_request(id) {
                        LogSignal::new(
                            "start_forwarder".to_string(),
                            "broker request found".to_string(),
                            broker_request.clone(),
                        )
                        .emit_debug();
                        /*
                        save off rpc method name for rule context telemetry
                        */
                        let rule_context_name = broker_request.rpc.method.clone();

                        let workflow_callback = broker_request.clone().workflow_callback;
                        let telemetry_response_listeners =
                            broker_request.clone().telemetry_response_listeners;
                        let sub_processed = broker_request.is_subscription_processed();
                        let rpc_request = broker_request.rpc.clone();
                        let session_id = rpc_request.ctx.get_id();
                        let is_subscription = rpc_request.is_subscription();
                        let mut apply_response_needed = false;

                        // Step 1: Create the data
                        if let Some(result) = response.result.clone() {
                            LogSignal::new(
                                "start_forwarder".to_string(),
                                "processing event".to_string(),
                                broker_request.clone(),
                            )
                            .emit_debug();

                            if is_event {
                                if let Some(method) = broker_request.rule.event_handler.clone() {
                                    let platform_state_c = platform_state.clone();
                                    let rpc_request_c = rpc_request.clone();
                                    let response_c = response.clone();
                                    let broker_request_c = broker_request.clone();

                                    tokio::spawn(Self::handle_event(
                                        platform_state_c,
                                        method,
                                        broker_request_c,
                                        rpc_request_c,
                                        response_c,
                                    ));

                                    continue;
                                }

                                if let Some(filter) =
                                    broker_request.rule.transform.get_transform_data(
                                        super::rules_engine::RuleTransformType::Event(
                                            rpc_request.ctx.context.contains(&RPC_V2.into()),
                                        ),
                                    )
                                {
                                    apply_rule_for_event(
                                        &broker_request,
                                        &result,
                                        &rpc_request,
                                        &filter,
                                        &mut response,
                                    );
                                }

                                if !apply_filter(&broker_request, &result, &rpc_request) {
                                    continue;
                                }

                                // check if the request transform has event_decorator_method
                                if let Some(decorator_method) =
                                    broker_request.rule.transform.event_decorator_method.clone()
                                {
                                    if let Some(func) =
                                        event_utility_clone.get_function(&decorator_method)
                                    {
                                        // spawn a tokio thread to run the function and continue the main thread.
                                        LogSignal::new(
                                            "start_forwarder".to_string(),
                                            "event decorator method found".to_string(),
                                            rpc_request.ctx.clone(),
                                        )
                                        .emit_debug();
                                        let session_id = rpc_request.ctx.get_id();
                                        let request_id = rpc_request.ctx.call_id;
                                        let protocol = rpc_request.ctx.protocol.clone();
                                        let platform_state_c = platform_state.clone();
                                        let ctx = rpc_request.ctx.clone();
                                        tokio::spawn(async move {
                                            if let Ok(value) = func(
                                                platform_state_c.clone(),
                                                ctx.clone(),
                                                Some(result.clone()),
                                            )
                                            .await
                                            {
                                                response.result = Some(value.expect("REASON"));
                                            }
                                            response.id = Some(request_id);

                                            let message = ApiMessage::new(
                                                protocol,
                                                serde_json::to_string(&response).unwrap(),
                                                rpc_request.ctx.request_id.clone(),
                                            );

                                            if let Some(session) = platform_state_c
                                                .session_state
                                                .get_session_for_connection_id(&session_id)
                                            {
                                                let _ = session.send_json_rpc(message).await;
                                            }
                                        });
                                        continue;
                                    } else {
                                        LogSignal::new(
                                            "start_forwarder".to_string(),
                                            "event decorator method not found".to_string(),
                                            rpc_request.ctx.clone(),
                                        )
                                        .emit_debug();
                                        error!(
                                            "Failed to invoke decorator method {:?}",
                                            decorator_method
                                        );
                                    }
                                }
                            } else if is_subscription {
                                if sub_processed {
                                    continue;
                                }
                                response.result = Some(json!({
                                    "listening" : rpc_request.is_listening(),
                                    "event" : rpc_request.ctx.method
                                }));
                                platform_state.endpoint_state.update_unsubscribe_request(id);
                            } else {
                                apply_response_needed = true;
                            }
                        } else {
                            trace!("start_forwarder: no result {:?}", response);
                            LogSignal::new(
                                "start_forwarder".to_string(),
                                "no result".to_string(),
                                rpc_request.ctx.clone(),
                            )
                            .with_diagnostic_context_item("response", response.to_string().as_str())
                            .emit_debug();
                            apply_response_needed = true;
                        }

                        if apply_response_needed {
                            // Apply response rule using params if there is any; otherwise, apply response rule using main broker request's response rule
                            let mut apply_response_using_main_req_needed = true;
                            if let Some(params) = output.data.params {
                                if let Some(param) = params.as_object() {
                                    for (key, value) in param {
                                        if key == "response" {
                                            if let Some(filter) = value.as_str() {
                                                apply_response_using_main_req_needed = false;
                                                apply_response(
                                                    filter.to_string(),
                                                    &rpc_request.ctx.method,
                                                    &mut response,
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                            if apply_response_using_main_req_needed {
                                if let Some(filter) =
                                    broker_request.rule.transform.get_transform_data(
                                        super::rules_engine::RuleTransformType::Response,
                                    )
                                {
                                    apply_response(filter, &rule_context_name, &mut response);
                                } else if response.result.is_none() && response.error.is_none() {
                                    response.result = Some(Value::Null);
                                }
                            }
                        }

                        let request_id = rpc_request.ctx.call_id;
                        response.id = Some(request_id);

                        if let Some(workflow_callback) = workflow_callback {
                            debug!("sending to workflow callback {:?}", response);
                            LogSignal::new(
                                "start_forwarder".to_string(),
                                "sending to workflow callback".to_string(),
                                rpc_request.ctx.clone(),
                            )
                            .emit_debug();
                            let _ = workflow_callback
                                .sender
                                .send(BrokerOutput::new(response.clone()))
                                .await;
                        } else {
                            let tm_str = get_rpc_header(&rpc_request);

                            if is_event {
                                response.update_event_message(&rpc_request);
                            }

                            // Step 2: Create the message
                            let mut message = ApiMessage::new(
                                rpc_request.ctx.protocol.clone(),
                                serde_json::to_string(&response).unwrap(),
                                rpc_request.ctx.request_id.clone(),
                            );
                            let mut status_code: i64 = 1;
                            if let Some(e) = &response.error {
                                if let Some(Value::Number(n)) = e.get("code") {
                                    if let Some(v) = n.as_i64() {
                                        status_code = v;
                                    }
                                }
                            }

                            platform_state.metrics.update_api_stats_ref(
                                &rpc_request.ctx.request_id,
                                add_telemetry_status_code(
                                    &tm_str,
                                    status_code.to_string().as_str(),
                                ),
                            );

                            if let Some(api_stats) = platform_state
                                .metrics
                                .get_api_stats(&rpc_request.ctx.request_id)
                            {
                                message.stats = Some(api_stats);

                                if rpc_request.ctx.app_id.eq_ignore_ascii_case("internal") {
                                    platform_state
                                        .metrics
                                        .remove_api_stats(&rpc_request.ctx.request_id);
                                }
                            }

                            // Step 3: Handle Non Extension
                            if matches!(rpc_request.ctx.protocol, ApiProtocol::Extn) {
                                if let Ok(extn_message) =
                                    platform_state.endpoint_state.get_extn_message(id, is_event)
                                {
                                    if is_event {
                                        forward_extn_event(
                                            &extn_message,
                                            response.clone(),
                                            &platform_state,
                                        )
                                        .await;
                                    } else {
                                        return_extn_response(message, extn_message)
                                    }
                                }
                            } else if let Some(session) = platform_state
                                .session_state
                                .get_session_for_connection_id(&session_id)
                            {
                                let _ = session.send_json_rpc(message).await;
                            }
                        }

                        for listener in telemetry_response_listeners {
                            let _ = listener.send(BrokerOutput::new(response.clone())).await;
                        }
                    } else {
                        error!(
                            "start_forwarder:{} request not found for {:?}",
                            line!(),
                            response
                        );
                    }
                } else {
                    error!(
                        "Error couldnt broker the event {:?} due to a missing request id",
                        output_c
                    )
                }
            }
        });
    }

    async fn handle_event(
        platform_state: PlatformState,
        method: String,
        broker_request: BrokerRequest,
        rpc_request: RpcRequest,
        mut response: JsonRpcApiResponse,
    ) {
        let session_id = rpc_request.ctx.get_id();
        let request_id = rpc_request.ctx.call_id;
        let protocol = rpc_request.ctx.protocol.clone();
        let mut platform_state_c = platform_state.clone();

        // FIXME: As we transition to full RPCv2 support we need to be able to post-process the results from an event
        // handler as defined by Rule::event_handler, however as currently implemented event_handler logic short-circuits
        // rule transform logic. Need to refactor to support this, disabing below for now.
        // ==============================================================================================================
        // if let Ok(Value::String(res)) =
        //     BrokerUtils::process_internal_main_request(&mut platform_state_c, method.as_str(), None)
        //         .await
        // {
        //     let mut filter = res.clone();
        //     if let Some(transform_data) = broker_request.rule.transform.get_transform_data(
        //         super::rules_engine::RuleTransformType::Event(
        //             rpc_request.ctx.context.contains(&RPC_V2.into()),
        //         ),
        //     ) {
        //         filter = transform_data
        //             .replace("$event_handler_response", format!("\"{}\"", res).as_str());
        //     }

        //     let response_result_value = serde_json::to_value(filter.clone()).unwrap();

        //     apply_rule_for_event(
        //         &broker_request,
        //         &response_result_value,
        //         &rpc_request,
        //         &filter,
        //         &mut response,
        //     );
        // } else {
        //     error!("handle_event: error processing internal main request");
        // }

        let params = if let Some(request) = broker_request.rule.transform.request {
            if let Ok(map) = serde_json::from_str::<serde_json::Map<String, Value>>(&request) {
                Some(Value::Object(map))
            } else {
                None
            }
        } else {
            None
        };
        // ==============================================================================================================

        if let Ok(res) = BrokerUtils::process_internal_main_request(
            &mut platform_state_c,
            method.as_str(),
            params,
        )
        .await
        {
            response.result = Some(res.clone());
        }

        response.id = Some(request_id);

        response.update_event_message(&rpc_request);

        let message = ApiMessage::new(
            protocol,
            serde_json::to_string(&response).unwrap(),
            rpc_request.ctx.request_id.clone(),
        );

        if let Some(session) = platform_state_c
            .session_state
            .get_session_for_connection_id(&session_id)
        {
            let _ = session.send_json_rpc(message).await;
        }
    }

    pub fn handle_non_jsonrpc_response(
        data: &[u8],
        callback: BrokerCallback,
        request: BrokerRequest,
    ) -> RippleResponse {
        // find if its event
        let method = if request.rpc.is_subscription() {
            Some(format!(
                "{}.{}",
                request.rpc.ctx.call_id, request.rpc.ctx.method
            ))
        } else {
            None
        };
        let parse_result = serde_json::from_slice::<Value>(data);
        debug!("parse result {:?}", parse_result);
        if parse_result.is_err() {
            return Err(RippleError::ParseError);
        }
        let result = Some(parse_result.unwrap());
        debug!("result {:?}", result);
        // build JsonRpcApiResponse
        let data = JsonRpcApiResponse {
            jsonrpc: "2.0".to_owned(),
            id: Some(request.rpc.ctx.call_id),
            method,
            result,
            error: None,
            params: None,
        };
        BrokerOutputForwarder::send_json_rpc_response_to_broker(data, callback.clone());
        Ok(())
    }
    pub fn send_json_rpc_response_to_broker(
        json_rpc_api_response: JsonRpcApiResponse,
        callback: BrokerCallback,
    ) {
        tokio::spawn(async move {
            callback
                .sender
                .send(BrokerOutput::new(json_rpc_api_response))
                .await
        });
    }
    pub fn send_json_rpc_success_response_to_broker(
        json_rpc_api_success_response: JsonRpcApiResponse,
        callback: BrokerCallback,
    ) {
        tokio::spawn(async move {
            callback
                .sender
                .send(BrokerOutput::new(json_rpc_api_success_response))
                .await
        });
    }
}

async fn forward_extn_event(
    extn_message: &ExtnMessage,
    v: JsonRpcApiResponse,
    platform_state: &PlatformState,
) {
    if let Ok(event) = extn_message.get_event(ExtnEvent::Value(serde_json::to_value(v).unwrap())) {
        if let Err(e) = platform_state
            .get_client()
            .get_extn_client()
            .send_message(event)
            .await
        {
            error!("couldnt send back event {:?}", e)
        }
    }
}

pub fn apply_response(
    result_response_filter: String,
    method: &str,
    response: &mut JsonRpcApiResponse,
) {
    match serde_json::to_value(response.clone()) {
        Ok(input) => {
            match jq_compile(
                input,
                &result_response_filter,
                format!("{}_response", method),
            ) {
                Ok(jq_out) => {
                    trace!(
                        "jq rendered output {:?} original input {:?} for filter {}",
                        jq_out,
                        response,
                        result_response_filter
                    );

                    if jq_out.is_object() && jq_out.get("error").is_some() {
                        response.error = Some(jq_out.get("error").unwrap().clone());
                        response.result = None;
                    } else {
                        response.result = Some(jq_out);
                        response.error = None;
                    }
                    trace!("mutated response {:?}", response);
                }
                Err(e) => {
                    response.error = Some(json!(e.to_string()));
                    error!("jq_compile error {:?}", e);
                }
            }
        }
        Err(e) => {
            response.error = Some(json!(e.to_string()));
            error!("json rpc response error {:?}", e);
        }
    }
}

pub fn apply_rule_for_event(
    broker_request: &BrokerRequest,
    result: &Value,
    rpc_request: &RpcRequest,
    filter: &str,
    response: &mut JsonRpcApiResponse,
) {
    if let Ok(r) = jq_compile(
        result.clone(),
        filter,
        format!("{}_event", rpc_request.ctx.method),
    ) {
        LogSignal::new(
            "apply_rule_for_event".to_string(),
            "broker request found".to_string(),
            broker_request.clone(),
        )
        .with_diagnostic_context_item("success", "true")
        .with_diagnostic_context_item("result", r.to_string().as_str())
        .emit_debug();
        response.result = Some(r);
    } else {
        LogSignal::new(
            "apply_rule_for_event".to_string(),
            "broker request found".to_string(),
            broker_request.clone(),
        )
        .with_diagnostic_context_item("success", "false")
        .emit_debug();
    }
}

fn apply_filter(broker_request: &BrokerRequest, result: &Value, rpc_request: &RpcRequest) -> bool {
    if let Some(filter) = broker_request.rule.filter.clone() {
        if let Ok(r) = jq_compile(
            result.clone(),
            &filter,
            format!("{}_event filter", rpc_request.ctx.method),
        ) {
            if r.is_null() {
                return false;
            } else {
                // get bool value for r and return
                return r.as_bool().unwrap();
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::rules_engine::RuleTransform;
    use ripple_sdk::{tokio::sync::mpsc::channel, Mockable};

    #[tokio::test]
    async fn test_send_error() {
        let (tx, mut tr) = channel(2);
        let callback = BrokerCallback { sender: tx };

        callback
            .send_error(
                BrokerRequest {
                    rpc: RpcRequest::mock(),
                    rule: Rule {
                        alias: "somecallsign.method".to_owned(),
                        transform: RuleTransform::default(),
                        endpoint: None,
                        filter: None,
                        event_handler: None,
                        sources: None,
                    },
                    subscription_processed: None,
                    workflow_callback: None,
                    telemetry_response_listeners: vec![],
                },
                RippleError::InvalidInput,
            )
            .await;
        let value = tr.recv().await.unwrap();
        assert!(value.data.error.is_some())
    }

    mod broker_output {
        use ripple_sdk::{api::gateway::rpc_gateway_api::JsonRpcApiResponse, Mockable};

        use crate::broker::endpoint_broker::BrokerOutput;

        #[test]
        fn test_result() {
            let mut data = JsonRpcApiResponse::mock();
            let mut output = BrokerOutput::default();
            let output = output.with_jsonrpc_response(data.clone());
            assert!(!output.is_result());
            data.result = Some(serde_json::Value::Null);
            let mut output = BrokerOutput::default();
            let output = output.with_jsonrpc_response(data);
            assert!(output.is_result());
        }

        #[test]
        fn test_get_event() {
            let mut data = JsonRpcApiResponse::mock();
            data.method = Some("20.events".to_owned());
            let mut output = BrokerOutput::default();
            let output = output.with_jsonrpc_response(data);
            assert_eq!(20, output.get_event().unwrap())
        }
    }

    mod endpoint_broker_state {
        use ripple_sdk::{
            api::gateway::rpc_gateway_api::RpcRequest, tokio, tokio::sync::mpsc::channel, Mockable,
        };

        use crate::{
            broker::{
                endpoint_broker::tests::RippleClient,
                rules_engine::{Rule, RuleEngine, RuleSet, RuleTransform},
            },
            state::{bootstrap_state::ChannelsState, metrics_state::MetricsState},
        };

        use super::EndpointBrokerState;

        #[tokio::test]
        async fn get_request() {
            let (tx, _) = channel(2);
            let client = RippleClient::new(ChannelsState::new());
            let state = EndpointBrokerState::new(
                MetricsState::default(),
                tx,
                RuleEngine {
                    rules: RuleSet::default(),
                },
                client,
            );
            let mut request = RpcRequest::mock();
            state.update_request(
                &request,
                Rule {
                    alias: "somecallsign.method".to_owned(),
                    transform: RuleTransform::default(),
                    endpoint: None,
                    filter: None,
                    event_handler: None,
                    sources: None,
                },
                None,
                None,
                vec![],
            );
            request.ctx.call_id = 2;
            state.update_request(
                &request,
                Rule {
                    alias: "somecallsign.method".to_owned(),
                    transform: RuleTransform::default(),
                    endpoint: None,
                    filter: None,
                    event_handler: None,
                    sources: None,
                },
                None,
                None,
                vec![],
            );

            // Hardcoding the id here will be a problem as multiple tests uses the atomic id and there is no guarantee
            // that this test case would always be the first one to run
            // Revisit this test case, to make it more robust
            // assert!(state.get_request(2).is_ok());
            // assert!(state.get_request(1).is_ok());
        }
    }

    #[tokio::test]
    async fn test_apply_response_contains_error() {
        let error = json!({"code":-32601,"message":"The service is in an illegal state!!!."});
        let ctx = CallContext::new(
            "session_id".to_string(),
            "request_id".to_string(),
            "app_id".to_string(),
            1,
            ApiProtocol::Bridge,
            "method".to_string(),
            Some("cid".to_string()),
            true,
        );
        let rpc_request = RpcRequest::new("new_method".to_string(), "params".to_string(), ctx);
        let mut data = JsonRpcApiResponse::mock();
        data.error = Some(error);
        let mut output: BrokerOutput = BrokerOutput::new(data.clone());
        let filter = "if .result and .result.success then (.result.stbVersion | split(\"_\") [0]) elif .error then if .error.code == -32601 then {error: { code: -1, message: \"Unknown method.\" }} else \"Error occurred with a different code\" end else \"No result or recognizable error\" end".to_string();
        //let mut response = JsonRpcApiResponse::mock();
        //response.error = Some(error);
        apply_response(filter, &rpc_request.ctx.method, &mut output.data);
        //let msg = output.data.error.unwrap().get("message").unwrap().clone();
        assert_eq!(
            output.data.error.unwrap().get("message").unwrap().clone(),
            json!("Unknown method.".to_string())
        );

        // securestorage.get code 22 in error response
        let error = json!({"code":22,"message":"test error code 22"});
        let mut data = JsonRpcApiResponse::mock();
        data.error = Some(error);
        let mut output: BrokerOutput = BrokerOutput::new(data);
        let filter = "if .result and .result.success then .result.value elif .error.code==22 or .error.code==43 then null else .error end".to_string();

        apply_response(filter, &rpc_request.ctx.method, &mut output.data);
        assert_eq!(output.data.error, None);
        assert_eq!(output.data.result.unwrap(), serde_json::Value::Null);

        // securestorage.get code other than 22 or 43 in error response
        let error = json!({"code":300,"message":"test error code 300"});
        let mut data = JsonRpcApiResponse::mock();
        data.error = Some(error.clone());
        let mut output: BrokerOutput = BrokerOutput::new(data);
        let filter = "if .result and .result.success then .result.value elif .error.code==22 or .error.code==43 then null else { error: .error } end".to_string();
        apply_response(filter, &rpc_request.ctx.method, &mut output.data);
        assert_eq!(output.data.error, Some(error));
    }

    #[tokio::test]
    async fn test_apply_response_contains_result() {
        // mock test
        let ctx = CallContext::new(
            "session_id".to_string(),
            "request_id".to_string(),
            "app_id".to_string(),
            1,
            ApiProtocol::Bridge,
            "method".to_string(),
            Some("cid".to_string()),
            true,
        );
        let rpc_request = RpcRequest::new("new_method".to_string(), "params".to_string(), ctx);

        // device.sku
        let filter = "if .result and .result.success then (.result.stbVersion | split(\"_\") [0]) elif .error then if .error.code == -32601 then {\"error\":\"Unknown method.\"} else \"Error occurred with a different code\" end else \"No result or recognizable error\" end".to_string();
        //let mut response = JsonRpcApiResponse::mock();
        let result = json!({"stbVersion":"SCXI11BEI_VBN_24Q3_sprint_20240717150752sdy_FG","receiverVersion":"7.6.0.0","stbTimestamp":"Wed 17 Jul 2024 15:07:52 UTC","success":true});
        //response.result = Some(result);
        let mut data = JsonRpcApiResponse::mock();
        data.result = Some(result);
        let mut output: BrokerOutput = BrokerOutput::new(data.clone());
        apply_response(filter, &rpc_request.ctx.method, &mut output.data);
        assert_eq!(output.data.result.unwrap(), "SCXI11BEI".to_string());

        // device.videoResolution
        let result = json!("Resolution1080P");
        let filter = "if .result then if .result | contains(\"480\") then ( [640, 480] ) elif .result | contains(\"576\") then ( [720, 576] ) elif .result | contains(\"1080\") then ( [1920, 1080] ) elif .result | contains(\"2160\") then ( [2160, 1440] ) end elif .error then if .error.code == -32601 then \"Unknown method.\" else \"Error occurred with a different code\" end else \"No result or recognizable error\" end".to_string();
        let mut response = JsonRpcApiResponse::mock();
        response.result = Some(result);
        apply_response(filter, &rpc_request.ctx.method, &mut response);
        assert_eq!(response.result.unwrap(), json!([1920, 1080]));

        // device.audio
        let result = json!({"currentAudioFormat":"DOLBY AC3","supportedAudioFormat":["NONE","PCM","AAC","VORBIS","WMA","DOLBY AC3","DOLBY AC4","DOLBY MAT","DOLBY TRUEHD","DOLBY EAC3 ATMOS","DOLBY TRUEHD ATMOS","DOLBY MAT ATMOS","DOLBY AC4 ATMOS","UNKNOWN"],"success":true});
        let filter = "if .result and .result.success then .result | {\"stereo\": (.supportedAudioFormat |  index(\"PCM\") > 0),\"dolbyDigital5.1\": (.supportedAudioFormat |  index(\"DOLBY AC3\") > 0),\"dolbyDigital5.1plus\": (.supportedAudioFormat |  index(\"DOLBY EAC3\") > 0),\"dolbyAtmos\": (.supportedAudioFormat |  index(\"DOLBY EAC3 ATMOS\") > 0)} elif .error then if .error.code == -32601 then \"Unknown method.\" else \"Error occurred with a different code\" end else \"No result or recognizable error\" end".to_string();
        let mut response = JsonRpcApiResponse::mock();
        response.result = Some(result);
        apply_response(filter, &rpc_request.ctx.method, &mut response);
        assert_eq!(
            response.result.unwrap(),
            json!({"dolbyAtmos": true, "dolbyDigital5.1": true, "dolbyDigital5.1plus": false, "stereo": true})
        );

        // device.network
        let result = json!({"interfaces":[{"interface":"ETHERNET","macAddress":
        "f0:46:3b:5b:eb:14","enabled":true,"connected":false},{"interface":"WIFI","macAddress
        ":"f0:46:3b:5b:eb:15","enabled":true,"connected":true}],"success":true});

        let filter = "if .result and .result.success then (.result.interfaces | .[] | select(.connected) | {\"state\": \"connected\",\"type\": .interface | ascii_downcase }) elif .error then if .error.code == -32601 then \"Unknown method.\" else \"Error occurred with a different code\" end else \"No result or recognizable error\" end".to_string();
        let mut response = JsonRpcApiResponse::mock();
        response.result = Some(result);
        apply_response(filter, &rpc_request.ctx.method, &mut response);
        assert_eq!(
            response.result.unwrap(),
            json!({"state":"connected", "type":"wifi"})
        );

        // device.name
        let result = json!({"friendlyName": "my_device","success":true});
        let filter = "if .result.success then (if .result.friendlyName | length == 0 then \"Living Room\" else .result.friendlyName end) else \"Living Room\" end".to_string();
        let mut response = JsonRpcApiResponse::mock();
        response.result = Some(result);
        apply_response(filter, &rpc_request.ctx.method, &mut response);
        assert_eq!(response.result.unwrap(), json!("my_device"));

        // localization.language
        let result = json!({"success": true, "value": "{\"update_time\":\"2024-07-29T20:23:29.539132160Z\",\"value\":\"FR\"}"});
        let filter = "if .result.success then (.result.value | fromjson | .value) else \"en\" end"
            .to_string();
        let mut response = JsonRpcApiResponse::mock();
        response.result = Some(result);
        apply_response(filter, &rpc_request.ctx.method, &mut response);

        assert_eq!(response.result.unwrap(), json!("FR"));

        // secondscreen.friendlyName
        let result = json!({"friendlyName": "my_device","success":true});
        let filter = "if .result.success then (if .result.friendlyName | length == 0 then \"Living Room\" else .result.friendlyName end) else \"Living Room\" end".to_string();
        let mut response = JsonRpcApiResponse::mock();
        response.result = Some(result);
        apply_response(filter, &rpc_request.ctx.method, &mut response);

        assert_eq!(response.result.unwrap(), json!("my_device"));

        // advertising.setSkipRestriction
        let result = json!({"success":true});
        let filter = "if .result.success then null else { code: -32100, message: \"couldn't set skip restriction\" } end".to_string();
        let mut response = JsonRpcApiResponse::mock();
        response.result = Some(result);
        apply_response(filter, &rpc_request.ctx.method, &mut response);

        assert_eq!(response.result.unwrap(), serde_json::Value::Null);

        // securestorage.get
        let result = json!({"value": "some_value","success": true,"ttl": 100});
        let filter = "if .result.success then .result.value elif .error.code==22 or .error.code==43 then \"null\" else .error end".to_string();
        let mut response = JsonRpcApiResponse::mock();
        response.result = Some(result);
        apply_response(filter, &rpc_request.ctx.method, &mut response);
        assert_eq!(response.result.unwrap(), "some_value");

        // localization.countryCode
        let result = json!({"territory": "USA","success": true});
        let filter = "if .result.success then if .result.territory == \"ITA\" then \"IT\" elif .result.territory == \"GBR\" then \"GB\" elif .result.territory == \"IRL\" then \"IE\" elif .result.territory == \"DEU\" then \"DE\" elif .result.territory == \"AUS\" then \"AU\" else \"GB\" end end".to_string();
        let mut response = JsonRpcApiResponse::mock();
        response.result = Some(result);
        apply_response(filter, &rpc_request.ctx.method, &mut response);
        assert_eq!(response.result.unwrap(), "GB");
    }
}
