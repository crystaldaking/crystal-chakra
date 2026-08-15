//! `PaymentService`: refund business logic on top of a provider.

use crate::provider::PaymentProvider;

/// Payment business operations, generic over the payment provider.
pub struct PaymentService<P: PaymentProvider> {
    provider: P,
}

impl<P: PaymentProvider> PaymentService<P> {
    pub fn new(provider: P) -> Self {
        Self { provider }
    }

    /// Refunds `amount_cents` for `transaction_id`; rejects zero amounts.
    pub fn refund(&self, transaction_id: &str, amount_cents: u64) -> Result<String, String> {
        if amount_cents == 0 {
            return Err("amount must be positive".to_owned());
        }
        self.provider.refund(transaction_id, amount_cents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::StripeProvider;

    fn service() -> PaymentService<StripeProvider> {
        PaymentService::new(StripeProvider {
            api_key: "sk_test".to_owned(),
        })
    }

    #[test]
    fn refund_delegates_to_provider() {
        let receipt = service().refund("tx_1", 500);
        assert!(receipt.is_ok());
    }

    #[test]
    fn refund_rejects_zero_amount() {
        assert!(service().refund("tx_1", 0).is_err());
    }
}
