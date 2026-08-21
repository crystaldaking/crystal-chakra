package conformance

import sharedalias "example.com/chakra/conformance/shared"

// CONFORMANCE_TEXT_NEEDLE: payment pipeline marker
func dispatchConformanceRequest() {
	_ = sharedalias.Marker
	sharedUniqueTarget()
	recordConformanceEvent()
}
