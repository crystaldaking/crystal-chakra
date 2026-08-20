// Concrete provider: CommonJS require bindings and module.exports.

const { PaymentProvider } = require("./provider.js");
const { recordEvent: auditEvent } = require("../shared.js");

class StripeProvider extends PaymentProvider {
    label = "stripe";
    apiKey = "";

    constructor(apiKey) {
        super();
        this.apiKey = apiKey;
    }

    refund(amountCents) {
        this.charge(amountCents);
        auditEvent("refund");
    }

    charge(amountCents) {
        if (amountCents <= 0) {
            throw new Error("amount must be positive");
        }
    }
}

module.exports = { StripeProvider };
