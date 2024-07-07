use bitcoin::Amount;
use common::{
	setup_bitcoind_and_electrsd, setup_liquidity_client_node, setup_liquidity_provider_node,
};

use crate::common::{premine_and_distribute_funds, random_config, setup_node};

mod common;

#[test]
fn liquidity_provider() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let liquidity_provider = setup_liquidity_provider_node(&electrsd);
	let liquidity_provider_listening_address =
		liquidity_provider.listening_addresses().unwrap();
	let liquidity_provider_address =
		liquidity_provider_listening_address.get(0).unwrap();
	let liquidity_provider_node_id = liquidity_provider.node_id();
	let liquidity_provider_token = None;
	let liquidity_client = setup_liquidity_client_node(
		&electrsd,
		liquidity_provider_address.clone(),
		liquidity_provider_node_id,
		liquidity_provider_token,
	);
	let liquidity_client_peer = setup_node(&electrsd, random_config(false));

	let liquidity_provider_addr = liquidity_provider.onchain_payment().new_address().unwrap();
	let liquidity_client_peer_addr = liquidity_client_peer.onchain_payment().new_address().unwrap();

	let premine_amount_sat = 100_000_000;

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![liquidity_provider_addr, liquidity_client_peer_addr],
		Amount::from_sat(premine_amount_sat),
	);
	liquidity_provider.sync_wallets().unwrap();
	liquidity_client.sync_wallets().unwrap();
	liquidity_client_peer.sync_wallets().unwrap();
	assert_eq!(liquidity_provider.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
	assert_eq!(liquidity_client.list_balances().spendable_onchain_balance_sats, 0);
	assert_eq!(liquidity_client_peer.list_balances().spendable_onchain_balance_sats, premine_amount_sat);

	assert!(liquidity_client_peer.connect(liquidity_provider_node_id.clone(), liquidity_provider_address.clone(), true).is_ok());

	let invoice = liquidity_client.bolt11_payment().receive_via_jit_channel(100_000, "test liqudiity provider", 180, None);
	let ret = liquidity_client_peer.bolt11_payment().send(&invoice.unwrap());
	dbg!(&ret);
}
