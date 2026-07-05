//! Wallet execution routing for relayer / Safe on-chain ops.

use std::env;

use crate::deposit_wallet_relay::use_deposit_wallet_relayer;

#[derive(Debug, Clone, Copy)]
pub enum WalletKind {
    DepositWallet,
    MagicProxy,
    GnosisSafe,
}

/// Route on-chain execution (merge) by wallet type.
///
/// `SIGNATURE_TYPE` takes precedence over the code-length heuristic: a Polymarket
/// Gnosis Safe (v1.3.0) proxy runtime is only ~124 bytes, so `code_len < 150` alone
/// misroutes it to the relayer `MagicProxy` path — whose CREATE2 guard then rejects the
/// safe-derived proxy — instead of the Safe `execTransaction` path. Spellings mirror
/// `parse_signature_type` / `use_relayer_by_config`. Falls back to code length when unset
/// or unknown.
pub fn classify_wallet(code_len: usize) -> WalletKind {
    let sig = env::var("SIGNATURE_TYPE").ok();
    classify(sig.as_deref(), use_deposit_wallet_relayer(), code_len)
}

fn classify(signature_type: Option<&str>, is_deposit_wallet: bool, code_len: usize) -> WalletKind {
    if is_deposit_wallet {
        return WalletKind::DepositWallet;
    }
    match signature_type.map(|v| v.trim().to_lowercase()).as_deref() {
        Some("gnosissafe") | Some("safe") => WalletKind::GnosisSafe,
        Some("proxy") | Some("magic") | Some("email") => WalletKind::MagicProxy,
        _ if code_len < 150 => WalletKind::MagicProxy,
        _ => WalletKind::GnosisSafe,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gnosis_safe_type_routes_to_safe_even_when_proxy_code_is_small() {
        // Polymarket Gnosis Safe v1.3.0 proxy runtime is ~124 bytes (< 150), but an
        // explicit SIGNATURE_TYPE=GnosisSafe must still route to the Safe execTransaction path.
        assert!(matches!(classify(Some("GnosisSafe"), false, 124), WalletKind::GnosisSafe));
        assert!(matches!(classify(Some("safe"), false, 124), WalletKind::GnosisSafe));
    }

    #[test]
    fn proxy_type_routes_to_magic_proxy() {
        assert!(matches!(classify(Some("proxy"), false, 124), WalletKind::MagicProxy));
        assert!(matches!(classify(Some("magic"), false, 45), WalletKind::MagicProxy));
    }

    #[test]
    fn deposit_wallet_takes_precedence() {
        assert!(matches!(classify(Some("gnosissafe"), true, 124), WalletKind::DepositWallet));
    }

    #[test]
    fn falls_back_to_code_len_when_type_unknown() {
        assert!(matches!(classify(None, false, 45), WalletKind::MagicProxy));
        assert!(matches!(classify(None, false, 200), WalletKind::GnosisSafe));
        assert!(matches!(classify(Some("eoa"), false, 200), WalletKind::GnosisSafe));
    }
}
