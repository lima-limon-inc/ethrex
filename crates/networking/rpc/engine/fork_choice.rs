use ethrex_blockchain::{
    error::{ChainError, InvalidForkChoice},
    fork_choice::apply_fork_choice,
    latest_canonical_block_hash,
    payload::{create_payload, BuildPayloadArgs},
};
use serde_json::Value;
use tracing::{info, warn};

use crate::{
    types::{
        fork_choice::{ForkChoiceResponse, ForkChoiceState, PayloadAttributesV3},
        payload::PayloadStatus,
    },
    utils::RpcRequest,
    RpcApiContext, RpcErr, RpcHandler,
};

#[derive(Debug)]
pub struct ForkChoiceUpdatedV3 {
    pub fork_choice_state: ForkChoiceState,
    #[allow(unused)]
    pub payload_attributes: Result<Option<PayloadAttributesV3>, String>,
}

impl TryFrom<ForkChoiceUpdatedV3> for RpcRequest {
    type Error = String;

    fn try_from(val: ForkChoiceUpdatedV3) -> Result<Self, Self::Error> {
        match val.payload_attributes {
            Ok(attrs) => Ok(RpcRequest {
                method: "engine_forkchoiceUpdatedV3".to_string(),
                params: Some(vec![
                    serde_json::json!(val.fork_choice_state),
                    serde_json::json!(attrs),
                ]),
                ..Default::default()
            }),
            Err(err) => Err(err),
        }
    }
}

impl RpcHandler for ForkChoiceUpdatedV3 {
    // TODO(#853): Allow fork choice to be executed even if fork choice updated v3 was not correctly parsed.
    fn parse(params: &Option<Vec<Value>>) -> Result<Self, RpcErr> {
        let params = params
            .as_ref()
            .ok_or(RpcErr::BadParams("No params provided".to_owned()))?;
        if params.len() != 2 {
            return Err(RpcErr::BadParams("Expected 2 params".to_owned()));
        }
        Ok(ForkChoiceUpdatedV3 {
            fork_choice_state: serde_json::from_value(params[0].clone())?,
            payload_attributes: serde_json::from_value(params[1].clone())
                .map_err(|e| e.to_string()),
        })
    }

    fn handle(&self, context: RpcApiContext) -> Result<Value, RpcErr> {
        info!(
            "New fork choice request with head: {}, safe: {}, finalized: {}.",
            self.fork_choice_state.head_block_hash,
            self.fork_choice_state.safe_block_hash,
            self.fork_choice_state.finalized_block_hash
        );

        let head_block = match apply_fork_choice(
            &context.storage,
            self.fork_choice_state.head_block_hash,
            self.fork_choice_state.safe_block_hash,
            self.fork_choice_state.finalized_block_hash,
        ) {
            Ok(head) => head,
            Err(error) => {
                let fork_choice_response = match error {
                    InvalidForkChoice::NewHeadAlreadyCanonical => {
                        ForkChoiceResponse::from(PayloadStatus::valid_with_hash(
                            latest_canonical_block_hash(&context.storage).unwrap(),
                        ))
                    }
                    InvalidForkChoice::Syncing => {
                        // Start sync
                        let current_number = context.storage.get_latest_block_number()?.unwrap();
                        let Some(current_head) =
                            context.storage.get_canonical_block_hash(current_number)?
                        else {
                            return Err(RpcErr::Internal(
                                "Missing latest canonical block".to_owned(),
                            ));
                        };
                        let sync_head = self.fork_choice_state.head_block_hash;
                        tokio::spawn(async move {
                            // If we can't get hold of the syncer, then it means that there is an active sync in process
                            if let Ok(mut syncer) = context.syncer.try_lock() {
                                syncer
                                    .start_sync(current_head, sync_head, context.storage.clone())
                                    .await
                            }
                        });
                        ForkChoiceResponse::from(PayloadStatus::syncing())
                    }
                    reason => {
                        warn!("Invalid fork choice state. Reason: {:#?}", reason);
                        return Err(RpcErr::InvalidForkChoiceState(reason.to_string()));
                    }
                };
                return serde_json::to_value(fork_choice_response)
                    .map_err(|error| RpcErr::Internal(error.to_string()));
            }
        };

        // Build block from received payload. This step is skipped if applying the fork choice state failed
        let mut response = ForkChoiceResponse::from(PayloadStatus::valid_with_hash(
            self.fork_choice_state.head_block_hash,
        ));

        match &self.payload_attributes {
            // Payload may be invalid but we had to apply fork choice state nevertheless.
            Err(e) => return Err(RpcErr::InvalidPayloadAttributes(e.into())),
            Ok(None) => (),
            Ok(Some(attributes)) => {
                info!("Fork choice updated includes payload attributes. Creating a new payload.");
                let chain_config = context.storage.get_chain_config()?;
                if !chain_config.is_cancun_activated(attributes.timestamp) {
                    return Err(RpcErr::UnsuportedFork(
                        "forkChoiceV3 used to build pre-Cancun payload".to_string(),
                    ));
                }
                if attributes.timestamp <= head_block.timestamp {
                    return Err(RpcErr::InvalidPayloadAttributes(
                        "invalid timestamp".to_string(),
                    ));
                }
                let args = BuildPayloadArgs {
                    parent: self.fork_choice_state.head_block_hash,
                    timestamp: attributes.timestamp,
                    fee_recipient: attributes.suggested_fee_recipient,
                    random: attributes.prev_randao,
                    withdrawals: attributes.withdrawals.clone(),
                    beacon_root: Some(attributes.parent_beacon_block_root),
                    version: 3,
                };
                let payload_id = args.id();
                response.set_id(payload_id);
                let payload = match create_payload(&args, &context.storage) {
                    Ok(payload) => payload,
                    Err(ChainError::EvmError(error)) => return Err(error.into()),
                    // Parent block is guaranteed to be present at this point,
                    // so the only errors that may be returned are internal storage errors
                    Err(error) => return Err(RpcErr::Internal(error.to_string())),
                };
                context.storage.add_payload(payload_id, payload)?;
            }
        }

        serde_json::to_value(response).map_err(|error| RpcErr::Internal(error.to_string()))
    }
}
