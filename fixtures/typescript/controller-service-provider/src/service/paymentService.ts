import { PaymentProvider } from "../provider/provider";
import { StripeProvider } from "../provider/stripeProvider";
import { sharedUniqueTarget } from "../shared";

export class PaymentService {
    private provider: PaymentProvider;

    constructor() {
        this.provider = new StripeProvider("test-key");
    }

    refund(amountCents: number): void {
        sharedUniqueTarget();
        this.provider.refund(amountCents);
    }
}

export function buildPaymentService(): PaymentService {
    return new PaymentService();
}
