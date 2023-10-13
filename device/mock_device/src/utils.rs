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

use std::{fs::File, io::BufReader, path::PathBuf, sync::Arc};

use ripple_sdk::{
    api::config::Config,
    extn::{client::extn_client::ExtnClient, extn_client_message::ExtnResponse},
    log::{debug, error},
    tokio::{self, sync::RwLock},
    utils::error::RippleError,
};
use serde_json::Value;
use url::{Host, Url};

use crate::{
    errors::{BootFailedError, LoadMockDataError, MockDeviceError},
    mock_data::{MockData, MockDataError, MockDataMessage},
    mock_web_socket_server::{MockWebSocketServer, WsServerParameters},
};

pub async fn boot_ws_server(
    mut client: ExtnClient,
    mock_data: Arc<RwLock<MockData>>,
) -> Result<Arc<MockWebSocketServer>, MockDeviceError> {
    debug!("Booting WS Server for mock device");
    let gateway = platform_gateway_url(&mut client).await?;

    if gateway.scheme() != "ws" {
        return Err(MockDeviceError::BootFailed(BootFailedError::BadUrlScheme));
    }

    if !is_valid_host(gateway.host()) {
        return Err(MockDeviceError::BootFailed(BootFailedError::BadHostname));
    }

    let mut server_config = WsServerParameters::new();
    server_config
        .port(gateway.port().unwrap_or(0))
        .path(gateway.path());
    let ws_server = MockWebSocketServer::new(mock_data, server_config)
        .await
        .map_err(|e| MockDeviceError::BootFailed(BootFailedError::ServerStartFailed(e)))?;

    let ws_server = Arc::new(ws_server);
    let server = ws_server.clone();

    tokio::spawn(async move {
        server.start_server().await;
    });

    Ok(ws_server)
}

async fn platform_gateway_url(client: &mut ExtnClient) -> Result<Url, MockDeviceError> {
    debug!("sending request for config.platform_parameters");
    if let Ok(response) = client.request(Config::PlatformParameters).await {
        if let Some(ExtnResponse::Value(value)) = response.payload.extract() {
            let gateway: Url = value
                .as_object()
                .and_then(|obj| obj.get("gateway"))
                .and_then(|val| val.as_str())
                .and_then(|s| s.parse().ok())
                .ok_or(MockDeviceError::BootFailed(
                    BootFailedError::GetPlatformGatewayFailed,
                ))?;
            debug!("{}", gateway);
            return Ok(gateway);
        }
    }

    Err(MockDeviceError::BootFailed(
        BootFailedError::GetPlatformGatewayFailed,
    ))
}

fn is_valid_host(host: Option<Host<&str>>) -> bool {
    match host {
        Some(Host::Ipv4(ipv4)) => ipv4.is_loopback() || ipv4.is_unspecified(),
        _ => false,
    }
}

async fn find_mock_device_data_file(mut client: ExtnClient) -> Result<PathBuf, MockDeviceError> {
    let file = client
        .get_config("mock_data_file")
        .unwrap_or("mock-device.json".to_owned());
    let path = PathBuf::from(file);

    debug!(
        "mock data path={} absolute={}",
        path.display(),
        path.is_absolute()
    );

    if path.is_absolute() {
        return Ok(path);
    }

    let saved_dir = client
        .request(Config::SavedDir)
        .await
        .and_then(|response| -> Result<PathBuf, RippleError> {
            if let Some(ExtnResponse::String(value)) = response.payload.extract() {
                if let Ok(buf) = value.parse::<PathBuf>() {
                    return Ok(buf);
                }
            }

            Err(RippleError::ParseError)
        })
        .map_err(|e| {
            error!("Config::SaveDir request error {:?}", e);
            LoadMockDataError::GetSavedDirFailed
        })?;

    debug!("received saved_dir {saved_dir:?}");
    if !saved_dir.is_dir() {
        return Err(LoadMockDataError::PathDoesNotExist(saved_dir))?;
    }

    let path = saved_dir.join(path);

    Ok(path)
}

pub async fn load_mock_data(client: ExtnClient) -> Result<MockData, MockDeviceError> {
    let path = find_mock_device_data_file(client).await?;
    debug!("path={:?}", path);
    if !path.is_file() {
        return Err(LoadMockDataError::PathDoesNotExist(path))?;
    }

    let file = File::open(path.clone()).map_err(|e| {
        error!("Failed to open mock data file {e:?}");
        LoadMockDataError::FileOpenFailed(path)
    })?;
    let reader = BufReader::new(file);
    let json: serde_json::Value =
        serde_json::from_reader(reader).map_err(|_| LoadMockDataError::MockDataNotValidJson)?;

    if let Some(list) = json.as_array() {
        let mock_data = list
            .iter()
            .map(|req_resp| {
                let (req, resps) = parse_request_responses(req_resp)?;
                let key = req.key()?;

                Ok((key, (req, resps)))
            })
            .collect::<Result<MockData, MockDeviceError>>()?
            .into_iter()
            .collect::<MockData>();

        Ok(mock_data)
    } else {
        Err(LoadMockDataError::MockDataNotArray)?
    }
}

fn parse_request_responses(
    request_responses: &Value,
) -> Result<(MockDataMessage, Vec<MockDataMessage>), MockDataError> {
    let req_resp = request_responses
        .as_object()
        .ok_or(MockDataError::NotAnObject)?;
    let req = req_resp
        .get("request")
        .ok_or(MockDataError::MissingRequestField)?;
    let res = req_resp
        .get("responses")
        .and_then(|res| {
            res.as_array()
                .and_then(|arr| if arr.is_empty() { None } else { Some(arr) })
        })
        .ok_or(MockDataError::MissingResponseField)?
        .iter()
        .map(MockDataMessage::try_from)
        .collect::<Result<Vec<MockDataMessage>, MockDataError>>()?;

    let req = MockDataMessage::try_from(req)?;

    Ok((req, res))
}

pub fn is_value_jsonrpc(value: &Value) -> bool {
    value.as_object().map_or(false, |req| {
        req.contains_key("jsonrpc") && req.contains_key("id") && req.contains_key("method")
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn test_is_value_jsonrpc_true() {
        assert!(is_value_jsonrpc(
            &json!({"jsonrpc": "2.0", "id": 1, "method": "someAction", "params": {}})
        ));
    }

    #[test]
    fn test_is_value_jsonrpc_false() {
        assert!(!is_value_jsonrpc(&json!({"key": "value"})));
    }
}
