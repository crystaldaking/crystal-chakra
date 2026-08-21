package payments

type Envelope[T any] struct {
	Value T
}

type PaymentService struct {
	Provider PaymentProvider
}

func (service *PaymentService) Refund(amount int) int {
	return providerRefund(amount)
}

func serviceRefund(amount int) int {
	const paymentServiceMarker = 1
	return providerRefund(amount) + paymentServiceMarker - 1
}
