#![allow(unused_variables)]
use crate::config::{PAYJOIN_REQUEST_TIMEOUT, PAYJOIN_RETRY_INTERVAL};
use crate::error::Error;
use crate::io::utils::ohttp_headers;
use crate::logger::FilesystemLogger;
use crate::types::{ChainSource, EventQueue, Wallet};
use crate::Event;

use bitcoin::block::Header;
use bitcoin::psbt::Psbt;
use bitcoin::{BlockHash, Script, Transaction, Txid};
use lightning::chain::channelmonitor::ANTI_REORG_DELAY;
use lightning::chain::transaction::TransactionData;
use lightning::chain::{BestBlock, Filter, WatchedOutput};
use lightning::util::logger::Logger;
use lightning::{log_error, log_info};

use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
enum PayjoinTransaction {
	PendingFirstConfirmation {
		tx: Transaction,
		first_broadcast_height: u32,
		first_broadcast_hash: BlockHash,
	},
	PendingThresholdConfirmations {
		tx: Transaction,
		first_broadcast_height: u32,
		first_broadcast_hash: BlockHash,
		latest_confirmation_height: u32,
		latest_confirmation_hash: BlockHash,
	},
}

impl PayjoinTransaction {
	fn txid(&self) -> Option<Txid> {
		match self {
			PayjoinTransaction::PendingFirstConfirmation { tx, .. } => Some(tx.txid()),
			PayjoinTransaction::PendingThresholdConfirmations { tx, .. } => Some(tx.txid()),
		}
	}
}

pub(crate) struct PayjoinSender {
	logger: Arc<FilesystemLogger>,
	payjoin_relay: payjoin::Url,
	chain_source: Arc<ChainSource>,
	best_known_block: Mutex<Option<BestBlock>>,
	transactions: Mutex<Vec<PayjoinTransaction>>,
	event_queue: Arc<EventQueue>,
	wallet: Arc<Wallet>,
}

impl PayjoinSender {
	pub(crate) fn new(
		logger: Arc<FilesystemLogger>, payjoin_relay: payjoin::Url, chain_source: Arc<ChainSource>,
		event_queue: Arc<EventQueue>, wallet: Arc<Wallet>,
	) -> Self {
		Self {
			logger,
			payjoin_relay,
			transactions: Mutex::new(Vec::new()),
			best_known_block: Mutex::new(None),
			chain_source,
			event_queue,
			wallet,
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

	fn internal_transactions_confirmed(
		&self, header: &Header, txdata: &TransactionData, height: u32,
	) {
		let (index, tx) = txdata[0];
		let txid = tx.txid();
		let mut transactions = self.transactions.lock().unwrap();
		let position = transactions.iter().position(|o| o.txid() == Some(txid)).unwrap();
		let pj_tx = transactions.remove(position);
		dbg!("found confirmed", &pj_tx);
		let pj_tx = match pj_tx {
			PayjoinTransaction::PendingFirstConfirmation {
				ref tx,
				first_broadcast_height,
				first_broadcast_hash,
			} => transactions.push(PayjoinTransaction::PendingThresholdConfirmations {
				tx: tx.clone(),
				first_broadcast_height,
				first_broadcast_hash,
				latest_confirmation_height: height,
				latest_confirmation_hash: header.block_hash(),
			}),
			PayjoinTransaction::PendingThresholdConfirmations {
				ref tx,
				first_broadcast_height,
				first_broadcast_hash,
				latest_confirmation_height,
				latest_confirmation_hash,
			} => {
				dbg!("Transaction confirmed here");
				dbg!("height: {}", height);
				dbg!("first_broadcast_height: {}", first_broadcast_height);
				if height - first_broadcast_height >= ANTI_REORG_DELAY {
					let _ = self.event_queue.add_event(Event::PayjoinTxSendSuccess { txid });
				} else {
					transactions.push(PayjoinTransaction::PendingThresholdConfirmations {
						tx: tx.clone(),
						first_broadcast_height,
						first_broadcast_hash,
						latest_confirmation_height: height,
						latest_confirmation_hash: header.block_hash(),
					});
				}
			},
		};
		// dbg!("here blocked");
		// *self.transactions.lock().unwrap() = transactions.clone();
		// dbg!("here2 unblocked");
	}

	pub(crate) fn finalise_payjoin_transaction(
		&self, payjoin_proposal: &mut Psbt, original_psbt: &mut Psbt,
	) -> Result<Transaction, Error> {
		let wallet = self.wallet.clone();
		wallet.sign_payjoin_proposal(payjoin_proposal, original_psbt)?;
		let tx = payjoin_proposal.clone().extract_tx();
		let our_input = tx.output.iter().find(|output| {
			wallet.is_mine(&output.script_pubkey).unwrap_or(false)
		});
		if let Some(our_input) = our_input {
			let best_known_block = self.best_known_block.lock().unwrap();
			dbg!(&best_known_block);
			let best_known_block = best_known_block.as_ref();
			dbg!(&best_known_block);
			let (current_height, current_hash) = match best_known_block {
				Some(b) => (b.height, b.block_hash),
				None => return Err(Error::PayjoinReceiverRequestValidationFailed), // fixeror
			};
			self.transactions.lock().unwrap().push(
				PayjoinTransaction::PendingFirstConfirmation {
					tx: tx.clone(),
					first_broadcast_height: current_height,
					first_broadcast_hash: current_hash,
				}
			);
			self.register_tx(&tx.txid(), &our_input.script_pubkey);
			Ok(tx)
		} else {
			Err(Error::PayjoinReceiverRequestValidationFailed) // fixeror
		}
	}
}

impl Filter for PayjoinSender {
	fn register_tx(&self, txid: &Txid, script_pubkey: &Script) {
		dbg!("Registering transaction {:?}", txid);
		self.chain_source.register_tx(txid, script_pubkey);
	}

	fn register_output(&self, output: WatchedOutput) {
		self.chain_source.register_output(output);
	}
}

impl lightning::chain::Confirm for PayjoinSender {
	fn transactions_confirmed(&self, header: &Header, txdata: &TransactionData, height: u32) {
		dbg!("Confirmed transaction");
		self.internal_transactions_confirmed(header, txdata, height);
	}
	fn transaction_unconfirmed(&self, txid: &Txid) {
		dbg!("Unconfirmed transaction {:?}", txid);
	}
	fn best_block_updated(&self, header: &Header, height: u32) {
		dbg!("Best block updated {:?}", height);
		*self.best_known_block.lock().unwrap() =
			Some(BestBlock { height, block_hash: header.block_hash() });
	}
	fn get_relevant_txids(&self) -> Vec<(Txid, u32, Option<BlockHash>)> {
		dbg!("Getting relevant txids");
		let state_lock = self.transactions.lock().unwrap();
		state_lock
			.iter()
			.filter_map(|o| match o {
				PayjoinTransaction::PendingThresholdConfirmations {
					tx,
					latest_confirmation_height,
					latest_confirmation_hash,
					..
				} => Some((
					tx.clone().txid(),
					latest_confirmation_height.clone(),
					Some(latest_confirmation_hash.clone()),
				)),
				_ => None,
			})
			.collect::<Vec<_>>()
	}
}
