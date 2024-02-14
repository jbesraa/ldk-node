/*
 *
 *
 * HTTPS SERVER
 */

use axum::Router;

pub struct HttpServer;

impl HttpServer {
	pub async fn new(port: u16, router: Router) {
		let url = format!("0.0.0.0:{}", port);
		let listener = tokio::net::TcpListener::bind(url).await.unwrap();
		axum::serve(listener, router).await.unwrap();
	}
}

/*
 *
 *
 * PAYJOIN RECEIVER
 */

pub mod payjoin_receiver {
	use axum::extract::State;
	use axum::http::HeaderMap;
	use axum::response::IntoResponse;
	use axum::routing::post;
	use axum::{extract::Request, Router};
	use bitcoin::address::NetworkChecked;
	use bitcoin::psbt::Psbt;
	use bitcoin::{base64, Address};
	use bitcoincore_rpc::RpcApi;
	use http_body_util::BodyExt;
	use payjoin::bitcoin::{self, Amount};
	use payjoin::receive::{PayjoinProposal, ProvisionalProposal};
	use payjoin::Uri;
	use std::sync::Arc;
	use std::{collections::HashMap, str::FromStr};

	use crate::types::Wallet;

	use super::HttpServer;

	struct Headers(HeaderMap);

	impl payjoin::receive::Headers for Headers {
		fn get_header(&self, key: &str) -> Option<&str> {
			self.0.get(key).and_then(|v| v.to_str().ok())
		}
	}

	fn build_pj_uri(
		address: bitcoin::Address, amount: Amount, pj: &'static str,
	) -> Uri<'static, NetworkChecked> {
		let pj_uri_string = format!("{}?amount={}&pj={}", address.to_qr_uri(), amount.to_btc(), pj);
		let pj_uri = Uri::from_str(&pj_uri_string).unwrap();
		pj_uri.assume_checked()
	}

	// Payjoin receiver
	//
	// This is the code that receives a Payjoin request from a sender.
	//
	// The receiver flow is:
	// 1. Extracting request data
	// 2  Check if the Original PSBT can be broadcast
	// 3. Check if the sender is trying to make us sign our own inputs
	// 4. Check if there are mixed input scripts, breaking stenographic privacy
	// 5. Check if we have seen this input before
	// 6. Augment a valid proposal to preserve privacy
	// 7. Extract the payjoin PSBT and sign it
	// 8. Respond to the sender's http request with the signed PSBT as payload
	pub struct Receiver {
		wallet: Arc<Wallet>,
	}

	impl Receiver {
		pub async fn handle_pj_request(
			State(wallet): State<Arc<Wallet>>, request: Request,
		) -> impl IntoResponse {
			// let receiver_wallet = unimplemented!();
			// Step 0: extract request data
			let (parts, body) = request.into_parts();
			let bytes = body.collect().await.unwrap().to_bytes();
			let headers = Headers(parts.headers.clone());
			let proposal =
				payjoin::receive::UncheckedProposal::from_request(&bytes[..], "", headers).unwrap();

			let min_fee_rate = None;
			// Step 1: Can the Original PSBT be Broadcast?
			// We need to know this transaction is consensus-valid.
			let checked_1 =
				proposal.check_broadcast_suitability(min_fee_rate, |tx| Ok(true)).unwrap();
			// Step 2: Is the sender trying to make us sign our own inputs?
			let checked_2 = checked_1.check_inputs_not_owned(|input| Ok(true)).unwrap();
			// Step 3: Are there mixed input scripts, breaking stenographic privacy?
			let checked_3 = checked_2.check_no_mixed_input_scripts().unwrap();
			// Step 4: Have we seen this input before?
			//
			// Non-interactive i.e. payment processors should be careful to keep track
			// of request inputs or else a malicious sender may try and probe
			// multiple responses containing the receiver utxos, clustering their wallet.
			let checked_4 = checked_3.check_no_inputs_seen_before(|_outpoint| Ok(false)).unwrap();
			// Step 5. Augment a valid proposal to preserve privacy
			//
			// Here's where the PSBT is modified.
			// Inputs may be added to break common input ownership heurstic.
			// There are a number of ways to select coins and break common input heuristic but
			// fail to preserve privacy because of  Unnecessary Input Heuristic (UIH).
			// Until February 2023, even BTCPay occasionally made these errors.
			// Privacy preserving coin selection as implemented in `try_preserving_privacy`
			// is precarious to implement yourself may be the most sensitive and valuable part of this kit.
			//
			// Output substitution is another way to improve privacy and increase functionality.
			// For example, if the Original PSBT output address paying the receiver is coming from a static URI,
			// a new address may be generated on the fly to avoid address reuse.
			// This can even be done from a watch-only wallet.
			// Output substitution may also be used to consolidate incoming funds to a remote cold wallet,
			// break an output into smaller UTXOs to fulfill exchange orders, open lightning channels, and more.
			//
			//
			// Using methods for coin selection not provided by this library may have dire implications for privacy.
			// Significant in-depth research and careful implementation iteration has
			// gone into privacy preserving transaction construction.
			let mut prov_proposal =
				checked_4.identify_receiver_outputs(|output_script| Ok(true)).unwrap();
			let unspent = wallet.list_unspent().unwrap();
			let _ = Self::try_contributing_inputs(&mut prov_proposal, unspent);
			// Select receiver payjoin inputs.
			let receiver_substitute_address = wallet.get_new_address().unwrap();
			prov_proposal.substitute_output_address(receiver_substitute_address);
			// Step 6. Extract the payjoin PSBT and sign it
			//
			// Fees are applied to the augmented Payjoin Proposal PSBT using calculation factoring both receiver's
			// preferred feerate and the sender's fee-related [optional parameters]
			// (https://github.com/bitcoin/bips/blob/master/bip-0078.mediawiki#optional-parameters).
			let payjoin_proposal: PayjoinProposal = prov_proposal
				.finalize_proposal(
					|psbt: &Psbt| Ok(wallet.wallet_process_psbt(psbt).unwrap()),
					Some(payjoin::bitcoin::FeeRate::MIN),
				)
				.unwrap();
			// Step 7. Respond to the sender's http request with the signed PSBT as payload
			//
			// BIP 78 senders require specific PSBT validation constraints regulated by prepare_psbt.
			// PSBTv0 was not designed to support input/output modification,
			// so the protocol requires this precise preparation step. A future PSBTv2 payjoin protocol may not.
			//
			// It is critical to pay special care when returning error response messages.
			// Responding with internal errors can make a receiver vulnerable to sender probing attacks which cluster UTXOs.
			let payjoin_proposal_psbt = payjoin_proposal.psbt();
			payjoin_proposal_psbt.to_string()
		}

		fn try_contributing_inputs(
			provisional_proposal: &mut ProvisionalProposal, unspent: Vec<bdk::LocalUtxo>,
		) -> Result<(), ()> {
			use payjoin::bitcoin::OutPoint;

			let available_inputs = unspent;
			let candidate_inputs: HashMap<payjoin::bitcoin::Amount, OutPoint> = available_inputs
				.iter()
				.map(|i| {
					(
						payjoin::bitcoin::Amount::from_sat(i.txout.value),
						OutPoint { txid: i.outpoint.txid, vout: i.outpoint.vout },
					)
				})
				.collect();

			let selected_outpoint =
				provisional_proposal.try_preserving_privacy(candidate_inputs).unwrap();
			let selected_utxo = available_inputs
				.iter()
				.find(|i| {
					i.outpoint.txid == selected_outpoint.txid
						&& i.outpoint.vout == selected_outpoint.vout
				})
				.unwrap();

			// calculate receiver payjoin outputs given receiver payjoin inputs and original_psbt
			let txo_to_contribute = payjoin::bitcoin::TxOut {
				value: selected_utxo.txout.value,
				script_pubkey: selected_utxo.txout.script_pubkey.clone(),
			};
			let outpoint_to_contribute = payjoin::bitcoin::OutPoint {
				txid: selected_utxo.outpoint.txid,
				vout: selected_utxo.outpoint.vout,
			};
			provisional_proposal
				.contribute_witness_input(txo_to_contribute, outpoint_to_contribute);
			Ok(())
		}
	}
}
