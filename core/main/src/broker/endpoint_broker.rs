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

use futures::executor::block_on;
use ripple_sdk::{
    api::{
        firebolt::fb_capabilities::JSON_RPC_STANDARD_ERROR_INVALID_PARAMS,
        gateway::rpc_gateway_api::{
            ApiMessage, ApiProtocol, CallContext, JsonRpcApiResponse, RpcRequest,
        },
        session::AccountSession,
    },
    extn::extn_client_message::{ExtnEvent, ExtnMessage},
    framework::RippleResponse,
    log::{debug, error, info},
    semver::Op,
    tokio::{
        self,
        sync::mpsc::{self, Receiver, Sender},
    },
    utils::error::RippleError,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, MutexGuard, RwLock,
    },
};

use crate::{
    firebolt::firebolt_gateway::{FireboltGatewayCommand, JsonRpcError},
    service::extn::ripple_client::RippleClient,
    state::platform_state::PlatformState,
    utils::router_utils::{return_api_message_for_transport, return_extn_response},
};

use super::{
    http_broker::HttpBroker,
    rules_engine::{jq_compile, JqError, Rule, RuleEndpoint, RuleEndpointProtocol, RuleEngine},
    thunder_broker::ThunderBroker,
    websocket_broker::WebsocketBroker,
};

#[derive(Clone, Debug)]
pub struct BrokerSender {
    pub sender: Sender<BrokerRequest>,
}

#[derive(Clone, Debug)]
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

#[derive(Clone, Debug)]
pub struct BrokerRequest {
    pub rpc: RpcRequest,
    pub rule: Rule,
    pub subscription_processed: Option<bool>,
}
impl Default for BrokerRequest {
    fn default() -> Self {
        Self {
            rpc: RpcRequest::default(),
            rule: Rule::default(),
            subscription_processed: None,
        }
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
    pub fn new(rpc_request: &RpcRequest, rule: Rule) -> BrokerRequest {
        BrokerRequest {
            rpc: rpc_request.clone(),
            rule,
            subscription_processed: None,
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
        let output = BrokerOutput { data };
        if let Err(e) = self.sender.send(output).await {
            error!("couldnt send error for {:?}", e);
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BrokerContext {
    pub app_id: String,
}

#[derive(Debug, Clone)]
pub struct BrokerOutput {
    pub data: JsonRpcApiResponse,
}
impl Default for BrokerOutput {
    fn default() -> Self {
        Self {
            data: JsonRpcApiResponse {
                jsonrpc: "2.0".to_owned(),
                id: None,
                method: None,
                result: None,
                error: None,
                params: None,
            },
        }
    }
}

pub fn get_event_id_from_method(method: Option<String>) -> Option<u64> {
    method.and_then(|m| {
        let event: Vec<&str> = m.split('.').collect();
        event.first().and_then(|v| v.parse::<u64>().ok())
    })
}
pub fn is_event(method: Option<String>) -> bool {
    get_event_id_from_method(method).is_some()
}
impl BrokerOutput {
    pub fn is_result(&self) -> bool {
        self.data.result.is_some()
    }

    pub fn get_event(&self) -> Option<u64> {
        get_event_id_from_method(self.data.method.clone())
    }
    pub fn is_event(&self) -> bool {
        is_event(self.data.method.clone())
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
        }
    }
}

impl EndpointBrokerState {
    pub fn new(
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
        };
        state.reconnect_thread(rec_tr, ripple_client);
        state
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
                    state.build_endpoint(v)
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
    ) -> BrokerRequest {
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
                },
            );
        }

        if extn_message.is_some() {
            let mut extn_map = self.extension_request_map.write().unwrap();
            let _ = extn_map.insert(id, extn_message.unwrap());
        }

        rpc_request_c.ctx.call_id = id;
        BrokerRequest::new(&rpc_request_c, rule)
    }

    pub fn build_thunder_endpoint(&mut self) {
        if let Some(endpoint) = self.rule_engine.rules.endpoints.get("thunder").cloned() {
            let request = BrokerConnectRequest::new(
                "thunder".to_owned(),
                endpoint.clone(),
                self.reconnect_tx.clone(),
            );
            self.build_endpoint(request);
        }
    }

    pub fn build_other_endpoints(&mut self, session: Option<AccountSession>) {
        for (key, endpoint) in self.rule_engine.rules.endpoints.clone() {
            let request = BrokerConnectRequest::new_with_sesssion(
                key,
                endpoint.clone(),
                self.reconnect_tx.clone(),
                session.clone(),
            );
            self.build_endpoint(request);
        }
    }

    fn build_endpoint(&mut self, request: BrokerConnectRequest) {
        let endpoint = request.endpoint.clone();
        let key = request.key.clone();
        let (broker, cleaner) = match endpoint.protocol {
            RuleEndpointProtocol::Http => (
                HttpBroker::get_broker(request, self.callback.clone()).get_sender(),
                None,
            ),
            RuleEndpointProtocol::Websocket => {
                let ws_broker = WebsocketBroker::get_broker(request, self.callback.clone());
                (ws_broker.get_sender(), Some(ws_broker.get_cleaner()))
            }
            RuleEndpointProtocol::Thunder => {
                let thunder_broker = ThunderBroker::get_broker(request, self.callback.clone());
                (
                    thunder_broker.get_sender(),
                    Some(thunder_broker.get_cleaner()),
                )
            }
        };

        {
            let mut endpoint_map = self.endpoint_map.write().unwrap();
            endpoint_map.insert(key, broker);
        }

        if let Some(cleaner) = cleaner {
            let mut cleaner_list = self.cleaner_list.write().unwrap();
            cleaner_list.push(cleaner);
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
    ) -> bool {
        let callback = self.callback.clone();
        let mut broker_sender = None;
        let mut found_rule = None;
        if let Some(rule) = self.rule_engine.get_rule(&rpc_request) {
            let _ = found_rule.insert(rule.clone());
            if let Some(endpoint) = rule.endpoint {
                if let Some(endpoint) = self.get_sender(&endpoint) {
                    let _ = broker_sender.insert(endpoint);
                }
            } else if let Some(endpoint) = self.get_sender("thunder") {
                let _ = broker_sender.insert(endpoint);
            }
        }

        if broker_sender.is_none() || found_rule.is_none() {
            return false;
        }
        let rule = found_rule.unwrap();
        let broker = broker_sender.unwrap();
        let updated_request = self.update_request(&rpc_request, rule, extn_message);
        tokio::spawn(async move {
            if let Err(e) = broker.send(updated_request.clone()).await {
                // send some rpc error
                callback.send_error(updated_request, e).await
            }
        });
        true
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

    // Get Broker Request from rpc_request
    pub fn get_broker_request(&self, rpc_request: &RpcRequest, rule: Rule) -> BrokerRequest {
        BrokerRequest {
            rpc: rpc_request.clone(),
            rule,
            subscription_processed: None,
        }
    }
}

/// Trait which contains all the abstract methods for a Endpoint Broker
/// There could be Websocket or HTTP protocol implementations of the given trait
pub trait EndpointBroker {
    fn get_broker(request: BrokerConnectRequest, callback: BrokerCallback) -> Self;

    fn get_sender(&self) -> BrokerSender;

    fn prepare_request(&self, rpc_request: &BrokerRequest) -> Result<Vec<String>, RippleError> {
        let response = Self::update_request(rpc_request)?;
        Ok(vec![response])
    }

    /// Adds BrokerContext to a given request used by the Broker Implementations
    /// just before sending the data through the protocol
    fn update_request(rpc_request: &BrokerRequest) -> Result<String, JqError> {
        let v = Self::apply_request_rule(rpc_request)?;
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
    fn apply_request_rule(rpc_request: &BrokerRequest) -> Result<Value, JqError> {
        if let Ok(mut params) = serde_json::from_str::<Vec<Value>>(&rpc_request.rpc.params_json) {
            if params.len() > 1 {
                if let Some(last) = params.pop() {
                    if let Some(filter) = rpc_request
                        .rule
                        .transform
                        .get_filter(super::rules_engine::RuleTransformType::Request)
                    {
                        return jq_compile(
                            last,
                            &filter,
                            format!("{}_request", rpc_request.rpc.ctx.method),
                        );
                    }
                    return Ok(serde_json::to_value(&last).unwrap());
                }
            } else {
                return Ok(Value::Null);
            }
        }
        Err(RippleError::ParseError.into())
    }

    /// Default handler method for the broker to remove the context and send it back to the
    /// client for consumption
    fn handle_jsonrpc_response(result: &[u8], callback: BrokerCallback) {
        let mut final_result = Err(RippleError::ParseError);
        if let Ok(data) = serde_json::from_slice::<JsonRpcApiResponse>(result) {
            final_result = Ok(BrokerOutput { data });
        }
        if let Ok(output) = final_result {
            tokio::spawn(async move { callback.sender.send(output).await });
        } else {
            error!("Bad broker response {}", String::from_utf8_lossy(result));
        }
    }

    fn get_cleaner(&self) -> BrokerCleaner;
}

/// Forwarder gets the BrokerOutput and forwards the response to the gateway.
pub struct BrokerOutputForwarder;
pub fn get_event_id(broker_output: BrokerOutput) -> Option<u64> {
    broker_output.get_event().or_else(|| broker_output.data.id)
}

#[derive(Debug, Clone)]
pub enum BrokerWorkFlowError {
    MissingValue,
    NoRuleFound,
    JqError(JqError),
    JsonParseError,
}

#[derive(Debug, Clone)]
pub struct SessionizedApiMessage {
    pub session_id: String,
    pub api_message: ApiMessage,
}
#[derive(Debug, Clone)]
pub enum BrokerWorkflowSuccess {
    SubscriptionProcessed(BrokerOutput, Option<u64>),
    Unsubcribe(BrokerOutput, Option<u64>),
    RuleAppliedToEvent(BrokerOutput, Option<u64>),
    FilterApplied(BrokerOutput, Option<u64>),
}
impl From<JqError> for BrokerWorkFlowError {
    fn from(e: JqError) -> Self {
        BrokerWorkFlowError::JqError(e)
    }
}
// impl From<BrokerOutput> for BrokerWorkflowSuccess {
//     fn from(e: BrokerOutput) -> Self {
//         BrokerWorkflowSuccess::FilterApplied(e)
//     }
// }

/*

Factor out broker workflow from tokio loop
*/
pub fn run_broker_workflow(
    broker_output: &BrokerOutput,
    broker_request: &BrokerRequest,
) -> Result<BrokerWorkflowSuccess, BrokerWorkFlowError> {
    let sub_processed = broker_request.is_subscription_processed();
    let rpc_request = broker_request.rpc.clone();
    let is_subscription = rpc_request.is_subscription();
    let is_event = is_event(broker_output.data.method.clone());
    let id = get_event_id(broker_output.clone());
    let request_id = rpc_request.ctx.call_id;
    println!("request={:?}", broker_output);
    if let Some(result) = broker_output.data.result.clone() {
        println!("here={:?}", result);
        let mut mutant = broker_output.clone();
        mutant.data.id = Some(request_id);
        if is_event {
            let f = apply_rule_for_event(&broker_request, &result, &rpc_request, &broker_output)?;
            return Ok(BrokerWorkflowSuccess::RuleAppliedToEvent(f, id));
        } else if is_subscription {
            if sub_processed {
                return Ok(BrokerWorkflowSuccess::SubscriptionProcessed(mutant, id));
            }
            mutant.data.result = Some(json!({
                "listening" : rpc_request.is_listening(),
                "event" : rpc_request.ctx.method
            }));
            return Ok(BrokerWorkflowSuccess::Unsubcribe(mutant, id));
            //platform_state.endpoint_state.update_unsubscribe_request(id);
        } else if let Some(filter) = broker_request
            .rule
            .transform
            .get_filter(super::rules_engine::RuleTransformType::Response)
        {
            // let broker_output = apply_response(result, filter, &rpc_request, broker_output)?;
            return Ok(BrokerWorkflowSuccess::FilterApplied(
                apply_response(result, filter, &rpc_request, broker_output)?,
                id,
            ));
        } else {
            return Err(BrokerWorkFlowError::NoRuleFound);
        }
    } else {
        return Err(BrokerWorkFlowError::JsonParseError);
    }
}

pub fn brokered_to_api_message_response(
    broker_output: BrokerOutput,
    broker_request: &BrokerRequest,
    request_id: String,
) -> Result<ApiMessage, BrokerWorkFlowError> {
    match serde_json::to_string(&broker_output.data) {
        Ok(jsonrpc_msg) => Ok(ApiMessage {
            request_id: request_id, //broker_request.rpc.ctx.call_id.to_string(),
            protocol: broker_request.rpc.ctx.protocol.clone(),
            jsonrpc_msg,
        }),
        Err(_) => Err(BrokerWorkFlowError::JsonParseError),
    }
}
pub fn get_request_id(broker_request: &BrokerRequest, request_id: Option<u64>) -> String {
    request_id
        .map(|v| v.to_string())
        .unwrap_or_else(|| broker_request.rpc.ctx.call_id.to_string())
}
pub fn broker_workflow(
    broker_output: &BrokerOutput,
    broker_request: &BrokerRequest,
) -> Result<SessionizedApiMessage, BrokerWorkFlowError> {
    match run_broker_workflow(broker_output, broker_request)? {
        BrokerWorkflowSuccess::SubscriptionProcessed(broker_output, request_id) => {
            Ok(SessionizedApiMessage {
                session_id: broker_request.rpc.ctx.get_id(),
                api_message: brokered_to_api_message_response(
                    broker_output,
                    broker_request,
                    broker_request.rpc.ctx.request_id.clone(),
                )?,
            })
        }
        BrokerWorkflowSuccess::Unsubcribe(broker_output, request_id) => Ok(SessionizedApiMessage {
            session_id: broker_request.rpc.ctx.get_id(),
            api_message: brokered_to_api_message_response(
                broker_output,
                broker_request,
                broker_request.rpc.ctx.request_id.clone(),
            )?,
        }),
        BrokerWorkflowSuccess::RuleAppliedToEvent(broker_output, _) => Ok(SessionizedApiMessage {
            session_id: broker_request.rpc.ctx.get_id(),
            api_message: brokered_to_api_message_response(
                broker_output,
                broker_request,
                broker_request.rpc.ctx.request_id.clone(),
            )?,
        }),
        BrokerWorkflowSuccess::FilterApplied(broker_output, _) => Ok(SessionizedApiMessage {
            session_id: broker_request.rpc.ctx.get_id(),
            api_message: brokered_to_api_message_response(
                broker_output,
                broker_request,
                broker_request.rpc.ctx.request_id.clone(),
            )?,
        }),
    }
}
impl BrokerOutputForwarder {
    pub fn start_forwarder(platform_state: PlatformState, mut rx: Receiver<BrokerOutput>) {
        tokio::spawn(async move {
            while let Some(mut broker_output) = rx.recv().await {
                if let Some(request_id) = get_event_id(broker_output.clone()) {
                    if let Ok(broker_request) =
                        platform_state.endpoint_state.get_request(request_id)
                    {
                        info!("processing request {:?}", request_id);

                        match broker_workflow(&broker_output, &broker_request) {
                            Ok(message) => {
                                let session_id = message.session_id;
                                let is_event = is_event(broker_output.data.method.clone());
                                //let session_id = get_request_id(&broker_request, None);
                                info!(
                                    "processing request id={} for session_id={:?}",
                                    request_id, session_id
                                );
                                if matches!(message.api_message.protocol, ApiProtocol::Extn) {
                                    if let Ok(extn_message) = platform_state
                                        .endpoint_state
                                        .get_extn_message(request_id, is_event)
                                    {
                                        if is_event {
                                            forward_extn_event(
                                                &extn_message,
                                                broker_output.data,
                                                &platform_state,
                                            )
                                            .await;
                                        } else {
                                            return_extn_response(message.api_message, extn_message)
                                        }
                                    }
                                } else if let Some(session) = platform_state
                                    .session_state
                                    .get_session_for_connection_id(&session_id)
                                {
                                    return_api_message_for_transport(
                                        session,
                                        message.api_message,
                                        platform_state.clone(),
                                    )
                                    .await
                                }
                            }
                            Err(e) => {
                                error!("Error couldnt broker the event {:?}", e)
                                /*
                                TODO - who do we tell about this?
                                */
                            }
                        }
                    }
                }
            }
        });
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
        if parse_result.is_err() {
            return Err(RippleError::ParseError);
        }
        let result = Some(parse_result.unwrap());
        // build JsonRpcApiResponse
        let data = JsonRpcApiResponse {
            jsonrpc: "2.0".to_owned(),
            id: Some(request.rpc.ctx.call_id),
            method,
            result,
            error: None,
            params: None,
        };
        let output = BrokerOutput { data };
        tokio::spawn(async move { callback.sender.send(output).await });
        Ok(())
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

//fn apply_response(
//     result: Value,
//     filter: String,
//     rpc_request: &RpcRequest,
//     broker_output: &BrokerOutput,
// ) -> Result<BrokerOutput, JqError> {
//     match jq_compile(
//         result.clone(),
//         &filter,
//         format!("{}_response", rpc_request.ctx.method),
//     ) {
//         Ok(r) => {
//             let mut mutant = broker_output.clone();
//             if r.to_string().to_lowercase().contains("null") {
//                 mutant.data.result = Some(Value::Null)
//             } else if result.get("success").is_some() {
//                 mutant.data.result = Some(r);
//                 mutant.data.error = None;
//             } else {
//                 mutant.data.error = Some(r);
//                 mutant.data.result = None;
//             }
//             Ok(mutant)
//         }
//         Err(e) => {
//             error!("jq_compile error {:?}", e);
//             Err(e)
//         }
//     }
// }
fn apply_response(
    raw_value: Value,
    filter: String,
    rpc_request: &RpcRequest,
    broker_output: &BrokerOutput,
) -> Result<BrokerOutput, JqError> {
    match jq_compile(
        raw_value.clone(),
        &filter,
        format!("{}_response", rpc_request.ctx.method),
    ) {
        Ok(compilation_result) => {
            let mut mutant = broker_output.clone();
            debug!(
                "jq_compile result {:?} for {}",
                compilation_result, raw_value
            );
            if compilation_result == Value::Null {
                error!(
                    "error processing: {} from {}",
                    compilation_result, raw_value
                );
                mutant.data.error = Some(Value::from(false));
                mutant.data.result = Some(Value::Null);
            } else if compilation_result.get("success").is_some() {
                mutant.data.result = Some(compilation_result.clone());
                mutant.data.error = match compilation_result.get("success") {
                    Some(v) => {
                        if v.is_boolean() {
                            if !v.as_bool().unwrap() {
                                Some(Value::from(false))
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    }
                    None => None,
                };
            } else if raw_value.get("success").is_some() {
                mutant.data.result = Some(compilation_result);
                mutant.data.error = match raw_value.get("success") {
                    Some(v) => {
                        if v.is_boolean() {
                            if !v.as_bool().unwrap() {
                                Some(Value::from(false))
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    }
                    None => None,
                }
            } else {
                mutant.data.error = None;
                mutant.data.result = Some(compilation_result);
            }
            Ok(mutant)
        }
        Err(e) => {
            error!("jq_compile error {:?}", e);
            Err(JqError::RuleParseFailed)
        }
    }
}
fn apply_rule_for_event(
    broker_request: &BrokerRequest,
    result: &Value,
    rpc_request: &RpcRequest,
    broker_output: &BrokerOutput,
) -> Result<BrokerOutput, JqError> {
    if let Some(filter) = broker_request
        .rule
        .transform
        .get_filter(super::rules_engine::RuleTransformType::Event)
    {
        let data = jq_compile(
            result.clone(),
            &filter,
            format!("{}_event", rpc_request.ctx.method),
        )?;
        let mut mutated_broker_output = broker_output.clone();
        mutated_broker_output.data.result = Some(data);
        Ok(mutated_broker_output.clone())
    } else {
        return Err(JqError::RuleNotFound(rpc_request.ctx.method.clone()));
    }
}

#[cfg(test)]
mod tests {
    use ripple_sdk::{tokio::sync::mpsc::channel, Mockable};

    use crate::broker::rules_engine::RuleTransform;

    use super::*;

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
                    },
                    subscription_processed: None,
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
            let output = BrokerOutput { data: data.clone() };
            assert!(!output.is_result());
            data.result = Some(serde_json::Value::Null);
            let output = BrokerOutput { data };
            assert!(output.is_result());
        }

        #[test]
        fn test_get_event() {
            let mut data = JsonRpcApiResponse::mock();
            data.method = Some("20.events".to_owned());
            let output = BrokerOutput { data };
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
            state::bootstrap_state::ChannelsState,
        };

        use super::EndpointBrokerState;

        use ripple_sdk::api::gateway::rpc_gateway_api::JsonRpcApiResponse;

        #[tokio::test]
        async fn get_request() {
            let (tx, _) = channel(2);
            let client = RippleClient::new(ChannelsState::new());
            let state = EndpointBrokerState::new(
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
                },
                None,
            );
            request.ctx.call_id = 2;
            state.update_request(
                &request,
                Rule {
                    alias: "somecallsign.method".to_owned(),
                    transform: RuleTransform::default(),
                    endpoint: None,
                },
                None,
            );

            // Hardcoding the id here will be a problem as multiple tests uses the atomic id and there is no guarantee
            // that this test case would always be the first one to run
            // Revisit this test case, to make it more robust
            // assert!(state.get_request(2).is_ok());
            // assert!(state.get_request(1).is_ok());
        }
    }
    /*add exhaustive unit tests for as many function as possible */
    #[cfg(test)]
    mod tests {

        use openrpc_validator::RpcResult;
        use serde_json::Number;

        use super::*;

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
                        },
                        subscription_processed: None,
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
                let output = BrokerOutput { data: data.clone() };
                assert!(!output.is_result());
                data.result = Some(serde_json::Value::Null);
                let output = BrokerOutput { data };
                assert!(output.is_result());
            }

            #[test]
            fn test_get_event() {
                let mut data = JsonRpcApiResponse::mock();
                data.method = Some("20.events".to_owned());
                let output = BrokerOutput { data };
                assert_eq!(20, output.get_event().unwrap())
            }
        }

        mod endpoint_broker_state {
            use super::*;
            use ripple_sdk::{
                api::gateway::rpc_gateway_api::RpcRequest, tokio, tokio::sync::mpsc::channel,
                Mockable,
            };

            use crate::{
                broker::{
                    endpoint_broker::{tests::RippleClient, EndpointBrokerState},
                    rules_engine::{Rule, RuleEngine, RuleSet, RuleTransform},
                },
                state::bootstrap_state::ChannelsState,
            };

            #[tokio::test]
            async fn get_request() {
                let (tx, _) = channel(2);
                let client = RippleClient::new(ChannelsState::new());
                let state = EndpointBrokerState::new(
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
                    },
                    None,
                );
                request.ctx.call_id = 2;
                state.update_request(
                    &request,
                    Rule {
                        alias: "somecallsign.method".to_owned(),
                        transform: RuleTransform::default(),
                        endpoint: None,
                    },
                    None,
                );

                // Hardcoding the id here will be a problem as multiple tests uses the atomic id and there is no guarantee
                // that this test case would always be the first one to run
                // Revisit this test case, to make it more robust
                // assert!(state.get_request(2).is_ok());
                // assert!(state.get_request(1).is_ok());
            }
        }

        #[test]
        fn test_run_broker_workflow() {
            let mut broker_request = BrokerRequest::default();
            broker_request.rule.transform.response = Some(".success".to_owned());
            let mut broker_output = BrokerOutput::default();
            let mut payload = JsonRpcApiResponse::default();
            payload.result = Some(serde_json::Value::Number(Number::from(1)));
            broker_output.data = payload;
            let result = run_broker_workflow(&broker_output, &broker_request);
            assert!(result.is_ok());
        }

        #[test]
        fn test_brokered_to_api_message_response() {
            let request_id = String::from("12345");
            let broker_request = BrokerRequest::default();
            let broker_output = BrokerOutput::default();
            let result =
                brokered_to_api_message_response(broker_output, &broker_request, request_id);
            assert!(result.is_ok());
        }

        #[test]
        fn test_get_request_id() {
            let broker_request = BrokerRequest::default();
            let request_id = get_request_id(&broker_request, None);
            assert!(!request_id.is_empty());
        }

        #[test]
        fn test_broker_workflow() {
            let mut broker_request = BrokerRequest::default();
            broker_request.rule.transform.response = Some(".success".to_owned());
            let mut broker_output = BrokerOutput::default();
            let mut payload = JsonRpcApiResponse::default();
            payload.result = Some(serde_json::Value::Number(Number::from(1)));
            broker_output.data = payload;
            let result = super::broker_workflow(&broker_output, &broker_request);
            assert!(result.is_ok());
        }

        // #[test]
        // fn test_start_forwarder() {
        //     let platform_state = PlatformState::default();
        //     let rx = Receiver::default();

        //     start_forwarder(platform_state, rx);
        //     // TODO: Add assertions or mock the tokio::spawn call to verify the behavior
        // }

        #[test]
        fn test_handle_non_jsonrpc_response() {
            let data: &[u8] = &[1, 2, 3];
            let callback = BrokerCallback::default();
            let request = BrokerRequest::default();
            let result =
                BrokerOutputForwarder::handle_non_jsonrpc_response(data, callback, request);
            assert!(result.is_err());
        }

        #[tokio::test]
        async fn test_forward_extn_event() {
            let extn_message = ExtnMessage::default();
            let v = JsonRpcApiResponse::default();
            let platform_state = PlatformState::default();
            forward_extn_event(&extn_message, v, &platform_state).await;
            // TODO: Add assertions or mock the platform_state.get_client().get_extn_client().send_message call to verify the behavior
        }

        #[test]
        fn test_apply_response() {
            let result = serde_json::json!({"success": true});
            let filter = String::from(".success");
            let rpc_request = RpcRequest::default();
            let broker_output = BrokerOutput::default();
            let result = apply_response(result, filter, &rpc_request, &broker_output);
            assert!(result.is_ok());
        }

        #[test]
        fn test_apply_rule_for_event() {
            let mut broker_request = BrokerRequest::default();
            broker_request.rule.transform.response = Some(".success".to_owned());
            let mut broker_output = BrokerOutput::default();
            let mut payload = JsonRpcApiResponse::default();
            payload.result = Some(serde_json::Value::Number(Number::from(1)));
            let result = json!({});
            let rpc_request = RpcRequest::default();
            broker_output.data = payload;
            broker_request.rule.transform.event = Some(".success".to_owned());
            let result =
                apply_rule_for_event(&broker_request, &result, &rpc_request, &broker_output);
            println!("result={:?}", result);
            assert!(result.is_ok());
        }
    }
}
