use crate::config::{
	PAYJOIN_REQUEST_TIMEOUT, PAYJOIN_RETRY_INTERVAL,
};
use crate::error::Error;
use crate::io::utils::ohttp_headers;
use crate::logger::FilesystemLogger;
use crate::types::Wallet;

use lightning::chain::chaininterface::BroadcasterInterface;
use lightning::util::logger::Logger;
use lightning::{log_error, log_info};

use bitcoin::{psbt::Psbt, Txid};

use std::ops::Deref;
use std::sync::Arc;

pub(crate) struct PayjoinSender<B: Deref>
where
	B::Target: BroadcasterInterface,
{
	logger: Arc<FilesystemLogger>,
	wallet: Arc<Wallet>,
	payjoin_relay: payjoin::Url,
	broadcaster: B,
}

impl<B: Deref> PayjoinSender<B>
where
	B::Target: BroadcasterInterface,
{
	pub(crate) fn new(
		logger: Arc<FilesystemLogger>, wallet: Arc<Wallet>, broadcaster: B, payjoin_relay: payjoin::Url,
	) -> Self {
        Self { logger, wallet, broadcaster, payjoin_relay }
	}

	pub(crate) fn create_payjoin_request(
		&self, payjoin_uri: payjoin::Uri<'static, bitcoin::address::NetworkChecked>,
		amount: Option<bitcoin::Amount>,
	) -> Result<(Psbt, payjoin::Request, payjoin::send::ContextV2), Error> {
		let amount_to_send = match (amount, payjoin_uri.amount) {
			(Some(amount), _) => amount,
			(None, Some(amount)) => amount,
			(None, None) => return Err(Error::PayjoinRequestMissingAmount),
		};
		let original_psbt = self.wallet.build_payjoin_transaction(
			payjoin_uri.address.script_pubkey(),
			amount_to_send.to_sat(),
		)?;
		let (request_data, request_context) =
			payjoin::send::RequestBuilder::from_psbt_and_uri(original_psbt.clone(), payjoin_uri)
				.and_then(|b| b.build_non_incentivizing())
				.and_then(|mut c| c.extract_v2(self.payjoin_relay.clone()))
				.map_err(|e| {
					log_error!(self.logger, "Failed to extract payjoin request: {}", e);
					Error::PayjoinRequestCreationFailed
				})?;
		Ok((original_psbt, request_data, request_context))
	}

	pub(crate) async fn fetch(&self, request: &payjoin::Request) -> Option<Vec<u8>> {
        let response = match reqwest::Client::new()
            .post(request.url.clone())
            .body(request.body.clone())
            .timeout(PAYJOIN_REQUEST_TIMEOUT)
            .headers(ohttp_headers())
            .send()
            .await
            {
                Ok(response) => response,
                Err(e) => {
                    log_error!(
                        self.logger,
                        "Error trying to poll Payjoin response: {}, retrying in {} seconds",
                        e,
                        PAYJOIN_RETRY_INTERVAL.as_secs()
                    );
                    return None;
                },
            };
        if response.status() == reqwest::StatusCode::OK {
            match response.bytes().await.and_then(|r| Ok(r.to_vec())) {
                Ok(response) => {
                    if response.is_empty() {
                        log_info!(
                            self.logger,
                            "Got empty response while polling Payjoin response, retrying in {} seconds", PAYJOIN_RETRY_INTERVAL.as_secs()
                        );
                        return None;
                    }
                    return Some(response);
                },
                Err(e) => {
                    log_error!(
                        self.logger,
                        "Error reading polling Payjoin response: {}, retrying in {} seconds",
                        e,
                        PAYJOIN_RETRY_INTERVAL.as_secs()
                    );
                    return None;
                },
            };
        } else {
            log_info!(
                self.logger,
                "Got status code {} while polling Payjoin response, retrying in {} seconds",
                response.status(),
                PAYJOIN_RETRY_INTERVAL.as_secs()
            );
            return None;
        }
	}

	pub(crate) fn process_payjoin_response(
		&self, context: payjoin::send::ContextV2, response: Vec<u8>, original_psbt: &mut Psbt,
	) -> Result<Txid, Error> {
		let psbt = context.process_response(&mut response.as_slice());
		match psbt {
			Ok(Some(psbt)) => {
				let txid = self.finalise_payjoin_transaction(psbt, original_psbt)?;
				return Ok(txid);
			},
			Ok(None) => return Err(Error::PayjoinResponseProcessingFailed),
			Err(e) => {
				log_error!(self.logger, "Failed to process Payjoin response: {}", e);
				return Err(Error::PayjoinResponseProcessingFailed);
			},
		}
	}

	fn finalise_payjoin_transaction(
		&self, mut payjoin_proposal_psbt: Psbt, original_psbt: &mut Psbt,
	) -> Result<Txid, Error> {
		// BDK only signs scripts that match its target descriptor by iterating through input map.
		// The BIP 78 spec makes receiver clear sender input map UTXOs, so process_response will
		// fail unless they're cleared.  A PSBT unsigned_tx.input references input OutPoints and
		// not a Script, so the sender signer must either be able to sign based on OutPoint UTXO
		// lookup or otherwise re-introduce the Script from original_psbt.  Since BDK PSBT signer
		// only checks Input map Scripts for match against its descriptor, it won't sign if they're
		// empty.  Re-add the scripts from the original_psbt in order for BDK to sign properly.
		// reference: https://github.com/bitcoindevkit/bdk-cli/pull/156#discussion_r1261300637
		let mut original_inputs =
			original_psbt.unsigned_tx.input.iter().zip(&mut original_psbt.inputs).peekable();
		for (proposed_txin, proposed_psbtin) in
			payjoin_proposal_psbt.unsigned_tx.input.iter().zip(&mut payjoin_proposal_psbt.inputs)
		{
			if let Some((original_txin, original_psbtin)) = original_inputs.peek() {
				if proposed_txin.previous_output == original_txin.previous_output {
					proposed_psbtin.witness_utxo = original_psbtin.witness_utxo.clone();
					proposed_psbtin.non_witness_utxo = original_psbtin.non_witness_utxo.clone();
					original_inputs.next();
				}
			}
		}

		match self.wallet.sign_transaction(&mut payjoin_proposal_psbt) {
			Ok(true) => {
				let tx = payjoin_proposal_psbt.extract_tx();
				self.broadcaster.broadcast_transactions(&[&tx]);
				Ok(tx.txid())
			},
			Ok(false) => {
				log_error!(self.logger, "Unable to finalise Payjoin transaction: signing failed");
				Err(Error::PayjoinResponseProcessingFailed)
			},
			Err(e) => {
				log_error!(self.logger, "Failed to sign Payjoin proposal: {}", e);
				Err(Error::PayjoinResponseProcessingFailed)
			},
		}
	}
}
