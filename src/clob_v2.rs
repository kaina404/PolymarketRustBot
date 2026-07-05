//! Polymarket V2 CLOB client factory (shared by main bot and test binaries).

use anyhow::Result;
use alloy::signers::local::LocalSigner;
use alloy::signers::Signer as _;
use polymarket_client_sdk_v2::clob::types::SignatureType;
use polymarket_client_sdk_v2::clob::{Client, Config as ClobConfig};
use polymarket_client_sdk_v2::types::Address;
use polymarket_client_sdk_v2::{derive_proxy_wallet, derive_safe_wallet, POLYGON};
use std::str::FromStr;
use tracing::warn;

pub const CLOB_API_URL_DEFAULT: &str = "https://clob.polymarket.com";

pub type AuthenticatedClobClient = Client<
    polymarket_client_sdk_v2::auth::state::Authenticated<
        polymarket_client_sdk_v2::auth::Normal,
    >,
>;

/// Parse V2 CLOB signature type from env string.
///
/// Pick by wallet kind: email/Magic proxy → `Proxy`; browser wallet (Gnosis Safe) → `GnosisSafe`;
/// Polymarket "deposit wallet" (EIP-1271) → `Poly1271`; direct EOA → `Eoa`.
/// The funder must equal the address for that kind, else the CLOB rejects orders. When the funder
/// is a derivable proxy/safe address, `create_authenticated_clob_client` auto-corrects a wrong value.
pub fn parse_signature_type(s: &str) -> SignatureType {
    match s.trim().to_lowercase().as_str() {
        "proxy" | "magic" | "email" => SignatureType::Proxy,
        "gnosissafe" | "safe" => SignatureType::GnosisSafe,
        "poly1271" | "deposit" | "deposit_wallet" | "3" => SignatureType::Poly1271,
        "eoa" | "0" => SignatureType::Eoa,
        _ => SignatureType::Poly1271,
    }
}

/// Reconcile the configured signature type against the wallet the funder actually is.
///
/// Polymarket binds the API key to the EOA, so an order's `signer` field must be the EOA.
/// Only `Poly1271` puts the funder into `signer` instead — the CLOB then rejects every order
/// with "the order signer address has to be the address of the API KEY". Since the Magic/email
/// proxy and browser Safe wallets are deterministic CREATE2 addresses, when the funder matches
/// one of them we trust that over a mis-set env value and override it (logging the change).
/// A funder matching neither is a genuine deposit wallet and the configured type is kept.
fn resolve_signature_type(
    eoa: Address,
    funder: Option<Address>,
    configured: SignatureType,
) -> SignatureType {
    let Some(funder) = funder else {
        return configured;
    };
    let detected = if derive_proxy_wallet(eoa, POLYGON) == Some(funder) {
        Some(SignatureType::Proxy)
    } else if derive_safe_wallet(eoa, POLYGON) == Some(funder) {
        Some(SignatureType::GnosisSafe)
    } else {
        None
    };
    match detected {
        Some(detected) if detected != configured => {
            warn!(
                "SIGNATURE_TYPE={:?} does not match funder {} (derived as {:?}); \
                 overriding to {:?}. Update .env to silence this warning.",
                configured, funder, detected, detected
            );
            detected
        }
        _ => configured,
    }
}

/// Build an authenticated V2 CLOB client (EIP-712 domain v2 / pUSD).
pub async fn create_authenticated_clob_client(
    private_key: &str,
    clob_api_url: &str,
    funder_address: Option<Address>,
    signature_type: SignatureType,
) -> Result<AuthenticatedClobClient> {
    if !matches!(signature_type, SignatureType::Eoa) && funder_address.is_none() {
        anyhow::bail!(
            "POLYMARKET_PROXY_ADDRESS (deposit wallet / proxy) is required for {:?} orders",
            signature_type
        );
    }

    let signer = LocalSigner::from_str(private_key)
        .map_err(|e| anyhow::anyhow!("Invalid private key: {}", e))?
        .with_chain_id(Some(POLYGON));

    let signature_type = resolve_signature_type(signer.address(), funder_address, signature_type);

    let clob_config = ClobConfig::builder().use_server_time(true).build();
    let mut auth_builder = Client::new(clob_api_url, clob_config)?
        .authentication_builder(&signer);

    if let Some(funder) = funder_address {
        auth_builder = auth_builder
            .funder(funder)
            .signature_type(signature_type);
    }

    auth_builder
        .authenticate()
        .await
        .map_err(|e| anyhow::anyhow!("CLOB V2 auth failed: {}", e))
}

/// Parse proxy/deposit wallet address from v1 SDK Address string representation.
pub fn v1_address_to_v2(addr: polymarket_client_sdk::types::Address) -> Address {
    addr.to_string().parse().expect("valid address")
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    // Arbitrary EOA; funders below are its real CREATE2-derived proxy/safe addresses.
    const EOA: Address = address!("e840B55bBB8A37609B3B516443D3A48084a6028e");

    #[test]
    fn overrides_to_gnosis_safe_when_funder_is_derived_safe() {
        let safe = derive_safe_wallet(EOA, POLYGON).unwrap();
        // Poly1271 would put the funder in the order's signer field and the CLOB rejects it.
        assert_eq!(
            resolve_signature_type(EOA, Some(safe), SignatureType::Poly1271),
            SignatureType::GnosisSafe
        );
    }

    #[test]
    fn overrides_to_proxy_when_funder_is_derived_proxy() {
        let proxy = derive_proxy_wallet(EOA, POLYGON).unwrap();
        assert_eq!(
            resolve_signature_type(EOA, Some(proxy), SignatureType::Poly1271),
            SignatureType::Proxy
        );
    }

    #[test]
    fn keeps_configured_for_unrecognized_funder() {
        // A genuine Poly1271 deposit wallet matches neither derivation; trust the config.
        let deposit = address!("00000000000000000000000000000000DeaDBeef");
        assert_eq!(
            resolve_signature_type(EOA, Some(deposit), SignatureType::Poly1271),
            SignatureType::Poly1271
        );
    }

    #[test]
    fn keeps_configured_when_no_funder() {
        assert_eq!(
            resolve_signature_type(EOA, None, SignatureType::Eoa),
            SignatureType::Eoa
        );
    }
}
