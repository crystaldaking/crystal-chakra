package payments

import "context"

type PaymentProvider interface {
	Refund(context.Context, int) error
}

type ProviderFunc func(context.Context, int) error

func providerRefund(amount int) int {
	return amount
}
