use std::time::Duration;

use bitcoin::{
    hashes::{Hash, sha256},
    secp256k1::{Secp256k1, SecretKey},
};
use lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};

use super::Bolt11Invoice;

pub(super) fn signed_direct_invoice(description: &str, amount_sats: u64) -> Bolt11Invoice {
    signed_direct_invoice_with(
        Currency::Bitcoin,
        description,
        amount_sats,
        Duration::from_secs(3_600),
    )
}

pub(super) fn signed_direct_invoice_with(
    currency: Currency,
    description: &str,
    amount_sats: u64,
    expiry: Duration,
) -> Bolt11Invoice {
    let payment_hash = sha256::Hash::from_byte_array([1; 32]);
    let private_key = SecretKey::from_slice(&[42; 32]).unwrap();
    let secp = Secp256k1::new();
    let invoice = InvoiceBuilder::new(currency)
        .amount_milli_satoshis(amount_sats * 1_000)
        .duration_since_epoch(Duration::from_secs(1_700_000_000))
        .description(description.to_owned())
        .payment_hash(payment_hash)
        .payment_secret(PaymentSecret([42; 32]))
        .expiry_time(expiry)
        .min_final_cltv_expiry_delta(18)
        .build_signed(|message| secp.sign_ecdsa_recoverable(message, &private_key))
        .unwrap();

    Bolt11Invoice::parse(&invoice.to_string()).unwrap()
}
