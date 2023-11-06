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

use serde::{Deserialize, Serialize};

use crate::api::device::entertainment_data::{
    EntityInfoParameters, EntityInfoResult, PurchasedContentParameters, PurchasedContentResult,
};

use super::{
    fb_keyboard::{KeyboardSessionRequest, KeyboardSessionResponse},
    fb_pin::{PinChallengeRequest, PinChallengeResponse},
    fb_player::{
        PlayerErrorResponse, PlayerLoadRequest, PlayerMediaSession, PlayerPlayRequest,
        PlayerProgress, PlayerProgressRequest, PlayerResponse, PlayerStatus, PlayerStatusRequest,
        PlayerStopRequest, StreamingPlayerCreateRequest, StreamingPlayerInstance,
    },
};

pub const ACK_CHALLENGE_EVENT: &str = "acknowledgechallenge.onRequestChallenge";
pub const ACK_CHALLENGE_CAPABILITY: &str = "xrn:firebolt:capability:usergrant:acknowledgechallenge";

// TODO: move the provider error structs to here

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum ProviderRequestPayload {
    KeyboardSession(KeyboardSessionRequest),
    PinChallenge(PinChallengeRequest),
    AckChallenge(Challenge),
    EntityInfoRequest(EntityInfoParameters),
    PurchasedContentRequest(PurchasedContentParameters),
    Generic(String),
    // TODO look into a better way to solve this
    PlayerLoad(PlayerLoadRequest),
    PlayerPlay(PlayerPlayRequest),
    PlayerStop(PlayerStopRequest),
    PlayerStatus(PlayerStatusRequest),
    PlayerProgress(PlayerProgressRequest),
    StreamingPlayerCreate(StreamingPlayerCreateRequest),
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(untagged)]
pub enum ProviderResponsePayload {
    ChallengeResponse(ChallengeResponse),
    PinChallengeResponse(PinChallengeResponse),
    KeyboardResult(KeyboardSessionResponse),
    // TODO: try to compress this to Player
    PlayerLoad(PlayerMediaSession),
    PlayerLoadError(PlayerErrorResponse),
    PlayerPlay(PlayerMediaSession),
    PlayerPlayError(PlayerErrorResponse),
    PlayerStop(PlayerMediaSession),
    PlayerStopError(PlayerErrorResponse),
    PlayerStatus(PlayerStatus),
    PlayerStatusError(PlayerErrorResponse),
    PlayerProgress(PlayerProgress),
    PlayerProgressError(PlayerErrorResponse),
    StreamingPlayerCreate(StreamingPlayerInstance),
    StreamingPlayerCreateError(PlayerErrorResponse),
    //
    // TODO: assess if boxing this is a productive move: https://rust-lang.github.io/rust-clippy/master/index.html#/large_enum_variant
    EntityInfoResponse(Box<Option<EntityInfoResult>>),
    PurchasedContentResponse(PurchasedContentResult),
}

// TODO: could this be replaced with Into trait?
impl ProviderResponsePayload {
    pub fn as_keyboard_result(&self) -> Option<KeyboardSessionResponse> {
        match self {
            ProviderResponsePayload::KeyboardResult(res) => Some(res.clone()),
            _ => None,
        }
    }

    pub fn as_player_response(&self) -> Option<PlayerResponse> {
        match self {
            ProviderResponsePayload::PlayerPlay(res) => Some(PlayerResponse::Play(res.clone())),
            ProviderResponsePayload::PlayerLoad(res) => Some(PlayerResponse::Load(res.clone())),
            _ => None,
        }
    }

    pub fn as_pin_challenge_response(&self) -> Option<PinChallengeResponse> {
        match self {
            ProviderResponsePayload::PinChallengeResponse(res) => Some(res.clone()),
            _ => None,
        }
    }

    pub fn as_challenge_response(&self) -> Option<ChallengeResponse> {
        match self {
            ProviderResponsePayload::ChallengeResponse(res) => {
                res.granted.map(|value| ChallengeResponse {
                    granted: Some(value),
                })
            }
            ProviderResponsePayload::PinChallengeResponse(res) => {
                res.get_granted().map(|value| ChallengeResponse {
                    granted: Some(value),
                })
            }
            _ => None,
        }
    }

    pub fn as_entity_info_result(&self) -> Option<Option<EntityInfoResult>> {
        match self {
            ProviderResponsePayload::EntityInfoResponse(res) => Some(*res.clone()),
            _ => None,
        }
    }

    pub fn as_purchased_content_result(&self) -> Option<PurchasedContentResult> {
        match self {
            ProviderResponsePayload::PurchasedContentResponse(res) => Some(res.clone()),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRequest {
    pub correlation_id: String,
    pub parameters: ProviderRequestPayload,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ProviderResponse {
    pub correlation_id: String,
    pub result: ProviderResponsePayload,
}

pub trait ToProviderResponse {
    fn to_provider_response(&self) -> ProviderResponse;
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ProviderResponseParams {
    pub response: ProviderResponse,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ExternalProviderRequest<T> {
    pub correlation_id: String,
    pub parameters: T,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalProviderResponse<T> {
    pub correlation_id: String,
    pub result: T,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChallengeResponse {
    pub granted: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChallengeRequestor {
    pub id: String,
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct FocusRequest {
    pub correlation_id: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Challenge {
    pub capability: String,
    pub requestor: ChallengeRequestor,
}
