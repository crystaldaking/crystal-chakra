// Service layer wired with CommonJS require bindings.

const { StripeProvider } = require("../provider/stripeProvider.js");
const { sharedUniqueTarget } = require("../shared.js");

class PaymentService {
    constructor() {
        this.provider = new StripeProvider("test-key");
    }

    refund(amountCents) {
        sharedUniqueTarget();
        this.provider.refund(amountCents);
    }
}

function buildPaymentService() {
    return new PaymentService();
}

module.exports = { PaymentService, buildPaymentService };
