// Provider contract and one concrete implementation.

export interface PaymentProvider {
    refund(amountCents: number): void;
    readonly label: string;
}

export type AmountCents = number;

export enum PaymentStatus {
    Open,
    Closed = "closed",
}
