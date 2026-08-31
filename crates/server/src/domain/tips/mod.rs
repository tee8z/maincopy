//! Static Lightning Address tip presentation.
//!
//! This domain only derives a wallet handoff. It does not resolve LNURL,
//! create invoices, observe payments, or store settlement state.

mod address;
mod handoff;

pub use address::{LightningAddress, LightningAddressError, MAX_LIGHTNING_ADDRESS_BYTES};
pub use handoff::{
    LightningWalletLink, LnurlEncodingError, LnurlPayEndpoint, LnurlPayload, TipRecipientProjection,
};
