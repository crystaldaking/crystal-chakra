from api.controller import PaymentController
from service.payment_service import PaymentService


def test_refund_delegates_to_provider():
    controller = PaymentController()
    controller.refund(1250)


def test_service_refund_reaches_the_shared_target():
    service = PaymentService()
    service.refund(500)
