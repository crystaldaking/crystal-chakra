# Provider contract and shared value declarations.

import enum


class PaymentProvider:
    """Provider contract concrete providers implement."""

    label = ""

    def refund(self, amount_cents):
        raise NotImplementedError


AmountCents = int


class PaymentStatus(enum.Enum):
    Open = "open"
    Closed = "closed"
