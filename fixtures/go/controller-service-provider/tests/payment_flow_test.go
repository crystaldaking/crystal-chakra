package payments

import "testing"

func TestRefundFlow(t *testing.T) {
	t.Helper()
	if got := handleRefund(42); got != 42 {
		t.Fatalf("refund = %d", got)
	}
}
