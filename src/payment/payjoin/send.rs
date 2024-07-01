#![allow(unused_variables)]
use crate::config::{PAYJOIN_REQUEST_TIMEOUT, PAYJOIN_RETRY_INTERVAL};
use crate::io::utils::ohttp_headers;
use crate::logger::FilesystemLogger;
use crate::types::Wallet;

use bitcoin::block::Header;
use bitcoin::{BlockHash, Script, ScriptBuf, Txid};
use lightning::chain::transaction::TransactionData;
use lightning::chain::WatchedOutput;
use lightning::util::logger::Logger;
use lightning::{log_error, log_info};

use std::sync::Arc;

enum PayjoinTransaction {
    PendingInitialBroadcast,
    PendingFirstConfirmation,
    PendingThresholdConfirmation,
}

pub(crate) struct PayjoinSender {
	logger: Arc<FilesystemLogger>,
	wallet: Arc<Wallet>,
	payjoin_relay: payjoin::Url,
	transaction_queue: std::sync::Mutex<Vec<(Txid, ScriptBuf)>>,
}

impl PayjoinSender {
	pub(crate) fn new(
		logger: Arc<FilesystemLogger>, wallet: Arc<Wallet>, payjoin_relay: payjoin::Url,
	) -> Self {
		Self {
			logger,
			wallet,
			payjoin_relay,
			transaction_queue: std::sync::Mutex::new(Vec::new()),
		}
	}

	pub(crate) fn payjoin_relay(&self) -> &payjoin::Url {
		&self.payjoin_relay
	}

	pub(crate) async fn send_request(&self, request: &payjoin::Request) -> Option<Vec<u8>> {
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
}

impl lightning::chain::Filter for PayjoinSender {
	fn register_tx(&self, txid: &Txid, script_pubkey: &Script) {
		dbg!("Registering transaction {:?}", txid);
		self.transaction_queue.lock().unwrap().push((txid.clone(), script_pubkey.into()));
	}
	fn register_output(&self, output: WatchedOutput) {}
}

impl lightning::chain::Confirm for PayjoinSender {
	fn transactions_confirmed(&self, header: &Header, txdata: &TransactionData, height: u32) {
		dbg!("Confirmed transaction {:?}", txdata);
		// let (index, tx) = txdata[0];
		// let txid = tx.txid();
		// let my_input =
		// 	tx.input.iter().find(|input| self.wallet.is_mine(&input.script_sig).unwrap_or(false));
		// if let Some(my_input) = my_input {
		// 	self.transaction_queue.push((txid, my_input.script_sig))
		// }
	}
	fn transaction_unconfirmed(&self, txid: &Txid) {
      dbg!("Unconfirmed transaction {:?}", txid);
  }
	fn best_block_updated(&self, header: &Header, height: u32) {
      dbg!("Best block updated {:?}", header);
  }
	fn get_relevant_txids(&self) -> Vec<(Txid, u32, Option<BlockHash>)> {
		// let state_lock = self.sweeper_state.lock().unwrap();
		// state_lock
		// 	.outputs
		// 	.iter()
		// 	.filter_map(|o| match o.status {
		// 		OutputSpendStatus::PendingThresholdConfirmations {
		// 			ref latest_spending_tx,
		// 			confirmation_height,
		// 			confirmation_hash,
		// 			..
		// 		} => Some((latest_spending_tx.txid(), confirmation_height, Some(confirmation_hash))),
		// 		_ => None,
		// 	})
		// 	.collect::<Vec<_>>()
    vec![]
	}
}
