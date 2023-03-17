use ripple_sdk::{
    api::{firebolt::fb_discovery::LaunchRequest, status_update::ExtnStatus},
    crossbeam::channel::Receiver,
    export_channel_builder, export_extn_metadata,
    extn::{
        client::{extn_client::ExtnClient, extn_sender::ExtnSender},
        extn_id::{ExtnClassId, ExtnId},
        ffi::{
            ffi_channel::{ExtnChannel, ExtnChannelBuilder},
            ffi_library::{CExtnMetadata, ExtnMetadata, ExtnSymbolMetadata},
            ffi_message::CExtnMessage,
        },
    },
    framework::ripple_contract::RippleContract,
    log::{debug, error, info},
    semver::Version,
    tokio::{self, runtime::Runtime},
    utils::{error::RippleError, logger::init_logger},
};

use crate::{
    launcher_lifecycle_processor::LauncherLifecycleEventProcessor, launcher_state::LauncherState,
    manager::app_launcher::AppLauncher,
};

fn init_library() -> CExtnMetadata {
    let _ = init_logger("launcher".into());

    let launcher_meta = ExtnSymbolMetadata::get(
        ExtnId::new_channel(ExtnClassId::Launcher, "internal".into()),
        RippleContract::Launcher,
        Version::new(1, 1, 0),
    );

    debug!("Returning launcher builder");
    let extn_metadata = ExtnMetadata {
        name: "launcher".into(),
        symbols: vec![launcher_meta],
    };
    extn_metadata.into()
}

export_extn_metadata!(CExtnMetadata, init_library);

fn start_launcher(sender: ExtnSender, receiver: Receiver<CExtnMessage>) {
    let _ = init_logger("launcher_channel".into());
    info!("Starting launcher channel");
    let runtime = Runtime::new().unwrap();
    let client = ExtnClient::new(receiver.clone(), sender);
    let client_for_receiver = client.clone();
    runtime.block_on(async move {
        tokio::spawn(async move {
            // create state
            let state = LauncherState::new(client.clone())
                .await
                .expect("state initialization to succeed");
            // Create a client for processors
            let mut client_for_processor = client.clone();
            let state_c = state.clone();

            // All Lifecyclemanagement events will come through this processor
            client_for_processor.add_event_processor(LauncherLifecycleEventProcessor::new(state));

            // Lets Main know that the launcher is ready
            let _ = client_for_processor.event(ExtnStatus::Ready).await;
            // Launches default app from library
            if let Some(default_app) = state_c.config.app_library_state.get_default_app() {
                let request =
                    LaunchRequest::new(default_app.app_id, "boot".into(), None, "boot".into());
                if let Err(e) = AppLauncher::launch(&state_c, request).await {
                    error!("default launch app failed {:?}", e);
                }
            }
        });
        client_for_receiver.initialize().await;
    });
}

fn build(extn_id: String) -> Result<Box<ExtnChannel>, RippleError> {
    if let Ok(id) = ExtnId::try_from(extn_id.clone()) {
        let current_id = ExtnId::new_channel(ExtnClassId::Launcher, "launcher".into());

        if id.eq(&current_id) {
            return Ok(Box::new(ExtnChannel {
                start: start_launcher,
            }));
        } else {
            Err(RippleError::ExtnError)
        }
    } else {
        Err(RippleError::InvalidInput)
    }
}

fn init_extn_builder() -> ExtnChannelBuilder {
    ExtnChannelBuilder {
        build,
        service: "launcher".into(),
    }
}

export_channel_builder!(ExtnChannelBuilder, init_extn_builder);
