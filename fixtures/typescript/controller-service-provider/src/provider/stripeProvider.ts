import { PaymentProvider } from "./provider";
import { recordEvent as auditEvent } from "../shared";

export class StripeProvider implements PaymentProvider {
    readonly label: string = "stripe";
    private apiKey: string = "";

    constructor(apiKey: string) {
        this.apiKey = apiKey;
    }

    refund(amountCents: number): void {
        this.charge(amountCents);
        auditEvent("refund");
    }

    private charge(amountCents: number): void {
        if (amountCents <= 0) {
            throw new Error("amount must be positive");
        }
    }
}
