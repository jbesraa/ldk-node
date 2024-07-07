use bitcoin::Amount;
use common::setup_bitcoind_and_electrsd;

use crate::common::{generate_blocks_and_wait, premine_and_distribute_funds, random_config, setup_node, setup_two_nodes};

mod common;

#[test]
fn rbf_drain_transaction() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let (node_a, node_b) = setup_two_nodes(&electrsd, true, false ,false);
	let node_c = setup_node(&electrsd, random_config(false));

	let node_a_addr = node_a.onchain_payment().new_address().unwrap();
	let node_b_addr = node_b.onchain_payment().new_address().unwrap();

	let premine_amount_sat = 1000;

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![node_a_addr.clone(), node_b_addr],
		Amount::from_sat(premine_amount_sat),
	);
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
	assert_eq!(node_b.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
	let txid = node_b.onchain_payment().send_all_to_address(&node_a_addr).unwrap();
	dbg!(&txid);
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	assert_eq!(node_b.list_balances().spendable_onchain_balance_sats, 0);
	assert_eq!(node_a.list_balances().total_onchain_balance_sats, 1886);
	let node_b_payjoin_payment = node_b.payjoin_payment();
	let payjoin_uri = node_b_payjoin_payment.receive(Amount::from_sat(500)).unwrap();
	let ret = node_c.payjoin_payment().send(payjoin_uri.clone().to_string());
	// now `node_b` should receive the payjoin payment and substitue the address with `node_a`
	// address and add the inputs from the first transaction
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	node_c.sync_wallets().unwrap();
	assert!(node_a.list_balances().total_onchain_balance_sats > premine_amount_sat*2);
	dbg!(&txid);
}

