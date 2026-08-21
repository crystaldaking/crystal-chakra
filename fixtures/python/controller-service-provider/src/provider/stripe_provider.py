from provider.provider import PaymentProvider
from shared import record_event as audit_event


class StripeProvider(PaymentProvider):
    label = "stripe"

    def __init__(self, api_key):
        self.api_key = api_key

    def refund(self, amount_cents):
        self.charge(amount_cents)
        audit_event("refund")

    def charge(self, amount_cents):
        if amount_cents <= 0:
            raise ValueError("amount must be positive")
