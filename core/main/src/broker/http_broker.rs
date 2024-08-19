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

use hyper::{Body, Client, Method, Request, Uri};
use ripple_sdk::{
    log::{debug, error},
    tokio::{self, sync::mpsc},
};

use super::endpoint_broker::{
    BrokerCallback, BrokerCleaner, BrokerConnectRequest, BrokerOutputForwarder, BrokerSender,
    EndpointBroker,
};

pub struct HttpBroker {
    sender: BrokerSender,
    cleaner: BrokerCleaner,
}

impl EndpointBroker for HttpBroker {
    fn get_broker(request: BrokerConnectRequest, callback: BrokerCallback) -> Self {
        let endpoint = request.endpoint.clone();
        let (tx, mut tr) = mpsc::channel(10);
        let broker = BrokerSender { sender: tx };
        let is_json_rpc = endpoint.jsonrpc;
        let uri: Uri = endpoint.get_url().parse().unwrap();
        let client = Client::new();
        tokio::spawn(async move {
            while let Some(request) = tr.recv().await {
                let method = request.clone().rule.alias;
                if let Ok(broker_request) = Self::update_request(&request) {
                    let body = Body::from(broker_request.clone());
                    let http_request = Request::new(body);
                    let (mut parts, body) = http_request.into_parts();
                    //TODO, need to refactor to support other methods
                    parts.method = Method::GET;
                    let uri: Uri = format!("{}{}", uri, method).parse().unwrap();
                    let new_request = Request::builder().uri(uri).body(()).unwrap();
                    let (uri_parts, _) = new_request.into_parts();

                    parts.uri = uri_parts.uri;
                    //parts.headers = headers.clone();

                    let http_request = Request::from_parts(parts, body);
                    debug!(
                        "Sending request ={} for broker_request={}",
                        http_request.uri(),
                        broker_request
                    );
                    if let Ok(v) = client.request(http_request).await {
                        let (parts, body) = v.into_parts();
                        if !parts.status.is_success() {
                            error!("Error in server");
                        }
                        if let Ok(bytes) = hyper::body::to_bytes(body).await {
                            let value: Vec<u8> = bytes.into();
                            let value = value.as_slice();
                            if is_json_rpc {
                                Self::handle_jsonrpc_response(value, callback.clone());
                            } else if let Err(e) =
                                BrokerOutputForwarder::handle_non_jsonrpc_response(
                                    value,
                                    callback.clone(),
                                    request.clone(),
                                )
                            {
                                error!("Error forwarding {:?}", e)
                            }
                        }
                    }
                }
            }
        });
        Self {
            sender: broker,
            cleaner: BrokerCleaner { cleaner: None },
        }
    }

    fn get_sender(&self) -> BrokerSender {
        self.sender.clone()
    }

    fn get_cleaner(&self) -> BrokerCleaner {
        self.cleaner.clone()
    }
}
