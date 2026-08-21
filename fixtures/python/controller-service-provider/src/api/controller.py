from service.payment_service import PaymentService, build_payment_service


class PaymentController:
    def __init__(self):
        self.service = build_payment_service()

    def refund(self, amount_cents):
        self.service.refund(amount_cents)
