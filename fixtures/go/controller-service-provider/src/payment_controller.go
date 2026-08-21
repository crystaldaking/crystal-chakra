package payments

type PaymentController struct {
	Service *PaymentService
}

func handleRefund(amount int) int {
	return serviceRefund(amount)
}
