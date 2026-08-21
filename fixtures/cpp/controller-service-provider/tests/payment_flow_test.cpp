#include "payment_service.hpp"

using chakra::payments::PaymentController;
using chakra::payments::PaymentProvider;
using chakra::payments::PaymentService;

void test_refund_flow() {
  PaymentProvider provider;
  PaymentService service(provider);
  PaymentController controller(service);
  controller.handle_refund("payment-42");
  chakra::payments::controller_refund();
}
