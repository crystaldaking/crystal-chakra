//! `PaymentController`: entry point of the refund flow.

use crate::provider::PaymentProvider;
use crate::service::payment_service::PaymentService;

/// HTTP-facing controller for payment endpoints.
pub struct PaymentController<P: PaymentProvider> {
    service: PaymentService<P>,
}

impl<P: PaymentProvider> PaymentController<P> {
    pub fn new(service: PaymentService<P>) -> Self {
        Self { service }
    }

    /// Handles `POST /refunds`.
    pub fn refund(&self, transaction_id: &str, amount_cents: u64) -> Result<String, String> {
        self.service.refund(transaction_id, amount_cents)
    }
}
