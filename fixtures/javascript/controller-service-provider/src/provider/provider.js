// Provider contract as a base class plus shared value declarations.

export class PaymentProvider {
    label = "";

    refund(amountCents) {
        throw new Error("not implemented");
    }
}

export const PaymentStatus = {
    Open: "open",
    Closed: "closed",
};
