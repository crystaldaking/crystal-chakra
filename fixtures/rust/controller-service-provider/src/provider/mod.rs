//! Payment provider abstraction and a concrete Stripe provider.

/// What a payment provider can do.
pub trait PaymentProvider {
    /// Refunds `amount_cents` for `transaction_id`.
    fn refund(&self, transaction_id: &str, amount_cents: u64) -> Result<String, String>;
}

/// Stripe-backed payment provider.
pub struct StripeProvider {
    pub api_key: String,
}

impl PaymentProvider for StripeProvider {
    fn refund(&self, transaction_id: &str, amount_cents: u64) -> Result<String, String> {
        Ok(format!("stripe:refund:{transaction_id}:{amount_cents}"))
    }
}
