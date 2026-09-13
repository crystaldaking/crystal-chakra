package conformance

import conformance.sharedUniqueTarget as sharedalias

// CONFORMANCE_TEXT_NEEDLE: payment pipeline marker
fun dispatchConformanceRequest() {
    sharedalias()
    sharedUniqueTarget()
    recordConformanceEvent()
}
