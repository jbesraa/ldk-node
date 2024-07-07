use lightning::util::logger::Logger;
use crate::config::PAYJOIN_REQUEST_TIMEOUT;
use crate::error::Error;
use crate::io::utils::ohttp_headers;
use crate::logger::FilesystemLogger;
use crate::types::{ChainSource, EventQueue, Wallet};
use crate::Event;

use bitcoin::address::NetworkChecked;
use bitcoin::block::Header;
use bitcoin::psbt::Psbt;
use bitcoin::{Address, Amount, BlockHash, Script, Transaction, Txid};
use lightning::chain::channelmonitor::ANTI_REORG_DELAY;
use lightning::chain::transaction::TransactionData;
use lightning::chain::{BestBlock, Filter, WatchedOutput};
use lightning::log_info;

use std::sync::{Arc, RwLock};

#[derive(Clone, Debug)]
enum PayjoinTransaction {
	PendingFirstConfirmation {
		tx: Transaction,
		receiver: Address,
		amount: Amount,
		first_broadcast_height: u32,
		first_broadcast_hash: BlockHash,
	},
	PendingThresholdConfirmations {
		tx: Transaction,
		receiver: Address,
		amount: Amount,
		first_broadcast_height: u32,
		first_broadcast_hash: BlockHash,
		first_confirmation_height: u32,
		first_confirmation_hash: BlockHash,
	},
}

impl PayjoinTransaction {
	fn txid(&self) -> Option<Txid> {
		match self {
			PayjoinTransaction::PendingFirstConfirmation { tx, .. } => Some(tx.txid()),
			PayjoinTransaction::PendingThresholdConfirmations { tx, .. } => Some(tx.txid()),
		}
	}
	fn first_confirmation_height(&self) -> Option<u32> {
		match self {
			PayjoinTransaction::PendingFirstConfirmation { .. } => None,
			PayjoinTransaction::PendingThresholdConfirmations {
				first_confirmation_height, ..
			} => Some(*first_confirmation_height),
		}
	}
	fn amount(&self) -> Amount {
		match self {
			PayjoinTransaction::PendingFirstConfirmation { amount, .. } => *amount,
			PayjoinTransaction::PendingThresholdConfirmations { amount, .. } => *amount,
		}
	}
	fn receiver(&self) -> Address {
		match self {
			PayjoinTransaction::PendingFirstConfirmation { receiver, .. } => receiver.clone(),
			PayjoinTransaction::PendingThresholdConfirmations { receiver, .. } => receiver.clone(),
		}
	}
}

pub(crate) struct PayjoinHandler {
	logger: Arc<FilesystemLogger>,
	payjoin_relay: payjoin::Url,
	chain_source: Arc<ChainSource>,
	best_known_block: RwLock<Option<BestBlock>>,
	transactions: RwLock<Vec<PayjoinTransaction>>,
	event_queue: Arc<EventQueue>,
	wallet: Arc<Wallet>,
}

impl PayjoinHandler {
	pub(crate) fn new(
		logger: Arc<FilesystemLogger>, payjoin_relay: payjoin::Url, chain_source: Arc<ChainSource>,
		event_queue: Arc<EventQueue>, wallet: Arc<Wallet>,
	) -> Self {
		Self {
			logger,
			payjoin_relay,
			transactions: RwLock::new(Vec::new()),
			best_known_block: RwLock::new(None),
			chain_source,
			event_queue,
			wallet,
		}
	}

	pub(crate) fn payjoin_relay(&self) -> &payjoin::Url {
		&self.payjoin_relay
	}

	pub(crate) async fn send_request(&self, request: &payjoin::Request) -> Result<Vec<u8>, Error> {
		let response = reqwest::Client::new()
			.post(request.url.clone())
			.body(request.body.clone())
			.timeout(PAYJOIN_REQUEST_TIMEOUT)
			.headers(ohttp_headers())
			.send()
			.await?;
		let response = response.error_for_status()?;
		let response = response.bytes().await?;
		let response = response.to_vec();
		Ok(response)
	}

	pub(crate) fn finalise_payjoin_transaction(
		&self, payjoin_proposal: &mut Psbt, original_psbt: &mut Psbt,
		payjoin_uri: payjoin::Uri<NetworkChecked>,
	) -> Result<Transaction, Error> {
		let wallet = self.wallet.clone();
		wallet.sign_payjoin_proposal(payjoin_proposal, original_psbt)?;
		let tx = payjoin_proposal.clone().extract_tx();
		let our_input =
			tx.output.iter().find(|output| wallet.is_mine(&output.script_pubkey).unwrap_or(false));
		if let Some(our_input) = our_input {
			let best_known_block = self.best_known_block.read().unwrap().clone();
			let (current_height, current_hash) = match best_known_block {
				Some(b) => (b.height, b.block_hash),
				None => return Err(Error::PayjoinReceiverRequestValidationFailed), // fixeror
			};
			self.transactions.write().unwrap().push(PayjoinTransaction::PendingFirstConfirmation {
				tx: tx.clone(),
				receiver: payjoin_uri.address,
				amount: payjoin_uri.amount.unwrap_or_default(),
				first_broadcast_height: current_height,
				first_broadcast_hash: current_hash,
			});
			self.register_tx(&tx.txid(), &our_input.script_pubkey);
			Ok(tx)
		} else {
			Err(Error::PayjoinReceiverRequestValidationFailed) // fixeror
		}
	}

	fn internal_transactions_confirmed(
		&self, header: &Header, txdata: &TransactionData, height: u32,
	) {
		let (_, tx) = txdata[0];
		let confirmed_tx_txid = tx.txid();
		let mut transactions = self.transactions.write().unwrap();
		let position = match transactions.iter().position(|o| o.txid() == Some(confirmed_tx_txid)) {
			Some(position) => position,
			None => {
				log_info!(self.logger, "Confirmed transaction {} not found in payjoin transactions", confirmed_tx_txid);
				return
			},
		};
		let pj_tx = transactions.remove(position);
		match pj_tx {
			PayjoinTransaction::PendingFirstConfirmation {
				ref tx,
				receiver,
				amount,
				first_broadcast_height,
				first_broadcast_hash,
			} => {
				transactions.push(PayjoinTransaction::PendingThresholdConfirmations {
					tx: tx.clone(),
					receiver,
					amount,
					first_broadcast_height,
					first_broadcast_hash,
					first_confirmation_height: height,
					first_confirmation_hash: header.block_hash(),
				});
			},
			_ => {
				unreachable!()
			},
		};
	}
}

impl Filter for PayjoinHandler {
	fn register_tx(&self, txid: &Txid, script_pubkey: &Script) {
		self.chain_source.register_tx(txid, script_pubkey);
	}

	fn register_output(&self, output: WatchedOutput) {
		self.chain_source.register_output(output);
	}
}

impl lightning::chain::Confirm for PayjoinHandler {
	fn transactions_confirmed(&self, header: &Header, txdata: &TransactionData, height: u32) {
		self.internal_transactions_confirmed(header, txdata, height);
	}

	fn transaction_unconfirmed(&self, _txid: &Txid) {}

	fn best_block_updated(&self, header: &Header, height: u32) {
		*self.best_known_block.write().unwrap() =
			Some(BestBlock { height, block_hash: header.block_hash() });
		let mut transactions = self.transactions.write().unwrap();
		transactions.retain(|tx| {
			if let (Some(first_conf), Some(txid)) = (tx.first_confirmation_height(), tx.txid()) {
				if height - first_conf >= ANTI_REORG_DELAY {
					let _ = self.event_queue.add_event(Event::PayjoinPaymentSuccess {
						txid,
						amount: tx.amount().to_sat(),
						receipient: tx.receiver().into(),
					});
					false
				} else {
					true
				}
			} else {
				true
			}
		});
	}

	fn get_relevant_txids(&self) -> Vec<(Txid, u32, Option<BlockHash>)> {
		let state_lock = self.transactions.read().unwrap();
		state_lock
			.iter()
			.filter_map(|o| match o {
				PayjoinTransaction::PendingThresholdConfirmations {
					tx,
					first_confirmation_height,
					first_confirmation_hash,
					..
				} => Some((
					tx.clone().txid(),
					first_confirmation_height.clone(),
					Some(first_confirmation_hash.clone()),
				)),
				_ => None,
			})
			.collect::<Vec<_>>()
	}
}
