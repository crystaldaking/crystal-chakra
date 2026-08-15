//! End-to-end: controller → service → provider.

use controller_service_provider::api::controller::PaymentController;
use controller_service_provider::provider::StripeProvider;
use controller_service_provider::service::payment_service::PaymentService;

#[test]
fn refund_flows_through_all_layers() {
    let provider = StripeProvider {
        api_key: "sk_test".to_owned(),
    };
    let service = PaymentService::new(provider);
    let controller = PaymentController::new(service);
    let receipt = controller.refund("tx_42", 1299);
    assert!(receipt.is_ok());
}
