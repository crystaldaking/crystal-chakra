from provider.provider import PaymentProvider
from provider.stripe_provider import StripeProvider
from shared import shared_unique_target


class PaymentService:
    def __init__(self):
        self.provider = StripeProvider("test-key")

    def refund(self, amount_cents):
        shared_unique_target()
        self.provider.refund(amount_cents)


def build_payment_service():
    return PaymentService()
