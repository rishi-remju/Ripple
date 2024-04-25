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
};

use std::any::type_name;

pub const ACK_CHALLENGE_EVENT: &str = "acknowledgechallenge.onRequestChallenge";
pub const ACK_CHALLENGE_CAPABILITY: &str = "xrn:firebolt:capability:usergrant:acknowledgechallenge";

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum ProviderRequestPayload {
    KeyboardSession(KeyboardSessionRequest),
    PinChallenge(PinChallengeRequest),
    AckChallenge(Challenge),
    EntityInfoRequest(EntityInfoParameters),
    PurchasedContentRequest(PurchasedContentParameters),
    Generic(String),
}

// <pca>
#[derive(Debug, Clone)]

pub enum ProviderResponsePayloadType {
    ChallengeResponse,
    ChallengeError,
    PinChallengeResponse,
    KeyboardResult,
    EntityInfoResponse,
    PurchasedContentResponse,
}

impl ToString for ProviderResponsePayloadType {
    fn to_string(&self) -> String {
        match self {
            ProviderResponsePayloadType::ChallengeResponse => "ChallengeResponse".into(),
            ProviderResponsePayloadType::ChallengeError => "ChallengeError".into(),
            ProviderResponsePayloadType::PinChallengeResponse => "PinChallengeResponse".into(),
            ProviderResponsePayloadType::KeyboardResult => "KeyboardResult".into(),
            ProviderResponsePayloadType::EntityInfoResponse => "EntityInfoResponse".into(),
            ProviderResponsePayloadType::PurchasedContentResponse => {
                "PurchasedContentResponse".into()
            }
        }
    }
}
// </pca>

#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
#[serde(untagged)]
pub enum ProviderResponsePayload {
    ChallengeResponse(ChallengeResponse),
    ChallengeError(ChallengeError),
    PinChallengeResponse(PinChallengeResponse),
    KeyboardResult(KeyboardSessionResponse),
    EntityInfoResponse(Option<EntityInfoResult>),
    PurchasedContentResponse(PurchasedContentResult),
}

impl ProviderResponsePayload {
    pub fn as_keyboard_result(&self) -> Option<KeyboardSessionResponse> {
        match self {
            ProviderResponsePayload::KeyboardResult(res) => Some(res.clone()),
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
            ProviderResponsePayload::EntityInfoResponse(res) => Some(res.clone()),
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

// <pca>
impl ToString for ProviderResponsePayload {
    fn to_string(&self) -> String {
        match self {
            ProviderResponsePayload::ChallengeResponse(_) => {
                ProviderResponsePayloadType::ChallengeResponse.to_string()
            }
            ProviderResponsePayload::ChallengeError(_) => {
                ProviderResponsePayloadType::ChallengeError.to_string()
            }
            ProviderResponsePayload::PinChallengeResponse(_) => {
                ProviderResponsePayloadType::PinChallengeResponse.to_string()
            }
            ProviderResponsePayload::KeyboardResult(_) => {
                ProviderResponsePayloadType::KeyboardResult.to_string()
            }
            ProviderResponsePayload::EntityInfoResponse(_) => {
                ProviderResponsePayloadType::EntityInfoResponse.to_string()
            }
            ProviderResponsePayload::PurchasedContentResponse(_) => {
                ProviderResponsePayloadType::PurchasedContentResponse.to_string()
            }
        }
    }
}
// </pca>

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

// <pca>
#[derive(Debug, Clone)]

pub struct ProviderAttributes {
    pub name: String,
    pub event: &'static str,
    pub response_type: &'static str,
    pub response_payload: ProviderResponsePayloadType,
    pub error_type: &'static str,
    pub error_payload: ProviderResponsePayloadType,
}

impl ProviderAttributes {
    pub fn get(name: &str) -> Option<ProviderAttributes> {
        println!("*** _DEBUG: ProviderAttributes::get: name={}", name);
        match name {
            "AcknowledgeChallenge" => Some(ProviderAttributes {
                name: String::from(name),
                event: ACK_CHALLENGE_EVENT,
                response_type: type_name::<ChallengeResponse>(),
                response_payload: ProviderResponsePayloadType::ChallengeResponse,
                error_type: type_name::<ChallengeError>(),
                error_payload: ProviderResponsePayloadType::ChallengeError,
            }),
            _ => None,
        }
    }
}
// </pca>

#[derive(Debug, Serialize, Deserialize, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub struct ChallengeResponse {
    pub granted: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub struct DataObject {}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub struct ChallengeError {
    pub code: u32,
    pub message: String,
    pub data: Option<DataObject>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
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

#[cfg(test)]
mod tests {
    use crate::api::{
        device::{
            device_request::AudioProfile,
            entertainment_data::{
                AdvisoriesValue, ContentIdentifiers, ContentRating, EntityInfo, EntityType,
                MusicType, OfferingType, ProgramType, RatingValue, SchemeValue, VideoQuality,
                WaysToWatch,
            },
        },
        firebolt::fb_pin::PinChallengeResultReason,
    };

    use super::*;
    use rstest::rstest;

    #[test]
    fn test_as_keyboard_result() {
        let response = ProviderResponsePayload::KeyboardResult(KeyboardSessionResponse {
            text: "text".to_string(),
            canceled: false,
        });
        assert_eq!(
            response.as_keyboard_result(),
            Some(KeyboardSessionResponse {
                text: "text".to_string(),
                canceled: false
            })
        );
    }

    #[test]
    fn test_as_pin_challenge_response() {
        let response = ProviderResponsePayload::PinChallengeResponse(PinChallengeResponse {
            granted: Some(true),
            reason: PinChallengeResultReason::NoPinRequired,
        });
        assert_eq!(
            response.as_pin_challenge_response(),
            Some(PinChallengeResponse {
                granted: Some(true),
                reason: PinChallengeResultReason::NoPinRequired,
            })
        );
    }

    #[rstest]
    fn test_as_challenge_response() {
        let response = ProviderResponsePayload::ChallengeResponse(ChallengeResponse {
            granted: Some(true),
        });
        assert_eq!(
            response.as_challenge_response(),
            Some(ChallengeResponse {
                granted: Some(true)
            })
        );
    }

    #[test]
    fn test_as_entity_info_result() {
        let entity_info = EntityInfo {
            entity_type: EntityType::Program,
            identifiers: ContentIdentifiers {
                asset_id: Some("asset_id".to_string()),
                entity_id: Some("entity_id".to_string()),
                season_id: Some("season_id".to_string()),
                series_id: Some("series_id".to_string()),
                app_content_data: Some("app_content_data".to_string()),
            },
            title: "title".to_string(),
            program_type: Some(ProgramType::Movie),
            music_type: Some(MusicType::Album),
            synopsis: Some("synopsis".to_string()),
            season_number: None,
            episode_number: None,
            release_date: Some("release_date".to_string()),
            content_ratings: Some(vec![ContentRating {
                rating: RatingValue::G,
                scheme: SchemeValue::CaMovie,
                advisories: Some(vec![AdvisoriesValue::G]),
            }]),
            ways_to_watch: Some(vec![WaysToWatch {
                identifiers: ContentIdentifiers {
                    asset_id: Some("asset_id".to_string()),
                    entity_id: Some("entity_id".to_string()),
                    season_id: Some("season_id".to_string()),
                    series_id: Some("series_id".to_string()),
                    app_content_data: Some("app_content_data".to_string()),
                },
                expires: Some("expires".to_string()),
                entitled: Some(true),
                entitled_expires: Some("entitled_expires".to_string()),
                offering_type: Some(OfferingType::FREE),
                has_ads: Some(false),
                price: Some(5.0),
                video_quality: Some(vec![VideoQuality::Sd]),
                audio_profile: Some(vec![AudioProfile::Stereo]),
                audio_languages: Some(vec!["en".to_string()]),
                closed_captions: Some(vec!["en".to_string()]),
                subtitles: Some(vec!["en".to_string()]),
                audio_descriptions: Some(vec!["en".to_string()]),
            }]),
        };
        let response = ProviderResponsePayload::EntityInfoResponse(Some(EntityInfoResult {
            expires: "expires".to_string(),
            entity: entity_info.clone(),
            related: None,
        }));
        assert_eq!(
            response.as_entity_info_result(),
            Some(Some(EntityInfoResult {
                expires: "expires".to_string(),
                entity: entity_info,
                related: None,
            }))
        );
    }

    #[test]
    fn test_as_purchased_content_result() {
        let response = ProviderResponsePayload::PurchasedContentResponse(PurchasedContentResult {
            expires: "expires".to_string(),
            total_count: 1,
            entries: vec![],
        });
        assert_eq!(
            response.as_purchased_content_result(),
            Some(PurchasedContentResult {
                expires: "expires".to_string(),
                total_count: 1,
                entries: vec![],
            })
        );
    }
}
