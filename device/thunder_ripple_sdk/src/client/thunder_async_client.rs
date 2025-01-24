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

use super::{
    device_operator::{DeviceChannelRequest, DeviceResponseMessage},
    thunder_async_client_plugins_status_mgr::{AsyncCallback, AsyncSender, StatusManager},
};
use crate::utils::get_next_id;
use futures::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use ripple_sdk::{
    api::gateway::rpc_gateway_api::JsonRpcApiResponse,
    log::{debug, error, info},
    tokio::{self, net::TcpStream, sync::mpsc::Receiver},
    utils::{error::RippleError, rpc_utils::extract_tcp_port},
};
use serde_json::json;
use std::time::Duration;
use tokio_tungstenite::{client_async, tungstenite::Message, WebSocketStream};

#[derive(Clone, Debug)]
pub struct ThunderAsyncClient {
    status_manager: StatusManager,
    sender: AsyncSender,
    callback: AsyncCallback,
}

#[derive(Clone, Debug)]
pub struct ThunderAsyncRequest {
    pub id: u64,
    request: DeviceChannelRequest,
}

impl ThunderAsyncRequest {
    pub fn new(request: DeviceChannelRequest) -> Self {
        Self {
            id: get_next_id(),
            request,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ThunderAsyncResponse {
    pub id: Option<u64>,
    pub result: Result<JsonRpcApiResponse, RippleError>,
}

impl ThunderAsyncClient {}

impl ThunderAsyncResponse {
    fn new_response(response: JsonRpcApiResponse) -> Self {
        Self {
            id: response.id,
            result: Ok(response),
        }
    }

    fn new_error(id: u64, e: RippleError) -> Self {
        Self {
            id: Some(id),
            result: Err(e),
        }
    }

    pub fn get_method(&self) -> Option<String> {
        if let Ok(e) = &self.result {
            return e.method.clone();
        }
        None
    }

    pub fn get_id(&self) -> Option<u64> {
        match &self.result {
            Ok(response) => response.id,
            Err(_) => None,
        }
    }

    pub fn get_device_resp_msg(&self, sub_id: Option<String>) -> Option<DeviceResponseMessage> {
        let json_resp = match &self.result {
            Ok(json_resp_res) => json_resp_res,
            _ => return None,
        };
        DeviceResponseMessage::create(json_resp, sub_id)
    }
}

impl ThunderAsyncClient {
    pub fn get_sender(&self) -> AsyncSender {
        self.sender.clone()
    }

    pub fn get_callback(&self) -> AsyncCallback {
        self.callback.clone()
    }
    async fn create_ws(
        endpoint: &str,
    ) -> (
        SplitSink<WebSocketStream<TcpStream>, Message>,
        SplitStream<WebSocketStream<TcpStream>>,
    ) {
        info!("Thunder_async_client Endpoint url {}", endpoint);
        let port = extract_tcp_port(endpoint);
        let tcp_port = port.unwrap();
        let mut index = 0;

        loop {
            // Try connecting to the tcp port first
            if let Ok(v) = TcpStream::connect(&tcp_port).await {
                // Setup handshake for websocket with the tcp port
                // Some WS servers lock on to the Port but not setup handshake till they are fully setup
                if let Ok((stream, _)) = client_async(endpoint, v).await {
                    break stream.split();
                }
            }
            if (index % 10).eq(&0) {
                error!(
                    "Thunder_async_client with {} failed with retry for last {} secs in {}",
                    endpoint, index, tcp_port
                );
            }
            index += 1;
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    fn prepare_request(&self, request: &ThunderAsyncRequest) -> Result<Vec<String>, RippleError> {
        let mut requests = Vec::new();
        let id: u64 = request.id;
        let (callsign, method) = request.request.get_callsign_method();

        // Check if the method is empty and return an error if it is
        if method.is_empty() {
            return Err(RippleError::InvalidInput);
        }

        // Check the status of the plugin using the status manager
        let status = match self.status_manager.get_status(callsign.clone()) {
            Some(v) => v.clone(),
            None => {
                // If the plugin status is not available, add the request to the pending list
                self.status_manager
                    .add_async_client_request_to_pending_list(callsign.clone(), request.clone());
                // Generate a request to check the plugin status and add it to the requests list
                let request = self
                    .status_manager
                    .generate_plugin_status_request(callsign.clone());
                requests.push(request.to_string());
                return Ok(requests);
            }
        };

        // If the plugin is missing, return a service error
        if status.state.is_missing() {
            error!("Plugin {} is missing", callsign);
            return Err(RippleError::ServiceError);
        }

        // If the plugin is activating, return a service not ready error
        if status.state.is_activating() {
            info!("Plugin {} is activating", callsign);
            return Err(RippleError::ServiceNotReady);
        }

        // If the plugin is not activated, add the request to the pending list and generate an activation request
        if !status.state.is_activated() {
            self.status_manager
                .add_async_client_request_to_pending_list(callsign.clone(), request.clone());
            let request = self
                .status_manager
                .generate_plugin_activation_request(callsign.clone());
            requests.push(request.to_string());
            return Ok(requests);
        }

        // Generate the appropriate JSON-RPC request based on the type of DeviceChannelRequest
        match &request.request {
            DeviceChannelRequest::Call(c) => requests.push(
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": c.method,
                    "params": c.params
                })
                .to_string(),
            ),
            DeviceChannelRequest::Unsubscribe(_) => requests.push(
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": format!("{}.unregister", callsign),
                    "params": {
                        "event": method,
                        "id": "client.events"
                    }
                })
                .to_string(),
            ),
            DeviceChannelRequest::Subscribe(_) => requests.push(
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": format!("{}.register", callsign),
                    "params": json!({
                        "event": method,
                        "id": "client.events"
                    })
                })
                .to_string(),
            ),
        }
        Ok(requests)
    }

    pub fn new(callback: AsyncCallback, sender: AsyncSender) -> Self {
        Self {
            status_manager: StatusManager::new(),
            sender,
            callback,
        }
    }

    pub async fn process_new_req(&self, request: String, url: String, callback: AsyncCallback) {
        let (mut new_wtx, mut new_wrx) = Self::create_ws(&url).await;
        let _feed = new_wtx
            .feed(tokio_tungstenite::tungstenite::Message::Text(request))
            .await;
        let _flush = new_wtx.flush().await;

        tokio::pin! {
            let read = new_wrx.next();
        }

        tokio::select! {
            Some(value) = &mut read => {
                match value {
                    Ok(v) => {
                        if let tokio_tungstenite::tungstenite::Message::Text(t) = v {
                            if self.status_manager.is_controller_response(self.get_sender(), callback.clone(), t.as_bytes()).await {
                                self.status_manager.handle_controller_response(self.get_sender(), callback.clone(), t.as_bytes()).await;
                            } else {
                                Self::handle_jsonrpc_response(t.as_bytes(), callback.clone()).await;
                                //close the newly created websocket here.
                                let _ = new_wtx.close().await;
                            }
                        }
                    },
                    Err(e) => {
                        error!("thunder_async_client Websocket error on read {:?}", e);
                    }
                }
            }
        }
    }

    pub async fn start(
        &self,
        url: &str,
        mut tr: Receiver<ThunderAsyncRequest>,
    ) -> Receiver<ThunderAsyncRequest> {
        let callback = self.callback.clone();
        let (mut ws_tx, mut ws_rx) = Self::create_ws(url).await;
        // send the controller statechange subscription request
        let status_request = self
            .status_manager
            .generate_state_change_subscribe_request();

        let _feed = ws_tx
            .feed(tokio_tungstenite::tungstenite::Message::Text(
                status_request.to_string(),
            ))
            .await;
        let _flush = ws_tx.flush().await;
        let client_c = self.clone();
        let callback_for_sender = callback.clone();
        tokio::pin! {
            let read = ws_rx.next();
        }
        loop {
            tokio::select! {
                Some(value) = &mut read => {
                    match value {
                        Ok(v) => {
                            if let tokio_tungstenite::tungstenite::Message::Text(t) = v {
                                if client_c.status_manager.is_controller_response(client_c.get_sender(), callback.clone(), t.as_bytes()).await {
                                    client_c.status_manager.handle_controller_response(client_c.get_sender(), callback.clone(), t.as_bytes()).await;
                                }
                                else {
                                    //let _id = Self::get_id_from_result(t.as_bytes()); for debug purpose
                                    // send the incoming text without context back to the sender
                                    Self::handle_jsonrpc_response(t.as_bytes(),callback.clone()).await
                                }
                            }
                        },
                        Err(e) => {
                            error!("Thunder_async_client Websocket error on read {:?}", e);
                            break;
                        }
                    }
                },
                Some(request) = tr.recv() => {
                    debug!("Got request from receiver for thunder {:?}", request);
                    // here prepare_request will check the plugin status and add json rpc format
                    match client_c.prepare_request(&request) {
                        Ok(updated_request) => {
                            debug!("Sending request to thunder {:?}", updated_request);
                            for r in updated_request {
                                match request.request {
                                    DeviceChannelRequest::Subscribe(_) => {
                                        let _feed = ws_tx.feed(tokio_tungstenite::tungstenite::Message::Text(r)).await;
                                        let _flush = ws_tx.flush().await;
                                    },
                                    DeviceChannelRequest::Unsubscribe(_) => {
                                        let _feed = ws_tx.feed(tokio_tungstenite::tungstenite::Message::Text(r)).await;
                                        let _flush = ws_tx.flush().await;
                                    },
                                    DeviceChannelRequest::Call(_) =>{
                                        let url_clone = url.to_string();
                                        let callback_clone = callback.clone();
                                        let self_clone = self.clone();
                                        tokio::spawn(async move {
                                            self_clone.process_new_req(r, url_clone, callback_clone.clone()).await;
                                            }
                                        );
                                    }

                                }
                            }
                        }
                        Err(e) => {
                            let response = ThunderAsyncResponse::new_error(request.id,e.clone());
                            match e {
                                RippleError::ServiceNotReady => {
                                    info!("Thunder Service not ready, request is now in pending list {:?}", request);
                                },
                                _ => {
                                    error!("error preparing request {:?}", e)
                                }
                            }
                            callback_for_sender.send(response).await;
                        }
                    }
                }
            }
        }
        // when WS is disconnected return the tr back to caller helps restabilish connection
        tr
    }

    /// Default handler method for the thunder async client to remove the context and send it back to the
    /// client for consumption
    async fn handle_jsonrpc_response(result: &[u8], callback: AsyncCallback) {
        if let Ok(message) = serde_json::from_slice::<JsonRpcApiResponse>(result) {
            callback
                .send(ThunderAsyncResponse::new_response(message))
                .await
        } else {
            error!("Invalid JSON RPC message sent by Thunder");
        }
    }

    pub async fn send(&self, request: ThunderAsyncRequest) {
        if let Err(e) = self.sender.send(request).await {
            error!("Failed to send thunder Async Request: {:?}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{device_operator::DeviceCallRequest, thunder_client::ThunderClient};
    use ripple_sdk::api::gateway::rpc_gateway_api::JsonRpcApiResponse;
    use ripple_sdk::utils::error::RippleError;
    use ripple_sdk::uuid::Uuid;
    use std::collections::HashMap;
    use std::sync::{Arc, RwLock};
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_thunder_async_request_new() {
        let callrequest = DeviceCallRequest {
            method: "org.rdk.System.1.getSerialNumber".to_string(),
            params: None,
        };

        let request = DeviceChannelRequest::Call(callrequest);
        let _async_request = ThunderAsyncRequest::new(request.clone());
        assert_eq!(
            _async_request.request.get_callsign_method(),
            request.get_callsign_method()
        );
    }

    #[tokio::test]
    async fn test_thunder_async_response_new_response() {
        let response = JsonRpcApiResponse {
            jsonrpc: "2.0".to_string(),
            id: Some(6),
            result: Some(json!({"key": "value"})),
            error: None,
            method: None,
            params: None,
        };

        let _async_response = ThunderAsyncResponse::new_response(response.clone());
        assert_eq!(_async_response.result.unwrap().result, response.result);
    }

    #[tokio::test]
    async fn test_thunder_async_response_new_error() {
        let error = RippleError::ServiceError;
        let async_response = ThunderAsyncResponse::new_error(1, error.clone());
        assert_eq!(async_response.id, Some(1));
        assert_eq!(async_response.result.unwrap_err(), error);
    }

    #[tokio::test]
    async fn test_thunder_async_response_get_event() {
        let response = JsonRpcApiResponse {
            jsonrpc: "2.0".to_string(),
            id: Some(6),
            result: Some(json!({"key": "value"})),
            error: None,
            method: Some("event_1".to_string()),
            params: None,
        };
        let async_response = ThunderAsyncResponse::new_response(response);
        assert_eq!(async_response.get_method(), Some("event_1".to_string()));
    }

    #[tokio::test]
    async fn test_thunder_async_response_get_id() {
        let response = JsonRpcApiResponse {
            jsonrpc: "2.0".to_string(),
            id: Some(42),
            result: Some(json!({"key": "value"})),
            error: None,
            method: Some("event_1".to_string()),
            params: None,
        };
        let async_response = ThunderAsyncResponse::new_response(response);
        assert_eq!(async_response.get_id(), Some(42));
    }

    #[tokio::test]
    async fn test_thunder_async_response_get_device_resp_msg() {
        let response = JsonRpcApiResponse {
            jsonrpc: "2.0".to_string(),
            id: Some(6),
            result: Some(json!({"key": "value"})),
            error: None,
            method: Some("event_1".to_string()),
            params: None,
        };
        let async_response = ThunderAsyncResponse::new_response(response);
        let device_resp_msg = async_response.get_device_resp_msg(None);
        assert_eq!(device_resp_msg.unwrap().message, json!({"key": "value"}));
    }

    #[tokio::test]
    async fn test_thunder_async_client_prepare_request() {
        let (resp_tx, _resp_rx) = mpsc::channel(10);
        let callback = AsyncCallback { sender: resp_tx };
        let (async_tx, _async_rx) = mpsc::channel(10);
        let async_sender = AsyncSender { sender: async_tx };
        let client = ThunderAsyncClient::new(callback, async_sender);

        let callrequest = DeviceCallRequest {
            method: "org.rdk.System.1.getSerialNumber".to_string(),
            params: None,
        };

        let request = DeviceChannelRequest::Call(callrequest);
        let async_request = ThunderAsyncRequest::new(request);
        let result = client.prepare_request(&async_request);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_thunder_async_client_send() {
        let (resp_tx, _resp_rx) = mpsc::channel(10);
        let callback = AsyncCallback { sender: resp_tx };
        let (async_tx, mut async_rx) = mpsc::channel(10);
        let async_sender = AsyncSender { sender: async_tx };
        let client = ThunderAsyncClient::new(callback, async_sender);

        let callrequest = DeviceCallRequest {
            method: "org.rdk.System.1.getSerialNumber".to_string(),
            params: None,
        };

        let request = DeviceChannelRequest::Call(callrequest);
        let async_request = ThunderAsyncRequest::new(request);
        client.send(async_request.clone()).await;
        let received = async_rx.recv().await;
        assert_eq!(received.unwrap().id, async_request.id);
    }

    #[tokio::test]
    async fn test_thunder_async_client_handle_jsonrpc_response() {
        let (resp_tx, mut resp_rx) = mpsc::channel(10);
        let callback = AsyncCallback { sender: resp_tx };
        let response = JsonRpcApiResponse {
            jsonrpc: "2.0".to_string(),
            id: Some(6),
            result: Some(json!({"key": "value"})),
            error: None,
            method: Some("event_1".to_string()),
            params: None,
        };
        let response_bytes = serde_json::to_vec(&response).unwrap();
        ThunderAsyncClient::handle_jsonrpc_response(&response_bytes, callback).await;
        let received = resp_rx.recv().await;
        assert_eq!(
            received.unwrap().result.unwrap().result,
            Some(json!({"key": "value"}))
        );
    }

    #[tokio::test]
    async fn test_thunder_async_client_start() {
        let (resp_tx, mut resp_rx) = mpsc::channel(10);
        let callback = AsyncCallback { sender: resp_tx };
        let (async_tx, _async_rx) = mpsc::channel(10);
        let async_sender = AsyncSender { sender: async_tx };
        let client = ThunderAsyncClient::new(callback.clone(), async_sender);

        let _thunder_client = ThunderClient {
            sender: None,
            pooled_sender: None,
            id: Uuid::new_v4(),
            plugin_manager_tx: None,
            subscriptions: None,
            thunder_async_client: Some(client),
            thunder_async_subscriptions: Some(Arc::new(RwLock::new(HashMap::new()))),
            thunder_async_callbacks: Some(Arc::new(RwLock::new(HashMap::new()))),
            use_thunder_async: true,
        };

        let response = json!({
            "jsonrpc": "2.0",
            "result": {
                "key": "value"
            }
        });

        ThunderAsyncClient::handle_jsonrpc_response(response.to_string().as_bytes(), callback)
            .await;
        let received = resp_rx.recv().await;
        assert!(received.is_some());
        let async_response = received.unwrap();
        assert_eq!(
            async_response.result.unwrap().result,
            Some(json!({"key": "value"}))
        );
    }
}
